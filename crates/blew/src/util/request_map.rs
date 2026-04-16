use std::collections::HashMap;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

/// Thread-safe map for pending request/response pairs keyed by arbitrary
/// `Eq + Hash` values (device IDs, `(device_id, char_uuid)` tuples, etc.).
///
/// Useful for platform backends where responses arrive on separate callbacks
/// (CoreBluetooth delegate methods, bluer notification streams, Android JNI).
pub struct KeyedRequestMap<K: Eq + Hash, V> {
    inner: Mutex<HashMap<K, V>>,
}

impl<K: Eq + Hash, V> KeyedRequestMap<K, V> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Insert `value` at `key`, returning the previous value if any.
    pub fn insert(&self, key: K, value: V) -> Option<V> {
        self.inner.lock().insert(key, value)
    }

    /// Remove and return the value at `key`, if present.
    pub fn take(&self, key: &K) -> Option<V> {
        self.inner.lock().remove(key)
    }

    /// Drain all entries — useful for cleanup on disconnect.
    pub fn drain(&self) -> Vec<(K, V)> {
        self.inner.lock().drain().collect()
    }
}

impl<K: Eq + Hash, V> Default for KeyedRequestMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe map for pending request/response pairs, keyed by
/// auto-generated sequential `u64` IDs. Built on [`KeyedRequestMap`].
pub struct RequestMap<V> {
    inner: KeyedRequestMap<u64, V>,
    next_id: AtomicU64,
}

impl<V> RequestMap<V> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: KeyedRequestMap::new(),
            next_id: AtomicU64::new(0),
        }
    }

    /// Insert `value` and return the assigned request ID.
    pub fn insert(&self, value: V) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner.insert(id, value);
        id
    }

    /// Remove and return the value associated with `id`, if present.
    pub fn take(&self, id: u64) -> Option<V> {
        self.inner.take(&id)
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

    #[test]
    fn keyed_insert_and_take() {
        let map = KeyedRequestMap::<String, u32>::new();
        assert_eq!(map.insert("a".into(), 1), None);
        assert_eq!(map.take(&"a".into()), Some(1));
        assert_eq!(map.take(&"a".into()), None);
    }

    #[test]
    fn keyed_insert_replaces() {
        let map = KeyedRequestMap::<&'static str, u32>::new();
        assert_eq!(map.insert("a", 1), None);
        assert_eq!(map.insert("a", 2), Some(1));
        assert_eq!(map.take(&"a"), Some(2));
    }

    #[test]
    fn keyed_drain() {
        let map = KeyedRequestMap::<u32, &'static str>::new();
        map.insert(1, "one");
        map.insert(2, "two");
        let mut drained = map.drain();
        drained.sort_by_key(|(k, _)| *k);
        assert_eq!(drained, vec![(1, "one"), (2, "two")]);
        assert!(map.take(&1).is_none());
    }
}
