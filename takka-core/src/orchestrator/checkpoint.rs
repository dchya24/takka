//! Session checkpoint wiring (P1-3).
//!
//! The orchestrator owns an optional [`SessionStore`]. When one is set:
//! - every run opens (or reuses) the session document (`session_begin`);
//! - the terminal state is persisted (`session_terminal` — success,
//!   failure, or cancellation);
//! - `Orchestrator::checkpoint()` snapshots the current state + message
//!   history, so hosts can also checkpoint at tool boundaries.
//!
//! Without a store, all of this is a no-op — the orchestrator stays
//! usable exactly as before.

use std::sync::Mutex;

use super::{Orchestrator, OrchestratorState};
use crate::events::Event;
use crate::session::{AgentSession, SessionStore};
use crate::AgenticError;

impl Orchestrator {
    // ------------------------------------------------------------------
    // Configuration
    // ------------------------------------------------------------------

    /// Attach a checkpoint store. Subsequent runs persist session
    /// documents under it.
    pub fn set_session_store(&mut self, store: SessionStore) {
        self.session_store = Some(Mutex::new(store));
    }

    /// Remove the checkpoint store (disables persistence).
    pub fn clear_session_store(&mut self) {
        self.session_store = None;
    }

    /// Drop the tracked session id — the next run opens a fresh
    /// session document (REPL turn separation).
    pub fn reset_session(&mut self) {
        *self.session_id.lock().unwrap() = None;
    }

    /// Id of the session being tracked, if any.
    pub fn session_id(&self) -> Option<String> {
        self.session_id.lock().unwrap().clone()
    }

    // ------------------------------------------------------------------
    // Lifecycle hooks (called from `run.rs`)
    // ------------------------------------------------------------------

    /// Run preamble: emit `SessionStarted` and open the checkpoint
    /// document (a new session id per run, unless one is already
    /// tracked). Without a store this is just the event emission —
    /// runs stay untracked.
    pub(crate) fn session_begin(&self) {
        self.events.emit(Event::SessionStarted);

        if self.session_store.is_none() {
            return;
        }

        // Adopt an existing id (resume) or mint a fresh one per run.
        let mut current = self.session_id.lock().unwrap();
        if current.is_none() {
            *current = Some(uuid::Uuid::new_v4().to_string());
        }
        let id = current.clone().unwrap();
        drop(current);

        self.write_checkpoint("created", None);
        tracing::debug!(session = %id, "session opened");
    }

    /// Run epilogue: terminal state on the P1-2 machine, lifecycle
    /// event, and the final checkpoint.
    pub(crate) fn session_terminal(&self, result: &Result<String, AgenticError>) {
        match result {
            Ok(content) => {
                self.set_state(OrchestratorState::Completed);
                self.write_checkpoint("completed", Some(content));
                self.events.emit(Event::SessionCompleted {
                    result: content.clone(),
                });
            }
            // Cancellation is user-initiated, not a failure.
            Err(e) if !matches!(e, AgenticError::Cancelled) => {
                self.set_state(OrchestratorState::Failed);
                self.write_checkpoint("failed", None);
                self.events.emit(Event::SessionFailed {
                    message: e.to_string(),
                });
            }
            Err(_) => {
                self.set_state(OrchestratorState::Cancelled);
                self.write_checkpoint("cancelled", None);
            }
        }
    }

    // ------------------------------------------------------------------
    // Checkpointing
    // ------------------------------------------------------------------

    /// Snapshot the current lifecycle state + conversation into the
    /// session document. `extra_result` carries the final answer on
    /// success so the checkpoint matches the `SessionCompleted` event.
    ///
    /// Best-effort: checkpoint failures are logged, never surfaced —
    /// persistence must not break a live run.
    pub fn checkpoint(&self) {
        let state = self.get_state();
        self.write_checkpoint(state.as_str(), None);
    }

    fn write_checkpoint(&self, state: &str, extra_result: Option<&str>) {
        let store_guard = match &self.session_store {
            Some(s) => s.lock().unwrap(),
            None => return,
        };

        let session_id = match self.session_id.lock().unwrap().clone() {
            Some(id) => id,
            None => return,
        };

        // Load-or-create: keeps created_at/created ordering stable
        // across checkpoints of the same session.
        let mut session = match store_guard.load(&session_id) {
            Ok(existing) => existing,
            Err(_) => AgentSession::new(&session_id, self.model.clone()),
        };

        let mut messages: Vec<crate::memory::Message> =
            self.memory.lock().unwrap().get_messages().to_vec();
        if let Some(final_text) = extra_result {
            if let Some(last) = messages.last_mut() {
                if last.role.as_str() == "assistant" && last.content.is_empty() {
                    last.content = final_text.to_string();
                }
            }
        }
        session.checkpoint(state, messages);
        if let Err(e) = store_guard.save(&session) {
            tracing::warn!(error = %e, "session checkpoint failed");
        }
    }

    // ------------------------------------------------------------------
    // Resume
    // ------------------------------------------------------------------

    /// Resume a stored session onto this orchestrator: attaches the
    /// store, replaces the conversation history with the checkpoint's,
    /// adopts its session id, and sets the model. The lifecycle state
    /// machine restarts from `Created` on the next `run` — the
    /// checkpoint's recorded state is informational (what the previous
    /// run died doing). (Distinct from [`Orchestrator::resume`], the
    /// pause-release.)
    pub fn resume_session(
        &mut self,
        store: SessionStore,
        session_id: &str,
    ) -> Result<(), AgenticError> {
        let session = store.load(session_id)?;
        let message_count = session.messages.len();
        {
            let mut mem = self.memory.lock().unwrap();
            mem.clear();
            for message in session.messages {
                mem.add_message(message);
            }
        }
        self.model = session.model;
        self.session_store = Some(Mutex::new(store));
        *self.session_id.lock().unwrap() = Some(session.session_id.clone());
        tracing::info!(
            session = %session.session_id,
            messages = message_count,
            died_in = %session.state,
            "session resumed"
        );
        Ok(())
    }
}
