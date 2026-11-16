//! ACP (Agent Client Protocol) v1 agent adapter — Fase 4.
//!
//! Design: `docs/superpowers/specs/2026-11-10-takka-acp-agent-design.md`.
//! takka implements the **Agent** side of ACP: an ACP Client (Zed, or any other
//! ACP-speaking editor) boots `takka-runtime --acp` as a subprocess and drives
//! the connection over JSON-RPC 2.0, newline-delimited on stdin/stdout (see the
//! ACP v1 *Transports* page).
//!
//! Phase 1 (this module grows over stories T-00 → T-07):
//! - T-00: the `--acp` entry, framing loop, inbound routing, typed JSON-RPC
//!   error responses for unimplemented methods.
//! - T-01: the `initialize` responder — v1 negotiation plus the capability-set
//!   decisions D7/D9/D11/D12 of the spec.
//! - T-02: the engine-`Event` → ACP `SessionUpdate` translator — pure
//!   functions over engine events, no I/O, fully unit-tested.
//! - T-03: the session registry — `session/new` validates the single-cwd
//!   contract (D11), boots one `RuntimeEngine` per session over a
//!   `MemoryTransport` (the daemon hosting pattern, D6), advertises the
//!   takka permission modes as ACP session modes (D9); `session/prompt`
//!   maps content blocks to task + attachments and runs the turn on its
//!   own thread so the framing loop stays responsive to `session/cancel`
//!   (minimum wiring: flag + cooperative `Request::Cancel`; full
//!   `stopReason: cancelled` mapping is the T-05 lane).
//! - T-04: connection capacity — at most `AcpConfig::max_sessions` live
//!   sessions; the N+1-th `session/new` gets a typed `invalid_request`
//!   error (`session capacity reached`).
//! - T-05: mid-turn cancellation resolves the prompt with
//!   `stopReason: "cancelled"` (not an error) and the session stays
//!   usable; the permission bridge (T-06) is wired into the turn loop —
//!   a `ConfirmationRequest` becomes an outbound
//!   `session/request_permission` whose client answer (or a cancel /
//!   teardown deny) unblocks the engine via `Request::ConfirmResponse`.
//! - T-07: the EOF teardown (spec §5 step 6) — cancel every live session
//!   (flag + cooperative `Request::Cancel`), free pending permission
//!   waiters (the waiter map itself is the T-05 lane's deliverable),
//!   drain the registry, and bounded-join every engine/turn thread (the
//!   daemon `join_bounded` pattern) so the process exits 0 without
//!   stranding threads. Stragglers are detached with a warning.
//!
//! The engine is never modified: sessions each own a `RuntimeEngine` over
//! a `MemoryTransport`, exactly like the daemon host. Nothing in this
//! module writes anything to stdout except framed ACP messages — logs stay
//! on stderr.
//!
//! Layout (mechanical split, no behaviour change): this file is the entry
//! point — configuration, the `initialize` responder, param/error helpers,
//! and the re-exports that keep `crate::acp::*` unchanged. The framing loop
//! and stdio entries live in `framing`, the router/registry in `service`,
//! session state in `session`, the `set_mode`/`set_config_option`/`load`
//! handlers in `session_config`, the prompt turn + content mapping in
//! `prompt`, the engine-event translator in `translate`, and the permission
//! bridge in `permissions`. `test_support` holds the shared test harness
//! (test builds only).

mod framing;
mod permissions;
mod prompt;
mod service;
mod session;
mod session_config;
mod translate;

#[cfg(test)]
mod test_support;

pub(crate) use framing::*;
pub(crate) use permissions::*;
pub(crate) use prompt::*;
pub(crate) use service::*;
pub(crate) use session::*;
pub(crate) use session_config::*;
pub(crate) use translate::*;

use std::time::Duration;

use agent_client_protocol_schema::{v1 as acp, ProtocolVersion};
use serde_json::Value;
use takka_core::sync::lock;

/// Default session capacity of one ACP connection (spec D6: bounded
/// engine-per-session; the `--acp-max-sessions` flag lands with the
/// integrator wiring `AcpConfig` in `takka-runtime/src/main.rs`).
pub const DEFAULT_MAX_SESSIONS: usize = 4;

/// Bound on the `InitOk` wait during `session/new` boot. Engine init is
/// local (config read, skill discovery, prompt assembly) — 10s is ample;
/// a hung boot must not wedge the framing loop.
pub(crate) const BOOT_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound on waiting for the engine's `PermissionModeChanged` ack of a
/// `session/set_mode` (D9). The engine answers from its request loop, so a
/// timeout means it is wedged/dead — never a slow turn.
pub(crate) const MODE_SWITCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Bound on the EOF teardown join (T-07). Cancelling is cooperative and
/// an engine thread only exits after its active run winds down at the
/// next boundary, so the join wait must cover a realistic cancel-to-
/// terminal span; after the deadline, stragglers are left detached (the
/// daemon `join_bounded` pattern) and the process still exits.
pub(crate) const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------------------
// Configuration.
// ---------------------------------------------------------------------------

/// Adapter configuration. Production callers use [`AcpConfig::default`];
/// tests inject a hermetic config path via [`serve_over`].
pub struct AcpConfig {
    /// Maximum live sessions per connection (spec D6). The N+1-th
    /// `session/new` is rejected with `session capacity reached`.
    pub max_sessions: usize,
    /// Optional config file handed to each session engine via
    /// `InitOverrides.config_path`. `None` = engine loads the user's
    /// ambient config (production); tests pin a hermetic file here.
    pub config_path: Option<String>,
}

