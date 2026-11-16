use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use crate::acp_permission::{self as acp_permission, PermissionAnswer};

use super::*;

// ---------------------------------------------------------------------------
// T-05: the permission bridge's pending waiters — the shared state between
// the turn thread (asks + parks), the framing loop (routes the client's
// answer), and the cancel/teardown paths (deny).
// ---------------------------------------------------------------------------

/// One parked `session/request_permission` gate.
pub(crate) struct PermissionWaiter {
    /// Owning session — `session/cancel` denies only this session's gates.
    pub(crate) session_id: String,
    /// The client's permission answer back to the parked turn thread.
    /// Capacity 1: every waiter receives exactly one answer, so a send
    /// cannot block.
    pub(crate) answer_tx: mpsc::SyncSender<PermissionAnswer>,
}

/// Pending `session/request_permission` waiters keyed by the outbound
/// request id (`acp-perm-1`, `acp-perm-2`, …). Shared via `Arc` because
/// prompt-turn threads outlive the `AcpService` borrow.
#[derive(Default)]
pub(crate) struct PermissionWaiters {
    pub(crate) inner: Mutex<HashMap<String, PermissionWaiter>>,
    /// Outbound id source, unique per connection.
    pub(crate) counter: AtomicU64,
}

impl PermissionWaiters {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn next_outbound_id(&self) -> String {
        format!(
            "acp-perm-{}",
            self.counter.fetch_add(1, Ordering::SeqCst) + 1
        )
    }

    pub(crate) fn insert(&self, outbound_id: String, waiter: PermissionWaiter) {
        // Called with a fresh id from `next_outbound_id` — never a collision.
        lock(&self.inner).insert(outbound_id, waiter);
    }

    /// Take the waiter for an answered request (framing loop).
    pub(crate) fn remove(&self, outbound_id: &str) -> Option<PermissionWaiter> {
        lock(&self.inner).remove(outbound_id)
    }

    /// Deny every waiter of one session (`session/cancel` mid-gate): the
    /// parked turn resumes with `false` and routes the cancel to its
    /// terminal instead of dead-locking on the gate.
    pub(crate) fn deny_session(&self, session_id: &str) {
        let denied: Vec<PermissionWaiter> = {
            let mut waiters = lock(&self.inner);
            let ids: Vec<String> = waiters
                .iter()
                .filter(|(_, waiter)| waiter.session_id == session_id)
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| waiters.remove(&id))
                .collect()
        };
        for waiter in denied {
            let _ = waiter.answer_tx.send(PermissionAnswer::Deny);
        }
    }

    /// Deny everything (EOF teardown, T-07).
    pub(crate) fn deny_all(&self) {
        let denied: Vec<PermissionWaiter> = lock(&self.inner)
            .drain()
            .map(|(_, waiter)| waiter)
            .collect();
        for waiter in denied {
            let _ = waiter.answer_tx.send(PermissionAnswer::Deny);
        }
    }
}

