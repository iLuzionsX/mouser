//! One bounded queue from input capture to the current connection.

use mouser_core::protocol::Message;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc::{self, error::TrySendError};

pub const CAPACITY: usize = 256;

pub struct Bridge {
    tx: mpsc::Sender<Message>,
    failed: AtomicBool,
}

impl Bridge {
    pub fn channel() -> (Self, mpsc::Receiver<Message>) {
        let (tx, rx) = mpsc::channel(CAPACITY);
        (
            Self {
                tx,
                failed: AtomicBool::new(false),
            },
            rx,
        )
    }

    /// Never block capture. A lost event invalidates the session: it may be a key-up.
    pub fn send(&self, msg: Message) -> Result<(), SendError> {
        if !self.is_connected() {
            return Err(SendError::Disconnected);
        }
        self.tx.try_send(msg).map_err(|e| {
            self.failed.store(true, Ordering::Relaxed);
            match e {
                TrySendError::Full(_) => SendError::QueueFull,
                TrySendError::Closed(_) => SendError::Disconnected,
            }
        })
    }

    pub fn is_connected(&self) -> bool {
        !self.failed.load(Ordering::Relaxed) && !self.tx.is_closed()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("outbound queue is full; reconnecting")]
    QueueFull,
    #[error("link task is not running")]
    Disconnected,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_reports_full_instead_of_blocking() {
        let (bridge, mut rx) = Bridge::channel();
        for i in 0..CAPACITY {
            bridge.send(Message::Ping(i as u64)).expect("should fit");
        }
        assert!(matches!(
            bridge.send(Message::Bye),
            Err(SendError::QueueFull)
        ));
        assert!(!bridge.is_connected(), "overflow must end the session");
        // Already queued input is still available until the receiver closes.
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn liveness_latches_off_after_a_disconnected_send() {
        let (bridge, rx) = Bridge::channel();
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