impl Default for AcpConfig {
    fn default() -> Self {
        Self {
            max_sessions: DEFAULT_MAX_SESSIONS,
            config_path: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Typed wire errors (the messages clients act on).
// ---------------------------------------------------------------------------

pub(crate) fn internal_error(message: impl Into<String>) -> acp::Error {
    acp::Error::new(i32::from(acp::ErrorCode::InternalError), message)
}

pub(crate) fn unsupported_content_type(kind: &str, hint: &str) -> acp::Error {
    let message = format!("unsupported content type {kind}: {hint}");
    acp::Error::new(i32::from(acp::ErrorCode::InvalidParams), message).data(serde_json::json!(
        { "method": SESSION_PROMPT, "reason": "unsupported content type", "contentType": kind }
    ))
}

pub(crate) fn invalid_params_error(message: impl Into<String>) -> acp::Error {
    acp::Error::new(i32::from(acp::ErrorCode::InvalidParams), message.into())
}

/// Unwraps `params` and deserializes into a typed request body.
pub(crate) fn parse_params<T: serde::de::DeserializeOwned>(
    frame: &Value,
    method: &str,
) -> Result<T, acp::Error> {
    let params = frame.get("params").cloned().unwrap_or(Value::Null);
    serde_json::from_value(params)
        .map_err(|error| acp::Error::invalid_params().data(params_error_data(method, &error)))
}

pub(crate) fn params_error_data(method: &str, error: &serde_json::Error) -> Value {
    serde_json::json!({ "method": method, "reason": error.to_string() })
}

// ---------------------------------------------------------------------------
// T-01: initialize responder — v1 negotiation + capability set (D7/D9/D11).
// ---------------------------------------------------------------------------

/// Pick the per-connection protocol version and answer with capabilities.
///
/// Decision D3: takka speaks ACP v1. A Client asking for v2 (still draft)
/// receives a v1 answer — per the negotiation contract the Client then decides
/// whether it can fall back. Version 0 is the pre-release, unnamed version and
/// also yields v1.
pub fn initialize_response(_request: &acp::InitializeRequest) -> acp::InitializeResponse {
    acp::InitializeResponse::new(ProtocolVersion::V1)
        .agent_capabilities(agent_capabilities())
        .agent_info(agent_implementation())
}

/// The capability set advertised to every connected Client (spec D7).
///
/// takka owns the filesystem and command execution itself (its own safety
/// layer runs the tools), so nothing branches on Client-side `fs`/`terminal`
/// capabilities — the agent simply never calls those methods, whatever the
/// Client advertises. `mcpCapabilities` stays false: takka's MCP servers come
/// from its own config today (client-provided servers are phase 2).
pub(crate) fn agent_capabilities() -> acp::AgentCapabilities {
    acp::AgentCapabilities::new()
        .prompt_capabilities(acp::PromptCapabilities::new().image(true))
        .mcp_capabilities(acp::McpCapabilities::new())
        .load_session(true)
        .session_capabilities(
            acp::SessionCapabilities::new().resume(acp::SessionResumeCapabilities::new()),
        )
}

pub(crate) fn agent_implementation() -> acp::Implementation {
    acp::Implementation::new("takka", env!("CARGO_PKG_VERSION")).title("takka")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // T-01: initialize negotiation + capability set.
    // ------------------------------------------------------------------

    fn init_request(version: u16) -> acp::InitializeRequest {
        serde_json::from_value(serde_json::json!({
            "protocolVersion": version,
            "clientCapabilities": { "fs": { "readTextFile": true } },
            "clientInfo": { "name": "test-client", "version": "0.0.0" }
        }))
        .expect("initialize request json shape")
    }

    fn init_response(version: u16) -> acp::InitializeResponse {
        initialize_response(&init_request(version))
    }

    #[test]
    fn initialize_answers_v1_regardless_of_requested_version() {
        for requested in [0u16, 1, 2] {
            let response = init_response(requested);
            assert_eq!(response.protocol_version, ProtocolVersion::V1);
        }
    }

    /// Decision D7: image-capable prompts only; no audio/embedded-context,
    /// no MCP transports, no loadSession, no auth methods, takka agentInfo.
    #[test]
    fn initialize_advertises_the_d7_capability_set() {
        let response = init_response(1);
        let caps = &response.agent_capabilities;
        // Stored-session lifecycle is supported: `session/load` replays the
        // checkpoint history, `session/resume` re-attaches live sessions.
        assert!(caps.load_session);
        assert!(caps.session_capabilities.resume.is_some());
        assert!(caps.prompt_capabilities.image);
        assert!(!caps.prompt_capabilities.audio);
        assert!(!caps.prompt_capabilities.embedded_context);
        assert!(!caps.mcp_capabilities.http);
        assert!(!caps.mcp_capabilities.sse);
        // auth: ACP AgentAuthCapabilities defaults to no logout flow; takka
        // reads its own config so no ACP auth methods exist.
        assert!(caps.auth.logout.is_none());
        let agent_info = response.agent_info.expect("agentInfo must be present");
        assert_eq!(agent_info.name, "takka");
        assert_eq!(agent_info.version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn initialize_response_serializes_to_the_v1_wire_shape() {
        let value = serde_json::to_value(init_response(1)).expect("serializes");
        assert_eq!(value["protocolVersion"], serde_json::json!(1));
        assert_eq!(
            value["agentCapabilities"]["promptCapabilities"]["image"],
            serde_json::json!(true)
        );
    }
}
