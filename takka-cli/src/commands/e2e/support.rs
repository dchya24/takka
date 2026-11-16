use super::*;

/// Minimal OpenAI-compatible SSE server for the streaming tests:
/// answers every POST with a scripted streaming completion — the given
/// text deltas (a delay before each, so the renderer's 80 ms ticks see
/// them arrive one by one), then a `finish_reason: "stop"` event and
/// `[DONE]`. Chunked-transfer framed; the connection is closed after
/// the stream ends. Loops over incoming connections so the daemon
/// (`Spawn` mode) can use it too.
pub(crate) fn spawn_scripted_sse_server(deltas: Vec<(std::time::Duration, String)>) -> u16 {
    spawn_scripted_sse_server_mode(deltas, false)
}

/// Streaming-preview harness variant: after the deltas the connection
/// drops without a stop frame or `[DONE]` — the provider-error path
/// (I/O matrix row: stream error mid-preview).
pub(crate) fn spawn_scripted_sse_server_abrupt(deltas: Vec<(std::time::Duration, String)>) -> u16 {
    spawn_scripted_sse_server_mode(deltas, true)
}

/// One scripted LLM turn for the multi-turn harness (story 1.3).
#[derive(Clone)]
pub(crate) enum ScriptedTurn {
    /// Text-only deltas, then a `stop` finish.
    Text(Vec<(std::time::Duration, String)>),
    /// Text deltas followed by ONE tool call, finished with
    /// `finish_reason: tool_calls` — the model asks to run `tool`
    /// next turn.
    TextThenTool(Vec<(std::time::Duration, String)>, String, String),
}

/// Multi-turn scripted provider (story 1.3): each incoming connection
/// consumes the next scripted turn, so POST 1 can answer with a tool
/// call whose result feeds POST 2 — a full agent loop against the
/// OpenAI-compatible wire format, no real provider involved.
pub(crate) fn spawn_scripted_sse_server_turns(turns: Vec<ScriptedTurn>) -> u16 {
    spawn_scripted_sse_server_turns_with_capture(turns, Default::default()).0
}

/// Same scripted multi-turn server but also capturing the JSON
/// bodies of each request (story 2.1's context assertions: the
/// steered text must show up in the provider's next turn input).
pub(crate) fn spawn_scripted_sse_server_turns_with_capture(
    turns: Vec<ScriptedTurn>,
    captured: Arc<std::sync::Mutex<Vec<String>>>,
) -> (u16, Arc<std::sync::Mutex<Vec<String>>>) {
    use std::io::Write;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind scripted SSE server");
    let port = listener.local_addr().expect("local addr").port();
    let turns = std::sync::Arc::new(std::sync::Mutex::new(turns));
    let captured_for_thread = captured.clone();
    std::thread::Builder::new()
        .name("scripted-sse-server-turns".into())
        .spawn(move || {
            let mut index = 0usize;
            while let Some(mut stream) =
                accept_scripted_connection_capture(&listener, &captured_for_thread)
            {
                let turn = {
                    let guard = turns.lock().unwrap();
                    guard.get(index).cloned()
                };
                index += 1;
                if stream.write_all(SCRIPTED_SSE_HEADER.as_bytes()).is_err() {
                    continue;
                }
                let _ = stream.flush();
                let Some(turn) = turn else {
                    // Out of script: behave like a provider that never
                    // finishes so the test's timeout reports the hang
                    // with a clear cause.
                    std::thread::sleep(std::time::Duration::from_secs(120));
                    continue;
                };
                let (deltas, tool_call) = match &turn {
                    ScriptedTurn::Text(deltas) => (deltas, None),
                    ScriptedTurn::TextThenTool(deltas, tool, arguments) => {
                        (deltas, Some((tool.clone(), arguments.clone())))
                    }
                };
                for (delay, delta) in deltas {
                    std::thread::sleep(*delay);
                    let event = serde_json::json!({
                        "id": "chatcmpl-scripted",
                        "choices": [{
                            "delta": {"content": delta},
                            "finish_reason": serde_json::Value::Null,
                        }]
                    });
                    if write_sse_frame(&mut stream, &format!("data: {event}\r\n\r\n")).is_err() {
                        break;
                    }
                }
                if let Some((tool, arguments)) = tool_call {
                    std::thread::sleep(std::time::Duration::from_millis(80));
                    let event = serde_json::json!({
                        "id": "chatcmpl-scripted",
                        "choices": [{
                            "delta": {"tool_calls": [{
                                "index": 0,
                                "id": "call-scripted",
                                "type": "function",
                                "function": {"name": tool, "arguments": arguments},
                            }]},
                            "finish_reason": serde_json::Value::Null,
                        }]
                    });
                    let _ = write_sse_frame(&mut stream, &format!("data: {event}\r\n\r\n"));
                    let stop = serde_json::json!({
                        "id": "chatcmpl-scripted",
                        "choices": [{
                            "delta": {},
                            "finish_reason": "tool_calls",
                        }]
                    });
                    let _ = write_sse_frame(&mut stream, &format!("data: {stop}\r\n\r\n"));
                } else {
                    let stop = serde_json::json!({
                        "id": "chatcmpl-scripted",
                        "choices": [{
                            "delta": {},
                            "finish_reason": "stop",
                        }]
                    });
                    let _ = write_sse_frame(&mut stream, &format!("data: {stop}\r\n\r\n"));
                }
                let _ = write_sse_frame(&mut stream, "data: [DONE]\r\n\r\n");
                // Terminal chunk (size 0) closes the response body.
                let _ = write_sse_frame(&mut stream, "");
            }
        })
        .expect("spawn scripted SSE turns server");
    (port, captured)
}

