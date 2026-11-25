//! Broker-level transport tests. They drive the real `serve()` over the
//! same seam the broker uses (`platform::open_connection`), so unix and
//! Windows run the identical assertions — the named-pipe path gets the
//! same coverage as the unix socket, not a weaker stand-in.

use super::platform::open_connection;
use super::*;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use takka_core::events::Event;
use takka_core::runtime::protocol::ProtocolEvent;

/// Every poll/read in these tests is bounded by this deadline so a
/// regression can never hang the suite.
const POLL_TIMEOUT: Duration = Duration::from_secs(10);

fn deadline() -> Instant {
    Instant::now() + POLL_TIMEOUT
}

/// Local tempdir (the `support` helpers live in takka-core's test
/// tree; this crate has no cross-crate test dependency).
///
/// The name is deliberately SHORT: `temp_dir()` on macOS runners is
/// already ~48 chars (`/var/folders/h5/…/T/`), and a unix socket path
/// must stay under `SUN_LEN` (104). A descriptive name plus a
/// `{tag}.sock` sibling blew that limit, so every daemon test in this
/// module failed with "path must be shorter than SUN_LEN" (the same
/// trap the CLI fixture hit; its fix never reached here).
fn tempdir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    // Two tag characters keep parallel instances readable without
    // spending the path budget.
    let short: String = tag
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(2)
        .collect();
    let dir = std::env::temp_dir().join(format!(
        "td{}{}{}",
        std::process::id() % 1000,
        nanos % 1_000_000,
        short
    ));
    assert!(
        dir.join("s.sock").as_os_str().len() < 100,
        "socket path budget exceeded: {:?}",
        dir.join("s.sock")
    );
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Hermetic provider config pinned to the discard port
/// (`127.0.0.1:9`): `Init` succeeds, a `Run` fails fast with a
/// connection error and never leaves the machine — no ambient
/// `~/.config/agentic/config.json` is ever read.
fn write_hermetic_config(dir: &Path) -> String {
    let path = dir.join("config.json");
    std::fs::write(
        &path,
        r#"{
  "providers": [
    {
      "name": "hermetic-test",
      "type": "openai-compatible",
      "api_base": "http://127.0.0.1:9",
      "api_key": "unused",
      "models": [
        { "model": "hermetic-model", "temperature": 0.0, "max_tokens": 64 }
      ]
    }
  ]
}"#,
    )
    .unwrap();
    path.to_string_lossy().into_owned()
}

fn spawn_serve(
    socket_path: PathBuf,
    info_path: PathBuf,
) -> std::thread::JoinHandle<std::io::Result<()>> {
    std::thread::spawn(move || {
        serve(DaemonConfig {
            socket_path,
            info_path,
        })
    })
}

