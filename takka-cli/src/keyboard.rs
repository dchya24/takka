//! Shared keyboard event filtering for terminal input modes.

use crossterm::event::KeyEventKind;

/// Whether a keyboard event should be dispatched to an input handler.
///
/// Windows reports both key-down and key-up events. Processing `Release`
/// would apply the same input mutation twice, while `Repeat` must remain
/// enabled so holding a key continues to work.
pub(crate) fn should_process_key_kind(kind: KeyEventKind) -> bool {
    !matches!(kind, KeyEventKind::Release)
}
