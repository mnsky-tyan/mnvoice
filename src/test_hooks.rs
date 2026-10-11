//! Test-only observation points.
//!
//! These exist so a test can assert on a decision the code made at runtime
//! instead of inferring it from a side effect. They are compiled out of every
//! release build, so they can never change behaviour a user sees.

use std::cell::Cell;

thread_local! {
    /// Set when the post-session drain on *this thread* breaks out on the first
    /// failed send rather than continuing through the backlog.
    ///
    /// The drain's early exit is a correctness rule with a performance motive:
    /// once a send fails the socket can carry no more data, so the packets
    /// behind it cannot be delivered. On a peer whose every send fails instantly
    /// the exit costs nothing measurable, so a test that only watched the clock
    /// would pass either way - this lets the test ask the drain what it did.
    ///
    /// Thread-local on purpose, not a process-wide static. `cargo test` runs the
    /// live-provider tests on parallel threads and several siblings reach the
    /// same break arm through their own failing peer, so a shared flag let one
    /// test read a `true` another test stored and pass without its own drain
    /// ever breaking - green while covering nothing. `run_stream` runs to
    /// completion on the calling thread, so the write and the read are the same
    /// thread and no other test can contribute to it.
    static DRAIN_BROKE_EARLY: Cell<bool> = const { Cell::new(false) };
}

/// Clear this thread's observation. Called by the driver before it starts a
/// session, so an earlier session on the same thread cannot satisfy the assert.
pub fn reset_drain_broke_early() {
    DRAIN_BROKE_EARLY.with(|flag| flag.set(false));
}

/// Record that this thread's drain broke at a failed send.
pub fn record_drain_broke_early() {
    DRAIN_BROKE_EARLY.with(|flag| flag.set(true));
}

/// Whether this thread's drain stopped at a failed send, resetting the flag.
pub fn drain_broke_early() -> bool {
    DRAIN_BROKE_EARLY.with(|flag| flag.replace(false))
}
