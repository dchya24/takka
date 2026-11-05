---
title: 'Story 1.1 — Live preview of streamed text through the painter'
type: 'feature'
ticket: 'story-live-preview-of-streamed-text-through-the-painter'
created: '2026-11-05'
status: 'draft'
baseline_revision: '79a3982f07f6de6ccb4c8702e084595c8350c4c9'
route: 'full'
route_source: 'auto'
risk: 'medium'
review: ''
review_source: ''
lenses_ran: []
review_loop_iteration: 0
followup_review_recommended: false
context:
  - '_bmad-output/initiative-repl-ux-hardening/tickets/epic-streaming-preview/epic-streaming-preview.md'
warnings: ['oversized']
deferred: []
---

<intent-contract>

## Intent

**Problem:** In the inline REPL (`takka run` and the interactive loop share `Commands::run`), the streamed answer is invisible while the run is in flight — the user stares at a status line for tens of seconds and only then sees the answer, contradicting the refined story criteria of epic-streaming-preview (R2/parent R2, design decision A).

**Approach:** Inside the existing single-writer renderer thread, on each tick flush newly drained `block_text` as styled markdown lines (`markdown::parse_partial`) printed above the transient status line through the painter's proven erase→print→repaint cycle; the printed lines ARE the final answer (no end-of-run re-render). Non-streaming runs and run-level failure paths keep today's behavior.

## Boundaries & Constraints

**Always:**
- Only the renderer thread touches the terminal; every preview flush takes the `RENDER_GATE` the way the console-log hook does (`try_lock`; on contention carry the pending text to the next tick, never block).
- Lines are printed once and never erased or restyled later (no row accounting, no scrollback rewriting in this story; wrap/resize rules belong to story 1.2).
- Styled-line stability comes from re-rendering the accumulated text each flush and printing only the suffix beyond what was already printed; existing lines must be a strict prefix of the new render.
- The status line remains the last line at every moment (flush order: consume → publish lines → repaint).

