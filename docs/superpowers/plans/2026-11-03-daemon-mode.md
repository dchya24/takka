# Daemon Mode Exclusive (Fase 3a) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Daemon long-lived di Unix socket (satu engine, satu sesi aktif): `agentic-runtime --daemon` (broker socket ⇄ MemoryTransport, engine tak berubah), discovery via `daemon.json` + token `/dev/urandom`, attach kedua ditolak `"daemon in use"`, re-attach mengadopsi sesi (`SessionAdopted`), `agentic attach` + `--shutdown`, auto-spawn dari CLI; Windows menerima error actionable (named pipe = TODO).

**Architecture:** *Broker, bukan engine baru* — daemon mem-pump frame antara koneksi socket dan `MemoryTransport` milik satu `RuntimeEngine`; detach → event di-drain ke discard sehingga engine tidak pernah macet dan orchestrator bertahan. Adopt-on-re-attach lewat flag engine `adopt_on_reinit` (default `false` → 2a strictness embedded/spawned tetap). Semuanya additive, `PROTOCOL_VERSION` tetap 1.

**Tech Stack:** Rust std (`UnixListener`/`UnixStream`, `/dev/urandom` untuk token), serde JSONL; tanpa dependensi baru. Reference spec: `docs/superpowers/specs/2026-11-03-daemon-mode-design.md`.

## Global Constraints

- Semua modul daemon/socket server-side `#[cfg(unix)]`; di Windows: flag `--daemon` dan mode Daemon CLI → **hard error actionable satu baris** (unit test pesannya); named pipe = TODO eksplisit.
- Wire additive: `Init.daemon_token: Option<String>` (`#[serde(default)]`), `Event::SessionAdopted { session_id: Option<String> }`, `InitOverrides` bertambah `PartialEq`. `PROTOCOL_VERSION` tetap 1.
- String error persis: `"unauthorized"`, `"daemon in use"`, `"daemon already running with different config; restart daemon or match its config"`, `"capability '<name>' required"` (dari 2a), `"no daemon running..."` (CLI attach saat daemon mati), `"already initialized"` (mode non-daemon, dari 2a — tetap).
- Adopt path HANYA saat `adopt_on_reinit == true` (di-set daemon broker saja): bandingkan `experimental_api` requested vs existing (requested tanpa dimiliki → error capability); `opt_out_notify` baru di-IGNORE (opt-out pertama tetap menang) — terdokumentasi di komentar engine; `InitOverrides` di-compare strict (beda → error config). Token dicek SEBELUM adopt logic.
- Engine TIDAK menerima perubahan perilaku lain; broker = komponen `agentic-runtime`; TIDAK ada run yang di-generate engine; klien-klien lain split: korelasi request-id tak tersentuh.
- Tanpa dependensi baru di workspace (token dari `/dev/urandom`).
- Semuanya hermetic: socket + info file dari `support::tempdir()`; smoke matriks CLI yang ada tetap hijau; `#[cfg(unix)]` pada semua test daemon/socket.
- CI gate: `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all --check`, seluruh test pass.
- Branch kerja: `feature/daemon-mode` dari `dev`.

---

### Task 1: Core — transport socket + `daemon_token` + adopt-on-re-init + `SessionAdopted`

**Files:**
- Modify: `core-agentic/src/runtime/transport.rs` (`UnixSocketTransport` client + `SocketConnectionTransport` server-side, `#[cfg(unix)]`)
- Modify: `core-agentic/src/runtime/protocol.rs` (`Init.daemon_token`, `InitOverrides: PartialEq`)
- Modify: `core-agentic/src/runtime/engine.rs` (`daemon_token`/`adopt_on_reinit` state + Init-arm logic; `Event::SessionAdopted` di-handle)
- Modify: `core-agentic/src/events.rs` (varian `SessionAdopted`)
- Test: `core-agentic/tests/runtime_engine.rs` (tamb), test transport di file masing-masing

