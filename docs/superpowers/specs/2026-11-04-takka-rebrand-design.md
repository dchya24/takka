# Design: Rebrand agentic → takka

**Status:** Draft v1 — disetujui brainstorming 2026-11-04
**Scope:** Rename penuh produk: binary, crate, lib identifier, env vars, config path (+migrasi), protokol (name + version bump 1→2), repo/installer/docs.
**Acuan:** Keputusan brainstorming — nama `takka` (bunyi kerja tak-tak-tak; 5 huruf; bersih di crates.io exact+modifier, GitHub org bebas).

## 1. Pemetaan Nama

| Sekarang | Menjadi |
|---|---|
| binary `agentic` | `takka` (clap name, help, contoh command) |
| crate `agentic-cli` | `takka-cli` (pkg; binary name `takka`) |
| crate `agentic-runtime` | `takka-runtime` (binary `takka-runtime`) |
| crate `core-agentic`, lib `core_agentic` | `takka-core`, lib `takka_core` |
| `AGENTIC_*` env (`AGENTIC_RUNTIME_BIN`, `AGENTIC_INSTALL_DIR`, `AGENTIC_VERSION`, `AGENTIC_RELEASE_BASE_URL`) | `TAKKA_*` + fallback baca `AGENTIC_*` lama satu versi (warning deprecation ke stderr, bukan silent) |
| `~/.config/agentic/` | `~/.config/takka/` — baca baru dulu, fallback lama bila baru tak ada; `takka config migrate` pindahkan file (konfirmasi user) |
| `PROTOCOL_NAME "agentic"`, `PROTOCOL_VERSION 1` | `"takka"`, `2` (satu-satunya breaking change; engine menolak frame versi lain dgn pesan jelas — kebijakan Fase 2a) |
| repo `dchya24/agentic` | `dchya24/takka` (install script URL + release workflows) |
| Tagline | *"takka — agent yang tak berhenti bekerja"* |

## 2. Prinsip

- Satu commit per lapisan (binary → lib → config/env compat → protokol → docs/installer); tiap commit gates hijau.
- Compat shim hanya untuk state user (config lama, env lama). Tidak ada alias crate/binary ganda.
- Semua behavior Fase 1–3a tidak berubah; hanya nama + envelope versi.
- Test menyebut `agentic` di-update mekanis; golden protokol eksplisit ke v2.

## 3. Risiko & Mitigasi

- User kehilangan config → fallback baca + `config migrate` (di-test eksplisit).
- Mismatch v1↔v2 → error jelas kedua sisi + docs "daemon & CLI harus versi sama".
- GitHub user `@takka` squatter → tidak relevan untuk crate/binary; org nanti out of scope.

## 4. Success Criteria

- `takka run/interactive/tui/attach --help` bekerja; `takka-runtime` ditemukan tanpa env lama.
- Config lama tetap terbaca (warning) + `takka config migrate` memindahkan ke path baru.
- Daemon v2 ↔ CLI v2 handshake hijau; CLI v2 ↔ daemon v1 ditolak dgn pesan jelas.
- `cargo test --workspace`, clippy `-D warnings`, fmt bersih; CI windows-latest hijau.
