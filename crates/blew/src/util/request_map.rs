use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

/// Thread-safe map for pending request/response pairs, keyed by auto-generated `u64` IDs.
///
/// Useful for platform backends where responses arrive in separate callbacks
/// (e.g. Android JNI).
pub struct RequestMap<V> {
    inner: Mutex<HashMap<u64, V>>,
    next_id: AtomicU64,
}

impl<V> RequestMap<V> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
        }
    }

    /// Insert `value` and return the assigned request ID.
    pub fn insert(&self, value: V) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner.lock().insert(id, value);
        id
    }

    /// Remove and return the value associated with `id`, if present.
    pub fn take(&self, id: u64) -> Option<V> {
        self.inner.lock().remove(&id)
    }
}

impl<V> Default for RequestMap<V> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_take() {
        let map = RequestMap::<String>::new();
        let id = map.insert("hello".to_string());
        assert_eq!(map.take(id), Some("hello".to_string()));
        assert_eq!(map.take(id), None);
    }

    #[test]
    fn ids_are_sequential() {
        let map = RequestMap::<u32>::new();
        let id0 = map.insert(0);
        let id1 = map.insert(1);
        let id2 = map.insert(2);
        assert_eq!(id0, 0);
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
    }

    #[test]
    fn take_nonexistent() {
        let map = RequestMap::<u32>::new();
        assert_eq!(map.take(999), None);
    }
}
