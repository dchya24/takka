use anyhow::Result;
use takka_core::sync;

use crate::widgets::{components, inline};

use super::*;

impl Commands {
    /// `takka attach` (Fase 3a): with `--shutdown`, ask the running
    /// daemon to stop (no session handshake — see
    /// [`crate::client::shutdown_daemon_at`]); otherwise connect to the
    /// already-running daemon (never auto-spawn — an attach that starts
    /// its own daemon would be a run in disguise) and keep the
    /// connection for the regular REPL (main.rs calls `interactive::run`
    /// with this same `Commands`, and `ensure_connection` reuses the
    /// established connection).
    pub async fn attach_daemon(&mut self, shutdown: bool) -> Result<()> {
        self.attach_daemon_inner(shutdown).await
    }

    /// Unix implementation: shutdown or connect to the live daemon.
    /// On non-unix this variant does not exist; the stub below returns
    /// the actionable error.
    #[cfg(unix)]
    pub(crate) async fn attach_daemon_inner(&mut self, shutdown: bool) -> Result<()> {
        if shutdown {
            let info_path = self
                .daemon_info_path_override
                .clone()
                .unwrap_or_else(crate::client::daemon_info_path);
            crate::client::shutdown_daemon_at(&info_path).await?;
            inline::print_blank();
            inline::print_line(&components::success_badge("Daemon shutdown requested."));
            inline::print_blank();
            return Ok(());
        }
        if self.connection.is_some() {
            return Ok(()); // already attached
        }
        let overrides = takka_core::runtime::protocol::InitOverrides {
            config_path: self.runtime_config_path.clone(),
            permission_mode: Some(self.permission_mode),
            model: self.config.active_model().map(|model| model.model.clone()),
            session_id: None,
            system_prompt: self.config.system_prompt.clone(),
        };
        let connection = match &self.daemon_info_path_override {
            Some(path) => {
                crate::client::AgentConnection::connect_daemon_at(
                    path,
                    overrides,
                    crate::client::CLI_CAPABILITIES,
                    false,
                )
                .await?
            }
            None => {
                crate::client::AgentConnection::connect_daemon(
                    overrides,
                    crate::client::CLI_CAPABILITIES,
                    false,
                )
                .await?
            }
        };
        self.connection = Some(connection);
        Ok(())
    }

    /// Non-unix stub: daemon mode is Unix-only. This is the ONLY
    /// `attach_daemon` body compiled on Windows — the unix variant above
    /// (with its `connect_daemon_at` / `shutdown_daemon_at` references)
    /// does not exist there, so no unix-only symbol leaks into the
    /// Windows build under `-D warnings`.
    #[cfg(not(unix))]
    pub(crate) async fn attach_daemon_inner(&mut self, _shutdown: bool) -> Result<()> {
        Err(anyhow::anyhow!(crate::client::DAEMON_NOT_SUPPORTED_MESSAGE))
    }

    pub(crate) async fn ensure_connection(&mut self) -> Result<()> {
        if self.connection.is_some() {
            return Ok(());
        }
        let overrides = takka_core::runtime::protocol::InitOverrides {
            config_path: self.runtime_config_path.clone(),
            permission_mode: Some(self.permission_mode),
            model: self.config.active_model().map(|model| model.model.clone()),
            session_id: None,
            system_prompt: self.config.system_prompt.clone(),
        };
        self.connection = Some(
            crate::client::AgentConnection::connect(
                self.runtime_mode,
                overrides,
                crate::client::CLI_CAPABILITIES,
            )
            .await?,
        );
        Ok(())
    }

    /// Route mid-run typed text per queue mode (story 2.4, shared by
    /// the TUI's key path and the inline reader): Interrupt → steer via
    /// the runtime client; FollowUp → queue client-side.
    pub async fn steer_text(&mut self, content: String) {
        if content.is_empty() {
            return;
        }
        match self.queue_mode {
            QueueMode::Interrupt => {
                self.ensure_connection()
                    .await
                    .expect("TUI steer_text requires a connection");
                if let Some(runtime) = self.connection.as_mut() {
                    let _ = runtime.steer(content).await;
                }
            }
            QueueMode::FollowUp => {
                sync::lock(&self.pending_follow_ups).push_back(content);
            }
        }
    }
}