/// Accept one scripted server connection and drain its HTTP request
/// (reads to the declared content-length). Shared by the single-script
/// and multi-turn SSE harnesses (review patch 2026-11-06: the drain
/// logic must not live twice).
pub(crate) fn accept_scripted_connection(
    listener: &std::net::TcpListener,
) -> Option<std::net::TcpStream> {
    accept_scripted_connection_capture(listener, &Default::default())
}

/// Same accept+drain but recording each request's JSON body into
/// `captured` — story 2.1's context assertions look for the steered
/// text in the provider's next-turn input.
pub(crate) fn accept_scripted_connection_capture(
    listener: &std::net::TcpListener,
    captured: &Arc<std::sync::Mutex<Vec<String>>>,
) -> Option<std::net::TcpStream> {
    use std::io::Read;
    let mut stream = listener.incoming().next()?.ok()?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .ok();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                let head_end = buf
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .map(|pos| pos + 4);
                if let Some(head_end) = head_end {
                    let head = String::from_utf8_lossy(&buf[..head_end]);
                    let content_length: usize = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.trim()
                                .eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().ok())?
                        })
                        .unwrap_or(0);
                    if buf.len() >= head_end + content_length {
                        captured
                            .lock()
                            .unwrap()
                            .push(String::from_utf8_lossy(&buf[head_end..]).into_owned());
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    Some(stream)
}

/// The chunked SSE response head + one frame writer, shared by the SSE
/// harnesses.
pub(crate) const SCRIPTED_SSE_HEADER: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";

pub(crate) fn write_sse_frame(
    stream: &mut std::net::TcpStream,
    payload: &str,
) -> std::io::Result<()> {
    use std::io::Write;
    write!(stream, "{:x}\r\n{payload}\r\n", payload.len())?;
    stream.flush()
}

pub(crate) fn spawn_scripted_sse_server_mode(
    deltas: Vec<(std::time::Duration, String)>,
    close_abruptly: bool,
) -> u16 {
    use std::io::Write;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind scripted SSE server");
    let port = listener.local_addr().expect("local addr").port();
    std::thread::Builder::new()
        .name("scripted-sse-server".into())
        .spawn(move || {
            while let Some(mut stream) = accept_scripted_connection(&listener) {
                // The response head is written raw — chunked framing
                // applies to the body only.
                if stream.write_all(SCRIPTED_SSE_HEADER.as_bytes()).is_err() {
                    continue;
                }
                let _ = stream.flush();
                for (delay, delta) in &deltas {
                    std::thread::sleep(*delay);
                    let event = serde_json::json!({
                        "id": "chatcmpl-scripted",
                        "choices": [{
                            "delta": {"content": delta},
                            "finish_reason": serde_json::Value::Null,
                        }]
                    });
                    if write_sse_frame(&mut stream, &format!("data: {event}\r\n\r\n")).is_err() {
                        break;
                    }
                }
                if !close_abruptly {
                    let stop = serde_json::json!({
                        "id": "chatcmpl-scripted",
                        "choices": [{
                            "delta": {},
                            "finish_reason": "stop",
                        }]
                    });
                    let _ = write_sse_frame(&mut stream, &format!("data: {stop}\r\n\r\n"));
                    let _ = write_sse_frame(&mut stream, "data: [DONE]\r\n\r\n");
                    // Terminal chunk (size 0) closes the response body.
                    let _ = write_sse_frame(&mut stream, "");
                }
                if close_abruptly {
                    // Broken chunked framing after the deltas: reqwest's
                    // body decoder fails mid-stream, which the provider
                    // surfaces as a real stream error (not a tidy EOF).
                    let _ = stream.write_all(b"this is not chunked framing\r\n");
                    let _ = stream.flush();
                    continue;
                }
            }
        })
        .expect("spawn scripted SSE server");
    port
}

// ── Story 1.4: finish and mid-stream cancel cleanup ─────────

/// Poll the live event tap until one of the streamed events contains
/// `needle` (bounded so a regression fails the test, never hangs it).
/// Runs on a plain driver thread while `commands` is held by the run.
pub(crate) fn wait_for_tap_event(
    tap: &std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    needle: &str,
) {
    wait_for_tap_event_count(tap, needle, 1);
}

/// Same as [`wait_for_tap_event`], but waits for the Nth occurrence —
/// used to land the cancel flag exactly inside an iteration-N provider
/// call (`ModelRequest` is emitted AFTER the iteration's boundary
/// check and BEFORE the provider call streams).
pub(crate) fn wait_for_tap_event_count(
    tap: &std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    needle: &str,
    count: usize,
) {
    // Issue #4: a wall-clock deadline from test start fails on busy
    // runners even while the run is making progress. Deadline resets
    // on every new matching event; only a quiet window with no
    // progress fails, with the last count in the message.
    let quiet = std::time::Duration::from_secs(10);
    let mut saw = 0;
    let mut deadline = std::time::Instant::now() + quiet;
    loop {
        let now_saw = tap
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.contains(needle))
            .count();
        if now_saw >= count {
            return;
        }
        // Checked after the satisfied condition: a starved poll thread
        // that woke up past the deadline with the condition already
        // met must not fail spuriously (story 1.4 review).
        if now_saw > saw {
            // Progress: grant a fresh quiet window.
            saw = now_saw;
            deadline = std::time::Instant::now() + quiet;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "event tap stalled: saw {saw}/{count}× {needle} with no new events for {quiet:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
