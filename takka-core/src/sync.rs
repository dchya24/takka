//! Poisoning-tolerant lock acquisition.
//!
//! A `std::sync::Mutex` is *poisoned* when a thread panics while holding it:
//! every later `lock()` then returns `Err(PoisonError)`, and the usual
//! `unwrap()` turns that into a second panic. One failed request would take
//! down unrelated sessions that merely share a bookkeeping mutex.
//!
//! The state behind these locks is bookkeeping — queues, counters, caches,
//! snapshots — never a multi-field invariant that a half-finished update
//! could tear, so handing back the poisoned guard is always better than
//! cascading the panic. This is the same recovery already used by the
//! `ENV_LOCK` fix (PR #13/#14).

use std::sync::{Mutex, MutexGuard};

/// Acquire `m`, recovering the guard instead of panicking when a previous
/// holder panicked.
///
/// Use this for every non-test `Mutex` acquisition in the workspace; the
/// only exceptions are sites that genuinely assert an invariant, which use
/// `expect("<reason>")` so the panic carries a real message.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// The behaviour the helper exists for: a thread panics while holding
    /// the lock, and the next thread still reads and writes it.
    #[test]
    fn recovers_a_poisoned_mutex() {
        let m = Arc::new(Mutex::new(0_u32));
        let poisoner = Arc::clone(&m);
        let handle = std::thread::spawn(move || {
            let _guard = lock(&poisoner);
            panic!("poison the mutex on purpose");
        });
        assert!(handle.join().is_err(), "poisoner thread should panic");
        // Poisoned now: `m.lock().unwrap()` would panic here.
        assert!(m.lock().is_err(), "the mutex should be poisoned");

        let mut guard = lock(&m);
        assert_eq!(*guard, 0);
        *guard = 7;
        drop(guard);
        assert_eq!(*lock(&m), 7);
    }
}
