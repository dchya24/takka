<!-- bmad:context -->
<!-- Verified 2026-11-05 against fa351ba223fd677a1b027c5c8c5ddd5f4b98804e. Managed by bmad-project-context; edits inside this block are replaced on refresh. Keep anything you want preserved outside the markers. -->

## takka

Rust coding agent: `takka-core` (agent engine: providers, tools, safety, skills,
MCP) + `takka-cli` (the `takka` binary: config, REPL, TUI) + `takka-runtime`
(daemon, protocol v2). Contributors' guide is `CONTRIBUTING.md`; release flow is
`docs/RELEASING.md`. Conceptual agent design notes live in
`AGENT_ARCHITECTURE.md`.

## Policy

- Never commit real API keys. They live in `~/.config/takka/config.json`
  (user state, never in the repo); examples use `$ENV_VAR` placeholders only.
- Never touch `.github/` without asking first — CI and the release automation
  rebuild artifacts on every change there.
- Branching: work lands on `feature/*` branches merged via PR into `dev`.
  `dev` is the main branch (there is no `main`). Releases are cut from `dev`
  via `docs/RELEASING.md`.

## Where things are

- Planning: design specs in `docs/superpowers/specs/`, implementation plans in
  `docs/superpowers/plans/`; legacy task lists in `tasks/`.
- BMad workspace: tickets and module state in `_bmad/`, generated reports in
  `_bmad-output/`.

## Running and verifying

- The daemon e2e smoke tests pin a hermetic config (`hermetic_runtime_config`
  in `takka-cli/src/commands.rs`) so they never read the ambient
  `~/.config/takka/config.json`. New tests that touch the runtime must do the
  same — reading ambient config calls a real provider with the developer's
  credentials and fails CI.
- Daemon smoke tests can flake on a busy socket slot (observed:
  `shutdown_busy_daemon_errors_after_deadline`). Rerun before diagnosing.

## Conventions that differ from defaults

- Docs, CHANGELOG entries, and commit messages are English (some historical
  design docs are Indonesian; do not continue that for new docs).
- Commit style: imperative conventional prefixes (`fix:`, `feat:`, `docs:`,
  `chore:`), matching git history.

<!-- /bmad:context -->
