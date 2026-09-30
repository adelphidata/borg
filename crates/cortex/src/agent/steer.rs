//! When a steer can still reach the turn it was typed into.
//!
//! A steer sits in the agent's `steer_rx` until a tool boundary or the
//! end-of-turn check drains it. Once that last check has run, nothing reads
//! the channel until some later turn, so a steer queued then was stranded: the
//! RPC said OK, the text left the composer, and the agent never saw it.
//!
//! The gate makes "is anything waiting?" and "stop taking steers" one step.
//! A sender takes the lock to queue, and the agent takes it to close only
//! when the channel is empty. A steer either lands before the close, and the
//! turn goes on to answer it, or is refused, and the caller still has the
//! text to send as a turn of its own.

use std::sync::{Mutex, MutexGuard, PoisonError};

use tokio::sync::mpsc;

/// Why a steer was not queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SteerRefused {
    /// No turn is running, or the running one has made its last check.
    #[error("no turn is running to take this message")]
    TurnEnded,
    /// The agent that would read the channel is gone.
    #[error("the session is closed")]
    Closed,
}

/// Opens with each turn and closes at its last steer check.
#[derive(Debug, Default)]
pub struct SteerGate {
    open: Mutex<bool>,
}

impl SteerGate {
    /// A closed gate: no turn is running yet.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, bool> {
        // The guarded value is a plain flag, so a panic mid-update cannot
        // leave it half-written.
        self.open.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queue `text` for the running turn, or refuse it when no turn will
    /// read it.
    pub fn send(
        &self,
        tx: &mpsc::UnboundedSender<String>,
        text: String,
    ) -> Result<(), SteerRefused> {
        let open = self.lock();
        if !*open {
            return Err(SteerRefused::TurnEnded);
        }
        tx.send(text).map_err(|_| SteerRefused::Closed)
    }

    /// A turn has started and will drain the channel.
    pub(crate) fn open(&self) {
        *self.lock() = true;
    }

    /// Stop taking steers.
    pub(crate) fn close(&self) {
        *self.lock() = false;
    }

    /// Close unless a steer is already waiting in `rx`. Returns whether it
    /// closed; `false` means the caller must drain and keep the turn going.
    pub(crate) fn close_unless_pending(
        &self,
        rx: Option<&mpsc::UnboundedReceiver<String>>,
    ) -> bool {
        let mut open = self.lock();
        if rx.is_some_and(|rx| !rx.is_empty()) {
            return false;
        }
        *open = false;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closed_gate_refuses_and_an_open_one_queues() {
        let gate = SteerGate::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert_eq!(gate.send(&tx, "early".into()), Err(SteerRefused::TurnEnded));

        gate.open();
        gate.send(&tx, "mid-turn".into()).unwrap();
        assert_eq!(rx.try_recv().ok().as_deref(), Some("mid-turn"));
    }

    /// Regression: a steer that arrives after the last check must not be
    /// taken, or it waits in the channel with no turn to read it.
    #[test]
    fn the_last_check_either_finds_a_steer_or_refuses_later_ones() {
        let gate = SteerGate::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        gate.open();
        gate.send(&tx, "redirect".into()).unwrap();
        // Something is waiting, so the turn must go on and stay open.
        assert!(!gate.close_unless_pending(Some(&rx)));
        assert_eq!(rx.try_recv().ok().as_deref(), Some("redirect"));
        gate.send(&tx, "still open".into()).unwrap();
        rx.try_recv().unwrap();

        // Nothing waiting: the turn ends, and a later steer is refused
        // rather than stranded.
        assert!(gate.close_unless_pending(Some(&rx)));
        assert_eq!(
            gate.send(&tx, "too late".into()),
            Err(SteerRefused::TurnEnded)
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_dropped_agent_reads_as_closed() {
        let gate = SteerGate::new();
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        gate.open();
        drop(rx);
        assert_eq!(gate.send(&tx, "x".into()), Err(SteerRefused::Closed));
        gate.close();
        assert_eq!(gate.send(&tx, "x".into()), Err(SteerRefused::TurnEnded));
    }
}
