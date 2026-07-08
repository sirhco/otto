//! App-wide pub/sub event bus.
//!
//! The Rust analog of opencode's Node `EventEmitter` app bus. A thin wrapper
//! over [`tokio::sync::broadcast`] that fans one published event out to every
//! live subscriber. Events are wrapped in [`Arc`] so delivery to many
//! subscribers stays cheap regardless of payload size.

use std::sync::Arc;

use tokio::sync::broadcast;

/// Default channel capacity — the number of buffered events a lagging
/// subscriber may fall behind before it starts receiving [`broadcast::error::RecvError::Lagged`].
pub const DEFAULT_CAPACITY: usize = 256;

/// A cloneable, multi-producer / multi-consumer event bus.
///
/// `T` is the app event type (e.g. [`crate::LLMEvent`]). Cloning the bus
/// clones the underlying sender, so every clone publishes into the same
/// channel.
#[derive(Debug, Clone)]
pub struct EventBus<T> {
    sender: broadcast::Sender<Arc<T>>,
}

impl<T> EventBus<T> {
    /// Create a bus with [`DEFAULT_CAPACITY`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Create a bus with an explicit channel capacity.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let (sender, _receiver) = broadcast::channel(capacity);
        Self { sender }
    }

    /// Publish an event to all current subscribers.
    ///
    /// Returns the number of subscribers the event was delivered to. Returns
    /// `0` when there are no live subscribers (mirroring an emit into the void
    /// on a Node `EventEmitter`).
    pub fn publish(&self, event: T) -> usize {
        self.sender.send(Arc::new(event)).unwrap_or(0)
    }

    /// Subscribe to future events. Each subscriber receives every event
    /// published after it subscribed.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<T>> {
        self.sender.subscribe()
    }

    /// Current number of live subscribers.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

impl<T> Default for EventBus<T> {
    fn default() -> Self {
        Self::new()
    }
}
