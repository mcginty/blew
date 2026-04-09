use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Fan-out from a single event source to multiple per-subscriber bounded channels.
///
/// Slow subscribers are dropped without affecting others.
pub struct EventFanout<E> {
    subscribers: Arc<Mutex<Vec<mpsc::Sender<E>>>>,
}

impl<E: Clone + Send + 'static> EventFanout<E> {
    /// Create a `(sender, fanout)` pair.
    #[must_use]
    pub fn new(_capacity: usize) -> (EventFanoutTx<E>, Self) {
        let subscribers: Arc<Mutex<Vec<mpsc::Sender<E>>>> = Default::default();
        let tx = EventFanoutTx {
            subscribers: Arc::clone(&subscribers),
        };
        let fanout = Self { subscribers };
        (tx, fanout)
    }

    /// Add a subscriber and return its receiver.
    #[must_use]
    pub fn subscribe(&self, capacity: usize) -> mpsc::Receiver<E> {
        let (tx, rx) = mpsc::channel(capacity);
        self.subscribers.lock().unwrap().push(tx);
        rx
    }
}

/// Sender side of an [`EventFanout`].
pub struct EventFanoutTx<E> {
    subscribers: Arc<Mutex<Vec<mpsc::Sender<E>>>>,
}

impl<E: Clone + Send + 'static> EventFanoutTx<E> {
    /// Broadcast `event` to all subscribers. Full or closed subscribers are removed.
    #[allow(clippy::needless_pass_by_value)]
    pub fn send(&self, event: E) {
        let mut subs = self.subscribers.lock().unwrap();
        subs.retain(|tx| tx.try_send(event.clone()).is_ok());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_subscriber() {
        let (tx, fanout) = EventFanout::<String>::new(16);
        let mut rx = fanout.subscribe(16);
        tx.send("hello".to_string());
        assert_eq!(rx.try_recv().ok(), Some("hello".to_string()));
    }

    #[test]
    fn multiple_subscribers() {
        let (tx, fanout) = EventFanout::<i32>::new(16);
        let mut rx1 = fanout.subscribe(16);
        let mut rx2 = fanout.subscribe(16);
        tx.send(42);
        assert_eq!(rx1.try_recv().ok(), Some(42));
        assert_eq!(rx2.try_recv().ok(), Some(42));
    }

    #[test]
    fn slow_subscriber_dropped() {
        let (tx, fanout) = EventFanout::<i32>::new(16);
        let _rx1 = fanout.subscribe(1); // capacity 1: will overflow on second send
        let mut rx2 = fanout.subscribe(16);
        tx.send(1);
        tx.send(2);
        assert_eq!(rx2.try_recv().ok(), Some(1));
        assert_eq!(rx2.try_recv().ok(), Some(2));
    }
}