/// One permission gate (T-05): send `session/request_permission` (D8) and
/// block the turn until the client's answer frame arrives (routed by the
/// framing loop through [`AcpService::route_client_response`]) or a
/// deny frees the waiter (`session/cancel` for this session, or the EOF
/// teardown). A deny and a dropped waiter both count as rejection —
/// fail-closed, and the engine is always unblocked so the turn can run
/// to its terminal.
pub(crate) fn request_permission(
    session: &Session,
    permissions: &PermissionWaiters,
    sink: &Arc<dyn AcpSink>,
    action: &str,
    description: &str,
) -> PermissionAnswer {
    let outbound_id = permissions.next_outbound_id();
    let (answer_tx, answer_rx) = mpsc::sync_channel::<PermissionAnswer>(1);
    permissions.insert(
        outbound_id.clone(),
        PermissionWaiter {
            session_id: session.id.clone(),
            answer_tx,
        },
    );
    let request =
        acp_permission::permission_request(&session.id, &outbound_id, action, description);
    sink.send_line(&serde_json::json!({
        "jsonrpc": "2.0", "id": outbound_id,
        "method": SESSION_REQUEST_PERMISSION,
        "params": serde_json::to_value(request).expect("permission request serializes")
    }));
    // No deadline here (a user may take minutes to decide) — cancel and
    // teardown are the events that free this park early. A dropped
    // sender (waiter removed without an answer) is also a denial.
    answer_rx.recv().unwrap_or(PermissionAnswer::Deny)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::test_support::*;

    use takka_core::runtime::protocol::Request;
    /// T-05/T-06 integration, allow path: the gate's wire shape (D8) and
    /// the full round trip — client answers `allow_once` with the echoed
    /// id → engine receives `ConfirmResponse { approved: true }` → run
    /// completes with `stopReason: end_turn`.
    #[test]
    fn permission_allow_and_confirm_response_flow() {
        let (seen_tx, seen_rx) = std::sync::mpsc::channel::<Request>();
        let fake = t05_fake_engine(true, true, seen_tx);
        let (acp, _probe) = TestAcp::spawn_with_fake(DEFAULT_MAX_SESSIONS, fake);
        let session = new_session(&acp, 1);

        send_prompt(&acp, 2, &session, text_prompt("gated turn"));
        let permission = wait_for_permission_request(&acp);

        // Wire shape: envelope + sessionId + toolCall identity/kind/title
        // + the default allow_once/reject_once option pair.
        assert_eq!(permission["jsonrpc"], serde_json::json!("2.0"));
        assert_eq!(
            permission["params"]["sessionId"],
            serde_json::json!(session)
        );
        assert_eq!(
            permission["params"]["toolCall"]["toolCallId"],
            serde_json::json!("acp-perm-1")
        );
        assert_eq!(
            permission["params"]["toolCall"]["kind"],
            serde_json::json!("execute")
        );
        assert_eq!(
            permission["params"]["toolCall"]["title"],
            serde_json::json!("delete the build dir")
        );
        let options = permission["params"]["options"]
            .as_array()
            .expect("options array");
        assert_eq!(options.len(), 3);
        assert_eq!(options[0]["optionId"], serde_json::json!("allow_once"));
        assert_eq!(options[1]["optionId"], serde_json::json!("allow_always"));
        assert_eq!(options[2]["optionId"], serde_json::json!("reject_once"));

        // The client answers allow_once, echoing the request id.
        acp.send(serde_json::json!({
            "jsonrpc": "2.0", "id": permission["id"],
            "result": { "outcome": { "outcome": "selected", "optionId": "allow_once" } }
        }));

        let response = acp.response(2);
        assert!(response["error"].is_null(), "allow flow: {response}");
        assert_eq!(
            response["result"]["stopReason"],
            serde_json::json!("end_turn")
        );
        assert!(
            seen_requests(&seen_rx)
                .iter()
                .any(|request| matches!(request, Request::ConfirmResponse { approved: true, .. })),
            "the engine must receive ConfirmResponse(approved=true)"
        );
        acp.close_and_join();
    }

    /// T-05/T-06 integration, reject path: `reject_once` routes
    /// `approved: false` to the engine (fail-closed mapping).
    #[test]
    fn permission_reject_routes_false() {
        let (seen_tx, seen_rx) = std::sync::mpsc::channel::<Request>();
        let fake = t05_fake_engine(true, true, seen_tx);
        let (acp, _probe) = TestAcp::spawn_with_fake(DEFAULT_MAX_SESSIONS, fake);
        let session = new_session(&acp, 1);

        send_prompt(&acp, 2, &session, text_prompt("gated turn"));
        let permission = wait_for_permission_request(&acp);
        acp.send(serde_json::json!({
            "jsonrpc": "2.0", "id": permission["id"],
            "result": { "outcome": { "outcome": "selected", "optionId": "reject_once" } }
        }));

        let response = acp.response(2);
        assert_eq!(
            response["result"]["stopReason"],
            serde_json::json!("end_turn")
        );
        assert!(
            seen_requests(&seen_rx).iter().any(|request| matches!(
                request,
                Request::ConfirmResponse {
                    approved: false,
                    ..
                }
            )),
            "the engine must receive ConfirmResponse(approved=false)"
        );
        acp.close_and_join();
    }

    /// H-02: the waiter map is bookkeeping shared by the turn threads, the
    /// framing loop, and the cancel/teardown paths. A panic while it was
    /// held must not make every later permission answer panic — the
    /// recovered guard is used, so the gate still opens.
    #[test]
    fn a_poisoned_waiter_map_still_answers_and_denies() {
        let waiters = PermissionWaiters::new();
        poison(&waiters.inner);

        // Insert → take → answer: the normal gate round trip.
        let (answer_tx, answer_rx) = mpsc::sync_channel::<PermissionAnswer>(1);
        waiters.insert(
            waiters.next_outbound_id(),
            PermissionWaiter {
                session_id: "sess-1".to_owned(),
                answer_tx,
            },
        );
        assert_eq!(lock(&waiters.inner).len(), 1);
        let waiter = waiters.remove("acp-perm-1").expect("the inserted waiter");
        waiter.answer_tx.send(PermissionAnswer::AllowOnce).unwrap();
        assert_eq!(answer_rx.recv().unwrap(), PermissionAnswer::AllowOnce);

        // Deny paths survive the poison too.
        let (_tx, rx) = mpsc::sync_channel::<PermissionAnswer>(1);
        waiters.insert(
            "acp-perm-2".to_owned(),
            PermissionWaiter {
                session_id: "sess-2".to_owned(),
                answer_tx: _tx,
            },
        );
        waiters.deny_session("sess-2");
        assert_eq!(rx.recv().unwrap(), PermissionAnswer::Deny);

        let (_tx, rx) = mpsc::sync_channel::<PermissionAnswer>(1);
        waiters.insert(
            "acp-perm-3".to_owned(),
            PermissionWaiter {
                session_id: "sess-3".to_owned(),
                answer_tx: _tx,
            },
        );
        waiters.deny_all();
        assert_eq!(rx.recv().unwrap(), PermissionAnswer::Deny);
        assert!(lock(&waiters.inner).is_empty());
    }

    /// H-02: a poisoned sink lock must not stop the connection's only
    /// output path — later frames are still recorded.
    #[test]
    fn a_poisoned_sink_lock_still_records_frames() {
        let sink = VecSink::default();
        poison(&sink.lines);
        sink.send_line(&serde_json::json!({ "jsonrpc": "2.0", "id": 5, "result": {} }));
        assert_eq!(lock(&sink.lines).len(), 1);
        assert!(sink.response_for(5).is_some());
        assert!(sink.updates().is_empty());
    }
}
