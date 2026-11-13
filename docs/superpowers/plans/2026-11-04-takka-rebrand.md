# Takka Rebrand Implementation Plan

>
> **Status:** LANDED — semua step selesai dieksekusi (checkbox disweep housekeeping 2026-11-12); fiturnya ada di CHANGELOG [0.6.0].
> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Rename penuh `agentic` → `takka` (binary, 3 crate, lib identifier, env vars + fallback, config path + migrate, protokol name/version 1→2, installer/docs) tanpa mengubah behavior Fase 1–3a.

**Architecture:** Rename mekanis berlapis; compat shim hanya di dua titik state user (config path fallback + env fallback dgn warning). Satu breaking change disengaja: `PROTOCOL_VERSION` 1→2 + `PROTOCOL_NAME` "takka".

**Tech Stack:** Rust workspace (cargo rename), clap help text, serde protocol; tanpa dependensi baru. Reference spec: `docs/superpowers/specs/2026-11-04-takka-rebrand-design.md`.

## Global Constraints

- Satu commit per task di bawah; tiap commit gates hijau (`cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all --check`) + `cargo check -p takka-cli --target x86_64-pc-windows-gnu` bersih (pola pelajaran Fase 3a).
- String pengganti: binary `takka`, crate `takka-cli`/`takka-runtime`/`takka-core`, lib `takka_core`, `PROTOCOL_NAME "takka"`, `PROTOCOL_VERSION 2`, tagline *"takka — agent yang tak berhenti bekerja"*.
- Fallback user-state: baca config baru→lama; env baru→lama + warning stderr satu baris per var; `takka config migrate` (konfirmasi).
- Error string versi: `"unsupported protocol version: {v}"` (sudah ada mekanismenya; versi baru 2).
- Tidak ada alias ganda; tidak ada behavior change; branch `feature/takka-rebrand` dari `dev`.

---

### Task 1: Binary + crate rename (takka / takka-cli / takka-runtime / takka-core)

**Files:**
- Modify: `Cargo.toml` (workspace members), `*/Cargo.toml` (package names, dep paths, binary names, descriptions), semua `use core_agentic::` → `use takka_core::`, `CARGO_BIN_EXE_agentic-runtime` → `CARGO_BIN_EXE_takka-runtime`, `[[bin]] name`, clap `name = "takka"`, help text `agentic` → `takka` (binary name, contoh command, log line).
- Test: smoke test yang spawn binary (path berubah), unit test help-text snapshot bila ada.

**Interfaces:**
- Produces: `takka_core` lib name; binary `takka` + `takka-runtime` ter-build di `target/debug/`.
- Consumes: — (task pertama).

- [x] **Step 1: Rename Cargo layer**
  - Root `Cargo.toml` members → `takka-core`, `takka-cli`, `takka-runtime`; `[workspace.package]` authors tetap.
  - `core-agentic/Cargo.toml`: `name = "takka-core"`, `[lib] name = "takka_core"` bila ada (atau default underscore mapping), description tetap.
  - `agentic-cli/` → rename dir `takka-cli/`; `Cargo.toml`: `name = "takka-cli"`, `[[bin]] name = "takka"`; dep `core-agentic` → `takka-core = { path = "../takka-core" }`; description update.
  - `agentic-runtime/` → rename dir `takka-runtime/`; sama pola; `takka-core` dep.
  - `mv` direktori via `git mv`.
- [x] **Step 2: Kode `use` + binary refs**
  - `grep -rn "core_agentic" --include="*.rs"` → `takka_core` (semua).
  - `CARGO_BIN_EXE_agentic-runtime` → `CARGO_BIN_EXE_takka-runtime`.
  - clap `name = "agentic"` → `"takka"`; `long_about` lead → tagline baru; semua contoh `agentic ...` → `takka ...` (cli.rs, main.rs strings, interactive help).
  - `TAKKA_RUNTIME_BIN` sebagai env utama di `runtime_binary()` + fallback `AGENTIC_RUNTIME_BIN` + warning (implementasi fallback di Task 2 boleh; di sini minimal rename + baca TAKKA dulu).
- [x] **Step 3: Build + test**
  - Run: `cargo build --workspace` lalu `cargo test --workspace`. Perbaiki semua sisa `agentic` yang bikin compile fail. Test failure karena string lama → update ekspektasi ke `takka`.
  - Expected: `target/debug/takka` + `target/debug/takka-runtime` ada; suite hijau.
- [x] **Step 4: Commit**
  ```bash
  git add -A && git commit -m "feat!: rename workspace to takka (binary, crates, lib identifier)"
  ```

### Task 2: Config path + env compat + `config migrate`

