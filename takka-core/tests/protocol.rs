use takka_core::events::Event;
use takka_core::runtime::protocol::{ProtocolEvent, ProtocolRequest, Request};

#[test]
fn event_serializes_flat_with_protocol_envelope() {
    let event = Event::ToolStarted {
        tool_call_id: "c1".into(),
        tool_name: "grep".into(),
        arguments: serde_json::json!({"pattern": "foo"}),
    };
    let envelope = ProtocolEvent::new(Some("r2".into()), event);
    let value = serde_json::to_value(envelope).unwrap();

    assert_eq!(value["v"], 1);
    assert_eq!(value["requestId"], "r2");
    assert_eq!(value["type"], "tool_started");
    assert_eq!(value["toolCallId"], "c1");
}

#[test]
fn request_serializes_with_id_and_version() {
    let request = ProtocolRequest::new(
        "r1",
        Request::Run {
            task: "fix bug".into(),
            attachments: vec![],
        },
    );
    let value = serde_json::to_value(request).unwrap();

    assert_eq!(value["v"], 1);
    assert_eq!(value["id"], "r1");
    assert_eq!(value["type"], "run");
    assert_eq!(value["task"], "fix bug");
}

#[test]
fn event_round_trips_from_json_line() {
    let line = r#"{"v":1,"requestId":"r2","type":"assistant_delta","content":"Searching..."}"#;
    let event: ProtocolEvent = serde_json::from_str(line).unwrap();

    assert_eq!(event.request_id.as_deref(), Some("r2"));
    assert!(matches!(event.event, Event::AssistantDelta { content } if content == "Searching..."));
}
