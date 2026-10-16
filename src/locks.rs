// One owner for the process's poison-recovery rule.
//
// Nineteen places in this crate take a `Mutex` and refuse to treat a poisoned
// lock as an error: a thread that panicked while holding it has already
// released the data's only other user, and the surviving thread's choice is
// between wedging the app forever and reading data that may be mid-update. For
// every lock here the answer is the same - take it and carry on - because each
// one guards a transcript string, an error slot, an instant, or a flag, and a
// wrong value is recoverable while a frozen tray is not.
//
// The rule was spelled inline at every site as
// `.lock().unwrap_or_else(|e| e.into_inner())`, which meant the decision itself
// had nineteen copies and no name: nothing said WHY the poison was ignored, a
// new site had to find an old one to copy, and the one place that DID write the
// reason down (`windows_app.rs`'s autostart lock) was an accident of that site
// needing a named accessor. It lives here now, and the individual sites keep
// their own comments for whatever is specific to them - which of two choices a
// poisoned value means, for instance, in `unix_audio`'s error slot, where
// recovering is the point precisely because "no error" would be a lie.
//
// Deliberately NOT a `RwLock`/`Condvar` generalisation, and not a wrapper type:
// the callers want the ordinary `MutexGuard` and its `Deref`, unchanged.

use std::sync::{Mutex, MutexGuard};

/// Lock `m`, taking the lock even when a previous holder panicked.
pub(crate) fn lock_poisoned<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The contract in one place: a poisoned lock still hands back its data
    /// instead of panicking, and the guard derefs like any other.
    #[test]
    fn a_poisoned_lock_is_taken_anyway() {
        let m = Mutex::new(String::from("kept"));
        // Poison it the way production would: a panic while the guard is held.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut g = m.lock().unwrap();
            *g = String::from("mid-update");
            panic!("holder died");
        }));
        assert!(m.is_poisoned(), "the setup must actually poison it");
        assert_eq!(
            *lock_poisoned(&m),
            "mid-update",
            "and the data is still read"
        );
    }
}