**Files:**
- Modify: `takka-cli/src/config.rs` (atau modul config yang ada — grep `\.config/agentic`): `config_dir()` → `~/.config/takka/`, fallback baca `~/.config/agentic/` bila baru tak ada (+ `eprintln!` warning sekali).
- Modify: env reads: `TAKKA_RUNTIME_BIN`/`TAKKA_INSTALL_DIR`/`TAKKA_VERSION`/`TAKKA_RELEASE_BASE_URL` utama, fallback `AGENTIC_*` + warning.
- Add: `takka config migrate` subcommand — pindahkan file config+sessions+memory dari lama ke baru (konfirmasi interaktif `y/N`, `--yes` flag), idempotent (bila baru sudah ada → tolak kecuali `--force`).
- Test: unit test fallback (config lama terbaca), migrate (dry-run + real di tempdir HOME override), env fallback warning.

**Interfaces:**
- Produces: `config_dir() -> PathBuf` baru + `legacy_config_dir()`; `fn resolve_env(new, legacy) -> (Option<String>, bool warned)`.
- Consumes: binary `takka` (Task 1).

- [x] **Step 1: Tulis test gagal** — config fallback + migrate + env fallback (tempdir HOME via `std::env::set_var` terkontrol serial test atau helper override).
- [x] **Step 2: Run → verify gagal** (compile error / assertion fail yang diharapkan).
- [x] **Step 3: Implementasi** sesuai interface; warning format: `takka: warning: using legacy AGENTIC_X; set TAKKA_X instead`.
- [x] **Step 4: Full suite hijau → commit** `feat!: takka config path with legacy fallback + config migrate`.

### Task 3: Protokol name/version 1→2 + daemon/CLI handshake

**Files:**
- Modify: `takka-core/src/runtime/protocol.rs` (`PROTOCOL_NAME "takka"`, `PROTOCOL_VERSION 2`), golden tests → v2, reject message untuk v1 frame.
- Modify: `takka-runtime`, `takka-cli` — tidak ada perubahan logic selain versi (handshake helper sudah baca konstanta).
- Test: unit serde v2, engine reject v1 (`unsupported protocol version: 1`), e2e smoke daemon+embedded tetap hijau di v2.

**Interfaces:**
- Produces: wire v2 homogen.
- Consumes: Task 1 (nama crate).

- [x] **Step 1: Tulis test gagal** — frame v1 ditolak engine v2; frame tanpa `v` default? (sesuai serde: tanpa default → deserialization error `protocol_error`; itu perilaku yang benar, pin di test).
- [x] **Step 2: Run → verify gagal.**
- [x] **Step 3: Implementasi** — ganti dua konstanta + update golden tests + doc comment versi.
- [x] **Step 4: Full suite + smoke daemon/flow → commit** `feat!: protocol v2 (takka) `.

### Task 4: Docs + installer + repo refs + gates

**Files:**
- Modify: `README.md`, `docs/*.md` (ganti `agentic` → `takka` di nama produk/command/path; simpan catatan migrasi satu paragraf), `scripts/install.sh`, `scripts/install.ps1` (REPOSITORY `dchya24/takka`, `TAKKA_*` vars + fallback note), `scripts/release.sh` (nama artefak), `.github/workflows/*` bila menyebut nama binary/artefak, `scripts/protocol-demo.js` (nama di log/komentar).
- Test: tidak ada (docs-only) + gates penuh.

- [x] **Step 1: Rename docs + scripts** (grep `agentic` case-insensitive sisa → takka; kecuali: `AGENTIC_*` legacy fallback docs, changelog history entries versi lama tetap `agentic` — catat eksplisit).
- [x] **Step 2: Skills dir fallback (triage dari Task 2 review — gap lintas plan)** — `takka-core/src/skills/discovery.rs`: tambah `~/.config/takka/skills` sebagai lokasi global pertama (new-wins), fallback legacy `~/.config/agentic/skills` bila takka kosong/tak ada. TANPA migrate (skills user-install jarang; fallback cukup). Update help text `takka-cli/src/cli.rs` yang menyebut `~/.config/agentic/skills` ke path baru + tool description `update_memory.rs:56` (`~/.config/agentic/memory.md` → takka). Test: new-wins + legacy-fallback.
- [x] **Step 3: Gates** — `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` + windows check (`cargo check -p takka-cli --target x86_64-pc-windows-gnu`, rustup target terinstal).
- [x] **Step 4: Commit** `docs: rebrand docs + installer to takka`.

## Self-Review

1. **Spec coverage**: pemetaan §1 semua ada task-nya (binary+crate T1, env+config T2, protokol T3, docs/installer T4); tagline di T1/T4; repo rename dicatat sebagai langkah manual GitHub (di luar kode) di T4. ✓
2. **Placeholder scan**: tidak ada TBD/TODO; semua step konkret. ✓
3. **Type consistency**: `takka_core`, `TAKKA_*`, `~/.config/takka/`, v2 — konsisten di semua task. ✓