// ── Daemon-mode e2e smoke (Fase 3a, unix only) ───────────
//
// The daemon mode of the `--runtime` matrix gets dedicated
// AgentConnection-level smokes instead of extending the Commands-level
// mode matrix: the broker's discovery file lives at a process-default
// path, and a Commands-driven smoke would either touch the developer's
// real ~/.config/agentic or need a process-global HOME override (racy
// under parallel cargo test). `AgentConnection::connect_daemon_at` is
// the same discovery + spawn + token-handshake path the mode flag and
// `attach` use, pointed at a tempdir fixture.
#[cfg(all(test, unix))]
mod daemon_smoke_tests {
    use super::*;
    use crate::client::{AgentConnection, CLI_CAPABILITIES};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};
    use takka_core::runtime::protocol::InitOverrides;

    /// Bound for every poll so a regression can never hang the suite.
    /// macOS CI runners have been observed needing >10s just to bring a
    /// daemon up when the runner is loaded — the bound is fixture plumbing,
    /// not the thing under test, so it only needs to be generous.
    const POLL_TIMEOUT: Duration = Duration::from_secs(60);

    /// Hermetic provider config (same shape as `hermetic_runtime_config`):
    /// `127.0.0.1:9` fails fast and never leaves the machine.
    const HERMETIC_CONFIG: &str = r#"{
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
}"#;

    struct DaemonFixture {
        dir: PathBuf,
        info_path: PathBuf,
        socket_path: PathBuf,
        config_path: PathBuf,
        child: tokio::process::Child,
    }

    impl DaemonFixture {
        /// Spawn the real `takka-runtime --daemon` binary with explicit
        /// endpoints under a fresh tempdir. The info file is written after
        /// bind, so its existence means the listener is ready.
        async fn spawn(_tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            // Path-length discipline: Unix socket paths must stay under
            // SUN_LEN (104) — macOS runners' tempdir prefix is ~50 chars,
            // so a long descriptive name + "test.sock" blew the limit
            // ("path must be shorter than SUN_LEN", every macOS daemon
            // smoke dead-on-arrival). Short unique dir keeps the socket
            // path under the limit while pid + truncated nanos separate
            // test instances; cleanup is per-dir.
            let dir = std::env::temp_dir().join(format!(
                "takka-da-{}-{}",
                std::process::id(),
                nanos % 91_000_000_000
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let config_path = dir.join("config.json");
            std::fs::write(&config_path, HERMETIC_CONFIG).unwrap();
            let socket_path = dir.join("test.sock");
            let info_path = dir.join("daemon.json");
            let binary = {
                // Same lock as client.rs's env-compat tests: a transient
                // TAKKA_/AGENTIC_RUNTIME_BIN value must never leak into
                // the runtime binary resolution here.
                let _g = crate::client::daemon_info_tests::ENV_LOCK
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                crate::client::runtime_binary().expect("runtime binary for daemon smoke")
            };
            let mut child = tokio::process::Command::new(&binary)
                .arg("--daemon")
                .arg("--socket-path")
                .arg(&socket_path)
                .arg("--info-path")
                .arg(&info_path)
                .env("RUST_LOG", "takka_runtime=debug,takka_core=debug")
                .stdin(std::process::Stdio::null())
                // Evidence pinning: a failing start on a CI runner is
                // invisible with null streams; park both in the fixture
                // directory so a timeout failure can quote them.
                .stdout(std::process::Stdio::from(
                    std::fs::File::create(dir.join("daemon-stdout.log")).unwrap(),
                ))
                .stderr(std::process::Stdio::from(
                    std::fs::File::create(dir.join("daemon-stderr.log")).unwrap(),
                ))
                .kill_on_drop(true) // a panicking test must not leak the daemon
                .spawn()
                .expect("spawn takka-runtime --daemon");
            let deadline = Instant::now() + POLL_TIMEOUT;
            while !info_path.exists() {
                if Instant::now() >= deadline {
                    let child_status = match child.try_wait() {
                        Ok(s) => match s {
                            Some(code) => format!("exited with {code}"),
                            None => "running".to_string(),
                        },
                        Err(e) => format!("wait error: {e}"),
                    };
                    // Quote the daemon's own last words, not just the
                    // deadline: startup crashes so far printed nothing
                    // anywhere, making every macOS CI failure a guess.
                    let stderr =
                        std::fs::read_to_string(dir.join("daemon-stderr.log")).unwrap_or_default();
                    let stdout =
                        std::fs::read_to_string(dir.join("daemon-stdout.log")).unwrap_or_default();
                    panic!(
                        "daemon info file did not appear within {:?}; binary {:?} alive={:?}; \\\n                         stderr: {}; stdout: {}",
                        POLL_TIMEOUT,
                        binary.display(),
                        child_status,
                        stderr,
                        stdout,
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Self {
                dir,
                info_path,
                socket_path,
                config_path,
                child,
            }
        }

        /// Init overrides pinning the spawned engine to the fixture's
        /// hermetic provider (the engine reads config from the Init
        /// frame, not from the ambient ~/.config).
        fn init_overrides(&self) -> InitOverrides {
            InitOverrides {
                config_path: Some(self.config_path.to_string_lossy().into_owned()),
                permission_mode: Some(takka_core::PermissionMode::Yolo),
                model: None,
                session_id: None,
                system_prompt: None,
            }
        }

        /// The daemon's last words, gathered with a bounded grace period:
        /// a child that is dying needs a moment before its stderr/stdout
        /// files contain the panic/error line. Without this, a failing
        /// attach on a CI runner quotes a log that is one beat too early
        /// and the failure stays a guess (issue #2).
        async fn evidence(&mut self) -> String {
            const GRACE: Duration = Duration::from_secs(2);
            let deadline = Instant::now() + GRACE;
            let status = loop {
                match self.child.try_wait() {
                    Ok(Some(status)) => break format!("exited with {status}"),
                    Ok(None) => {}
                    Err(error) => break format!("wait error: {error}"),
                }
                if Instant::now() >= deadline {
                    break format!("still running after {GRACE:?}");
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            };
            let read = |name: &str| {
                std::fs::read_to_string(self.dir.join(name))
                    .unwrap_or_else(|error| format!("<unreadable: {error}>"))
            };
            format!(
                "daemon {status}; stderr: {}; stdout: {}",
                read("daemon-stderr.log"),
                read("daemon-stdout.log"),
            )
        }

        async fn connect(&mut self) -> AgentConnection {
            match AgentConnection::connect_daemon_at(
                &self.info_path,
                self.init_overrides(),
                CLI_CAPABILITIES,
                false,
            )
            .await
            {
                Ok(conn) => conn,
                Err(error) => {
                    // Quote the daemon's own logs and exit status: a
                    // handshake refusal on a CI runner otherwise dies
                    // silent (macOS failures were a guess chain until
                    // the spawn evidence patch).
                    let evidence = self.evidence().await;
                    panic!("daemon attach failed: {error}; {evidence}");
                }
            }
        }

        /// Wait until the broker exited cleanly and unlinked its
        /// endpoints (serve's shutdown cleanup).
        async fn wait_clean_exit(&mut self) {
            let status = tokio::time::timeout(POLL_TIMEOUT, self.child.wait())
                .await
                .expect("daemon did not exit within 10s after shutdown")
                .expect("daemon wait failed");
            assert_eq!(status.code(), Some(0), "daemon exited with {status}");
            let deadline = Instant::now() + POLL_TIMEOUT;
            while self.socket_path.exists() || self.info_path.exists() {
                assert!(
                    Instant::now() < deadline,
                    "daemon endpoints were not unlinked within 10s"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    impl Drop for DaemonFixture {
        fn drop(&mut self) {
            // kill_on_drop(true) reaps the daemon (no-op after a clean
            // exit); files are best-effort so a panicking test never
            // leaks a tempdir.
            let _ = std::fs::remove_file(&self.socket_path);
            let _ = std::fs::remove_file(&self.info_path);
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Full lifecycle: spawn daemon → discovery + token handshake →
    /// run (frontend-issued) → SessionStarted + terminal event →
    /// shutdown stops the broker and unlinks socket + info files.
    #[tokio::test(flavor = "multi_thread")]
    async fn daemon_lifecycle_smoke() {
        let mut fx = DaemonFixture::spawn("lifecycle").await;
        let mut conn = fx.connect().await;

        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = events.clone();
        let result = tokio::time::timeout(
            Duration::from_secs(60),
            conn.run(
                "list files".to_string(),
                Vec::new(),
                |_| {},
                move |evt| captured.lock().unwrap().push(format!("{evt:?}")),
                |_| None,
                // The watcher is inert here: the process flag stays
                // false for the whole test process (cancel smokes use
                // their own injected flags).
                crate::cancel_flag(),
                None,
            ),
        )
        .await
        .expect("daemon run hung: no terminal event within 60s");

        // The pinned unreachable provider makes the session fail
        // deterministically; only the streamed lifecycle is asserted
        // (terminal value is Done-or-Error by design).
        let _ = result;
        let all = events.lock().unwrap().join(" ");
        assert!(
            all.contains("SessionStarted"),
            "expected SessionStarted over the daemon socket, got: {all}"
        );
        assert!(
            all.contains("SessionFailed"),
            "expected SessionFailed after provider failure, got: {all}"
        );

        conn.shutdown().await.expect("daemon shutdown");
        fx.wait_clean_exit().await;
    }

    /// While connection A is active, a second attach is refused with the
    /// exact `"daemon in use"` error (surfaced through the shared
    /// handshake), and A keeps working.
    #[tokio::test(flavor = "multi_thread")]
    async fn in_use_refusal_smoke() {
        let mut fx = DaemonFixture::spawn("refusal").await;
        let mut a = fx.connect().await;

        let refused = AgentConnection::connect_daemon_at(
            &fx.info_path,
            fx.init_overrides(),
            CLI_CAPABILITIES,
            false,
        )
        .await;
        match refused {
            Err(error) => {
                assert!(
                    error.to_string().contains("daemon in use"),
                    "expected daemon-in-use refusal, got: {error}"
                );
            }
            Ok(_) => panic!("second attach must be refused while A is active"),
        }

        // A is unaffected by B's refused attach.
        let event = a
            .send_and_wait(takka_core::runtime::protocol::Request::ListTools)
            .await
            .expect("ListTools on the active connection");
        assert!(
            matches!(event.event, takka_core::Event::ToolList { .. }),
            "expected ToolList on A, got {:?}",
            event.event
        );

        a.shutdown().await.expect("daemon shutdown");
        fx.wait_clean_exit().await;
    }

    /// A attaches, detaches; B reattaches and the engine adopts the live
    /// session: handshake InitOk followed by SessionAdopted.
    #[tokio::test(flavor = "multi_thread")]
    async fn adopt_smoke() {
        let mut fx = DaemonFixture::spawn("adopt").await;
        let a = fx.connect().await;
        drop(a); // detach — the broker's engine keeps running

        // If A's reader thread has not noticed the EOF yet, the broker
        // still holds the slot and refuses B — poll until adoption.
        let deadline = Instant::now() + POLL_TIMEOUT;
        let mut b = loop {
            match AgentConnection::connect_daemon_at(
                &fx.info_path,
                fx.init_overrides(),
                CLI_CAPABILITIES,
                false,
            )
            .await
            {
                Err(error) if error.to_string().contains("daemon in use") => {
                    assert!(Instant::now() < deadline, "reattach window expired");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                other => break other.expect("reattach failed"),
            }
        };

        // Handshake consumed InitOk (cli-1); the adoption replay is the
        // very next frame, tagged with the same request id.
        let event = tokio::time::timeout(POLL_TIMEOUT, b.next_event())
            .await
            .expect("timed out waiting for SessionAdopted")
            .expect("event stream closed after reattach");
        assert_eq!(event.request_id.as_deref(), Some("cli-1"));
        assert!(
            matches!(event.event, takka_core::Event::SessionAdopted { .. }),
            "expected SessionAdopted, got {:?}",
            event.event
        );

        b.shutdown().await.expect("daemon shutdown");
        fx.wait_clean_exit().await;
    }

    /// Auto-spawn: with no info file at all, connect_daemon_at launches
    /// the runtime binary itself, discovers it, handshakes, and a
    /// shutdown tears the daemon down (endpoints unlinked).
    #[tokio::test(flavor = "multi_thread")]
    async fn autospawn_smoke() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "takka-au-{}-{}",
            std::process::id(),
            nanos % 91_000_000_000
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.json");
        std::fs::write(&config_path, HERMETIC_CONFIG).unwrap();
        let info_path = dir.join("daemon.json");

        let mut conn = AgentConnection::connect_daemon_at(
            &info_path,
            InitOverrides {
                config_path: Some(config_path.to_string_lossy().into_owned()),
                permission_mode: Some(takka_core::PermissionMode::Yolo),
                model: None,
                session_id: None,
                system_prompt: None,
            },
            CLI_CAPABILITIES,
            true,
        )
        .await
        .expect("auto-spawn + connect failed");

        // Sanity: the spawned broker's engine answers on the wire.
        let event = tokio::time::timeout(
            Duration::from_secs(30),
            conn.send_and_wait(takka_core::runtime::protocol::Request::ListTools),
        )
        .await
        .expect("ListTools timed out")
        .expect("ListTools failed");
        assert!(matches!(event.event, takka_core::Event::ToolList { .. }));

        conn.shutdown().await.expect("daemon shutdown");
        // The spawned daemon is detached (not our child); its clean-exit
        // signal is the endpoint cleanup.
        let deadline = Instant::now() + POLL_TIMEOUT;
        while info_path.exists() {
            assert!(
                Instant::now() < deadline,
                "auto-spawned daemon did not exit within 10s after shutdown"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Attach without a running daemon (`spawn_if_missing=false`) fails
    /// with the exact actionable message.
    #[tokio::test]
    async fn attach_without_daemon_is_actionable() {
        let dir = std::env::temp_dir().join(format!(
            "takka-cli-nodaemon-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let info_path = dir.join("daemon.json");

        let error = match AgentConnection::connect_daemon_at(
            &info_path,
            InitOverrides::default(),
            CLI_CAPABILITIES,
            false,
        )
        .await
        {
            Ok(_) => panic!("attach without daemon must fail"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), crate::client::NO_DAEMON_MESSAGE);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Commands-level composition of `takka attach --shutdown` against
    /// a fixture daemon: connect (no auto-spawn) → fire-and-forget
    /// shutdown → the broker exits and unlinks its endpoints.
    #[tokio::test(flavor = "multi_thread")]
    async fn commands_attach_shutdown_stops_daemon() {
        let mut fx = DaemonFixture::spawn("attach-shutdown").await;
        let mut commands = Commands::new(Config::fallback())
            .with_daemon_info_path(fx.info_path.clone())
            .with_runtime_config_path(Some(fx.config_path.to_string_lossy().into_owned()))
            .with_permission_mode(takka_core::PermissionMode::Yolo);

        tokio::time::timeout(POLL_TIMEOUT, commands.attach_daemon(true))
            .await
            .expect("attach --shutdown hung")
            .expect("attach --shutdown failed");
        // Fire-and-forget: wait for the broker to exit and clean up,
        // then attach must fail with the exact actionable message.
        fx.wait_clean_exit().await;
        let error = tokio::time::timeout(POLL_TIMEOUT, commands.attach_daemon(false))
            .await
            .expect("attach hung")
            .expect_err("attach after shutdown must fail");
        assert_eq!(error.to_string(), crate::client::NO_DAEMON_MESSAGE);
    }

    /// Shutdown delivery survives the slot race: A detaches and the
    /// shutdown fires immediately — if the broker's reader has not
    /// processed A's EOF yet, the request is refused ("daemon in use")
    /// and must be retried, not silently lost.
    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_retries_through_busy_slot() {
        let mut fx = DaemonFixture::spawn("shutdown-retry").await;
        let a = fx.connect().await;
        drop(a); // EOF races our shutdown attempt below

        let mut commands =
            Commands::new(Config::fallback()).with_daemon_info_path(fx.info_path.clone());
        tokio::time::timeout(POLL_TIMEOUT, commands.attach_daemon(true))
            .await
            .expect("attach --shutdown hung")
            .expect("shutdown through busy slot failed");
        fx.wait_clean_exit().await;
    }

    /// A shutdown while another client HOLDS the connection is refused
    /// until the retry deadline, then surfaces a busy error (the active
    /// client is never touched).
    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_busy_daemon_errors_after_deadline() {
        let mut fx = DaemonFixture::spawn("shutdown-busy").await;
        let mut a = fx.connect().await; // stays attached

        let mut commands =
            Commands::new(Config::fallback()).with_daemon_info_path(fx.info_path.clone());
        let error = tokio::time::timeout(POLL_TIMEOUT, commands.attach_daemon(true))
            .await
            .expect("attach --shutdown hung")
            .expect_err("shutdown of a busy daemon must fail after the deadline");
        assert!(
            error.to_string().contains("daemon busy"),
            "expected busy error, got: {error}"
        );

        // The active client is unaffected by the refused shutdown.
        let event = a
            .send_and_wait(takka_core::runtime::protocol::Request::ListTools)
            .await
            .expect("ListTools on the active connection");
        assert!(matches!(event.event, takka_core::Event::ToolList { .. }));

        a.shutdown().await.expect("daemon shutdown");
        fx.wait_clean_exit().await;
    }

    /// Commands-level no-daemon path: attach without a running daemon
    /// surfaces the actionable message (never spawns).
    #[tokio::test]
    async fn commands_attach_without_daemon_errors_actionable() {
        let dir = std::env::temp_dir().join(format!(
            "takka-cli-cmd-nodaemon-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let info_path = dir.join("daemon.json");

        let mut commands = Commands::new(Config::fallback()).with_daemon_info_path(info_path);
        let error = tokio::time::timeout(POLL_TIMEOUT, commands.attach_daemon(false))
            .await
            .expect("attach hung")
            .expect_err("attach without daemon must fail");
        assert_eq!(error.to_string(), crate::client::NO_DAEMON_MESSAGE);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
