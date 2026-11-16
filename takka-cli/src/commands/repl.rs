use anyhow::Result;
use ratatui::style::{Color as RColor, Modifier as RModifier, Style as RStyle};
use ratatui::text::{Line as RLine, Span as RSpan};
use std::sync::atomic::{AtomicBool, Ordering};
use takka_core::sync;

use crate::confirmation::{prompt_confirmation, ConfirmationResponse};

use super::*;

impl Commands {
    // ── Run task ────────────────────────────────────────────

    pub async fn run(&mut self, task: &str) -> Result<()> {
        use crate::widgets::{components, inline, markdown as md_widget, progress};

        // When --mode plan is active, route through the planner agent
        // instead of the regular orchestrator loop. This creates a real
        // plan, shows it, and executes approved steps.
        if self.permission_mode == takka_core::PermissionMode::Plan {
            return self.plan_run(task).await;
        }

        // Expand @file references and extract any image attachments
        // (`@photo.png`) for the vision channel. Pending attachments
        // queued by `/image` are drained here too.
        let expanded = crate::file_ref::expand_with_attachments(task);
        let mut attachments = self.drain_pending_attachments();
        attachments.extend(expanded.attachments);
        let task = &expanded.text;

        if !attachments.is_empty() {
            // Pre-flight capability check so we fail fast with a clear,
            // user-actionable error instead of waiting on a provider 4xx.
            let caps = self.active_model_capabilities();
            if !caps.vision {
                inline::print_blank();
                inline::print_line(&components::error_badge(
                    "Active model does not support image input.",
                ));
                inline::print_line(&RLine::from(vec![
                    RSpan::raw("  Switch with "),
                    RSpan::styled("/models", RStyle::default().add_modifier(RModifier::BOLD)),
                    RSpan::raw(" to a vision-capable model (e.g. gpt-4o, claude-3-5-sonnet)."),
                ]));
                inline::print_blank();
                return Ok(());
            }
        }

        self.ensure_connection().await?;
        #[cfg(test)]
        self.test_runs
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // Runtime events are forwarded to the existing single-writer renderer.
        let show_tool_calls = self.config.output.show_tool_calls;
        // The model's pre-tool reasoning is rendered as a DIM "thinking" block
        // when enabled (config `output.show_thoughts`, `--no-thoughts`,
        // `/thoughts`). The full text still reaches the renderer either way to
        // drive the status line's phase.
        let show_thoughts = self.show_thoughts();
        // `/timeline compact` keeps one row per tool: no streamed output, no
        // error bodies, no diff hunks.
        let timeline_detailed = self.timeline_style() == TimelineStyle::Detailed;
        // ── Live rendering strategy (single writer) ───────────
        //
        // ALL terminal output during the run flows through ONE task
        // (the `renderer` below). This fixes two bugs the old two-writer
        // design suffered from:
        //
        //   1. Corruption during tool use: the spinner ticker and the
        //      on_chunk callback wrote to stdout from different tasks and
        //      raced on cursor position, so streamed-text fragments
        //      (e.g. a command tail like `-maxdepth 4 …`) leaked into the
        //      "Thinking…" spinner line.
        //   2. Duplicate final text: the end-of-run markdown re-render
        //      miscounted visual lines (it ignored line wrapping and the
        //      tool-panel lines interleaved between streamed turns), so
        //      streamed plaintext was left behind after the re-render.
        //
        // Now chunks are forwarded to a channel and printed by the SAME
        // task that ticks the spinner and renders events, so the three
        // can never interleave. A state machine maintains the invariant
        // "the spinner is always the last printed line, cursor directly
        // below it"; streamed text replaces the spinner in place, and a
        // tool panel always ends by re-arming a fresh spinner line.
        //
        // At the end of the run, the accumulated text block is rendered
        // as styled markdown (instead of leaving raw plaintext on screen),
        // giving the same polished output the old markdown re-render aimed
        // for — but without line-counting bugs, because the renderer tracks
        // visual lines accurately for the ONE block it replaces.

        /// One queue item: a streamed delta or a lifecycle event (story 1.3).
        ///
        /// ONE ordered queue, drained in arrival order: stdout positions fall
        /// out of the queue itself, so text announced live sits above the
        /// tool/thinking row that arrived after it. The order is total
        /// because every runtime mode folds deltas and events through the
        /// same async `run_turn` loop (`takka-cli/src/client.rs`) — two
        /// separate channels drained chunks-first-every-tick could print a
        /// tool row below text that arrived after it (the same-tick
        /// inversion story 1.1 kept in scope).
        pub(crate) enum RendererUpdate {
            /// A streamed answer delta (appends to the preview text).
            Chunk(String),
            /// A lifecycle event (tool row / thinking block / badges).
            Event(takka_core::Event),
            /// The current mid-run input buffer (story 2.1): the
            /// renderer draws it as one transient line above the status
            /// row (empty string = the input row collapses).
            InputEcho(String),
        }

        let (update_tx, update_rx) = std::sync::mpsc::channel::<RendererUpdate>();

        let progress = std::sync::Arc::new(std::sync::Mutex::new({
            let mut p = progress::ProgressState::new();
            p.start();
            // The connection is already established by `ensure_connection`;
            // what remains is waiting on the model.
            p.set_phase(progress::Phase::Thinking);
            p
        }));
        let stop_flag = std::sync::Arc::new(AtomicBool::new(false));

        // Mark the spinner renderer as alive before spawning it: the
        // question handler reads this to decide whether it must consume
        // / re-arm the transient spinner line around its prompt.
        SPINNER_ACTIVE.store(true, Ordering::Relaxed);

        // Renderer state machine.
        //
        // Strategy: NEVER write streamed chunks to stdout.  Only the
        // spinner line and tool panels touch the terminal.  At the end
        // of the run, the spinner is cleared and the accumulated text
        // is rendered as styled markdown in one shot.  This eliminates
        // ALL cursor-position bugs (line-counting, MoveUp overshoot,
        // trailing-newline ambiguity) because there is nothing to
        // "replace" — the markdown is the first and only text output.
        pub(crate) struct RenderFinal {
            /// Accumulated text from all streaming blocks.
            pub(crate) block_text: String,
            /// Plain text of the preview lines printed live above the
            /// status line, in print order (empty when nothing was
            /// previewed — the no-chunk fallback signal).
            pub(crate) preview_committed: Vec<String>,
            /// True when the progressive re-render stopped extending the
            /// printed prefix mid-run; the end-of-run path then keeps
            /// today's full markdown render.
            pub(crate) preview_diverged: bool,
            /// cfg(test) only: the interleaved print order of the run —
            /// `text:<line>` for live preview lines, `tool:<name>` for
            /// timeline rows, `thinking` for thinking blocks. The e2e
            /// smokes assert the story-1.3 interleave against this list.
            #[cfg(test)]
            pub(crate) render_order: Vec<String>,
            /// cfg(test) only: the mid-run input echo frames (story 2.1).
            #[cfg(test)]
            pub(crate) input_echoes: Vec<String>,
        }

        let r_progress = progress.clone();
        let r_stop = stop_flag.clone();
        let render_panels = show_tool_calls;
        let renderer = std::thread::Builder::new()
            .name("takka-renderer".into())
            .spawn(move || {
                let block_text = std::sync::Mutex::new(String::new());

                // ── status line painter ────────────────────────────────
                // Shared with the console-log hook below, which repaints the
                // status line after printing a log line.
                let painter = std::sync::Arc::new(std::sync::Mutex::new(StatusPainter::new()));
                let commit_spinner = {
                    let painter = painter.clone();
                    let r_progress = r_progress.clone();
                    move || sync::lock(&painter).commit(&r_progress)
                };
                let redraw_spinner = {
                    let painter = painter.clone();
                    let r_progress = r_progress.clone();
                    move || sync::lock(&painter).redraw(&r_progress)
                };

                // Live-preview state (story 1.1 machinery, unchanged): the
                // plain text of the lines already printed, the completed
                // text they were rendered from (skips re-parsing when
                // nothing new completed), and the divergence flag that
                // ends previewing for this run.
                let mut preview = PreviewTracker::default();

                // Print the initial spinner.
                commit_spinner();

                // Route console logs around the spinner line: while this hook
                // is installed, every stderr log line erases the spinner,
                // prints itself, and re-arms the spinner, so the renderer's
                // "spinner is the last line" invariant survives log output
                // from other threads (config load, transport warnings, ...).
                // Story-2.1 review fix: the hook consumes the whole
                // transient band (status + the mid-run input row) — the
                // input row is re-rendered by the reader's next echo frame.
                // The transient input row's liveness: shared with every
                // path whose erase must consume the whole band (hook,
                // announces, event rows) — story-2.1 review fix.
                let input_rows: std::sync::Arc<std::sync::atomic::AtomicUsize> = Default::default();
                let _log_hook_guard;
                {
                    let hook_painter = painter.clone();
                    let hook_progress = r_progress.clone();
                    let hook_input_rows = input_rows.clone();
                    let hook: crate::widgets::log_writer::SpinnerHook =
                        std::sync::Arc::new(move |bytes: &[u8]| {
                            // try_lock: the gate may already be held by us (a
                            // widget that logs) or by an interactive prompt.
                            // Blocking there would deadlock; fall back to a
                            // plain write instead.
                            let gate = RENDER_GATE.try_lock();
                            if gate.is_err() {
                                crate::widgets::log_writer::write_line_to_stderr(bytes);
                                return;
                            }

                            let depth = 1 + hook_input_rows.load(Ordering::Relaxed);
                            let mut s = std::io::stdout();
                            {
                                use crossterm::cursor::{MoveToColumn, MoveUp};
                                use crossterm::terminal::{Clear, ClearType};
                                use crossterm::ExecutableCommand;
                                for _ in 0..depth {
                                    let _ = s.execute(MoveUp(1));
                                }
                                let _ = s.execute(MoveToColumn(0));
                                let _ = s.execute(Clear(ClearType::FromCursorDown));
                            }
                            crate::widgets::log_writer::write_line_to_stderr(bytes);
                            let mut painter = sync::lock(&hook_painter);
                            painter.commit(&hook_progress);
                        });
                    _log_hook_guard = crate::widgets::log_writer::SpinnerHookGuard::install(hook);
                }

                // Wrap the receiver in a Mutex so we can access it
                // (it is !Sync but we only access from this thread).
                let update_rx = std::sync::Mutex::new(update_rx);
                let mut updates_done = false;
                // Tool call arguments keyed by call id, for the timeline rows.
                let mut timeline = ToolTimeline::default();

                // Mid-run input line state (story 2.1): the transient
                // buffer row drawn between the scrollback and the status
                // row. `active` = the row is currently on screen (or in
                // the redraw band). Frame tap echoes ride the test taps.
                // The row's liveness goes through `input_rows` (declared
                // above the log-hook install), whose depth EVERY transient
                // row erase consumes — story-2.1 review fix.
                // Erase every transient row (status + input band) — the
                // shared erase for announce rows, event rows and echo
                // frames alike.
                let erase_transient = {
                    let input_rows = input_rows.clone();
                    move || {
                        use crossterm::cursor::{MoveToColumn, MoveUp};
                        use crossterm::terminal::{Clear, ClearType};
                        use crossterm::ExecutableCommand;
                        let mut s = std::io::stdout();
                        for _ in 0..(1 + input_rows.load(Ordering::Relaxed)) {
                            let _ = s.execute(MoveUp(1));
                        }
                        let _ = s.execute(MoveToColumn(0));
                        let _ = s.execute(Clear(ClearType::FromCursorDown));
                    }
                };
                // The shared buffer slot so any announce/echo render can
                // re-draw the CURRENT mid-run input row.
                let input_buffer_shared: std::sync::Arc<std::sync::Mutex<String>> =
                    Default::default();
                let redraw_input_line_action = {
                    let input_rows = input_rows.clone();
                    let input_buffer_shared = input_buffer_shared.clone();
                    move || {
                        let buffer = sync::lock(&input_buffer_shared).clone();
                        if buffer.is_empty() {
                            input_rows.store(0, Ordering::Relaxed);
                            return;
                        }
                        let echo = RLine::from(vec![
                            RSpan::styled(
                                "  ↳ ".to_string(),
                                RStyle::default().fg(RColor::Rgb(110, 110, 140)),
                            ),
                            RSpan::styled(
                                format!("{}▏", buffer),
                                RStyle::default().fg(RColor::Rgb(200, 200, 215)),
                            ),
                        ]);
                        inline::print_line(&echo);
                        input_rows.store(1, Ordering::Relaxed);
                    }
                };
                let mut input_active = false;
                #[allow(unused_assignments)]
                let mut input_buffer = String::new();
                #[cfg(test)]
                let mut input_echoes: Vec<String> = Vec::new();

                loop {
                    if updates_done {
                        break;
                    }

                    // Sleep 80ms between spinner ticks.
                    std::thread::sleep(std::time::Duration::from_millis(80));

                    // Hold the render gate for the whole terminal-touching
                    // phase. While the `question` handler is prompting the
                    // user it owns this gate and we park here without
                    // writing, so the dialoguer list never races the
                    // spinner's MoveUp/Clear redraws.
                    let _render_gate = sync::lock(&RENDER_GATE);

                    if !r_stop.load(Ordering::Relaxed) {
                        redraw_spinner();
                    }

                    // Drain the queue in ARRIVAL ORDER (story 1.3): chunks
                    // append to the preview text; an event's row renders
                    // only after the text that arrived before it has been
                    // announced; trailing chunks announce at tick end —
                    // today's pure-text behavior. The whole drain runs
                    // under the render gate this iteration already holds;
                    // when a prompt owns the gate the renderer parks at the
                    // top of the tick (blocking lock), which defers the
                    // whole tick to a later one — the gate is never
                    // contended twice and nothing prints while the prompt
                    // is up.
                    let mut pending_text = false;
                    if !updates_done {
                        loop {
                            match sync::lock(&update_rx).try_recv() {
                                Ok(RendererUpdate::InputEcho(text)) => {
                                    // Story 2.1: the transient input line.
                                    input_buffer = text;
                                    *sync::lock(&input_buffer_shared) = input_buffer.clone();
                                    if input_active || !input_buffer.is_empty() {
                                        // Erase the transient band (status
                                        // + prior input row) via the shared
                                        // erase, redraw the input row (when
                                        // a buffer exists), and commit the
                                        // status below it.
                                        erase_transient();
                                        redraw_input_line_action();
                                        input_active = !input_buffer.is_empty();
                                        commit_spinner();
                                        #[cfg(test)]
                                        input_echoes.push(input_buffer.clone());
                                    }
                                }
                                Ok(RendererUpdate::Chunk(chunk)) => {
                                    if !chunk.is_empty() {
                                        sync::lock(&block_text).push_str(&chunk);
                                        pending_text = true;
                                    }
                                }
                                Ok(RendererUpdate::Event(event)) => {
                                    // Arrival order: announce the streamed
                                    // text queued before this event first,
                                    // so its row prints BELOW that text.
                                    if pending_text {
                                        let block_guard = sync::lock(&block_text);
                                        preview.announce(
                                            block_guard.as_str(),
                                            &erase_transient,
                                            &redraw_input_line_action,
                                            &commit_spinner,
                                        );
                                        pending_text = false;
                                    }
                                    // Thinking suppression (story 1.3
                                    // decision, 2026-11-06): the tool-call
                                    // turn's Thinking event re-posts the
                                    // turn's streamed content, which the
                                    // preview has already printed as the
                                    // answer (design decision A) — skip it
                                    // whenever the full text is inside the
                                    // flushed preview. Not fully previewed,
                                    // or a diverged preview: renders as
                                    // today. No terminal work happens when
                                    // suppressed, so the status line keeps
                                    // its position untouched.
                                    if let takka_core::Event::Thinking { content } = &event {
                                        if !preview.diverged
                                            && !content.trim().is_empty()
                                            && preview
                                                .flushed
                                                .trim_end()
                                                .ends_with(content.trim_end())
                                        {
                                            continue;
                                        }
                                    }
                                    // Erase the transient band first (story
                                    // 2.1 review fix: the status + the
                                    // mid-run input row — every row-print
                                    // erase must consume the whole band, or
                                    // the input row strands and the next
                                    // echo destroys real scrollback).
                                    erase_transient();
                                    // Tool rows follow `show_tool_calls`; steered
                                    // hand-over acks render regardless (they
                                    // are the story's visible contract); the
                                    // reasoning block follows `show_thoughts`.
                                    if render_panels
                                        || matches!(
                                            event,
                                            takka_core::Event::Thinking { .. }
                                                | takka_core::Event::SteerQueued { .. }
                                                | takka_core::Event::Steered { .. }
                                        )
                                    {
                                        render_event(&event, &mut timeline, timeline_detailed);
                                        // cfg(test): record what actually
                                        // printed for the interleave
                                        // assertions — the thinking block
                                        // (non-empty content only) and the
                                        // tool completion row (under
                                        // `show_tool_calls`). ToolFinished
                                        // is a synthesized echo with no
                                        // `render_event` arm (prints
                                        // nothing), and ToolDelta lines only
                                        // print in detailed mode — neither
                                        // is marked.
                                        #[cfg(test)]
                                        match &event {
                                            takka_core::Event::Thinking { content }
                                                if !content.trim().is_empty() =>
                                            {
                                                preview.order.push("thinking".to_string())
                                            }
                                            takka_core::Event::ToolOutput { .. }
                                                if render_panels =>
                                            {
                                                preview.order.push("tool".to_string())
                                            }
                                            takka_core::Event::SteerQueued { content } => preview
                                                .order
                                                .push(format!("steer-queued:{content}")),
                                            takka_core::Event::Steered { content } => {
                                                preview.order.push(format!("steered:{content}"))
                                            }
                                            _ => {}
                                        }
                                    }
                                    // Story 2.1 review fix: the input row
                                    // re-draws BELOW every announced row and
                                    // ABOVE the re-committed status line,
                                    // keeping the status last.
                                    redraw_input_line_action();
                                    commit_spinner();
                                }
                                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                    updates_done = true;
                                    break;
                                }
                            }
                        }
                    }
                    // End-of-tick flush: pure-text ticks announce
                    // their completed paragraphs here (unchanged rule for
                    // ticks without events).
                    if pending_text {
                        let block_guard = sync::lock(&block_text);
                        preview.announce(
                            block_guard.as_str(),
                            &erase_transient,
                            &redraw_input_line_action,
                            &commit_spinner,
                        );
                    }
                }

                // The spinner cleanup in `run` owns the terminal from here on.
                // Drop the hook first so a late log line can't re-arm a
                // spinner after we are gone.
                drop(_log_hook_guard);

                RenderFinal {
                    block_text: block_text.into_inner().unwrap(),
                    preview_committed: preview.committed,
                    preview_diverged: preview.diverged,
                    #[cfg(test)]
                    render_order: preview.order,
                    #[cfg(test)]
                    input_echoes,
                }
            })
            .expect("failed to spawn renderer thread");

