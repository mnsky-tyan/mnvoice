//! Test-only observation points.
//!
//! These exist so a test can assert on a decision the code made at runtime
//! instead of inferring it from a side effect. They are compiled out of every
//! release build, so they can never change behaviour a user sees.

use std::cell::Cell;

thread_local! {
    /// This thread's drain stopped at a failed send rather than continuing
    /// through the backlog.
    ///
    /// The drain's early exit is a correctness rule with a performance motive:
    /// once a send fails the socket can carry no more data, so the packets
    /// behind it cannot be delivered. On a peer whose every send fails
    /// instantly the exit costs nothing measurable, so a test that only
    /// watched the clock would pass either way - this lets the test ask the
    /// drain what it actually did.
    ///
    /// Per-thread, not process-wide: `cargo test` runs the live-provider
    /// tests on parallel threads and several of them reach this same drain
    /// through their own failing peer, so a shared flag let one test read a
    /// value another test stored.
    static DRAIN_BROKE_EARLY: Cell<bool> = const { Cell::new(false) };

    /// This thread's drain attempted every queued packet, so the backlog ran
    /// out rather than being abandoned.
    ///
    /// The other half of the same decision. A flag that only records the
    /// failed send is satisfied whether or not the drain then kept walking,
    /// so deleting the `break` on its own would leave the test green; this
    /// read false is what pins the break.
    static DRAIN_BACKLOG_EXHAUSTED: Cell<bool> = const { Cell::new(false) };
}

/// Clear both observations, for a test about to drive a session of its own.
pub fn reset_drain_observation() {
    DRAIN_BROKE_EARLY.with(|flag| flag.set(false));
    DRAIN_BACKLOG_EXHAUSTED.with(|flag| flag.set(false));
}

/// Record that this thread's drain stopped at a failed send.
pub fn record_drain_broke_early() {
    DRAIN_BROKE_EARLY.with(|flag| flag.set(true));
}

/// Record that this thread's drain attempted every queued packet.
pub fn record_drain_backlog_exhausted() {
    DRAIN_BACKLOG_EXHAUSTED.with(|flag| flag.set(true));
}

/// True if this thread's drain stopped at a failed send, resetting the flag.
pub fn drain_broke_early() -> bool {
    DRAIN_BROKE_EARLY.with(|flag| flag.replace(false))
}

/// True if this thread's drain ran the backlog to its end, resetting the flag.
pub fn drain_backlog_exhausted() -> bool {
    DRAIN_BACKLOG_EXHAUSTED.with(|flag| flag.replace(false))
}
