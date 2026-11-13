//! Permission bridge takka ⇄ ACP v1 (story T-06, unit-level).
//! (#) Terwire sejak T-05: turn loop memanggil `permission_request` saat
//! `ConfirmationRequest`, `outcome_approves` + `confirm_response` saat
//! jawaban klien dirutekan kembali ke engine.

use agent_client_protocol_schema::v1 as acp;

use takka_core::runtime::protocol::Request;

/// What the client answered to one permission gate (the bridge's answer
/// currency between the turn thread and the framing loop).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PermissionAnswer {
    /// Allow this one call.
    AllowOnce,
    /// Allow this call AND every equivalent future call this session
    /// (persisted engine-side as a dynamic auto-approve entry).
    AllowAlways,
    /// Deny (reject or cancelled or dropped).
    Deny,
}

/// D8 (revisi 2026-11-13): `allow_once`/`allow_always`/`reject_once`.
/// `allow_always` mempersist perintah itu ke daftar auto-approve
/// dinamis engine untuk sisa sesi (in-session; tidak ditulis ke config).
pub(crate) fn permission_options() -> Vec<acp::PermissionOption> {
    vec![
        acp::PermissionOption::new("allow_once", "Allow", acp::PermissionOptionKind::AllowOnce),
        acp::PermissionOption::new(
            "allow_always",
            "Always allow",
            acp::PermissionOptionKind::AllowAlways,
        ),
        acp::PermissionOption::new(
            "reject_once",
            "Reject",
            acp::PermissionOptionKind::RejectOnce,
        ),
    ]
}

/// Outcome klien -> jawaban permission (approve + persistensi).
pub(crate) fn outcome_answer(outcome: &acp::RequestPermissionOutcome) -> PermissionAnswer {
    match outcome {
        acp::RequestPermissionOutcome::Selected(selected) => match &*selected.option_id.0 {
            "allow_once" => PermissionAnswer::AllowOnce,
            "allow_always" => PermissionAnswer::AllowAlways,
            // Enum ACP `#[non_exhaustive]`: option lain (reject_*) → deny.
            _ => PermissionAnswer::Deny,
        },
        acp::RequestPermissionOutcome::Cancelled => PermissionAnswer::Deny,
        // Outcome lain di versi protokol berikutnya default ke tolak
        // (fail-closed).
        _ => PermissionAnswer::Deny,
    }
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
        assert_eq!(options.len(), 3);
        assert_eq!(options[0]["optionId"], "allow_once");
        assert_eq!(options[0]["name"], "Allow");
        assert_eq!(options[0]["kind"], "allow_once");
        assert_eq!(options[1]["optionId"], "allow_always");
        assert_eq!(options[1]["name"], "Always allow");
        assert_eq!(options[1]["kind"], "allow_always");
        assert_eq!(options[2]["optionId"], "reject_once");
        assert_eq!(options[2]["name"], "Reject");
        assert_eq!(options[2]["kind"], "reject_once");
    }

    #[test]
    fn outcome_answer_matches_allow_variants() {
        assert_eq!(
            outcome_answer(&selected("allow_once")),
            PermissionAnswer::AllowOnce
        );
        assert_eq!(
            outcome_answer(&selected("allow_always")),
            PermissionAnswer::AllowAlways
        );
        assert_eq!(
            outcome_answer(&selected("reject_once")),
            PermissionAnswer::Deny
        );
        assert_eq!(
            outcome_answer(&selected("reject_always")),
            PermissionAnswer::Deny
        );
        assert_eq!(
            outcome_answer(&RequestPermissionOutcome::Cancelled),
            PermissionAnswer::Deny
        );
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