**Interfaces:**
- Consumes: `Transport` trait, `MemoryTransport`, handshake helpers yang ada.
- Produces (dipakai Task 2–3):
  - `#[cfg(unix)] pub struct UnixSocketTransport` + `pub fn connect(path: &Path) -> io::Result<Self>` + `impl Transport`
  - `#[cfg(unix)] pub struct SocketConnectionTransport` + `pub fn new(stream: UnixStream) -> io::Result<Self>` (try_clone untuk reader) + `impl Transport`
  - `Request::Init` arm logic: token check → strict init/adopt; field daftar:
    - `RuntimeEngine::set_daemon_token(&mut self, token: Option<String>)` (None = non-daemon, token di-ignored)
    - `RuntimeEngine::set_adopt_on_reinit(&mut self, bool)` (default false)
  - `Event::SessionAdopted { session_id: Option<String> }` — wire `session_adopted` (serde rename, pola Task 1 Fase 2b)
  - `InitOverrides: PartialEq` derive + `ConnectionCapabilities`: `compatible_caps(&self, requested: &Self) -> bool` (= `!requested.experimental_api || self.experimental_api`)

- [ ] **Step 1: Tulis test gagal**

Di `core-agentic/src/runtime/transport.rs`, module test `(cfg(unix))` + (file-level serialization tests masuk di protocol.rs/engine tests):

1. protocol.rs inline tests:
```rust
    #[test]
    fn init_carries_daemon_token_and_defaults_absent() {
        let frame = r#"{"v":1,"id":"i","type":"init","daemonToken":"tok-1"}"#;
        let req: ProtocolRequest = serde_json::from_str(frame).unwrap();
        match req.request {
            Request::Init { daemon_token, .. } => assert_eq!(daemon_token.as_deref(), Some("tok-1")),
            other => panic!("expected Init, got {other:?}"),
        }
        let bare: Request = serde_json::from_str(r#"{"type":"init"}"#).unwrap();
        assert!(matches!(bare, Request::Init { daemon_token: None, .. }));
    }

    #[test]
    fn capabilities_compatibility_gates_only_experimental() {
        let base = ConnectionCapabilities::default();
        assert!(base.compatible_caps(&ConnectionCapabilities::default()));
        assert!(!base.compatible_caps(&ConnectionCapabilities { experimental_api: true, opt_out_notify: vec![] }));
        let granted = ConnectionCapabilities { experimental_api: true, opt_out_notify: vec![] };
        assert!(granted.compatible_caps(&base));
        // opt-out baru di-IGNORE (bukan fatal), tidak masuk kompatibilitas.
        assert!(granted.compatible_caps(&ConnectionCapabilities { experimental_api: true, opt_out_notify: vec![NotificationKind::Thinking] }));
    }
```

2. engine tests (`runtime_engine.rs`): (pola `MemoryTransport` + worker thread yang ada)
```rust
/// (non-daemon engine) token di-ignore; token tak berdampak — jalur lama.
/// (daemon engine) token salah → "unauthorized" SEBELUM adopt logic.
#[test]
fn daemon_wrong_token_is_unauthorized_before_init() {
    // engine = RuntimeEngine::with_session(...); engine.set_daemon_token(Some("secret".into()));
    // worker; kirim Init{daemon_token:"wrong"} → Error "unauthorized"
    // (dan orchestrator tetap initialized? — dengan with_session sudah initialized;
    //  cukup assert error unauthorized muncul; init tak berulang)
}

#[test]
fn adopt_on_reinit_accepts_compatible_init_and_emits_session_adopted() {
    // with_session + set_adopt_on_reinit(true) + set_daemon_token(Some(t))
    // kirim Init{daemon_token:t, capabilities:Default} → InitOk lalu Event::SessionAdopted
    // requestId "adopt-1" ter-tag di InitOk (SessionAdopted event juga)
}

#[test]
fn adopt_rejects_new_capability_and_overrides_mismatch() {
    // sama setup; Init kedua dgn capabilities{experimental_api:true} →
    //   Error "capability 'experimental_api' required"
    // Init ketiga dgn overrides{model: Some("x")} tak sama dgn current →
    //   Error "daemon already running with different config; restart daemon or match its config"
}

#[test]
fn strict_already_initialized_still_default_without_adopt_flag() {
    // with_session TANPA adopt flag → Init → tetap Error "already initialized"
    // (regresi 2a eksplisit)
}

#[cfg(unix)] #[test]
fn socket_transport_round_trips_frames() {
    // bind listener di support::tempdir(); spawn server thread yang menimbulkan
    // SocketConnectionTransport atas accept lalu menulis ProtocolEvent Ready;
    // client UnixSocketTransport::connect(path).next == Ready dan balas Request
    // yang server baca kembali (uji both directions).
}
```

