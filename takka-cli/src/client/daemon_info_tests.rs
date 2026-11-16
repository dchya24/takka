use super::*;

/// Serializes env mutation between the env-compat tests in this
/// module and the daemon-smoke fixtures in `commands.rs` (which
/// resolve `runtime_binary()` and must not observe a transient
/// env value).
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Wire-format pin for the discovery file the daemon broker writes
/// (`takka-runtime/src/daemon.rs::write_info_file`): snake_case
/// keys, no renaming — a mismatch silently breaks `RuntimeMode::
/// Daemon` discovery.
#[test]
fn daemon_info_parses_fields() {
    let json = r#"{"pid":4242,"socket_path":"/tmp/x.sock","token":"tok","version":"0.0.0-test"}"#;
    let info = parse_daemon_info_str(json).unwrap();
    assert_eq!(info.pid, 4242);
    assert_eq!(info.socket_path, "/tmp/x.sock");
    assert_eq!(info.token, "tok");
    assert_eq!(info.version, "0.0.0-test");
}

#[test]
fn daemon_info_rejects_corrupt_or_incomplete_json() {
    assert!(parse_daemon_info_str("not json at all").is_err());
    // Missing the token (the shared secret) → unusable for attach.
    let no_token = r#"{"pid":1,"socket_path":"/tmp/x.sock","version":"0"}"#;
    assert!(parse_daemon_info_str(no_token).is_err());
    // pid must be a number.
    let bad_pid = r#"{"pid":"4242","socket_path":"/tmp/x.sock","token":"t","version":"0"}"#;
    assert!(parse_daemon_info_str(bad_pid).is_err());
}

/// Default discovery location must agree with the runtime broker's
/// own default (`daemon_config_dir()` in
/// `takka-runtime/src/main.rs`), including the HOME-less fallback.
/// Both sides moved to `~/.config/takka` in lockstep during the
/// rebrand — this pin keeps them honest.
#[test]
fn daemon_info_path_matches_broker_default() {
    match std::env::var_os("HOME") {
        Some(home) => {
            assert_eq!(
                daemon_info_path(),
                PathBuf::from(home)
                    .join(".config")
                    .join("takka")
                    .join("daemon.json")
            );
        }
        None => {
            assert_eq!(
                daemon_info_path(),
                std::env::temp_dir()
                    .join("takka-daemon")
                    .join("daemon.json")
            );
        }
    }
}

/// Env-compat shim: `TAKKA_RUNTIME_BIN` is the primary name and
/// wins when both are set (the legacy name must never shadow it).
#[test]
fn runtime_binary_env_prefers_takka_name() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    std::env::set_var("TAKKA_RUNTIME_BIN", "/tmp/takka-runtime-new");
    std::env::set_var("AGENTIC_RUNTIME_BIN", "/tmp/takka-runtime-legacy");
    assert_eq!(
        runtime_binary().unwrap(),
        PathBuf::from("/tmp/takka-runtime-new")
    );
    std::env::remove_var("TAKKA_RUNTIME_BIN");
    std::env::remove_var("AGENTIC_RUNTIME_BIN");
}

/// The pre-rebrand `AGENTIC_RUNTIME_BIN` name still resolves so
/// existing setups keep working until they switch (with a stderr
/// warning emitted by `runtime_binary`).
#[test]
fn runtime_binary_env_falls_back_to_agentic_name() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    std::env::remove_var("TAKKA_RUNTIME_BIN");
    std::env::set_var("AGENTIC_RUNTIME_BIN", "/tmp/takka-runtime-legacy");
    assert_eq!(
        runtime_binary().unwrap(),
        PathBuf::from("/tmp/takka-runtime-legacy")
    );
    std::env::remove_var("AGENTIC_RUNTIME_BIN");
}

/// Windows-level guard contract, compiled on every host (same
/// pattern as `takka-runtime`'s `not_supported_error` test): the
/// message `connect` produces on a non-unix host is exactly one
/// actionable line pointing at the modes that DO work.
#[test]
fn daemon_mode_windows_guard_message_is_actionable() {
    assert_eq!(
        DAEMON_NOT_SUPPORTED_MESSAGE,
        "daemon mode not supported on Windows yet — use --runtime embedded|spawn"
    );
    assert!(!DAEMON_NOT_SUPPORTED_MESSAGE.contains('\n'));
}

/// The no-daemon attach error must tell the user how to get a
/// daemon (or pick a mode that does not need one).
#[test]
fn no_daemon_message_is_actionable() {
    assert_eq!(
        NO_DAEMON_MESSAGE,
        "no daemon running — start it with 'takka-runtime --daemon' or use --runtime embedded"
    );
    assert!(!NO_DAEMON_MESSAGE.contains('\n'));
}
