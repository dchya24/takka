//! Transport-neutral runtime protocol types.
//!
//! # Wire contract (versioned, eksternal)
//!
//! Tipe di bawah adalah KONTRAK LINTAS-PROSES: `ProtocolRequest`,
//! `ProtocolEvent`, `Request`, `Event`, `InitOverrides`,
//! `ConnectionCapabilities`, `NotificationKind`. Perubahan harus additive
//! (`#[serde(default)]`) di dalam `PROTOCOL_VERSION` yang sama; rename,
//! penghapusan, atau perubahan semantik = bump versi. Frontend non-Rust
//! memakai kontrak ini mentah-mentah — jangan menambah field "sementara".
//!
//! Riwayat versi:
//! - v1: `PROTOCOL_NAME = "agentic"`.
//! - v2: rebrand ke `PROTOCOL_NAME = "takka"` (wire-breaking; engine
//!   menolak frame v1 dengan `unsupported protocol version`).
//!
//! # Internal
//!
//! Detail di luar kontrak wire (mis. helper dan modul #[cfg(test)]) tidak boleh
//! bocor ke tipe di atas.

use serde::{Deserialize, Serialize};

use crate::attachments::Attachment;
use crate::events::Event;
use crate::safety::PermissionMode;
use crate::tools::QuestionAnswer;

pub const PROTOCOL_NAME: &str = "takka";
pub const PROTOCOL_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolEvent {
    pub v: u32,
    #[serde(rename = "requestId", skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(flatten)]
    pub event: Event,
}

impl ProtocolEvent {
    pub fn new(request_id: Option<String>, event: Event) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            request_id,
            event,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolRequest {
    pub v: u32,
    pub id: String,
    #[serde(flatten)]
    pub request: Request,
}

impl ProtocolRequest {
    pub fn new(id: impl Into<String>, request: Request) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id: id.into(),
            request,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Init {
        #[serde(default)]
        overrides: InitOverrides,
        #[serde(default)]
        capabilities: ConnectionCapabilities,
        /// Daemon shared-secret (Fase 3a): engine yang di-set token
        /// menolak Init tanpa token yang cocok dengan `unauthorized`
        /// SEBELUM menyentuh jalur init/adopt. `None` = non-daemon
        /// (token di-ignored, jalur lama).
        #[serde(default, rename = "daemonToken")]
        daemon_token: Option<String>,
    },
    Run {
        task: String,
        #[serde(default)]
        attachments: Vec<Attachment>,
    },
    /// Steer the currently-running agent: content masuk sebagai user
    /// message di iteration boundary berikutnya (reply: `SteerQueued`).
    Steer {
        content: String,
    },
    Cancel,
    ResetSession,
    SearchMemory {
        query: String,
    },
    /// Ask the daemon for its registered tool set (for `/tools`).
    ListTools,
    /// Activate a skill in the daemon's orchestrator (for `/skill`).
    SkillActivate {
        name: String,
    },
    AddSystemMessage {
        content: String,
    },
    Plan {
        goal: String,
        #[serde(default = "default_true")]
        require_approval: bool,
    },
    ConfirmResponse {
        #[serde(rename = "requestId", default)]
        request_id: Option<String>,
        approved: bool,
    },
    QuestionResponse {
        #[serde(rename = "requestId", default)]
        request_id: Option<String>,
        #[serde(default)]
        answers: Vec<QuestionAnswer>,
    },
    Shutdown,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitOverrides {
    pub config_path: Option<String>,
    pub permission_mode: Option<PermissionMode>,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
}

/// Capabilities yang dideklarasikan frontend saat `Init` — sekali per
/// koneksi, imutabel setelahnya (init kedua ditolak engine).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionCapabilities {
    /// Opt-in API eksperimental (saat ini: `Request::Plan`).
    #[serde(default)]
    pub experimental_api: bool,
    /// Notification yang TIDAK dikirim engine ke frontend ini. Selalu
    /// dalam allowlist `NotificationKind`; nilai lain tidak bisa
    /// direpresentasikan (embedded) / gagal deserialisasi (spawn).
    #[serde(default)]
    pub opt_out_notify: Vec<NotificationKind>,
}

impl ConnectionCapabilities {
    /// Kompatibilitas adopt (Fase 3a): requested capability TIDAK boleh
    /// minta API eksperimental yang belum dipegang engine berjalan.
    /// Opt-out baru di-IGNORE (bukan fatal) — tidak masuk kompatibilitas.
    pub fn compatible_caps(&self, requested: &Self) -> bool {
        !requested.experimental_api || self.experimental_api
    }
}

/// Allowlist tertutup notification yang boleh di-opt-out. Event di luar
/// varian ini (per-request respons dan interaktif gate) TIDAK PERNAH
/// difilter — opt-out tidak bisa membunuh alur run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    Thinking,
    AssistantDelta,
    Warning,
    TodoChanged,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeNotice {
    Ready,
    InitOk,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_event_flattens_event_payload() {
        let event = ProtocolEvent::new(
            Some("r2".into()),
            Event::ToolStarted {
                tool_call_id: "c1".into(),
                tool_name: "grep".into(),
                arguments: serde_json::json!({ "pattern": "foo" }),
            },
        );
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["v"], PROTOCOL_VERSION);
        assert_eq!(value["requestId"], "r2");
        assert_eq!(value["type"], "tool_started");
        assert_eq!(value["toolCallId"], "c1");
    }

