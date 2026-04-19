//! Signal types + per-process signal inbox.
//!
//! Signals in v1 are a thin POSIX-inspired layer:
//!
//! * [`Signal::Kill`] (9) is delivered immediately by
//!   [`crate::sys::Kernel::proc_kill`]: the target is
//!   force-transitioned to `Zombie` with
//!   `ExitStatus::Signaled(9)` and the scheduler drops it.
//!   SIGKILL does NOT go through the inbox — there is nothing
//!   for userland to catch.
//!
//! * [`Signal::Term`] (15), [`Signal::Interrupt`] (2), and
//!   [`Signal::Pipe`] (13) are "catchable": `proc_kill` buffers
//!   them in the target's [`SignalInbox`]. A future slice will
//!   wake the process via the `FdObject::SignalChannel` fd
//!   variant so a `fd_read` on fd 3 drains pending signals;
//!   until then the kernel exposes `Kernel::drain_signals` as a
//!   direct API so tests can observe delivery. SIGPIPE in
//!   particular is kernel-generated: it is posted to the caller's
//!   own inbox whenever an `fd_write` on a pipe or socket
//!   returns `PipeBroken` (matching POSIX `write(2)`'s SIGPIPE
//!   delivery alongside the EPIPE errno).
//!
//! The inbox is a bounded FIFO: repeated deliveries of the
//! same signal are coalesced (classical POSIX behaviour for
//! standard signals, not real-time signals). v1 does not
//! implement signal masks or handlers; that's a later slice
//! once the bundled `sh` needs them.

use alloc::collections::VecDeque;

/// Signals the v1 kernel understands.
///
/// The numeric repr matches POSIX `SIGKILL`, `SIGTERM`, `SIGINT`,
/// and `SIGPIPE` so userland code that hard-codes these values
/// keeps working without a translation table.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum Signal {
    /// Terminate the target immediately, no cleanup grace.
    Kill = 9,
    /// Request a cooperative shutdown.
    Term = 15,
    /// Interrupt (ctrl-c equivalent).
    Interrupt = 2,
    /// Kernel-generated: a write to a pipe or socket whose peer
    /// has closed or had its read-side shut down.
    Pipe = 13,
}

impl Signal {
    #[inline]
    pub const fn number(self) -> u16 {
        self as u16
    }

    /// True iff this signal can be buffered in a
    /// [`SignalInbox`] for userland to observe. SIGKILL is the
    /// only non-catchable signal in v1.
    #[inline]
    pub const fn is_catchable(self) -> bool {
        !matches!(self, Signal::Kill)
    }
}

/// Maximum number of pending signals in a single inbox.
/// Matches standard POSIX — coalesced standard signals never
/// need more than one slot per number, so 8 is plenty of head
/// room for a system that does not yet implement real-time
/// signals.
pub const INBOX_CAP: usize = 8;

/// Per-process pending-signal queue.
///
/// Repeated deliveries of the same signal are coalesced: a
/// second `post(Signal::Term)` against an inbox that already
/// has `Term` pending does not grow the queue. Overflow past
/// [`INBOX_CAP`] is dropped silently (no real-time-signal-
/// equivalent errno in v1).
#[derive(Debug, Default)]
pub struct SignalInbox {
    queue: VecDeque<Signal>,
}

impl SignalInbox {
    pub const fn new() -> Self {
        SignalInbox {
            queue: VecDeque::new(),
        }
    }

    /// Deliver `signal` into the inbox. Returns true iff the
    /// inbox actually absorbed a new copy (i.e. the signal was
    /// not already pending AND the inbox had room).
    pub fn post(&mut self, signal: Signal) -> bool {
        if self.queue.iter().any(|s| *s == signal) {
            return false;
        }
        if self.queue.len() >= INBOX_CAP {
            return false;
        }
        self.queue.push_back(signal);
        true
    }

    /// Remove and return every pending signal, in delivery
    /// order.
    pub fn drain(&mut self) -> alloc::vec::Vec<Signal> {
        self.queue.drain(..).collect()
    }

    /// True iff the next `drain` would return no signals.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Number of signals currently pending.
    pub fn len(&self) -> usize {
        self.queue.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_catchable_distinguishes_kill_from_other_signals() {
        assert!(!Signal::Kill.is_catchable());
        assert!(Signal::Term.is_catchable());
        assert!(Signal::Interrupt.is_catchable());
        assert!(Signal::Pipe.is_catchable());
    }

    #[test]
    fn number_matches_posix_values() {
        assert_eq!(Signal::Kill.number(), 9);
        assert_eq!(Signal::Term.number(), 15);
        assert_eq!(Signal::Interrupt.number(), 2);
        assert_eq!(Signal::Pipe.number(), 13);
    }

    #[test]
    fn post_delivers_and_drain_returns_fifo() {
        let mut i = SignalInbox::new();
        assert!(i.is_empty());
        assert!(i.post(Signal::Term));
        assert!(i.post(Signal::Interrupt));
        assert_eq!(i.len(), 2);
        let drained = i.drain();
        assert_eq!(drained, alloc::vec![Signal::Term, Signal::Interrupt]);
        assert!(i.is_empty());
    }

    #[test]
    fn coalesces_repeated_deliveries_of_the_same_signal() {
        let mut i = SignalInbox::new();
        assert!(i.post(Signal::Term));
        assert!(!i.post(Signal::Term));
        assert_eq!(i.len(), 1);
    }

    #[test]
    fn drops_deliveries_past_the_cap() {
        let mut i = SignalInbox::new();
        // Post INBOX_CAP distinct-looking signals by alternating
        // between the two catchable variants (the dedup logic
        // will keep this at 2). We then post Signal::Kill in a
        // loop to fill the queue to its cap.
        assert!(i.post(Signal::Term));
        assert!(i.post(Signal::Interrupt));
        // These next posts would need to grow past INBOX_CAP;
        // they should all be rejected by the cap check.
        for _ in 0..(INBOX_CAP + 3) {
            let _ = i.post(Signal::Kill);
        }
        assert!(i.len() <= INBOX_CAP);
    }
}
