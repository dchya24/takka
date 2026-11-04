use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

#[test]
fn runtime_emits_ready_and_shutdown_exits_zero() {
    let binary = env!("CARGO_BIN_EXE_takka-runtime");
    let mut child = Command::new(binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, r#"{{"v":2,"id":"shutdown-1","type":"shutdown"}}"#).unwrap();
    drop(stdin);

    let stdout = child.stdout.take().unwrap();
    let lines: Vec<String> = BufReader::new(stdout).lines().map(Result::unwrap).collect();
    let status = child.wait().unwrap();

    assert!(status.success());
    let ready: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(ready["type"], "ready");
    assert_eq!(ready["protocol"], "takka");
    assert_eq!(ready["version"], 2);
}

#[test]
fn malformed_line_emits_protocol_error_and_runtime_continues() {
    let binary = env!("CARGO_BIN_EXE_takka-runtime");
    let mut child = Command::new(binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "not-json").unwrap();
    writeln!(stdin, r#"{{"v":2,"id":"shutdown-2","type":"shutdown"}}"#).unwrap();
    drop(stdin);

    let stdout = child.stdout.take().unwrap();
    let lines: Vec<String> = BufReader::new(stdout).lines().map(Result::unwrap).collect();
    let status = child.wait().unwrap();
    assert!(status.success());
    let events: Vec<serde_json::Value> = lines
        .iter()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(events.iter().any(|event| {
        event["type"] == "error"
            && event["message"]
                .as_str()
                .unwrap_or_default()
                .contains("protocol_error")
    }));
}

#[test]
fn frame_without_version_emits_protocol_error() {
    // `v` tidak punya serde default: frame tanpa versi gagal
    // deserialisasi dan muncul sebagai `protocol_error` (di-pin),
    // bukan silently dianggap v1.
    let binary = env!("CARGO_BIN_EXE_takka-runtime");
    let mut child = Command::new(binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, r#"{{"id":"nov-1","type":"shutdown"}}"#).unwrap();
    writeln!(stdin, r#"{{"v":2,"id":"shutdown-3","type":"shutdown"}}"#).unwrap();
    drop(stdin);

    let stdout = child.stdout.take().unwrap();
    let lines: Vec<String> = BufReader::new(stdout).lines().map(Result::unwrap).collect();
    let status = child.wait().unwrap();
    assert!(status.success());
    let events: Vec<serde_json::Value> = lines
        .iter()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(events.iter().any(|event| {
        event["type"] == "error"
            && event["message"]
                .as_str()
                .unwrap_or_default()
                .contains("protocol_error")
    }));
}
