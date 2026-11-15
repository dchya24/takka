use super::*;
use std::collections::VecDeque;

/// Scripted connection: replies to `raw_send` with a fixed request id
/// and hands back queued events in order.
struct ScriptedIo {
    events: VecDeque<ProtocolEvent>,
    sent: Vec<Request>,
}

impl ScriptedIo {
    fn new(events: Vec<ProtocolEvent>) -> Self {
        Self {
            events: events.into(),
            sent: Vec::new(),
        }
    }
}

impl ConnectionIo for ScriptedIo {
    async fn raw_send(&mut self, request: Request) -> Result<String> {
        self.sent.push(request);
        Ok("init-1".to_string())
    }

    async fn raw_next_event(&mut self) -> Result<ProtocolEvent> {
        self.events
            .pop_front()
            .ok_or_else(|| anyhow!("scripted event stream exhausted"))
    }
}

fn ready_frame() -> ProtocolEvent {
    ProtocolEvent::new(
        None,
        takka_core::Event::Ready {
            protocol: "takka".into(),
            version: 2,
        },
    )
}

fn init_ok_frame(request_id: &str) -> ProtocolEvent {
    ProtocolEvent::new(
        Some(request_id.to_string()),
        takka_core::Event::InitOk {
            protocol: "takka".into(),
            version: 2,
        },
    )
}

/// The issue #2 signature: the broker forwards the engine's startup
/// `Ready` in the window before the drain loop would discard it. The
/// attach must succeed anyway.
#[tokio::test]
async fn stray_ready_does_not_fail_the_attach() {
    let mut io = ScriptedIo::new(vec![ready_frame(), init_ok_frame("init-1")]);
    perform_handshake(
        &mut io,
        InitOverrides::default(),
        CLI_CAPABILITIES,
        Some("secret".into()),
    )
    .await
    .expect("a stray Ready must be skipped, not treated as the init reply");
    assert_eq!(io.sent.len(), 1, "exactly one Init is sent");
}

/// A frame tagged with someone else's request id (a replayed
/// adoption) is equally not our answer.
#[tokio::test]
async fn foreign_request_id_is_not_our_reply() {
    let mut io = ScriptedIo::new(vec![
        ProtocolEvent::new(
            Some("other-client".to_string()),
            takka_core::Event::SessionAdopted { session_id: None },
        ),
        init_ok_frame("init-1"),
    ]);
    perform_handshake(
        &mut io,
        InitOverrides::default(),
        CLI_CAPABILITIES,
        Some("secret".into()),
    )
    .await
    .expect("foreign frames must be skipped");
}

/// A broker-level refusal carries no request id (it answers the
/// attach attempt, not a decoded request) and must never be skipped
/// as a "foreign frame" — that would turn "daemon in use" into a
/// connection reset.
#[tokio::test]
async fn untagged_broker_refusal_is_surfaced() {
    let mut io = ScriptedIo::new(vec![ProtocolEvent::new(
        None,
        takka_core::Event::Error {
            message: "daemon in use".into(),
        },
    )]);
    let error = perform_handshake(
        &mut io,
        InitOverrides::default(),
        CLI_CAPABILITIES,
        Some("secret".into()),
    )
    .await
    .expect_err("a broker refusal must fail the attach");
    assert_eq!(error.to_string(), "daemon in use");
}

/// A genuine init failure still surfaces with its message.
#[tokio::test]
async fn matching_error_still_fails_the_attach() {
    let mut io = ScriptedIo::new(vec![ProtocolEvent::new(
        Some("init-1".to_string()),
        takka_core::Event::Error {
            message: "unauthorized".into(),
        },
    )]);
    let error = perform_handshake(
        &mut io,
        InitOverrides::default(),
        CLI_CAPABILITIES,
        Some("bad".into()),
    )
    .await
    .expect_err("a matching Error frame is an init failure");
    assert_eq!(error.to_string(), "unauthorized");
}

/// A stream of unrelated frames must end in an honest, bounded error
/// rather than a hang or a misleading "unexpected initialization
/// event".
#[tokio::test]
async fn endless_foreign_frames_give_up_bounded() {
    let mut io = ScriptedIo::new(vec![ready_frame(); 64]);
    let error = perform_handshake(
        &mut io,
        InitOverrides::default(),
        CLI_CAPABILITIES,
        Some("secret".into()),
    )
    .await
    .expect_err("no Init reply must be an error");
    assert!(
        error.to_string().contains("did not answer Init"),
        "expected the bounded no-reply error, got: {error}"
    );
}
