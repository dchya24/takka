use crossterm::cursor::{MoveToColumn, MoveUp};
use crossterm::terminal::{Clear, ClearType};
use crossterm::ExecutableCommand;
use ratatui::text::{Line as RLine, Span as RSpan};
use takka_core::sync;
use termcolor::{Color, ColorSpec, StandardStream, WriteColor};

use crate::widgets::capabilities;
use crate::widgets::{components, inline};

/// Erase the transient spinner line that `Commands::run`'s renderer
/// thread keeps on screen. The renderer's invariant is that the spinner
/// is the last printed line with the cursor directly below it, so one
/// `MoveUp` lands on it.
pub(crate) fn consume_spinner_line() {
    if !inline::is_stdout_tty() {
        return;
    }
    let mut stdout = std::io::stdout();
    let _ = stdout.execute(MoveUp(1));
    let _ = stdout.execute(MoveToColumn(0));
    let _ = stdout.execute(Clear(ClearType::FromCursorDown));
}

/// Print a fresh status line under the current cursor. Used by the question
/// handler to restore the renderer's "status line is the last line" invariant
/// after a prompt; the renderer replaces it with the live status on its next
/// tick.
pub(crate) fn commit_spinner_placeholder() {
    if !inline::is_stdout_tty() {
        return;
    }
    let mut p = crate::widgets::progress::ProgressState::new();
    p.start();
    p.set_phase(crate::widgets::progress::Phase::Thinking);
    inline::print_line(&crate::widgets::spinner::status_line(&p, false));
}

// ── Inline event rendering helper (used by run()'s ticker task) ───────────
//
// Free function so it can be called from inside the spawned tokio task
// without moving a closure (and therefore without capturing &self).

// ── Transient status line ─────────────────────────────────────

/// Per-run figures the REPL shows in its end-of-run summary.
#[derive(Debug, Default, Clone, Copy)]
pub struct RunStats {
    /// Prompt characters sent for this run (fallback token estimate).
    pub input_chars: usize,
    /// Response characters streamed back for this run (fallback estimate).
    pub output_chars: usize,
    /// Tools executed during this run.
    pub tool_calls: u32,
    /// Prompt tokens the provider reported (`None` when it reported nothing).
    pub prompt_tokens: Option<u32>,
    /// Completion tokens the provider reported.
    pub completion_tokens: Option<u32>,
    /// Prompt-cache reads / writes the provider reported.
    pub cache_read_tokens: u32,
    pub cache_creation_tokens: u32,
    /// This run's spend in USD (H-06). `0.0` when the model has no known
    /// list price or the provider reported no usage.
    pub cost_usd: f64,
}

impl RunStats {
    /// Input/output tokens for this run, preferring the provider's report and
    /// falling back to the ~4-chars-per-token estimate.
    pub fn tokens(&self) -> (u32, u32) {
        let estimate = |chars: usize| (chars / 4).min(u32::MAX as usize) as u32;
        (
            self.prompt_tokens
                .unwrap_or_else(|| estimate(self.input_chars)),
            self.completion_tokens
                .unwrap_or_else(|| estimate(self.output_chars)),
        )
    }

    /// `true` when the numbers came from the provider rather than an estimate.
    pub fn tokens_are_measured(&self) -> bool {
        self.prompt_tokens.is_some() || self.completion_tokens.is_some()
    }
}

/// How much detail the inline tool timeline prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineStyle {
    /// Branch row per tool plus nested output, errors and diffs (default).
    Detailed,
    /// One row per tool — no streamed output, error bodies or diff hunks.
    Compact,
}

impl TimelineStyle {
    pub fn as_str(&self) -> &'static str {
        match self {
            TimelineStyle::Detailed => "detailed",
            TimelineStyle::Compact => "compact",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            TimelineStyle::Detailed => "rows plus nested output, errors and diffs",
            TimelineStyle::Compact => "one row per tool, no nested detail",
        }
    }
}

/// Token totals reported by the runtime during one `run()` call.
#[derive(Debug, Default)]
pub(crate) struct UsageTotals {
    pub(crate) prompt: u32,
    pub(crate) completion: u32,
    pub(crate) cache_read: u32,
    pub(crate) cache_creation: u32,
    pub(crate) measured: bool,
}

impl UsageTotals {
    pub(crate) fn add(
        &mut self,
        prompt: u32,
        completion: u32,
        cache_read: u32,
        cache_creation: u32,
    ) {
        self.prompt = self.prompt.saturating_add(prompt);
        self.completion = self.completion.saturating_add(completion);
        self.cache_read = self.cache_read.saturating_add(cache_read);
        self.cache_creation = self.cache_creation.saturating_add(cache_creation);
        self.measured = true;
    }
}