**Never:**
- No changes to engine wire/protocol, `run_with_callbacks`/TUI path, tool timeline row shapes, thinking-header style, confirmation/question prompt flow, log-writer hook, or the two-stage Ctrl+C semantics.
- No final markdown re-render when the preview already printed the answer (the duplication trap); the today-style full render stays only for runs with no streamed chunks.
- No new terminal writer threads; no raw ANSI escapes outside the widget stack.

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| Streaming chunks arrive | provider emits text deltas over several turns | styled lines appear above the status line within ≤2 renderer ticks, in arrival order, each line exactly once | no error expected |
| Contention on the render gate | question/confirmation prompt holds the gate at flush time | flush is deferred to a later tick; nothing is printed while the gate is held | spinner kept intact by placeholder flow |
| No streamed chunks (non-streaming run) | `block_text` stays empty, `final_result` non-empty | live preview prints nothing; end-of-run styled markdown renders once (today's path) | no error expected |
| Stream error / provider failure mid-preview | `Err` after some preview lines | printed preview stays as ordinary scrollback; error badge printed as today | error badge via existing path |
| `final_result` falls outside streamed text | Done result carries content never streamed | today's full markdown render of `final_result` runs after the preview | duplication acceptable only here, documented |
</intent-contract>

## Code Map

- `takka-cli/src/commands.rs` — `Commands::run` renderer thread (spawn ~line 1770; loop with `chunk_rx` drain, `progress`/`StatusPainter`, closures `commit_spinner`/`redraw_spinner`/`consume_spinner`; end-of-run cleanup + markdown render in the `let mut s = std::io::stdout();` block after `renderer.join()`). All in-story changes live in/next to this block.
- `takka-cli/src/commands.rs` — `StatusPainter` (commit/redraw/erase) and the console-log hook inside the renderer: the exact gate protocol to mirror for preview flush (`RENDER_GATE.try_lock()`, plain-write fallback only when the hook cannot take the gate — the renderer itself defers instead).
- `takka-cli/src/widgets/markdown.rs` — `MarkdownContent::parse_partial` (line ~41; also `close_unclosed_fence`), the styling source for preview lines; `parse` for the fallback end-of-run render.
- `takka-cli/src/widgets/inline.rs` — `print_line/print_lines`; publishing preview lines uses the widget stack only.
- `takka-cli/src/widgets/spinner.rs` — `status_line` (phase/elapsed/tokens) — untouched, but the painter's pad rule (frames padded to widest seen) is the invariant to preserve when deferring flushes.
- Reuse, do not re-derive: painter closures, gate protocol, `chunk_tx/chunk_rx` channel, `SPINNER_ACTIVE`, existing tests `status_painter_tests`, PTY-based e2e smoke in `takka-cli/src/commands.rs` (`e2e_tests`/`daemon_smoke_tests` modules run full tests as `draw`-less PTY? no — those are threads; keep unit tests for the flush logic).

## Tasks & Acceptance

**Execution:**
- [ ] `takka-cli/src/commands.rs` — extract a pure `fn preview_flush(committed: &[String], next_paragraph: &str) -> Vec<String>`-style helper that, given the already-printed styled lines and the newly accumulated full text, returns only the NEW styled lines to print (re-parse accumulated text with `parse_partial`; strict-line-prefix comparison; divergence → empty → caller falls back to today's render-at-end for that run).
- [ ] `takka-cli/src/commands.rs` — in the renderer loop, drain chunks into `block_text` as today, then if flush is due (either everywhere: on every successful drain), under `RENDER_GATE.try_lock()` publish only-new lines (`consume → print_lines → commit`); on gate contention defer that text to a later tick; track `preview_used` for the end-of-run decision.
- [ ] `takka-cli/src/commands.rs` — end-of-run cleanup: when the preview printed the streamed content, skip the markdown re-render of `final_result`/`block_text` when the final text is contained in what was printed (run-level divergence keeps today's full render); erase status line exactly once as today; `SPINNER_ACTIVE`/progress stop unchanged.
- [ ] `takka-cli/src/commands.rs` — unit tests for the flush helper: streaming arrival produces the suffixed-only lines; gate contention path (helper pure-tested); no-chunk fallback; divergence fallback selection.
- [ ] `takka-cli/src/commands.rs` — PTY-free integration test: renderer + scripted chunk feed through the real loop asserts styled lines printed above status per tick and single final output (reuse the existing in-repo test patterns that drive `Commands::run` against pinned configs; keep hermetic — never ambient `~/.config/takka/config.json`, per `AGENTS.md`).

**Acceptance Criteria:**
- Given a scripted streaming provider and a PTY capture of `Commands::run` (both `takka run` and the REPL path use it), when the provider emits text deltas with delays, then styled answer lines appear above the status line within two renderer ticks of arrival, in arrival order, each exactly once (AC1, AC2 analogues of the story criteria 1–3).
- Given a run whose chunks never arrive (non-streaming stub), when the run finishes, then no live lines printed and the single styled markdown render appears once (story criterion 5).
- Given the gate is held by a prompt stub when a flush falls due, when the run continues, then the deferred lines print after the gate frees and the status line never leaves the bottom (story criteria 4 + the Always list).
- Given Ctrl+C after several preview lines (driven by the existing cancel test seams), when the run unwinds, then printed lines remain and only the status line is erased (story criterion 6, minimal form — deep cleanup is story 1.4).
- Given the whole workspace test suite, when it runs after the change, then `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace --all-targets` pass.

## Implementation Notes

<!-- Empty at planning time (full route) -->

## Plan Change Log

<!-- Populated by step-04 review loops -->

## Review Triage Log

<!-- Populated by step-04 on every review pass -->

## Design Notes

- Design decision A (initiative memlog, 2026-11-05): the preview renders styled markdown progressively via `parse_partial`, and the printed lines are the final answer. No final re-render on the streamed path — that is the "answer appears exactly once" guarantee. The `takka run` end-of-run render becomes conditional: full render only when nothing was streamed (or the final text escaped the stream).
- Line-level styled stability: `parse_partial` auto-closes open fences, so a line's styling, once printed, does not change later in the constructs the fallback renders. The strict-prefix check is the guardrail that turns any unforeseen instability into the documented today's-render fallback instead of corrupted output.
- The status painter paints one line whose width it pads itself; preview lines are plain `Line`s — no painter involvement beyond the existing publish cycle.

## Verification

**Commands:**
- `cargo fmt --all -- --check` — expected: no diff.
- `cargo clippy --workspace --all-targets -- -D warnings` — expected: zero warnings.
- `cargo test --workspace --all-targets` — expected: all pass, including new flush-helper tests and the EPIC e2e smoke suite; hermetic config only (no ambient `~/.config/takka/config.json`).
- `RUST_LOG=<unset> cargo run -- <...>` manual: scripted-provider run shows progressive styled text with the status line pinned last; cancel leaves it as scrollback (manual PTY check mirroring the session's fake-provider harness).

**Manual checks (if no CLI):**
- None beyond the above.
