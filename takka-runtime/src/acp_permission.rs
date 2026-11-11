//! Permission bridge takka ⇄ ACP v1 (story T-06, unit-level).
//! (#) Terwire sejak T-05: turn loop memanggil `permission_request` saat
//! `ConfirmationRequest`, `outcome_approves` + `confirm_response` saat
//! jawaban klien dirutekan kembali ke engine.

use agent_client_protocol_schema::v1 as acp;

use takka_core::runtime::protocol::Request;

/// D8: satu pasangan default `allow_once`/`reject_once`;
/// `*_always` hanya muncul bila tool ada di jalur auto-approve (T-05 dialan).
pub(crate) fn permission_options() -> Vec<acp::PermissionOption> {
    vec![
        acp::PermissionOption::new("allow_once", "Allow", acp::PermissionOptionKind::AllowOnce),
        acp::PermissionOption::new(
            "reject_once",
            "Reject",
            acp::PermissionOptionKind::RejectOnce,
        ),
    ]
}

/// ConfirmationRequest.action → ToolKind yang dirender editor:
/// run/run_command/run_script → Execute; write/edit/apply_patch → Edit;
/// delete → Delete; read → Read; move → Move; lain → Other.
pub(crate) fn permission_tool_kind(action: &str) -> acp::ToolKind {
    match action {
        "run" | "run_command" | "run_script" => acp::ToolKind::Execute,
        "write" | "edit" | "apply_patch" => acp::ToolKind::Edit,
        "delete" => acp::ToolKind::Delete,
        "read" => acp::ToolKind::Read,
        "move" => acp::ToolKind::Move,
        _ => acp::ToolKind::Other,
    }
}

/// Bangun permintaan permission: ToolCallUpdate (id = id request outbound
/// unik, title = description, kind = mapping), options D8.
pub(crate) fn permission_request(
    session_id: &str,
    outbound_id: &str,
    action: &str,
    description: &str,
) -> acp::RequestPermissionRequest {
    let tool_call = acp::ToolCallUpdate::new(
        acp::ToolCallId::new(outbound_id),
        acp::ToolCallUpdateFields::new()
            .title(description)
            .kind(permission_tool_kind(action)),
    );
    acp::RequestPermissionRequest::new(
        acp::SessionId::new(session_id),
        tool_call,
        permission_options(),
    )
}

/// Outcome klien → approved?
/// Cancelled → false; Selected("allow_once"/"allow_always") → true;
/// Selected lainnya (reject_*) → false.
pub(crate) fn outcome_approves(outcome: &acp::RequestPermissionOutcome) -> bool {
    match outcome {
        acp::RequestPermissionOutcome::Selected(selected) => {
            matches!(&*selected.option_id.0, "allow_once" | "allow_always")
        }
        acp::RequestPermissionOutcome::Cancelled => false,
        // Enum ACP `#[non_exhaustive]`: outcome lain yang muncul di versi
        // protokol berikutnya default ke tolak (fail-closed).
        _ => false,
    }
}

/// Outcome → Request::ConfirmResponse yang kembali ke engine (approved bool).
pub(crate) fn confirm_response(request_id: Option<String>, approved: bool) -> Request {
    Request::ConfirmResponse {
        request_id,
        approved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acp::{RequestPermissionOutcome, SelectedPermissionOutcome};

    fn selected(option_id: &str) -> RequestPermissionOutcome {
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id.to_string()))
    }

    /// T-06: outbound `session/request_permission` JSON asserted (unit-level).
    /// Asersi per field penting, bukan string-compare penuh — crate ACP
    /// bebas menambah field opsional (mis. _meta) tanpa memecah test.
    #[test]
    fn wire_shape_request_permission() {
        let request = permission_request("sess-1", "perm-1", "run", "delete the build dir");
        let value = serde_json::to_value(&request).expect("serialize RequestPermissionRequest");

        assert_eq!(value["sessionId"], "sess-1");
        assert_eq!(value["toolCall"]["toolCallId"], "perm-1");
        assert_eq!(value["toolCall"]["title"], "delete the build dir");
        assert_eq!(value["toolCall"]["kind"], "execute");

        let options = value["options"].as_array().expect("options array");
        assert_eq!(options.len(), 2);
        assert_eq!(options[0]["optionId"], "allow_once");
        assert_eq!(options[0]["name"], "Allow");
        assert_eq!(options[0]["kind"], "allow_once");
        assert_eq!(options[1]["optionId"], "reject_once");
        assert_eq!(options[1]["name"], "Reject");
        assert_eq!(options[1]["kind"], "reject_once");
    }

    #[test]
    fn outcome_approves_matches_allow_variants() {
        assert!(outcome_approves(&selected("allow_once")));
        assert!(outcome_approves(&selected("allow_always")));
        assert!(!outcome_approves(&selected("reject_once")));
        assert!(!outcome_approves(&selected("reject_always")));
        assert!(!outcome_approves(&RequestPermissionOutcome::Cancelled));
    }

    #[test]
    fn tool_kind_mapping_covers_actions() {
        // run/run_command/run_script → Execute
        assert_eq!(permission_tool_kind("run"), acp::ToolKind::Execute);
        assert_eq!(permission_tool_kind("run_command"), acp::ToolKind::Execute);
        assert_eq!(permission_tool_kind("run_script"), acp::ToolKind::Execute);
        // write/edit/apply_patch → Edit
        assert_eq!(permission_tool_kind("write"), acp::ToolKind::Edit);
        assert_eq!(permission_tool_kind("edit"), acp::ToolKind::Edit);
        assert_eq!(permission_tool_kind("apply_patch"), acp::ToolKind::Edit);
        // delete → Delete; read → Read; move → Move
        assert_eq!(permission_tool_kind("delete"), acp::ToolKind::Delete);
        assert_eq!(permission_tool_kind("read"), acp::ToolKind::Read);
        assert_eq!(permission_tool_kind("move"), acp::ToolKind::Move);
        // lain → Other
        assert_eq!(permission_tool_kind("search"), acp::ToolKind::Other);
        assert_eq!(permission_tool_kind(""), acp::ToolKind::Other);
        assert_eq!(
            permission_tool_kind("totally_unknown"),
            acp::ToolKind::Other
        );
    }

    #[test]
    fn confirm_response_maps_to_engine_request() {
        let response = confirm_response(Some("req-9".to_string()), false);
        match &response {
            Request::ConfirmResponse {
                request_id,
                approved,
            } => {
                assert_eq!(request_id.as_deref(), Some("req-9"));
                assert!(!approved);
            }
            other => panic!("expected ConfirmResponse, got {other:?}"),
        }
        // Serde: wire shape `{"type":"confirm_response","requestId":"req-9","approved":false}`.
        let value = serde_json::to_value(&response).expect("serialize Request");
        assert_eq!(value["type"], "confirm_response");
        assert_eq!(value["requestId"], "req-9");
        assert_eq!(value["approved"], false);
    }
}