/// Paints the transient activity line that sits at the bottom of the inline
/// output during a run.
///
/// The line is overwritten in place (`MoveUp` + repaint) instead of being
/// erased and redrawn — the erase/draw cycle blinks on Windows conhost. Since
/// its width now varies with the phase label, elapsed time and token counter,
/// every frame is padded — under the min-cover rule
/// ([`frame_pad_on_resize`]) — to cover everything the predecessor could
/// have painted unwrapped at the terminal's current width, so a shorter
/// frame can never leave stale characters behind. The ambient width is
/// re-measured fresh every frame (nothing caches it), and the returned pad
/// becomes the next baseline: the painter's only stored state is
/// `frame_width`.
pub(crate) struct StatusPainter {
    pub(crate) frame_width: usize,
    /// `ctrl+c to cancel` is only worth the columns when the terminal is wide
    /// enough to hold the status line plus the hint.
    pub(crate) show_cancel_hint: bool,
}

impl StatusPainter {
    pub(crate) fn new() -> Self {
        Self {
            frame_width: 0,
            show_cancel_hint: inline::is_stdout_tty() && inline::terminal_width() >= 60,
        }
    }

    /// Build the next frame, padded per the min-cover resize rule: the
    /// ambient width is re-measured fresh each frame and the pad covers
    /// everything the predecessor could have painted unwrapped at that
    /// width. The returned pad doubles as the new baseline.
    pub(crate) fn next_line(
        &mut self,
        state: &std::sync::Mutex<crate::widgets::progress::ProgressState>,
    ) -> RLine<'static> {
        let mut line = {
            let mut progress = sync::lock(state);
            progress.tick();
            crate::widgets::spinner::status_line(&progress, self.show_cancel_hint)
        };
        let width = line.width();
        let pad = frame_pad_on_resize(self.frame_width, inline::terminal_width(), width);
        if pad > width {
            line.spans.push(RSpan::raw(" ".repeat(pad - width)));
        }
        self.frame_width = pad;
        line
    }

    /// Paint a fresh status line below the cursor.
    pub(crate) fn commit(
        &mut self,
        state: &std::sync::Mutex<crate::widgets::progress::ProgressState>,
    ) {
        let line = self.next_line(state);
        inline::print_line(&line);
    }

    /// Overwrite the status line in place.
    pub(crate) fn redraw(
        &mut self,
        state: &std::sync::Mutex<crate::widgets::progress::ProgressState>,
    ) {
        use crossterm::cursor::{MoveToColumn, MoveUp};
        use crossterm::ExecutableCommand;

        let line = self.next_line(state);
        let mut stdout = std::io::stdout();
        let _ = stdout.execute(MoveUp(1));
        let _ = stdout.execute(MoveToColumn(0));
        inline::print_line(&line);
    }

    /// Remove the status line and anything printed after it.
    pub(crate) fn erase(&self) {
        use crossterm::cursor::{MoveToColumn, MoveUp};
        use crossterm::terminal::{Clear, ClearType};
        use crossterm::ExecutableCommand;

        let mut stdout = std::io::stdout();
        let _ = stdout.execute(MoveUp(1));
        let _ = stdout.execute(MoveToColumn(0));
        let _ = stdout.execute(Clear(ClearType::FromCursorDown));
    }
}

/// The pad width for the next status frame under a mid-run resize
/// (story 1.2, the min-cover rule):
///
/// `pad = max(line_width, min(frame_width, ambient_now))`
///
/// Cover every column the predecessor could have painted WITHOUT WRAPPING
/// at the width the terminal has now — `redraw`'s in-place overwrite
/// writes no `Clear`, so anything short of that coverage strands stale
/// glyphs. Within one ambient width (the baseline was measured at the
/// current width) this is exactly `max(line_width, frame_width)` —
/// today's pad-to-widest, bit-for-bit. On a partial narrowing the
/// retained baseline still fits (`min` keeps it); on a deep narrow (the
/// baseline itself no longer fits) the baseline CONTRIBUTION is capped
/// at the new width, so the frame wraps only when its own content is
/// wider than the terminal (status content can exceed a very narrow
/// width — documented residual, out of this story's fix scope). In that
/// case the cap still covers the maximal recoverable one row's worth of
/// predecessor columns. The one irrecoverable exception is
/// the transition frame painted before the new width was first observed
/// (wrap/clipped residue until the terminal reflows it; on the wider
/// state after a deep-narrow the clamped baseline cannot cover columns
/// beyond the clamp until a naturally wider frame paints — same family
/// of cosmetic, bounded stale-column residuals; every frame after the
/// observation fits exactly). The caller's only state is
/// `frame_width` — the returned pad
/// becomes the next baseline; no ambient measurement is stored.
pub(crate) fn frame_pad_on_resize(
    frame_width: usize,
    ambient_now: usize,
    line_width: usize,
) -> usize {
    line_width.max(frame_width.min(ambient_now))
}