/// The info file is written after bind and before the accept loop,
/// so its existence means the listener is ready.
fn wait_for_info_file(info_path: &Path) {
    let deadline = deadline();
    while !info_path.exists() {
        assert!(
            Instant::now() < deadline,
            "daemon info file did not appear within 10s"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn info_value(info_path: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(info_path).expect("read info file");
    serde_json::from_str(&text).expect("parse info file")
}

fn wait_serve_exit(handle: std::thread::JoinHandle<std::io::Result<()>>) {
    let deadline = deadline();
    while !handle.is_finished() {
        assert!(Instant::now() < deadline, "serve did not exit within 10s");
        std::thread::sleep(Duration::from_millis(10));
    }
    handle
        .join()
        .expect("serve thread did not panic")
        .expect("serve returned Ok");
}

/// Minimal test client: raw framed JSON over the broker's transport
/// (the public client transports have no event-reading API from this
/// crate — this is the same one-JSON-per-line framing).
struct TestConn {
    reader: BufReader<LocalStream>,
    writer: BufWriter<LocalStream>,
}

impl TestConn {
    fn connect(transport_path: &Path, deadline: Instant) -> Self {
        loop {
            match open_connection(transport_path) {
                Ok(stream) => {
                    // Set the read bound ONCE: every wait in this
                    // helper is covered by it, and repeating
                    // SO_RCVTIMEO mid-test is a platform quirk magnet
                    // (macOS returned EINVAL for an in-test value).
                    // Named pipes have no synchronous read timeout;
                    // there `set_read_timeout` is a no-op and the
                    // bounded assertions carry the guarantee.
                    stream
                        .set_read_timeout(Some(POLL_TIMEOUT))
                        .expect("set read timeout at connect");
                    let reader = BufReader::new(stream.try_clone().expect("clone reader half"));
                    return Self {
                        reader,
                        writer: BufWriter::new(stream),
                    };
                }
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("connect to {transport_path:?} failed: {error}"),
            }
        }
    }

    fn send(&mut self, frame: &serde_json::Value) {
        serde_json::to_writer(&mut self.writer, frame).expect("encode frame");
        self.writer.write_all(b"\n").expect("write frame newline");
        self.writer.flush().expect("flush frame");
    }

    fn next_event(&mut self, deadline: Instant) -> ProtocolEvent {
        loop {
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => panic!("daemon closed the connection while waiting for an event"),
                Ok(_) if line.trim().is_empty() => continue,
                Ok(_) => {
                    return serde_json::from_str(line.trim())
                        .unwrap_or_else(|error| panic!("decode event frame: {error}"))
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::TimedOut =>
                {
                    panic!("timed out waiting for an event frame (deadline {deadline:?})")
                }
                Err(error) => panic!("read event frame failed: {error}"),
            }
        }
    }

    /// The broker must never forward the engine's startup `Ready`:
    /// a client's first frame is its `Init` reply (`InitOk`). Any
    /// `Ready` here is the timing-dependent leak from issue #2, so
    /// it is a hard failure rather than a skip.
    fn expect_init_ok(&mut self, deadline: Instant, context: &str) -> ProtocolEvent {
        let event = self.next_event(deadline);
        match &event.event {
            Event::Ready { .. } => panic!(
                "{context}: broker forwarded the engine's startup Ready \
                 (clients must only see InitOk) — the drain filter regressed"
            ),
            Event::InitOk { .. } => event,
            Event::Error { message } => panic!("{context}: got error frame: {message}"),
            other => panic!("{context}: unexpected frame: {other:?}"),
        }
    }

    fn expect_terminal(&mut self, deadline: Instant) -> ProtocolEvent {
        loop {
            let event = self.next_event(deadline);
            match &event.event {
                Event::Done { .. } | Event::Error { .. } => return event,
                _ => continue,
            }
        }
    }

    fn expect_eof(&mut self, deadline: Instant) {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => {}
            Ok(_) => panic!("expected EOF, got another frame: {line}"),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                panic!("timed out waiting for EOF (deadline {deadline:?})")
            }
            // Closing a socket that still holds unread input (our
            // init frame was never read by the refusing daemon)
            // signals RST rather than FIN — both mean "closed".
            // A Windows named pipe says the same thing with
            // ERROR_BROKEN_PIPE ("pipe has been ended", os error
            // 109), which std does not map to ConnectionReset.
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
            #[cfg(windows)]
            Err(error) if error.raw_os_error() == Some(109) => {}
            Err(error) => panic!("read after refusal failed: {error}"),
        }
    }
}

fn init_frame(id: &str, token: &str, config_path: &str) -> serde_json::Value {
    serde_json::json!({
        "v": 2,
        "id": id,
        "type": "init",
        "overrides": { "configPath": config_path },
        "daemonToken": token,
    })
}

fn run_frame(id: &str, task: &str) -> serde_json::Value {
    serde_json::json!({ "v": 2, "id": id, "type": "run", "task": task })
}

fn shutdown_frame(id: &str) -> serde_json::Value {
    serde_json::json!({ "v": 2, "id": id, "type": "shutdown" })
}

fn list_tools_frame(id: &str) -> serde_json::Value {
    serde_json::json!({ "v": 2, "id": id, "type": "list_tools" })
}

fn spawn_daemon(
    tag: &str,
) -> (
    PathBuf,
    PathBuf,
    String,
    std::thread::JoinHandle<std::io::Result<()>>,
) {
    let dir = tempdir(tag);
    let config_path = write_hermetic_config(&dir);
    let socket_path = dir.join("s.sock");
    let info_path = dir.join("daemon.json");
    let handle = spawn_serve(socket_path.clone(), info_path.clone());
    wait_for_info_file(&info_path);
    let token = info_value(&info_path)["token"]
        .as_str()
        .expect("info file carries the token")
        .to_string();
    (socket_path, PathBuf::from(config_path), token, handle)
}

/// The transport seam under the broker, asserted for real: a client
/// connects, the listener accepts, and bytes flow both ways. This is
/// the test that replaces the old "unsupported platform" guard — on
/// every host it exercises the actual transport `serve` uses.
#[test]
fn transport_round_trips_bytes_both_ways() {
    use std::io::Read;
    let dir = tempdir("seam");
    let path = dir.join("s.sock");
    let listener = LocalListener::bind(&path).expect("bind");
    listener.set_nonblocking(true).expect("arm the listener");

    let mut client = open_connection(&path).expect("connect");
    let accepted = loop {
        match listener.accept() {
            Ok(stream) => break stream,
            Err(ref error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline(), "accept did not complete");
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("accept failed: {error}"),
        }
    };
    listener.rearm();
    let mut server = make_blocking(accepted).expect("served connection");

    client.write_all(b"ping\n").expect("client write");
    client.flush().expect("client flush");
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while !line.ends_with(b"\n") {
        let read = server.read(&mut byte).expect("server read");
        assert_eq!(read, 1, "server read returned 0 before the frame");
        line.push(byte[0]);
    }
    assert_eq!(line, b"ping\n");

    server.write_all(b"pong\n").expect("server write");
    server.flush().expect("server flush");
    let mut reply = Vec::new();
    while !reply.ends_with(b"\n") {
        let read = client.read(&mut byte).expect("client read");
        assert_eq!(read, 1, "client read returned 0 before the frame");
        reply.push(byte[0]);
    }
    assert_eq!(reply, b"pong\n");
}

/// The pipe-name derivation must be (a) collision-free across the
/// endpoints the smokes actually generate and (b) byte-identical to
/// the CLI's copy of the rule
/// (`takka-cli/src/client.rs` `mod transport`). These vectors are
/// duplicated verbatim there — change one, change both.
#[test]
fn pipe_name_hashing_is_unique_and_stable() {
    use super::pipe_id::{fnv1a64, normalize, pipe_name_for};

    assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(normalize("/tmp/x.SOCK/"), r"\tmp\x.sock");

    // Two fixtures that differ ONLY in their unique dir (the exact
    // shape the smokes produce) must not share a pipe name.
    let a = pipe_name_for("/tmp/takka-da-123-456/ep-456.sock");
    let b = pipe_name_for("/tmp/takka-da-123-457/ep-457.sock");
    assert_ne!(a, b, "distinct endpoints must not collide");

    // Same endpoint, different spelling (case, separator style,
    // trailing slash): one pipe name, so client and server always
    // agree.
    assert_eq!(
        pipe_name_for(r"C:\Temp\takka-da-1-2\ep-3.sock"),
        pipe_name_for(r"c:/temp/TAKKA-DA-1-2/ep-3.sock\"),
    );

    // Pinned vectors: the CLI copy asserts the same strings.
    assert_eq!(
        pipe_name_for("/tmp/takka-da-123-456/ep-456.sock"),
        r"\\.\pipe\takka-0013c643e0315b28"
    );
    assert_eq!(
        pipe_name_for(r"C:\Temp\takka-da-1-2\ep-3.sock"),
        r"\\.\pipe\takka-2dd13866d5f634ea"
    );

    // A fully qualified name is pinned verbatim (tests and callers
    // can choose their own pipe).
    assert_eq!(
        pipe_name_for(r"\\.\pipe\takka-pinned"),
        r"\\.\pipe\takka-pinned"
    );
}

/// The broker's client half refuses a path nobody listens on, so the
/// CLI's "stale discovery file" error stays reachable on both
/// transports.
#[test]
fn connecting_to_a_dead_endpoint_fails() {
    let dir = tempdir("dead");
    let path = dir.join("nobody-home.sock");
    assert!(
        open_connection(&path).is_err(),
        "connect to a non-existent endpoint must fail"
    );
}

/// The broker's forwarding rule, unit-tested without timing: the
/// engine's `Ready` is the only event a socket client must never
/// see; everything else keeps flowing.
#[test]
fn drain_filter_drops_only_the_startup_ready() {
    let ready = ProtocolEvent::new(
        None,
        Event::Ready {
            protocol: "takka".into(),
            version: 2,
        },
    );
    assert!(!should_forward(&ready));

    let init_ok = ProtocolEvent::new(
        Some("init-1".into()),
        Event::InitOk {
            protocol: "takka".into(),
            version: 2,
        },
    );
    assert!(should_forward(&init_ok));

    let error = ProtocolEvent::new(
        None,
        Event::Error {
            message: "boom".into(),
        },
    );
    assert!(should_forward(&error));
}

/// The accept-mode invariant behind issue #2: whatever `accept()`
/// inherits from a non-blocking listener (macOS/BSD inherit
/// `O_NONBLOCK`, Linux clears it), a served connection must end up
/// blocking — otherwise the reader mistakes a silent client for a
/// detach and closes a healthy connection.
///
/// Unix-only in practice: it asserts the *inherited* flag the BSD
/// `accept` returns. Named-pipe handles are synchronous by
/// construction (there is no non-blocking flag to inherit), so on
/// Windows the seam promise is checked by
/// [`served_connection_waits_for_a_silent_client`] instead.
#[cfg(unix)]
#[test]
fn accepted_connections_are_forced_blocking() {
    use std::io::Read;
    let dir = tempdir("accept-mode");
    let path = dir.join("s.sock");
    let listener = LocalListener::bind(&path).expect("bind");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");

    let client = open_connection(&path).expect("connect");
    let deadline = deadline();
    let accepted = loop {
        match listener.accept() {
            Ok(stream) => break stream,
            Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "accept did not complete");
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("accept failed: {error}"),
        }
    };
    let mut accepted = make_blocking(accepted).expect("clear the non-blocking flag");
    // No getter for the flag, so prove it by behaviour: a blocking
    // stream with a read timeout WAITS for the timeout, while one
    // that still carries O_NONBLOCK returns instantly.
    accepted
        .set_read_timeout(Some(Duration::from_millis(400)))
        .expect("set read timeout");
    let started = Instant::now();
    let mut probe = [0u8; 1];
    assert!(
        accepted.read(&mut probe).is_err(),
        "an idle peer must not deliver a frame"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(350),
        "a served connection must block, not return WouldBlock instantly \
         (it still carries the listener's non-blocking flag); elapsed {:?}",
        started.elapsed()
    );
    drop(client);
}

/// A client that connects and stays silent is normal (it may be slow
/// to send its `Init`). The broker must keep the connection open;
/// closing it here is the issue #2 failure mode verbatim.
#[test]
fn an_idle_connection_is_not_mistaken_for_a_detach() {
    let (socket_path, config_path, token, serve_handle) = spawn_daemon("idle");

    let mut client = TestConn::connect(&socket_path, deadline());
    std::thread::sleep(Duration::from_millis(300)); // deliberately quiet

    client.send(&init_frame(
        "init-idle",
        &token,
        &config_path.to_string_lossy(),
    ));
    let event = client.expect_init_ok(deadline(), "init after an idle period");
    assert_eq!(event.request_id.as_deref(), Some("init-idle"));

    client.send(&shutdown_frame("down"));
    drop(client);
    wait_serve_exit(serve_handle);
}

/// Happy path: connect → Init (token + hermetic config) → InitOk →
/// Run → a terminal frame (Done or the provider's Error — both are
/// legal terminals for an unreachable provider) → Shutdown stops
/// serve cleanly and unlinks its files.
#[test]
fn serve_round_trips_basic_run() {
    let (socket_path, config_path, token, serve_handle) = spawn_daemon("roundtrip");

    let mut client = TestConn::connect(&socket_path, deadline());
    client.send(&init_frame(
        "init-1",
        &token,
        &config_path.to_string_lossy(),
    ));
    let event = client.expect_init_ok(deadline(), "init");
    assert_eq!(event.request_id.as_deref(), Some("init-1"));

    client.send(&run_frame("run-1", "say hi"));
    let event = client.expect_terminal(deadline());
    assert_eq!(event.request_id.as_deref(), Some("run-1"));

    client.send(&shutdown_frame("down-1"));
    drop(client);
    wait_serve_exit(serve_handle);
    assert!(!socket_path.exists(), "socket file must be unlinked");
    assert!(!config_path.parent().unwrap().join("daemon.json").exists());
}

/// While connection A is active, B gets exactly one
/// `"daemon in use"` error frame and a close; A keeps working.
#[test]
fn second_connection_is_refused() {
    let (socket_path, config_path, token, serve_handle) = spawn_daemon("refusal");

    let mut a = TestConn::connect(&socket_path, deadline());
    a.send(&init_frame(
        "init-a",
        &token,
        &config_path.to_string_lossy(),
    ));
    a.expect_init_ok(deadline(), "init A");

    let mut b = TestConn::connect(&socket_path, deadline());
    b.send(&init_frame(
        "init-b",
        &token,
        &config_path.to_string_lossy(),
    ));
    let event = b.next_event(deadline());
    match &event.event {
        Event::Error { message } => assert_eq!(message, "daemon in use"),
        other => panic!("expected refusal frame, got {other:?}"),
    }
    b.expect_eof(deadline()); // refused connection is closed by the daemon

    // A is unaffected by B's refused attach.
    a.send(&list_tools_frame("lt-1"));
    let event = a.next_event(deadline());
    assert!(
        matches!(event.event, Event::ToolList { .. }),
        "expected ToolList on the active connection, got {:?}",
        event.event
    );
    assert_eq!(event.request_id.as_deref(), Some("lt-1"));

    a.send(&shutdown_frame("down"));
    drop(a);
    drop(b);
    wait_serve_exit(serve_handle);
}

/// A inits, detaches; B re-inits with the same token/caps/config
/// and the engine adopts the session (`InitOk` + `SessionAdopted`).
#[test]
fn reattach_adopts_session() {
    let (socket_path, config_path, token, serve_handle) = spawn_daemon("adopt");

    let mut a = TestConn::connect(&socket_path, deadline());
    a.send(&init_frame(
        "init-a",
        &token,
        &config_path.to_string_lossy(),
    ));
    a.expect_init_ok(deadline(), "init A");
    drop(a); // detach

    // If A's reader thread has not noticed the EOF yet, the broker
    // still holds the slot and refuses B — poll until adoption.
    let deadline = Instant::now() + POLL_TIMEOUT;
    loop {
        assert!(
            Instant::now() < deadline,
            "reattach did not succeed within 10s"
        );
        let mut b = TestConn::connect(&socket_path, deadline);
        b.send(&init_frame(
            "init-b",
            &token,
            &config_path.to_string_lossy(),
        ));
        let event = b.next_event(deadline);
        match &event.event {
            Event::Error { message } if message == "daemon in use" => {
                drop(b);
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            Event::Error { message } => {
                panic!("unexpected error on reattach: {message}")
            }
            Event::InitOk { .. } => {
                assert_eq!(event.request_id.as_deref(), Some("init-b"));
                let adopted = b.next_event(deadline);
                assert_eq!(adopted.request_id.as_deref(), Some("init-b"));
                assert!(
                    matches!(adopted.event, Event::SessionAdopted { .. }),
                    "expected SessionAdopted, got {:?}",
                    adopted.event
                );
                b.send(&shutdown_frame("down"));
                drop(b);
                break;
            }
            other => panic!("unexpected frame on reattach: {other:?}"),
        }
    }
    wait_serve_exit(serve_handle);
}

/// Shutdown over an initialized connection: serve returns cleanly
/// and the socket + info files are gone.
#[test]
fn shutdown_unlinks_files() {
    let (socket_path, config_path, token, serve_handle) = spawn_daemon("shutdown");
    // Readiness is the discovery file on both transports: a unix
    // socket is a file on disk, a Windows named pipe is not, so
    // asserting `socket_path.exists()` would be unix-only.
    assert!(config_path.parent().unwrap().join("daemon.json").exists());

    let mut client = TestConn::connect(&socket_path, deadline());
    client.send(&init_frame(
        "init-1",
        &token,
        &config_path.to_string_lossy(),
    ));
    client.expect_init_ok(deadline(), "init");

    client.send(&shutdown_frame("down-1"));
    drop(client);
    wait_serve_exit(serve_handle);
    assert!(!socket_path.exists(), "socket file must be unlinked");
    assert!(
        !config_path.parent().unwrap().join("daemon.json").exists(),
        "info file must be unlinked"
    );
}

/// A stale info file with a dead pid (plus a garbage socket file)
/// must NOT block startup: serve overwrites the info file with its
/// own pid, replaces the socket file with a live listener, and
/// answers a shutdown.
#[test]
fn stale_info_file_is_cleaned() {
    let dir = tempdir("stale");
    let socket_path = dir.join("stale.sock");
    let info_path = dir.join("daemon.json");

    // Guaranteed-dead pid: spawn, then reap via wait().
    let mut child = std::process::Command::new("sleep")
        .arg("0")
        .spawn()
        .expect("spawn sleep 0");
    let dead_pid = child.id();
    child.wait().expect("reap sleep 0");

    std::fs::write(
        &info_path,
        format!(
            r#"{{"pid": {dead_pid}, "socket_path": "{}", "token": "stale-token", "version": "0.0.0"}}"#,
            socket_path.display()
        ),
    )
    .unwrap();
    std::fs::write(&socket_path, b"garbage").unwrap(); // bind must survive this

    let serve_handle = spawn_serve(socket_path.clone(), info_path.clone());

    // If serve mistook the dead pid for a live daemon it would have
    // exited with "daemon already running" instead of rewriting the
    // file with its own pid.
    let deadline = Instant::now() + POLL_TIMEOUT;
    loop {
        assert!(
            !serve_handle.is_finished(),
            "serve exited early: {:?}",
            serve_handle.join()
        );
        let overwritten = std::fs::read_to_string(&info_path)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|value| {
                value["pid"]
                    .as_u64()
                    .map(|pid| pid == u64::from(std::process::id()))
            })
            .unwrap_or(false);
        if overwritten {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "stale info file was not rewritten within 10s"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // The garbage socket file was replaced by a real listener.
    let mut client = TestConn::connect(&socket_path, deadline);
    client.send(&shutdown_frame("down"));
    drop(client);
    wait_serve_exit(serve_handle);
    assert!(!socket_path.exists());
    assert!(!info_path.exists());
}