- [ ] **Step 2: Jalankan test → verify gagal**

Run: `cargo test -p core-agentic`
Expected: COMPILE ERROR — field/signature tak ada (`daemon_token`, `SessionAdopted`, `set_daemon_token`, `UnixSocketTransport`, `compatible_caps`).

- [ ] **Step 3: Implementasi**

1. `transport.rs`: duplikasi struktur BufReader/Mutex seperti `StdioTransport`, tapi atas `UnixStream`:
```rust
#[cfg(unix)]
pub struct UnixSocketTransport {
    input: Mutex<BufReader<UnixStream>>,
    output: Mutex<BufWriter<UnixStream>>,
}
#[cfg(unix)]
impl UnixSocketTransport {
    pub fn connect(path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        Self::from_stream(stream)
    }
    pub fn from_stream(stream: UnixStream) -> io::Result<Self> { /* try_clone → reader */ }
}
#[cfg(unix)]
impl Transport for UnixSocketTransport { /* identik StdioTransport (line framing) */ }

#[cfg(unix)]
pub struct SocketConnectionTransport { /* field sama; ctor `new(stream: UnixStream) -> io::Result<Self>` */ }
```
   Frame format: satu `ProtocolEvent`/`ProtocolRequest` JSON per baris + `\n` (persis StdioTransport; line`.trim()` empty skip).

2. `events.rs`: varian `#[serde(rename = "session_adopted")] SessionAdopted { session_id: Option<String> }` (+ EventType mapping jika ada).

3. `protocol.rs`: `Init { overrides, capabilities, daemon_token: Option<String> }` (`#[serde(default, rename="daemonToken")]`); `InitOverrides` derive tambah `PartialEq`; `ConnectionCapabilities::compatible_caps` seperti interface.

4. `engine.rs`:
   - field: `daemon_token: Option<String>` (None default), `adopt_on_reinit: bool` (false default), `current_overrides: InitOverrides` (tersimpan saat init pertama ATAU adopt-sukses? — simpan INIT pertama saja; adopt tidak menimpa).
   - setters: `set_daemon_token`, `set_adopt_on_reinit`.
   - `Init` arm (tanggalkan arm dari 2a; string error yang ada TIDAK diubah):
   ```rust
                Request::Init { overrides, capabilities, daemon_token } => {
                    if let Some(expected) = &self.daemon_token {
                        if daemon_token.as_deref() != Some(expected.as_str()) {
                            self.emit(Some(request.id), Event::Error { message: "unauthorized".to_string() });
                            continue;
                        }
                    }
                    if self.initialized && !self.adopt_on_reinit {
                        self.emit(Some(request.id), Event::Error { message: "already initialized".to_string() });
                        continue;
                    }
                    if self.initialized {
                        // Adopt path (daemon-only).
                        if !self.capabilities.compatible_caps(&capabilities) {
                            self.emit(Some(request.id), Event::Error { message: "capability 'experimental_api' required".to_string() });
                            continue;
                        }
                        if self.current_overrides != overrides {
                            self.emit(Some(request.id), Event::Error { message: "daemon already running with different config; restart daemon or match its config".to_string() });
                            continue;
                        }
                        // Caps payload sendiri disimpan? TIDAK — caps intanggible:
                        // experimental_api milik engine lama tetap menang; opt-out
                        // baru di-IGNORE (terdokumentasi). Session melanjutkan.
                        let session_id = self.orchestrator.as_ref().and_then(|o| o.session_id());
                        self.emit(Some(request.id), Event::InitOk { protocol: PROTOCOL_NAME.to_string(), version: PROTOCOL_VERSION });
                        self.emit(Some(request.id), Event::SessionAdopted { session_id });
                        continue;
                    }
                    // ... jalur normal initialize (dari 2a), TAPI simpan:
                    self.current_overrides = overrides;
                    self.capabilities = capabilities;
                    self.sink.set_opt_outs(&self.capabilities.opt_out_notify.clone());
                    self.initialized = true;
                    self.emit ..., Event::InitOk);
                }
   ```
   > Implementer WAJIB menjaga arm lain (Run/Plan dst.) tidak tersentuh, dan seluruh strings persis.