// ── Live preview of streamed text (story 1.1) ────────────────
//
// While a run streams, the renderer announces each newly completed line
// of `block_text` as styled markdown printed above the status line, using
// the console-log hook's proven erase → print → repaint cycle. The printed
// lines ARE the final answer: at end of run only the suffix the preview
// never reached is printed, so the answer appears exactly once.
//
// Wrap & resize doctrine (story 1.2, the console-log hook rule): a
// streamed line is announced once, whole, and the TERMINAL does all
// wrapping — an over-long line wraps once at the terminal's own ambient
// width, exactly as `log_writer::write_line_to_stderr` writes whole raw
// log lines with zero custom wrapping. Nothing in the preview path
// measures or caches a width; `preview_flush` is width-blind by
// construction. On a resize, lines already printed are never re-rendered,
// re-wrapped, or truncated — their scrollback is final; only lines not
// yet announced see the new width (the status painter's per-frame
// re-measure is the one width-aware surface, and it resets only its pad
// baseline, never a printed line).

/// One renderer tick's live-preview decision.
pub(crate) enum PreviewFlush {
    /// Styled lines to print below the already-printed ones (may be
    /// empty — nothing new completed since the last flush).
    Print(Vec<RLine<'static>>),
    /// The re-render no longer extends the printed prefix (an earlier
    /// line changed — e.g. a soft line break joined two printed lines).
    /// The caller stops previewing for this run; the end-of-run path
    /// falls back to today's full render.
    Diverged,
}

/// Plain-text projection of a styled line, for prefix comparison.
pub(crate) fn line_plain_text(line: &RLine<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// Which terminal badge a run's `Err` value renders as (story 1.4).
/// "Cancelled by user" is a desired outcome, so it surfaces as the
/// neutral cancel notice instead of the red error badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunOutcomeBadge {
    Cancelled,
    Error,
}

/// Decide the badge for a run error. The cancel fact crosses the
/// protocol as a plain string (orchestrator `Err(AgenticError::
/// Cancelled)` → engine `Event::Error { message }` → `anyhow!(message)`
/// in `run_turn`), so the type is gone by the time it lands here.
/// Recomputing the same `thiserror` literal keeps both sides coupled to
/// the one variant — if core ever rewords it, this follows
/// automatically (documented coupling, story 1.4 design notes).
pub(crate) fn run_outcome_badge(error: &anyhow::Error) -> RunOutcomeBadge {
    if error.to_string() == takka_core::AgenticError::Cancelled.to_string() {
        RunOutcomeBadge::Cancelled
    } else {
        RunOutcomeBadge::Error
    }
}

/// The portion of `text` safe to announce this tick: everything up to and
/// including the last newline. A trailing fragment without a newline can
/// still grow into a longer line, so it stays pending until the newline
/// arrives (whole-line flush granularity — epic decision 2026-11-05a).
pub(crate) fn completed_text(text: &str) -> &str {
    // Whole-line granularity: a trailing fragment without a newline can
    // still grow into a longer line, so it stays pending (deferred to a
    // later tick).
    let some = match text.rfind('\n') {
        Some(end) => &text[..=end],
        None => "",
    };
    // Whole-paragraph granularity on top of it: markdown renders a single
    // newline inside a paragraph as a soft break that JOINS the lines
    // (`SoftBreak => push_span(" ")`), so announcing a paragraph's first
    // line early would later change already-printed lines — the divergence
    // fallback duplicating the answer for the common hard-wrapped prose
    // shape. Announce up to the last blank-line boundary only: a paragraph
    // renders complete, once, and never diverges.
    let boundary = match some.rfind("\n\n") {
        Some(pos) => pos + 2,
        None => return "",
    };
    // Whole-fence granularity: never announce across an unclosed code
    // fence — its temporary close (`parse_partial`) shifts when the block
    // grows, so a fence is held until its close (and the boundary after
    // it) completes. Back off to before the most recent triple-tick while
    // the announced region has odd fence parity.
    let mut cut = boundary;
    while cut > 0 {
        let region = &some[..cut];
        if region.matches("```").count() % 2 == 0 {
            break;
        }
        cut = region.rfind("```").unwrap_or(0);
    }
    &some[..cut]
}

/// Strict-prefix check: every already-printed preview line must equal the
/// plain text of the re-rendered line at the same position. This is the
/// guardrail that turns any styled-line instability into the documented
/// render-at-end fallback instead of corrupted scrollback. An empty
/// `committed` matches trivially — the caller then prints the whole
/// render, which is exactly today's no-preview behavior.
pub(crate) fn preview_prefix_matches(committed: &[String], rendered: &[RLine<'_>]) -> bool {
    rendered.len() >= committed.len()
        && committed
            .iter()
            .zip(rendered)
            .all(|(printed, line)| *printed == line_plain_text(line))
}

/// Decide what a preview flush prints: re-render the completed streamed
/// text with `parse_partial` (auto-closing open fences, so open constructs
/// can't leak their styling into later lines) and emit only the lines
/// beyond what was already printed.
pub(crate) fn preview_flush(committed: &[String], completed: &str) -> PreviewFlush {
    if completed.is_empty() {
        return PreviewFlush::Print(Vec::new());
    }
    let mut rendered = crate::widgets::markdown::MarkdownContent::parse_partial(completed);
    if !preview_prefix_matches(committed, &rendered.lines) {
        return PreviewFlush::Diverged;
    }
    PreviewFlush::Print(rendered.lines.split_off(committed.len()))
}

/// Whether the multi-turn end-of-run rule upgrades the print source to
/// the FULL streamed text (story 1.3): `block_text` accumulates EVERY
/// turn's deltas (T1+T2+T3) while the orchestrator returns only the last
/// turn's answer (T3) — with the story-1.1 preview live, T3 is already on
/// screen, so today's source would print it again. This is a GATE only:
/// the caller still verifies the strict line-prefix match against the
/// full stream's parse and falls back to today's source and decision when
/// it fails (plan task 3: "otherwise keep today's decision"). Divergence
/// and empty previews keep today's source. Pure and unit-testable.
pub(crate) fn end_of_run_uses_full_stream(
    committed: &[String],
    diverged: bool,
    block_text: &str,
    final_result: &str,
) -> bool {
    !diverged
        && !committed.is_empty()
        && (final_result.is_empty() || block_text.ends_with(final_result))
}

/// Live-preview run state (story 1.1 machinery, regrouped in story 1.3 so
/// the drain loop can announce text at arrival-order boundaries instead of
/// one place per tick).
#[derive(Default)]
pub(crate) struct PreviewTracker {
    /// Plain text of the preview lines already printed, in print order.
    pub(crate) committed: Vec<String>,
    /// The completed text the committed lines were rendered from (skips
    /// re-parsing when nothing new completed).
    pub(crate) flushed: String,
    /// True when the progressive re-render stopped extending the printed
    /// prefix mid-run; the end-of-run path then keeps today's full render.
    pub(crate) diverged: bool,
    /// cfg(test) only: the interleaved print order of the run —
    /// `text:<line>` for live preview lines, alongside the event markers
    /// the renderer drains pushes adjacent to them.
    #[cfg(test)]
    pub(crate) order: Vec<String>,
}

impl PreviewTracker {
    /// Announce newly completed streamed text as styled markdown above the
    /// status line, riding the console-log hook's erase → print → repaint
    /// cycle (the same atom for every printed item, so the status line is
    /// always the last line). `consume` erases ALL transient rows under it
    /// (status + the story-2.1 input row when it is live), so the announced
    /// lines never leave the input row stranded; the fly-by preview's next
    /// frame re-renders the input row below itself.
    pub(crate) fn announce(
        &mut self,
        block_text: &str,
        consume: &dyn Fn(),
        input_restored: &dyn Fn(),
        commit: &dyn Fn(),
    ) {
        if self.diverged {
            return;
        }
        let completed = completed_text(block_text);
        if completed == self.flushed {
            return;
        }
        match preview_flush(&self.committed, completed) {
            PreviewFlush::Print(new_lines) => {
                self.flushed = completed.to_string();
                if !new_lines.is_empty() {
                    // The shared transient-band erase: status + the
                    // story-2.1 input row (when it is live) — so the
                    // announced rows land at the band top and the input
                    // row is re-drawn below them before the status row
                    // re-commits.
                    consume();
                    // The announce's own input redraw needs the CURRENT
                    // mid-run buffer; the caller supplies a callback that
                    // reads the live state (this.story-2.1 review: an
                    // inert announce would strand/destroy that row).
                    // PreviewTracker doesn't hold the mid-run text, so
                    // the closure receives nothing and the caller's
                    // `redraw_input_line` reads it from shared state.
                    let no_context = "";
                    let _ = no_context;
                    inline::print_lines(&new_lines);
                    input_restored();
                    commit();
                    #[cfg(test)]
                    self.order.extend(
                        new_lines
                            .iter()
                            .map(|l| format!("text:{}", line_plain_text(l))),
                    );
                    self.committed.extend(new_lines.iter().map(line_plain_text));
                }
            }
            PreviewFlush::Diverged => {
                // A later re-render changed lines that were already printed
                // (e.g. a soft line break joined two of them). Stop
                // previewing; the end-of-run path falls back to today's
                // full render.
                self.diverged = true;
            }
        }
    }
}

// ── Print helpers (shared widgets) ──────────────────────────
//  - CLI and TUI share one styling vocabulary
//  - Color/TTY decisions live in one place (`widgets::capabilities`)
pub(crate) fn print_success(text: &str) {
    inline::print_line(&components::success_badge(text));
}

pub(crate) fn print_warning(text: &str) {
    inline::print_line(&components::warning_badge(text));
}

pub(crate) fn print_error(text: &str, _color_enabled: bool) {
    // Errors go to stderr in plain styled text. We keep a simple format here
    // because `inline::print_line` writes to stdout; for stderr we render
    // directly with optional ANSI based on capabilities.
    if capabilities::should_use_color() {
        let mut stderr = StandardStream::stderr(termcolor::ColorChoice::Always);
        let _ = stderr.set_color(
            ColorSpec::new()
                .set_fg(Some(Color::White))
                .set_bg(Some(Color::Red)),
        );
        eprint!("  ✗ {} ", text);
        let _ = stderr.reset();
        eprintln!();
    } else {
        eprintln!("  ✗ {}", text);
    }
}

pub(crate) fn print_info(text: &str) {
    inline::print_line(&components::info_badge(text));
}

/// Extract up to `max_snippets` short windows around occurrences of
/// `query` in `content`. Each window is bounded by `max_chars`, breaks
/// on UTF-8 boundaries, and is prefixed/suffixed with an ellipsis when
/// truncated.
///
/// The match itself is preserved verbatim; we don't attempt to highlight
/// the matched substring inside the snippet (that would require a richer
/// span builder than the rest of the inline renderer expects).
pub(crate) fn extract_match_snippets(
    content: &str,
    query: &str,
    max_chars: usize,
    max_snippets: usize,
) -> Vec<String> {
    if query.is_empty() {
        return vec![truncate_one_line(content, max_chars)];
    }

    let lower_content = content.to_lowercase();
    let lower_query = query.to_lowercase();

    let mut out = Vec::with_capacity(max_snippets);
    let mut search_from = 0usize;
    let context = max_chars / 2;

    while out.len() < max_snippets {
        let Some(rel) = lower_content[search_from..].find(&lower_query) else {
            break;
        };
        let match_start = search_from + rel;
        let match_end = match_start + lower_query.len();

        // Window: pad `context` chars before/after the match, snapped to
        // UTF-8 boundaries.
        let start = floor_char_boundary(content, match_start.saturating_sub(context));
        let end = ceil_char_boundary(content, (match_end + context).min(content.len()));
        let mut snippet = String::new();
        if start > 0 {
            snippet.push('…');
        }
        snippet.push_str(content[start..end].replace('\n', " ").trim());
        if end < content.len() {
            snippet.push('…');
        }
        out.push(snippet);

        // Advance past this match so we don't loop on the same hit.
        search_from = match_end;
    }

    if out.is_empty() {
        out.push(truncate_one_line(content, max_chars));
    }
    out
}

pub(crate) fn truncate_one_line(s: &str, max_chars: usize) -> String {
    let one = s.replace('\n', " ");
    if one.chars().count() <= max_chars {
        return one;
    }
    let cut = floor_char_boundary(&one, max_chars);
    format!("{}…", &one[..cut])
}

pub(crate) fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

pub(crate) fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    let len = s.len();
    if i >= len {
        return len;
    }
    while i < len && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Walk up from `start` looking for `target` directory, creating it if not found.
/// If none exists up to the filesystem root, falls back to `start`.
pub(crate) fn find_or_create_dir(start: &std::path::Path, target: &str) -> std::path::PathBuf {
    let mut current: Option<&std::path::Path> = Some(start);
    while let Some(dir) = current {
        let candidate = dir.join(target);
        if candidate.is_dir() {
            return candidate;
        }
        current = dir.parent();
    }
    // Create in cwd
    let fallback = start.join(target);
    let _ = std::fs::create_dir_all(&fallback);
    fallback
}

#[cfg(test)]
mod status_painter_tests {
    use super::*;
    use crate::widgets::progress::{Phase, ProgressState};

    fn state() -> std::sync::Mutex<ProgressState> {
        let mut p = ProgressState::new();
        p.start();
        std::sync::Mutex::new(p)
    }

    /// The redraw overwrites in place without `Clear`, so a frame narrower
    /// than the previous one must be padded or stale glyphs stay on screen.
    #[test]
    fn narrower_frames_are_padded_to_the_widest() {
        let state = state();
        let mut painter = StatusPainter::new();
        painter.show_cancel_hint = true;

        state
            .lock()
            .unwrap()
            .set_tool("a_very_long_tool_name_indeed");
        let wide = painter.next_line(&state).width();

        state.lock().unwrap().set_phase(Phase::Thinking);
        let narrow = painter.next_line(&state).width();

        assert_eq!(narrow, wide, "narrow frame must be padded");
    }

    #[test]
    fn status_line_grows_with_the_token_counter() {
        let state = state();
        let mut painter = StatusPainter::new();

        state.lock().unwrap().set_phase(Phase::Streaming);
        state.lock().unwrap().add_stream_chars(40);
        let small = painter.next_line(&state).width();

        state.lock().unwrap().add_stream_chars(400_000);
        let big = painter.next_line(&state).width();

        assert!(big > small, "token counter should widen the line");
    }

    // ── Resize doctrine (story 1.2) ─────────────────────────
    //
    // `redraw` overwrites in place with MoveUp(1) + repaint and no
    // Clear, so the next frame must cover every column its predecessor
    // could have painted WITHOUT WRAPPING at the width the terminal has
    // now. `frame_pad_on_resize` is that pure min-cover rule:
    // `pad = max(line_width, min(frame_width, ambient_now))`.

    #[test]
    fn pad_unchanged_width_is_todays_pad_to_widest_bit_for_bit() {
        // Baseline measured at the current ambient: exactly today's
        // pad-to-widest rule, unchanged.
        assert_eq!(frame_pad_on_resize(60, 100, 40), 60);
        assert_eq!(frame_pad_on_resize(40, 100, 40), 40);
        // First frame: no baseline yet, the line sets it.
        assert_eq!(frame_pad_on_resize(0, 100, 55), 55);
        // A wider frame re-baselines the pad upward within one width.
        assert_eq!(frame_pad_on_resize(60, 100, 70), 70);
    }

    #[test]
    fn pad_on_widening_keeps_the_widest_baseline() {
        // Baseline 60 measured at 80 cols; the terminal widens to 120.
        // The widest-so-far baseline still pads (60 fits unwrapped at
        // 120), and only a wider frame lifts the baseline.
        assert_eq!(frame_pad_on_resize(60, 120, 40), 60);
        assert_eq!(frame_pad_on_resize(60, 120, 80), 80);
    }

    #[test]
    fn pad_on_partial_narrowing_keeps_the_retained_baseline() {
        // Baseline 60 built at 100 cols; the terminal narrows to 80. The
        // predecessor painted 60 columns unwrapped (60 ≤ 80), so the
        // next frame must still cover those 60 — resetting the pad to
        // the new frame's own width would strand stale glyphs in
        // columns 40–59 under the no-Clear in-place overwrite.
        assert_eq!(frame_pad_on_resize(60, 80, 40), 60);
    }

    #[test]
    fn pad_on_deep_narrowing_caps_at_the_new_ambient_width() {
        // Baseline 60 built at 100 cols; the terminal narrows to 50. The
        // baseline itself no longer fits: best-effort cover the maximal
        // one row, and the frame never exceeds the new width.
        assert_eq!(frame_pad_on_resize(60, 50, 40), 50);
    }

    #[test]
    fn pad_widen_then_narrow_needs_only_the_frame_width_state() {
        // The caller's only stored state is `frame_width`; no second,
        // stale ambient measurement participates in the decision. The
        // same baseline pads correctly across a widening and a later
        // narrowing, decided purely by the fresh per-frame measurement.
        let baseline = 70;
        assert_eq!(frame_pad_on_resize(baseline, 100, 50), 70); // widened
        assert_eq!(frame_pad_on_resize(baseline, 60, 50), 60); // narrowed
        assert_eq!(frame_pad_on_resize(baseline, 80, 90), 90); // wider frame re-baselines
    }
}

// ── Live-preview flush helper tests (story 1.1) ──────────────

#[cfg(test)]
mod preview_flush_tests {
    use super::*;
    use crate::widgets::markdown::MarkdownContent as Md;

    fn plains(lines: &[RLine<'static>]) -> Vec<String> {
        lines.iter().map(line_plain_text).collect()
    }

    /// The happy-path shape of one flush: `Print` with plain-text lines.
    fn printed(committed: &[String], text: &str) -> Vec<String> {
        match preview_flush(committed, text) {
            PreviewFlush::Print(lines) => plains(&lines),
            PreviewFlush::Diverged => panic!("expected Print, got Diverged for {text:?}"),
        }
    }

    /// Whole-line granularity: a trailing fragment without a newline can
    /// still grow into a longer line, so it stays pending (deferred to a
    /// later tick — the gate-contention parking is this same deferral).
    /// Whole-paragraph granularity (design decision A, "exactly once"): a
    /// single newline inside a paragraph is a soft break that joins lines,
    /// so a paragraph stays pending until a blank-line boundary completes
    /// it — announcing its first line early would later change printed
    /// lines and trip the duplication fallback.
    #[test]
    fn trailing_partial_line_and_open_paragraph_are_held_back() {
        assert_eq!(completed_text("Hello wor"), "");
        assert_eq!(completed_text(""), "");
        // A single newline does NOT complete a paragraph.
        assert_eq!(completed_text("Hello\nwor"), "");
        assert_eq!(completed_text("Hello\nworld\n"), "");
        // A blank-line boundary completes the paragraph.
        assert_eq!(completed_text("First para\n\n"), "First para\n\n");
        assert_eq!(
            completed_text("# Title\n\nFirst para\n\n"),
            "# Title\n\nFirst para\n\n"
        );
    }

    /// Streaming arrival: the first flush prints the completed lines…
    #[test]
    fn first_flush_prints_completed_lines() {
        assert_eq!(printed(&[], "Hello\n"), vec!["Hello"]);
    }

    /// …and each later flush prints only the suffix beyond what was
    /// already printed — in arrival order, each line exactly once.
    /// (Lines are blank-line-separated paragraphs: single-newline soft
    /// breaks join into one line and are the divergence case below.)
    #[test]
    fn later_flushes_print_only_the_suffix() {
        let committed = vec!["Hello".to_string()];
        assert_eq!(printed(&committed, "Hello\n"), Vec::<String>::new());
        assert_eq!(printed(&committed, "Hello\n\nWorld\n"), vec!["World"]);
        assert_eq!(
            printed(&committed, "Hello\n\nWorld\n\nSecond para\n"),
            vec!["World", "Second para"]
        );
    }

    /// Open-construct safety (story criterion 2): a chunk ending inside an
    /// unfenced code block renders as a temporarily closed code block, so
    /// no later non-code text can inherit code styling.
    #[test]
    fn open_fence_is_temporarily_closed() {
        let p = printed(&[], "```rust\nfn a() {\n");
        assert!(p.iter().any(|l| l.contains("fn a() {")), "code line: {p:?}");
        let last = p.last().expect("non-empty render");
        assert!(
            last.contains("drag-select to copy"),
            "fence closed temporarily: {p:?}"
        );
    }

    /// …and the fence closes properly once only the closer arrives: the
    /// temporary render was already exact, so the suffix is empty.
    #[test]
    fn fence_closes_properly_when_only_the_closer_arrives() {
        let committed: Vec<String> = match preview_flush(&[], "```rust\nfn a() {\n") {
            PreviewFlush::Print(lines) => lines.iter().map(line_plain_text).collect(),
            PreviewFlush::Diverged => panic!("expected Print"),
        };
        assert_eq!(
            printed(&committed, "```rust\nfn a() {\n```\n"),
            Vec::<String>::new()
        );
    }

    /// Guardrail, code-block case: more code arriving after a temporary
    /// close shifts the block's bottom border into a line that was
    /// already printed — the flush reports divergence instead of
    /// corrupted scrollback, and the run falls back to today's render.
    #[test]
    fn code_lines_after_a_temporary_close_diverge_to_the_fallback() {
        let committed: Vec<String> = match preview_flush(&[], "```rust\nfn a() {\n") {
            PreviewFlush::Print(lines) => lines.iter().map(line_plain_text).collect(),
            PreviewFlush::Diverged => panic!("expected Print"),
        };
        assert!(matches!(
            preview_flush(&committed, "```rust\nfn a() {\n}\n```\nDone\n"),
            PreviewFlush::Diverged
        ));
    }

    /// Instability guardrail: when a later re-render changes a line that
    /// was already printed (a soft line break joins two printed lines),
    /// the flush reports divergence and the caller falls back to today's
    /// render-at-end for that run.
    #[test]
    fn shifted_lines_diverge_to_the_fallback() {
        let committed = vec!["line one".to_string()];
        assert!(matches!(
            preview_flush(&committed, "line one\nline two\n"),
            PreviewFlush::Diverged
        ));
    }

    /// End-of-run selection, no-chunk fallback (story criterion 5): an
    /// empty printed prefix matches trivially, so the "suffix" is the
    /// whole render — exactly today's single full render. (The leading
    /// empty line is the markdown renderer's own heading spacing.)
    #[test]
    fn no_preview_matches_everything_and_yields_the_full_render() {
        let parsed = Md::parse("# Title\n\nBody\n");
        assert!(preview_prefix_matches(&[], &parsed.lines));
        assert_eq!(plains(&parsed.lines), vec!["", "# Title", "Body"]);
    }

    /// End-of-run selection: printed lines the final render no longer
    /// contains (final text escaped the stream, or a mid-run divergence)
    /// must keep today's full render.
    #[test]
    fn mismatching_or_overshooting_prefixes_do_not_match() {
        let parsed = Md::parse("real answer\n");
        assert!(!preview_prefix_matches(
            &["stale".to_string()],
            &parsed.lines
        ));
        let longer = vec!["extra".to_string(), "lines".to_string()];
        assert!(!preview_prefix_matches(&longer, &parsed.lines));
        assert!(preview_prefix_matches(
            &["real answer".to_string()],
            &parsed.lines
        ));
    }

    // ── Wrap / resize doctrine (story 1.2) ──────────────────

    /// Over-long streamed lines wrap once, at the terminal's own ambient
    /// width, by the terminal itself — the console-log hook rule
    /// (`log_writer::write_line_to_stderr` writes whole raw lines with
    /// zero custom wrapping). The flush therefore announces a 300-char
    /// paragraph as exactly ONE logical line carrying every character;
    /// nothing code-side wraps, truncates, or splits it.
    #[test]
    fn overlong_paragraph_flushes_as_one_whole_line() {
        let para = "w".repeat(300);
        let text = format!("{para}\n\n");
        let completed = completed_text(&text);
        assert_eq!(
            completed,
            format!("{para}\n\n"),
            "paragraph completes whole"
        );
        let lines = printed(&[], completed);
        assert_eq!(lines.len(), 1, "one logical line, no splitting");
        assert_eq!(lines[0].chars().count(), 300, "no truncation");
    }

    /// The flush is width-blind by construction: its signature takes no
    /// width and its body consults none, so output is a pure function of
    /// (committed, completed) — whatever the terminal width does between
    /// ticks cannot change it. A re-flush of the same stream therefore
    /// prints only the genuinely new lines, and the already-printed
    /// `committed` entries stay untouched opaque strings (never
    /// re-derived, re-wrapped, or re-rendered on a resize).
    #[test]
    fn flush_is_width_blind_and_prints_only_genuinely_new_lines() {
        let para = "w".repeat(300);
        let committed = vec![para.clone()];
        let committed_before = committed.clone();
        // "Width changed mid-stream" is invisible here by construction —
        // the flush sees no width: the 300-char line already printed is
        // not re-printed, and only genuinely new lines come out.
        assert_eq!(
            printed(&committed, &format!("{para}\n\n")),
            Vec::<String>::new()
        );
        assert_eq!(
            printed(&committed, &format!("{para}\n\nNext\n\n")),
            vec!["Next"]
        );
        assert_eq!(committed, committed_before, "committed stays untouched");
    }

    // ── End-of-run source selection (story 1.3) ──────────────

    /// Multi-turn runs accumulate every turn's deltas in `block_text`
    /// while the orchestrator returns only the last turn's answer: with a
    /// non-empty, undiverged preview and `final_result` a suffix of the
    /// stream, the multi-turn gate fires (the caller still verifies the
    /// strict line-prefix match against the full stream's parse).
    #[test]
    fn end_of_run_uses_full_stream_gate_covers_the_multi_turn_shape() {
        let stream = "First para\n\nSecond para\n\nThird para\n\n";
        assert!(end_of_run_uses_full_stream(
            &["First para".to_string(), "Second para".to_string()],
            false,
            stream,
            "Third para\n\n"
        ));
        // The same inputs diverged: today's source (final_result) wins.
        assert!(!end_of_run_uses_full_stream(
            &["First para".to_string()],
            true,
            stream,
            "Third para\n\n"
        ));
        // A final result that escaped the stream: today's source wins.
        assert!(!end_of_run_uses_full_stream(
            &["First para".to_string()],
            false,
            stream,
            "escaped final answer"
        ));
        // Empty commit (no-chunk or diverged-empty preview): today's source.
        assert!(!end_of_run_uses_full_stream(
            &[],
            false,
            stream,
            "Third para\n\n"
        ));
    }

    /// Single-turn runs keep today's decision byte-for-byte: when
    /// `final_result` equals the accumulated stream (the normal
    /// single-turn shape — the orchestrator returns the raw delta
    /// accumulation), the gate may fire but the two parses are the same,
    /// so the caller's first branch matches and behavior is unchanged.
    #[test]
    fn end_of_run_uses_full_stream_single_turn_is_identical() {
        let stream = "Hello world\n\n";
        assert!(end_of_run_uses_full_stream(
            &["Hello world".to_string()],
            false,
            stream,
            stream
        ));
        // final_result non-empty but not a stream suffix: gate off.
        assert!(!end_of_run_uses_full_stream(
            &["Hello world".to_string()],
            false,
            stream,
            "final"
        ));
    }
}

#[cfg(test)]
mod search_snippet_tests {
    use super::*;

    #[test]
    fn extract_finds_substring_case_insensitive() {
        let s = "Hello WORLD, this is a test. The world is round.";
        let snips = extract_match_snippets(s, "world", 40, 5);
        assert_eq!(snips.len(), 2);
        assert!(snips[0].to_lowercase().contains("world"));
        assert!(snips[1].to_lowercase().contains("world"));
    }

    #[test]
    fn extract_respects_max_snippets() {
        let s = "foo ".repeat(50);
        let snips = extract_match_snippets(&s, "foo", 40, 3);
        assert_eq!(snips.len(), 3);
    }

    #[test]
    fn extract_falls_back_to_truncated_line_when_no_match() {
        let s = "some content with no needle";
        let snips = extract_match_snippets(s, "needle", 10, 2);
        // "needle" *is* in s; ensure positive case still works.
        assert_eq!(snips.len(), 1);
        assert!(snips[0].to_lowercase().contains("needle"));

        let snips = extract_match_snippets("unrelated text", "missing", 8, 2);
        assert_eq!(snips.len(), 1);
        // Truncated to ~8 chars + ellipsis.
        assert!(snips[0].chars().count() <= 9);
    }

    #[test]
    fn extract_handles_utf8_boundaries() {
        let s = "你好世界 你好世界";
        let snips = extract_match_snippets(s, "世界", 6, 5);
        assert!(!snips.is_empty());
        // Must not panic and must contain the query.
        for snip in &snips {
            assert!(snip.contains("世界"));
        }
    }
}
