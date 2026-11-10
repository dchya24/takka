# TUI Feature Gap — Closing the Gap with Interactive Mode

**Last audited:** 2026-11-10 (gentakka v0.6.0, commit `f0fd4b0`)  
**Scope:** `takka-cli/src/tui/`, `takka-cli/src/interactive.rs`  
**Status:** Parity ~95% — audit ulang menggantikan versi 2026-07-14 (semua item lama sudah landed)

---

## Status lama (2026-07-14) → kini

Dokumen lama mendata 6 gap (G-01 – G-06). Semua sudah landed dan diverifikasi di
source v0.6.0:

| Gap lama | Status | Ruang |
|---|---|---|
| G-01 `/` command tab completion | ✅ landed | story 3.1 (epic-tui-completions), idle + mid-run |
| G-02 `@` file path completion | ✅ landed | story 3.1, idle + mid-run |
| G-03 `/models <name>` switch model | ✅ landed | `tui/app.rs` (`/models` browse + complete) |
| G-04 `/new` proper session reset | ✅ landed | `tui/app.rs` |
| G-05 `/sessions` list & resume | ✅ landed | `tui/app.rs` |
| G-06 `/search` conversation search | ✅ landed | `tui/app.rs` |

Inventaris perintah saat ini (dari `match` di `interactive.rs` dan `tui/app.rs`):

- **Interactive (19):** `/help` `/new` `/clear` `/config` `/history` `/tools`
  `/models` `/sessions` `/skills` `/search` `/image` `/thoughts` `/mcp`
  `/provider` `/queue` `/timeline` `/stats` `/plan` `/quit`
- **TUI (15):** `/help` `/new` `/clear` `/sessions` `/models` `/search`
  `/image` `/provider` `/mcp` `/skills` `/plan` `/quit` `/config` `/tools`
  `/history`

## Sisa gap (per audit 2026-11-10)

### T-01: Q4 perintah yang belum ada di TUI

Interactive punya 4 perintah yang belum porting ke TUI:

| Perintah | Fungsi | Perkiraan effort |
|---|---|---|
| `/queue [interrupt\|follow-up]` | mode steer/queue mid-run | kecil (engine contract sudah ada) |
| `/timeline [detailed\|compact]` | detail tool timeline | kecil (renderer TUI punya data) |
| `/thoughts` / `/thinking` | toggle tampil reasoning dim | kecil |
| `/stats` | ringkasan statistik session | kecil (TUI status bar sudah menaikkan usage) |

TUI mid-run **sudah** menerima teks dan mengikuti behaviour `--queue-mode`
(story 2.4) — yang belum adalah mengubah mode live via slash command.

### T-02: context-budget indicator (minor, nomor kejelasan perlu dicek)

Status bar TUI sudah menaikkan token usage (↑↓, termasuk cache) sejak v0.6.0.
Indikator *persen budget konteks* belum dikonfirmasi paritas dengan status line
interactive — perlu dicek saat implement T-01.

## Out of scope dari dokumen ini (idea list terpisah)

Theme system (kustom/predefined), error styling dengan context/suggestion,
dan performa dropdown pada proyek besar berada dalam daftar idea
`tasks/02072026-interactive-ui-improvement.md` (audit: sebagian besar belum
ter implementasi; lihat header status di file tersebut).