- [ ] **Step 4: Jalankan test → verify pass 2 lapis**

Run: `cargo test -p core-agentic` lalu `cargo test -p agentic-cli`
Expected: semua pass (termasuk test regresi `strict_already_initialized_still_default_without_adopt_flag`). Smoke laras embedded/spawned tetap hijau.

- [ ] **Step 5: Commit**

```bash
git add core-agentic
git commit -m "feat(core): unix-socket transports, daemon token, adopt-on-re-init + SessionAdopted"
```

---

### Task 2: agentic-runtime — broker daemon + flags + stale cleanup

**Files:**
- Create: `agentic-runtime/src/daemon.rs` (`#[cfg(unix)]`)
- Modify: `agentic-runtime/src/main.rs` (flag `--daemon [--socket-path P] [--info-path P]`; Windows guard)
- Modify: `agentic-runtime/src/stdio_transport.rs` (tidak tersentuh, hanya jika perlu share helper framing — optional: helper kecil `write_proto_line`/`read_proto_line` dibuat `pub(crate)` di runtime crate untuk reuse)
- Test: `agentic-runtime/src/daemon.rs` (in-process broker + UnixListener tests)

**Interfaces:**
- Consumes (Task 1): `UnixSocketTransport`/`SocketConnectionTransport`, engine token/adopt setters, `support::tempdir` — NOTE: support ada di core tests; runtime crate membuat helper tempdir sendiri kecil (11 baris) supaya tak cross-dep.
- Produces (Task 3):
  - `pub struct DaemonConfig { pub socket_path: PathBuf, pub info_path: PathBuf }` (keduanya di-utamakan eksplisit oleh CLI/tests)
  - `#[cfg(unix)] pub fn serve(config: DaemonConfig) -> io::Result<()>` (blocking, dipanggil dari main; test memanggil di thread)
  - info file schema: `{"pid": u32, "socket_path": String, "token": String, "version": String}` perms 600
  - token 32 hex random dari `/dev/urandom` (`read 16`)
  - Windows: `pub fn not_supported_error() -> &'static str` clearinghouse pesan tunggal yang dipakai main (unit test Windows-level tapi compiles di CI unix также)

- [ ] **Step 1: Broker sketch (net `serve`)**

```rust
pub struct DaemonConfig { pub socket_path: PathBuf, pub info_path: PathBuf }

#[cfg(unix)]
pub fn serve(config: DaemonConfig) -> io::Result<()> {
    // 1. Stale guard:
    //    - info file exists & payload pid alive → Err(io::Error::other(format!(
    //        "daemon already running (pid {})", pid)))
    //    - socket file exists & connect succeeds with live info → idem (via pid)
    //    - else: unlink stale info + socket file.
    // 2. Bind UnixListener, token = 32 hex dari /dev/urandom.
    // 3. Engine via MemoryTransport channels:
    //    let (req_tx, req_rx) = mpsc::channel();
    //    let (evt_tx, evt_rx) = mpsc::channel();
    //    let mut engine = RuntimeEngine::new(MemoryTransport::new(req_rx, evt_tx));
    //    engine.set_daemon_token(Some(token.clone()));
    //    engine.set_adopt_on_reinit(true);
    //    engine thread: engine.run()  (Setelah engine run() selesai (Shutdown) → tambuh shutdown flag.)
    // 4. Tulis info file (perms 600). SIGTERM/INT tidak di-handle: sengaja rely
    //    pada stale-cleanup daemon berikutnya (terdokumentasi).
    // 5. Loop: listener.set_nonblocking(true);
    //    - poll accept (sleep 20ms) — juga cek shutdown flag yang di-set engine thread
    //      engine thread men-set Arc<AtomicBool> SHUTDOWN saat run() return.
    //    - accepted → handle_connection(stream, req_tx, evt_rx, active_conn_state)
    // 6. setelah loop: join engine thread, unlink socket + info, Ok(()).
}
```
   - `handle_connection` aturan pertama: SATU koneksi aktif. Simpan `Arc<Mutex<Option<WriterHandle>>>`; koneksi baru saat ada yang aktif → tulis `ProtocolEvent::new(None, Event::Error { message: "daemon in use" })` lalu tutup. Koneksi aktif: thread pembaca frame → `req_tx.send`; drain loop menulis frame dari `evt_rx.recv()` ke SocketWriter, atau discard saat detached (writer drop → set slot jadi None).
   - Saat detached: engine tidak pernah block (req channel unbounded; evt drain terus berjalan).

