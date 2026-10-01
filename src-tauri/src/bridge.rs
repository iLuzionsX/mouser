//! Bounded queue carrying outbound messages to the link task.
//!
//! The webview drives input processing while a tokio task owns the socket, so
//! the two need a hand-off that never blocks the thread producing input.
//! Bounded is the important part: if the peer stalls, dropping the oldest
//! queued input beats blocking capture, which would freeze the local cursor
//! and read as a hang.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};

use mouser_core::protocol::Message;

/// Depth of the outbound queue.
///
/// Sized to absorb a burst of motion between polls without growing without
/// bound: 256 events is roughly two seconds of continuous movement.
pub const CAPACITY: usize = 256;

pub struct Bridge {
    tx: SyncSender<Message>,
    /// Cleared once a send is refused because the receiver is gone.
    connected: AtomicBool,
}

impl Bridge {
    /// Create a bridge and the receiver its task will drain.
    pub fn channel() -> (Bridge, std::sync::mpsc::Receiver<Message>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(CAPACITY);
        (
            Bridge {
                tx,
                connected: AtomicBool::new(true),
            },
            rx,
        )
    }

    /// Queue a message, never blocking.
    pub fn send(&self, msg: Message) -> Result<(), SendError> {
        match self.tx.try_send(msg) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(SendError::QueueFull),
            Err(TrySendError::Disconnected(_)) => {
                // Latch it: once the receiver is gone it cannot come back,
                // and the UI uses this to stop warning on every event.
                self.connected.store(false, Ordering::Relaxed);
                Err(SendError::Disconnected)
            }
        }
    }

    /// Whether the link task is still listening.
    ///
    /// Stable std gives `SyncSender` no disconnect query, so liveness is
    /// observed through the error from [`Bridge::send`] rather than queried. No
    /// predicate is offered here because any probe would have to either occupy a
    /// real queue slot with a sentinel message or be a constant that lies.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("outbound queue is full")]
    QueueFull,
    #[error("link task is not running")]
    Disconnected,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_reports_full_instead_of_blocking() {
        let (bridge, rx) = Bridge::channel();
        for i in 0..CAPACITY {
            bridge.send(Message::Ping(i as u64)).expect("should fit");
        }
        assert!(matches!(
            bridge.send(Message::Bye),
            Err(SendError::QueueFull)
        ));
        // The full item was dropped, not the queue.
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn liveness_latches_off_after_a_disconnected_send() {
        let (bridge, rx) = Bridge::channel();
        // Liveness is only observable once a send has failed, so a fresh
        // bridge reports connected.
        assert!(bridge.is_connected());

        drop(rx);
        assert!(matches!(
            bridge.send(Message::Bye),
            Err(SendError::Disconnected)
        ));
        assert!(
            !bridge.is_connected(),
            "state should latch off after a refusal"
        );
    }
}