    #[test]
    fn init_request_without_capabilities_field_defaults() {
        // Frame gaya lama (mis. protocol-demo.js): tidak membawa capabilities.
        let frame = r#"{"v":2,"id":"i1","type":"init"}"#;
        let req: ProtocolRequest = serde_json::from_str(frame).unwrap();
        match req.request {
            Request::Init { capabilities, .. } => {
                assert_eq!(capabilities, ConnectionCapabilities::default());
                assert!(!capabilities.experimental_api);
                assert!(capabilities.opt_out_notify.is_empty());
            }
            other => panic!("expected Init, got {other:?}"),
        }
    }

    #[test]
    fn init_request_with_capabilities_deserializes() {
        let frame = r#"{"v":2,"id":"i2","type":"init","capabilities":{"experimentalApi":true,"optOutNotify":["thinking","todo_changed"]}}"#;
        let req: ProtocolRequest = serde_json::from_str(frame).unwrap();
        match req.request {
            Request::Init { capabilities, .. } => {
                assert!(capabilities.experimental_api);
                assert!(capabilities
                    .opt_out_notify
                    .contains(&NotificationKind::Thinking));
                assert!(capabilities
                    .opt_out_notify
                    .contains(&NotificationKind::TodoChanged));
            }
            other => panic!("expected Init, got {other:?}"),
        }
    }

    #[test]
    fn unknown_notification_name_fails_frame_deserialization() {
        // Allowlist tertutup: nama di luar enum tidak bisa lewat jalur wire.
        let frame = r#"{"v":2,"id":"i3","type":"init","capabilities":{"experimentalApi":false,"optOutNotify":["done"]}}"#;
        assert!(serde_json::from_str::<ProtocolRequest>(frame).is_err());
    }

    #[test]
    fn init_carries_daemon_token_and_defaults_absent() {
        let frame = r#"{"v":2,"id":"i","type":"init","daemonToken":"tok-1"}"#;
        let req: ProtocolRequest = serde_json::from_str(frame).unwrap();
        match req.request {
            Request::Init { daemon_token, .. } => {
                assert_eq!(daemon_token.as_deref(), Some("tok-1"))
            }
            other => panic!("expected Init, got {other:?}"),
        }
        let bare: Request = serde_json::from_str(r#"{"type":"init"}"#).unwrap();
        assert!(matches!(
            bare,
            Request::Init {
                daemon_token: None,
                ..
            }
        ));
    }

    #[test]
    fn capabilities_compatibility_gates_only_experimental() {
        let base = ConnectionCapabilities::default();
        assert!(base.compatible_caps(&ConnectionCapabilities::default()));
        assert!(!base.compatible_caps(&ConnectionCapabilities {
            experimental_api: true,
            opt_out_notify: vec![],
        }));
        let granted = ConnectionCapabilities {
            experimental_api: true,
            opt_out_notify: vec![],
        };
        assert!(granted.compatible_caps(&base));
        // opt-out baru di-IGNORE (bukan fatal), tidak masuk kompatibilitas.
        assert!(granted.compatible_caps(&ConnectionCapabilities {
            experimental_api: true,
            opt_out_notify: vec![NotificationKind::Thinking],
        }));
    }
}
