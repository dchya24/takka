# Tasks: Agentic Project Overview

**Overview**: Task lists for the takka runtime, CLI and core library
(renamed from `agentic` in the 2026-11 `takka` rebrand)
**Created**: 2026-04-20
**Updated**: 2026-11-16

---

## Snapshot

| Crate / surface | Status |
|-----------------|--------|
| `takka-core` library | Foundation complete; ~95% architecture coverage. Orchestrator/safety/memory/toolset hardened through the 2026-11-14 audit: bounded shell tools with timeout/cancel/process-group kill, poisoning-tolerant locks, single token estimator, spend ceiling (`agent.max_cost_usd`). **Open work:** see `docs/ROADMAP.md`. |
| `takka-runtime` daemon | ACP v1 adapter (PRs #16–#19) + Windows named-pipe transport (PR #26) on `dev`. Note: the Windows path is compile-checked; real-Windows verification pending. |
| `takka-cli` binary | Run / interactive / TUI flows complete. Shared widgets stack lands all output through ratatui primitives; budget chip + cost-line in the status bar. |

## Task Files

| File | Description |
|------|-------------|
| `tasks-core-agentic.md` | Core Rust library |
| `tasks-agentic-cli.md` | Standalone CLI binary |

## Reading Order

```
1. tasks-core-agentic.md → Library (foundation)
        │
        ▼
2. tasks-agentic-cli.md → CLI binary (uses the library)
```

## Branch Pointers

```bash
# Foundation work landed on `dev`; new work branches off dev.
git checkout -b feature/<your-feature>
```

---

## Notes

- Each task file is independent but the CLI depends on the library
- Check off tasks (`- [ ]` → `- [x]`) as you complete them
- For deep architecture context see `docs/architecture-alignment-overview-25052026.md`
- For roadmap-level priorities see `docs/ROADMAP.md`
