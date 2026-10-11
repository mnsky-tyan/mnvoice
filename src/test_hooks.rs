//! Test-only observation points.
//!
//! These exist so a test can assert on a decision the code made at runtime
//! instead of inferring it from a side effect. They are compiled out of every
//! release build, so they can never change behaviour a user sees.

use std::sync::atomic::{AtomicBool, Ordering};

/// Set when the post-session drain breaks out on the first failed send rather
/// than continuing through the backlog.
///
/// The drain's early exit is a correctness rule with a performance motive: once
/// a send fails the socket can carry no more data, so the packets behind it
/// cannot be delivered. On a peer whose every send fails instantly the exit
/// costs nothing measurable, so a test that only watched the clock would pass
/// either way - this lets the test ask the drain what it actually did.
pub static DRAIN_BROKE_EARLY: AtomicBool = AtomicBool::new(false);

/// True if the drain stopped at a failed send, resetting the flag first.
pub fn drain_broke_early() -> bool {
    DRAIN_BROKE_EARLY.swap(false, Ordering::SeqCst)
}