- [ ] **Step 2: main.rs flag dispatch**

```rust
let args: Vec<String> = std::env::args().skip(1).collect();
// hand-parse: --daemon, --socket-path P, --info-path P (nilai default:
//   {config_dir}/agentic-daemon-<pid>.sock + {config_dir}/daemon.json,
//   config_dir = $HOME/.config/agentic, fallback tempdir bila HOME tak set)
if args.iter().any(|a| a == "--daemon") {
    #[cfg(not(unix))] { eprintln!("daemon mode not supported on Windows yet — use --runtime embedded|spawn"); std::process::exit(1); }
    #[cfg(unix)] { daemon::serve(config) ... exit code dari result }
}
// tanpa --daemon: jalur lama (StdioTransport) tidak berubah
```
   (Implementer dapat menata helper parse; tangibles: Windows message HANYA satu baris yang itu.)

- [ ] **Step 3: Tests**

Di `daemon.rs` mod tests (semuanya `#[cfg(unix)]`):
```rust
#[test] fn serve_round_trips_basic_run()       // spawn serve di thread (tempdir sockets),
                                              // client UnixSocketTransport::connect →
                                              // handshake (Ready→Init→InitOk) → Run... pakai
                                              // engine with_session? — broker internal buat engine
                                              // baru (initialize path). Init w/ hermetic config
                                              // fixture (config_path) → InitOk → Run → Done/Error
                                              // (provider unreachable). Join.
#[test] fn second_connection_is_refused()     // koneksi aktif (ghanta Run in-flight: pakai
                                              // provider menggantung mid-run? Simpler: connection
                                              // A publish Init; koneksi B connect → baca satu frame
                                              // = Error "daemon in use".
#[test] fn reattach_adopts_session()          // A: Init → InitOk; drop A (close stream);
                                              // shed<Event>(evt side) tetap hidup; B connect →
                                              // Init (same token+caps+overrides) → InitOk+SessionAdopted.
#[test] fn shutdown_unlinks_files()           // Request::Shutdown → serve returns; socket & info
                                              // files tidak ada. (serve di thread; poll dir.)
#[test] fn stale_info_file_is_cleaned()       // info file dgn pidExisting(dead pid: spawn & wait,
                                              // atau pid tidak hidup) → serve TIDAK melaporkan "already
                                              // running", bind sukses → file baru.
```
   Implementer menambah worker helper sendiri (channel→tempdir per test). Batas waktu 10s di semua poll.

- [ ] **Step 4: Build + gates cepat**

Run: `cargo build --workspace && cargo test -p agentic-runtime && cargo test -p core-agentic && cargo test -p agentic-cli`
Expected: semua pass; CLI smoke lama tetap hijau (binary baru mampu CLI still works — runtime binary yang dibedah oleh T tests disk cover has `--daemon` flag optional).

- [ ] **Step 5: Commit**

```bash
git add agentic-runtime
git commit -m "feat(runtime): unix-socket daemon broker with token, in-use refusal, attach adoption, stale cleanup"
```