        // Resolve the run's cancel flag before the connection borrow
        // (story 1.4): `run_turn` watches it between events.
        let cancel_flag = self.run_cancel_flag();
        // Per-run two-stage semantics (story 1.4 review): the first Ctrl+C
        // of THIS run is the cooperative one. A flag surviving from the
        // previous run (set at the idle prompt or carried over from a
        // cancelled run) would insta-cancel this run and turn the NEXT
        // press into the force-exit — reset it before the watcher's first
        // read.
        cancel_flag.store(false, std::sync::atomic::Ordering::Relaxed);

        // ── Mid-run input (story 2.1, both queue modes now — 2.2) ─────
        //
        // While the run owns the terminal, ONE spawned reader thread
        // collects typed characters, echoes them through the renderer's
        // ordered queue (`RendererUpdate::InputEcho` — the single-writer
        // discipline holds), and on Enter routes the line per the
        // captured queue mode: Interrupt → `Request::Steer` via
        // `steer_tx` (the `run_turn` watcher); FollowUp (the CLI default)
        // → the client-side follow-up queue + a log-hook notice, whose
        // existing post-run drain executes the line as the next run.
        // Ctrl+C rides the story-1.4 two-stage cancel path from the same
        // thread. The reader ends when the run's stop flag flips.
        // Mid-run `/queue` mode switching is out of scope: the mode is
        // captured at spawn (a switch applies from the next run).
        // The reader's notice write: production goes to the log hook
        // (through the reader's closure), cfg(test) smokes capture it
        // per-instance — no global, no parallel-test collisions.
        // Unconditional steering channel: follows both queue modes; the
        // reader only writes to it in Interrupt mode (story 2.2 does not
        // need `None` in follow-up — see implementation notes).
        let steer_rx;
        let queue_mode_captured = self.queue_mode;
        let pending_follow_ups_shared = self.pending_follow_ups.clone();
        #[allow(unused_assignments)] // review: see the join at the drain
        let mut reader_thread_handle: Option<std::thread::JoinHandle<()>> = None;
        {
            let (steer_tx, steer_line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
            let (echo_tx, echo_rx) = std::sync::mpsc::channel::<String>();
            steer_rx = Some(steer_line_rx);
            // No standing "input is live" badge: an Interrupt run with no
            // typed text stays byte-for-byte today's output (story-2.1
            // review fix, zero-residue rule).
            #[cfg(test)]
            let notice_sink = self.test_mid_run_notice_sink.clone().unwrap_or_default();
            // cfg(test) injection: the cancel smokes feed deterministic
            // keys instead of the keyboard (story 1.4's cancel-driver
            // pattern). `test_mid_run_keys` is a one-shot take.
            #[cfg(test)]
            let test_keys = self.test_mid_run_keys.clone();
            // cfg(test): the reader's prompt gate is PER-INSTANCE — the
            // parallel steering smokes own their own gate flag instead of
            // the process-global prompt flag (which the prompt-gate smoke
            // flips for a different Commands instance).
            #[cfg(test)]
            let test_prompt_gate = self.test_prompt_gate.clone();
            let r_stop_reader = stop_flag.clone();
            let cancel_reader = cancel_flag.clone();
            // Story-2.2: the reader routes its notices through the log
            // hook in production; a cfg-test smoke may install a
            // per-instance capture sink (no globals, no cross-test
            // captures — the smoke asserts the reader's exact notice).
            let mid_run_notice = move |msg: String| {
                crate::widgets::log_writer::write_notice_dispatch(
                    msg,
                    #[cfg(test)]
                    Some(crate::widgets::log_writer::NoticeSink(notice_sink.clone())),
                    #[cfg(not(test))]
                    None,
                );
            };
            let reader_spawn_handle = std::thread::Builder::new()
                .name("takka-mid-run-input".into())
                .spawn(move || {
                    let mut buffer = String::new();
                    #[cfg(test)]
                    let injected = test_keys.lock().unwrap().take();
                    #[cfg(test)]
                    if injected.is_none() {
                        // No injection source configured: nothing keeps
                        // this reader alive in test builds.
                        let _ = echo_tx.send(String::new());
                        return;
                    }
                    loop {
                        if r_stop_reader.load(Ordering::Relaxed) {
                            break;
                        }
                        // Story-2.3 fix: while a confirmation/question
                        // prompt owns stdin (cooked mode, dialoguer), the
                        // reader must not race it for keystrokes — park AND
                        // discard. Keys typed behind the gate belong to the
                        // prompt's stdin; buffering them for later steering
                        // would lose them into a run the user already meant
                        // to talk to, so they drop. Per-INSTANCE gate (the
                        // prompt flag flows through the run's prompt
                        // messages) — not the global, which other tests
                        // own.
                        #[cfg(test)]
                        if test_prompt_gate.load(Ordering::Relaxed) {
                            let _ = injected
                                .as_ref()
                                .map(|rx| rx.try_recv());
                            std::thread::sleep(std::time::Duration::from_millis(20));
                            continue;
                        }
                        if r_stop_reader.load(Ordering::Relaxed) {
                            break;
                        }
                        #[cfg(test)]
                        let key: Option<MidRunKey> = 'blk: {
                            let Some(rx) = &injected else {
                                break 'blk None;
                            };
                            use std::sync::mpsc::TryRecvError;
                            match rx.try_recv() {
                                Ok(key) => break 'blk Some(key),
                                Err(TryRecvError::Empty) => {
                                    std::thread::sleep(std::time::Duration::from_millis(20));
                                    break 'blk None;
                                }
                                Err(TryRecvError::Disconnected) => break 'blk None,
                            }
                        };
                        #[cfg(not(test))]
                        let key: Option<MidRunKey> = poll_crossterm_key();
                        match key {
                            None => continue,
                            Some(MidRunKey::CtrlC) => {
                                // Two-stage cancel, reader parity with the
                                // signal task (main.rs): the first press
                                // requests the cooperative cancel, the
                                // second force-exits at 130.
                                if cancel_reader.swap(true, Ordering::SeqCst) {
                                    mid_run_notice("\n\u{26a0} Force-exiting on second Ctrl+C.".to_string());
                                    std::process::exit(130);
                                }
                                mid_run_notice("\n\u{26a0} Cancel requested (press Ctrl+C again to force-exit).".to_string());
                                buffer.clear();
                                let _ = echo_tx.send(String::new());
                            }
                            Some(MidRunKey::Char(ch)) => {
                                buffer.push(ch);
                                let _ = echo_tx.send(buffer.clone());
                            }
                            Some(MidRunKey::Enter) => {
                                let line = std::mem::take(&mut buffer);
                                let _ = echo_tx.send(String::new());
                                if !line.is_empty() {
                                    match queue_mode_captured {
                                        QueueMode::Interrupt => {
                                            let _ = steer_tx.send(line);
                                        }
                                        QueueMode::FollowUp => {
                                            // Story 2.2: the default mode
                                            // queues the text client-side;
                                            // the drain runs it as the next
                                            // run, and the log hook makes
                                            // the queueing visible in the
                                            // same transient band the
                                            // notices use.
                                            sync::lock(&pending_follow_ups_shared)
                                                .push_back(line.clone());
                                            mid_run_notice(format!(
                                                "\n\u{21b3} queued for the next run: \"{}\"",
                                                line
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    let _ = echo_tx.send(String::new());
                })
                .expect("failed to spawn mid-run input reader");
            // Keep the reader's handle: the drain runs only after the
            // reader joined, so an Enter landing inside the stop-flag
            // window cannot push past the promised next-run drain
            // (story-2.2 review fix #2).
            reader_thread_handle = Some(reader_spawn_handle);
            let update_tx_mid = update_tx.clone();
            let _echo_spooler = std::thread::Builder::new()
                .name("takka-mid-run-echo".into())
                .spawn(move || {
                    while let Ok(frame) = echo_rx.recv() {
                        if update_tx_mid
                            .send(RendererUpdate::InputEcho(frame))
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .expect("failed to spawn mid-run echo spooler");
        }
        let runtime = self
            .connection
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Runtime not initialized"))?;

        // Live phase tracking for the status line. Both callbacks run on
        // runtime threads, so they only touch the shared `ProgressState`
        // (never the terminal — the renderer owns that).
        let phase_progress = progress.clone();
        let run_tools = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let run_usage = std::sync::Arc::new(std::sync::Mutex::new(UsageTotals::default()));

        let result = runtime
            .run(
                task.to_string(),
                attachments,
                |chunk| {
                    if !chunk.is_empty() {
                        {
                            let mut p = sync::lock(&phase_progress);
                            p.set_phase(progress::Phase::Streaming);
                            p.add_stream_chars(chunk.chars().count());
                        }
                        let _ = update_tx.send(RendererUpdate::Chunk(chunk.to_string()));
                    }
                },
                {
                    // Test tap: drained follow-up runs travel through this
                    // path, so e2e tests observe their lifecycle here.
                    #[cfg(test)]
                    let test_events = std::sync::Arc::clone(&self.test_events);
                    // `move` (needed for the test tap) moves this block's
                    // captured sender; shadow it with a clone so the
                    // original stays alive and `drop(update_tx)` below
                    // still releases the renderer.
                    let update_tx = update_tx.clone();
                    let e_progress = progress.clone();
                    let e_tools = run_tools.clone();
                    let e_usage = run_usage.clone();
                    move |event| {
                        if let Ok(mut p) = e_progress.lock() {
                            match &event {
                                takka_core::Event::ToolCall { tool_name, .. } => {
                                    p.set_tool(tool_name.clone());
                                }
                                takka_core::Event::ToolStarted { tool_name, .. } => {
                                    p.set_tool(tool_name.clone());
                                    e_tools.fetch_add(1, Ordering::Relaxed);
                                }
                                takka_core::Event::ToolOutput { .. } => {
                                    p.set_phase(progress::Phase::Thinking);
                                }
                                takka_core::Event::ConfirmationRequest { .. }
                                | takka_core::Event::QuestionRequest { .. } => {
                                    p.set_phase(progress::Phase::WaitingApproval);
                                }
                                _ => {}
                            }
                        }
                        // Real provider usage: accumulate for the end-of-run
                        // summary. It is not forwarded to the renderer — the
                        // numbers are shown once, not floated mid-run.
                        if let takka_core::Event::Usage {
                            prompt_tokens,
                            completion_tokens,
                            cache_read_input_tokens,
                            cache_creation_input_tokens,
                        } = &event
                        {
                            sync::lock(&e_usage).add(
                                *prompt_tokens,
                                *completion_tokens,
                                cache_read_input_tokens.unwrap_or(0),
                                cache_creation_input_tokens.unwrap_or(0),
                            );
                        }
                        #[cfg(test)]
                        test_events.lock().unwrap().push(format!("{event:?}"));
                        let is_usage = matches!(event, takka_core::Event::Usage { .. });
                        // ModelChunk is the raw per-delta echo; the renderer
                        // gets the same text as a Chunk and has no use for
                        // the event (story 1.3: one queue, no no-op spins).
                        let is_noop_model_chunk =
                            matches!(event, takka_core::Event::ModelChunk { .. });
                        if !is_usage
                            && !is_noop_model_chunk
                            && (show_thoughts
                                || !matches!(event, takka_core::Event::Thinking { .. }))
                        {
                            let _ = update_tx.send(RendererUpdate::Event(event));
                        }
                    }
                },
                |event| match &event.event {
                    takka_core::Event::ConfirmationRequest {
                        target,
                        action,
                        description,
                        risk_level,
                    } => {
                        // Use the shared confirmation panel (risk badge, diff
                        // preview, y/n/a/q). It also parks the renderer and
                        // consumes the status line for the duration of the
                        // prompt — a bare dialoguer prompt here let the spinner
                        // redraw over the question in REPL mode.
                        let request = takka_core::ConfirmationRequest {
                            action: action.clone(),
                            description: description.clone(),
                            target: target.clone(),
                            risk_level: match risk_level.as_str() {
                                "low" => takka_core::RiskLevel::Low,
                                "high" => takka_core::RiskLevel::High,
                                "critical" => takka_core::RiskLevel::Critical,
                                _ => takka_core::RiskLevel::Medium,
                            },
                            risk_score: 0.5,
                            reason: description.clone(),
                            timestamp: chrono::Utc::now(),
                            preview_diff: None,
                        };
                        let approved = matches!(
                            prompt_confirmation(&request),
                            Some(ConfirmationResponse::Yes | ConfirmationResponse::Always)
                        );
                        Some(takka_core::runtime::protocol::Request::ConfirmResponse {
                            request_id: event.request_id.clone(),
                            approved,
                        })
                    }
                    takka_core::Event::QuestionRequest { questions } => {
                        let answers =
                            takka_core::QuestionHandler::handle(&CliQuestionHandler, questions);
                        Some(takka_core::runtime::protocol::Request::QuestionResponse {
                            request_id: event.request_id.clone(),
                            answers,
                        })
                    }
                    _ => None,
                },
                cancel_flag,
                steer_rx,
            )
            .await;

        drop(update_tx);
        stop_flag.store(true, Ordering::Relaxed);

        let render_final = renderer.join().expect("renderer thread panicked");
        // Test tap: e2e tests observe what the live preview printed (the
        // renderer thread's terminal output is not capturable in-process).
        #[cfg(test)]
        {
            *self.test_preview.lock().unwrap() = render_final.preview_committed.clone();
            *self.test_preview_diverged.lock().unwrap() = render_final.preview_diverged;
            *self.test_render_order.lock().unwrap() = render_final.render_order.clone();
            *self.test_input_echoes.lock().unwrap() = render_final.input_echoes.clone();
        }
        // The question handler only does its spinner consume/re-arm dance
        // while this flag is set, so it never touches the screen in modes
        // where no transient spinner exists (e.g. the full-screen TUI).
        SPINNER_ACTIVE.store(false, Ordering::Relaxed);
        sync::lock(&progress).stop();

        // Cursor cleanup + final result.  The renderer always runs in
        // "spinner mode" (we never wrote chunks to stdout), so cleanup
        // is the same in every case: erase the spinner line, then render
        // the accumulated text as styled markdown.
        #[cfg(test)]
        self.test_terminal_cleanup_count
            .fetch_add(1, Ordering::Relaxed);
        let mut s = std::io::stdout();
        {
            use crossterm::cursor::{MoveToColumn, MoveUp};
            use crossterm::terminal::{Clear, ClearType};
            use crossterm::ExecutableCommand;
            let _ = s.execute(MoveUp(1));
            let _ = s.execute(MoveToColumn(0));
            let _ = s.execute(Clear(ClearType::FromCursorDown));
        }
        // Flush buffered writes so inline::print_* output below is not
        // reordered with the terminal-control sequences written above.
        let _ = std::io::Write::flush(&mut s);

        let usage = {
            let totals = sync::lock(&run_usage);
            (
                totals.measured,
                totals.prompt,
                totals.completion,
                totals.cache_read,
                totals.cache_creation,
            )
        };
        let (measured, prompt_tokens, completion_tokens, cache_read, cache_creation) = usage;
        self.last_run = RunStats {
            input_chars: task.len(),
            output_chars: 0,
            tool_calls: run_tools.load(Ordering::Relaxed),
            prompt_tokens: measured.then_some(prompt_tokens),
            completion_tokens: measured.then_some(completion_tokens),
            cache_read_tokens: cache_read,
            cache_creation_tokens: cache_creation,
        };

        match result {
            Ok(final_result) => {
                // RunStats keeps today's semantics: the orchestrator's
                // final answer (or the stream when it returned nothing) —
                // NOT the suffix-decision source (review patch 2026-11-06).
                let today_source = if final_result.is_empty() {
                    &render_final.block_text
                } else {
                    &final_result
                };
                self.last_run.output_chars = today_source.chars().count();
                if !today_source.is_empty() {
                    // The live preview printed the streamed answer above the
                    // status line: those lines ARE the final answer (design
                    // decision A), so only the suffix the preview never
                    // reached is emitted here — usually the trailing partial
                    // line. A prefix mismatch (mid-run divergence, or a
                    // final text that escaped the stream) keeps today's full
                    // render — the story's documented duplication case. An
                    // empty preview matches trivially, so the suffix is the
                    // whole render: the no-chunk fallback stays today's path
                    // exactly. Multi-turn runs (story 1.3): `final_result`
                    // covers only the LAST turn, so today's source would
                    // re-print the last block after it was already previewed
                    // — the print source upgrades to the FULL stream, but
                    // only after the strict line-prefix match against its
                    // parse verifies; a mismatch falls back to today's
                    // source and today's decision (the documented rule).
                    let mut parsed = md_widget::MarkdownContent::parse(today_source);
                    let mut suffix_start = if !render_final.preview_diverged
                        && preview_prefix_matches(&render_final.preview_committed, &parsed.lines)
                    {
                        Some(render_final.preview_committed.len())
                    } else {
                        None
                    };
                    if suffix_start.is_none()
                        && end_of_run_uses_full_stream(
                            &render_final.preview_committed,
                            render_final.preview_diverged,
                            &render_final.block_text,
                            &final_result,
                        )
                        && preview_prefix_matches(
                            &render_final.preview_committed,
                            &md_widget::MarkdownContent::parse(&render_final.block_text).lines,
                        )
                    {
                        parsed = md_widget::MarkdownContent::parse(&render_final.block_text);
                        suffix_start = Some(render_final.preview_committed.len());
                    }
                    let final_lines: &[ratatui::text::Line<'_>] = if let Some(n) = suffix_start {
                        &parsed.lines[n..]
                    } else {
                        &parsed.lines
                    };
                    #[cfg(test)]
                    {
                        *self.test_final_print.lock().unwrap() = final_lines
                            .iter()
                            .map(|l| {
                                l.spans
                                    .iter()
                                    .map(|sp| sp.content.to_string())
                                    .collect::<String>()
                            })
                            .collect();
                    }
                    inline::print_lines(final_lines);
                }
                inline::print_blank();
            }
            Err(e) => {
                // Story 1.4: "Cancelled by user" is a desired outcome —
                // the cancel notice, not the red error badge.
                let badge = run_outcome_badge(&e);
                match badge {
                    RunOutcomeBadge::Cancelled => {
                        inline::print_line(&components::info_badge(&e.to_string()));
                    }
                    RunOutcomeBadge::Error => {
                        inline::print_line(&components::error_badge(&e.to_string()));
                    }
                }
                #[cfg(test)]
                self.test_notice.lock().unwrap().push(badge);
                inline::print_blank();
            }
        }

        // After the run's terminal event (Done or Error), drain queued
        // follow-ups: text submitted mid-run in FollowUp mode is sent as
        // the next run (client-issued, depth-guarded loop). Both outcome
        // branches above converge here.
        //
        // Story-2.2 review fix: the mid-run reader joins FIRST — an Enter
        // landing inside the stop-flag window cannot push a line past the
        // drain (the "next run" promise stays literal).
        if let Some(reader) = reader_thread_handle {
            let _ = reader.join();
        }
        self.drain_follow_ups().await?;

        Ok(())
    }

    /// Run task with separate callbacks for streaming chunks and runtime
    /// events (tool calls + results). Used by the TUI to surface tool
    /// activity in the message log alongside the streaming response.
    ///
    /// `on_chunk` receives token deltas as the model generates them.
    /// `on_event` receives `takka_core::Event` for every tool call and
    /// result emitted by the orchestrator.
    ///
    /// Both callbacks must be `Send + Sync + 'static` because the event
    /// stream is dispatched on blocking-pool threads.
    pub async fn run_with_callbacks<C, E>(
        &mut self,
        task: &str,
        mut on_chunk: C,
        on_event: E,
    ) -> Result<String>
    where
        C: FnMut(&str),
        E: Fn(takka_core::Event) + Send + Sync + 'static,
    {
        let expanded = crate::file_ref::expand_with_attachments(task);
        self.ensure_connection().await?;
        #[cfg(test)]
        self.test_runs
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Resolve the run's cancel flag before the connection borrow
        // (story 1.4): the TUI path gets the same cancel wiring as the
        // inline REPL — both UIs fold every event through `run_turn`.
        // Per-run two-stage reset: the TUI's first Ctrl+C is the
        // cooperative one (story 1.4 review fix).
        let cancel_flag = self.run_cancel_flag();
        // Per-run two-stage semantics (story 1.4 review): the first Ctrl+C
        // of THIS run is the cooperative one. A flag surviving from the
        // previous run (set at the idle prompt or carried over from a
        // cancelled run) would insta-cancel this run and turn the NEXT
        // press into the force-exit — reset it before the watcher's first
        // read.
        cancel_flag.store(false, std::sync::atomic::Ordering::Relaxed);
        let runtime = self
            .connection
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Runtime not initialized"))?;

        let mut event_callback = Some(on_event);
        let result = runtime
            .run(
                expanded.text,
                expanded.attachments,
                |chunk| on_chunk(chunk),
                |event| {
                    if let Some(callback) = event_callback.as_mut() {
                        callback(event);
                    }
                },
                |event| match &event.event {
                    takka_core::Event::ConfirmationRequest { description, .. } => {
                        let approved = prompt_confirmation(&takka_core::ConfirmationRequest {
                            action: "runtime".to_string(),
                            description: description.clone(),
                            target: None,
                            risk_level: takka_core::RiskLevel::Medium,
                            risk_score: 0.5,
                            reason: description.clone(),
                            timestamp: chrono::Utc::now(),
                            preview_diff: None,
                        })
                        .map(|response| {
                            matches!(
                                response,
                                ConfirmationResponse::Yes | ConfirmationResponse::Always
                            )
                        })
                        .unwrap_or(false);
                        Some(takka_core::runtime::protocol::Request::ConfirmResponse {
                            request_id: event.request_id.clone(),
                            approved,
                        })
                    }
                    takka_core::Event::QuestionRequest { questions } => {
                        let answers =
                            takka_core::QuestionHandler::handle(&CliQuestionHandler, questions);
                        Some(takka_core::runtime::protocol::Request::QuestionResponse {
                            request_id: event.request_id.clone(),
                            answers,
                        })
                    }
                    _ => None,
                },
                cancel_flag,
                None,
            )
            .await;

        // Same drain-after-Done contract as `run()`: after the run's
        // terminal event, queued follow-ups are sent as the next run(s).
        // Fires for both outcomes (Ok/Err) — a failed session also has a
        // terminal event, and queued text should still reach the engine.
        self.drain_follow_ups().await?;

        result
    }
}

/// Story 1.4: the run-Err badge decision — "Cancelled by user" is a
/// desired outcome and renders as the cancel notice; every other error
/// keeps the red error badge.
#[cfg(test)]
mod cancel_notice_tests {
    use super::*;

    #[test]
    fn cancelled_error_picks_the_cancel_notice() {
        let error = anyhow::anyhow!(takka_core::AgenticError::Cancelled.to_string());
        assert_eq!(run_outcome_badge(&error), RunOutcomeBadge::Cancelled);
    }

    #[test]
    fn other_errors_keep_the_red_error_badge() {
        for message in [
            "Provider error: connection refused",
            "Cancelled by userx", // near-miss must not match
            "",
        ] {
            let error = anyhow::anyhow!(message);
            assert_eq!(
                run_outcome_badge(&error),
                RunOutcomeBadge::Error,
                "{message}"
            );
        }
    }
}