---

### Task 3: CLI — `RuntimeMode::Daemon`, discovery + auto-spawn, `agentic attach`

**Files:**
- Modify: `agentic-cli/src/client.rs` (+`RuntimeMode::Daemon`, `DaemonConnection`, handshake reuse, spawn-daemon helper)
- Modify: `agentic-cli/src/commands.rs` (`ensure_connection` branch, attach helpers)
- Modify: `agentic-cli/src/cli.rs` (+`Attach` subcommand, `--shutdown` flag)
- Modify: `agentic-cli/src/main.rs` (attach dispatch)
- Test: `agentic-cli/src/client.rs` tests, `commands.rs` e2e smoke daemon mode

**Interfaces:**
- Consumes: `UnixSocketTransport`, daemon.json schema (Task 1–2), `runtime_binary()` yang ada.
- Produces:
  - `RuntimeMode::Daemon` (+ `RuntimeModeArg::Daemon` di cli.rs → warna error actionable di Windows build; pilih: konversi tetap jalan, tetapi `connect` yang memeriksa `cfg!(unix)` dan melempar `daemon mode not supported on Windows yet — use --runtime embedded|spawn`)
  - `struct DaemonInfo { pid: u32, socket_path: String, token: String, version: String }` + `parse_daemon_info(path)`, `daemon_info_path()` (default `$HOME/.config/agentic/daemon.json`)
  - `AgentConnection::connect_daemon(overrides, caps, spawn_if_missing: bool) -> Result<Self>`:
    1. read info → **invalid/stale/missing** && spawn_if_missing → spawn `runtime_binary() --daemon` (poll daemon.json ≤10s) → re-read;
    2. still absent → `Err("no daemon running — start it with 'agentic-runtime --daemon' or use --runtime embedded")`
    3. pid alive check (`/proc` or `UnixStream::connect` fail → stale treat) — implementer memilih salah satu utilitary; boleh kombinasi.
    4. connect — rekomendasi: async-native pakai `tokio::net::UnixStream` (tokio features full sudah ada) — implement `DaemonConnection implements ConnectionIo` (async line read/write — mirip line framing jalur spawned tapi atas UnixStream async); handshake lewat `perform_handshake` (ConnectionIo).
    5. Init checks: `daemon_token: Some(token)` di frame.
  - `Commands::attach_daemon(shutdown: bool) -> Result<()>`: connect spawn_if_missing=false → shutdown? `send_and_wait(Request::Shutdown)` + bye : else REPL biasa (route via connection sama).
  - e2e smoke (commands.rs): `daemon_lifecycle_smoke` (spawn daemon binary dengan `--info-path/--socket-path` tempdir + config fixture hermetic; jalankan run sepanjang connection; assert `SessionStarted`→ terminal); `in_use_refusal_smoke`; `adopt_smoke` (disconnect → reconnect → `SessionAdopted`).

- [ ] **Step 1: Tulis test gagal**

1. client.rs unit (linting `RuntimeMode::Daemon` data):
```rust
    #[test]
    fn daemon_info_parses_fields() {
        let json = r#"{"pid":4242,"socket_path":"/tmp/x.sock","token":"tok","version":"0.0.0-test"}"#;
        let info = parse_daemon_info_str(json).unwrap();
        assert_eq!(info.pid, 4242);
        assert_eq!(info.socket_path, "/tmp/x.sock");
        assert_eq!(info.token, "tok");
    }
```
2. e2e smoke (pola sama dengan smoke embedded/spawned): matriks tambah mode `daemon` (spawn daemon binary memakai helper test CLI yang sudah spawn runtime binary, argumen tambahan `--daemon`).

- [ ] **Step 2: Jalankan test → verify gagal**

Run: `cargo test -p agentic-cli`
Expected: COMPILE ERROR — `RuntimeMode::Daemon`/`DaemonConnection`/`parse_daemon_info` tak ada.

- [ ] **Step 3: Implementasi** (ikut sketch; implementer boleh membuat beberapa sub-commit selama akhirnya satu task rapat — yang WAJIB: seluruh isi task jadi sebelum lanjut)

- [ ] **Step 4: gates + matriks**

Run: `cargo test -p agentic-cli` dan `cargo test --workspace`
Expected: hijau semua; smoke matriks minimal: daemon-lifecycle + in-use-refusal + adopt, semuanya lewat spawn dari CLI.

- [ ] **Step 5: Commit**

```bash
git add agentic-cli
git commit -m "feat(cli): RuntimeMode::Daemon with discovery, auto-spawn, attach command"
```

---

### Task 4: Docs + gate CI

**Files:**
- Modify: `CHANGELOG.md`, `docs/BENCHMARK_CODEX.md` (Fase 3 landed sebagai 3a; Fase 3b = multiplexing open), `docs/ROADMAP.md` (update open-work), `README.md` (Known limitations: Windows daemon TODO; `agentic attach` Quick Start snippet)

- [ ] **Step 1: CHANGELOG** — `## [Unreleased]` → `### Added`:

```markdown
- Runtime: **daemon mode (Fase 3a)** — `agentic-runtime --daemon` hosts a
  single long-lived engine behind a Unix domain socket (`#[cfg(unix)]`),
  discovered via `~/.config/agentic/daemon.json` (pid, socket path, random
  token, version). CLIs attach with `--runtime daemon`
  (auto-spawns the daemon when absent/stale) or `agentic attach`; a second
  attach is refused with `daemon in use`; detach keeps the engine running
  and a re-attach adopts the live session (`InitOk` + `session_adopted`),
  with config/capability mismatches producing actionable errors. Windows
  daemon mode and named-pipe transport are planned (TODO).
```

- [ ] **Step 2: BENCHMARK_CODEX.md** — `### Fase 3 — Daemon mode ✅ (landed 2026-11, sebagai 3a: satu sesi aktif; 3b multi-session open)`; term health: `Daemon long-lived` row `🔜 fase lanjut` → `✅ (3a: unix socket, satu attach)`; `SDK bahasa lain` stays 🔜. Tambah 3b subsection dengan gap list (session id routing, protokol v2, SQ/EQ).

- [ ] **Step 3: ROADMAP.md** — Next: Fase 3a landed; berikutnya = Fase 3b multiplexing (protokol v2) atau Fase 4 SDK. Tandai: Windows named pipe + input-during-run (Fase UX) masih open.

- [ ] **Step 4: README.md** — Tabel/section Known Limitations: daemon mode not yet on Windows; Quick Start snippet `agentic attach` satu blok yang menyebut takia `--runtime daemon`.

- [ ] **Step 5: gates + full test**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Perbaiki hingga bersih.

- [ ] **Step 6: Commit**

```bash
git add CHANGELOG.md README.md docs/BENCHMARK_CODEX.md docs/ROADMAP.md
git commit -m "docs: daemon mode (Fase 3a) landed notes"
```

---

## Self-Review (dijalankan penulis plan — dilakukan)

1. **Spec coverage**: transport (T1), token + adopt + SessionAdopted + strictness regression (T1), broker + refusal + detach-drain + Shutdown + stale (T2), discovery/auto-spawn/attach/Windows message (T3), windows-unknown (T3 connects guard), docs + gates + auth/file perms (T2 step write 600), success criteria (semua task). Multiplexing eksplisit out (3b). ✓
2. **Placeholder scan**: tidak ada TBD/TODO kecuali istilah "TODO (named pipe)" yang didokumentasi dan mendukung sukses criteria. Sketch broker sengaja tidak 100% garis — kontrak fungsi publik dan perilaku kasus edge (refusal, detach-drain, stale, shutdown) sudah eksplisit; detail implementasi ditahan oleh loop review. ✓
3. **Type consistency**: `DaemonConfig{socket_path,info_path}`, `DaemonInfo{pid,socket_path,token,version}`, `set_daemon_token(Option<String>)`, `set_adopt_on_reinit(bool)`, `compatible_caps(&ConnectionCapabilities)`, `Event::SessionAdopted{session_id}` — konsisten antar Task 1–3. String errors konsisten dg Global Constraints. ✓
