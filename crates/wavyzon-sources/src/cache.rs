//! A bounded, thread-safe LRU cache whose entries expire after a TTL.
//!
//! Port of Cadence's `internal/cache`. Recency is tracked with a monotonically
//! increasing tick per access, ordered in a `BTreeMap`, so `get`/`set` are
//! `O(log n)`.

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// A clock returning the current instant; injectable for tests.
pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

struct Entry<V> {
    value: V,
    expires: Instant,
    tick: u64,
}

struct Inner<K, V> {
    items: HashMap<K, Entry<V>>,
    order: BTreeMap<u64, K>,
    next_tick: u64,
}

/// Bounded TTL LRU. Cloning a value out on `get` keeps the lock short, so `V`
/// is usually cheap to clone (an `Arc`, a small struct).
pub struct Lru<K, V> {
    cap: usize,
    ttl: Duration,
    now: Clock,
    inner: Mutex<Inner<K, V>>,
}

impl<K: Eq + Hash + Clone, V: Clone> Lru<K, V> {
    /// A cache holding at most `capacity` entries (min 1) that expire `ttl`
    /// after they were last set.
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self::with_clock(capacity, ttl, Arc::new(Instant::now))
    }

    /// Like [`Lru::new`] with a custom clock.
    pub fn with_clock(capacity: usize, ttl: Duration, now: Clock) -> Self {
        Self {
            cap: capacity.max(1),
            ttl,
            now,
            inner: Mutex::new(Inner {
                items: HashMap::new(),
                order: BTreeMap::new(),
                next_tick: 0,
            }),
        }
    }

    /// The value for `key` if present and not expired; marks it recently used.
    pub fn get(&self, key: &K) -> Option<V> {
        let now = (self.now)();
        let mut g = self.inner.lock();
        let inner = &mut *g;
        let entry = inner.items.get_mut(key)?;
        if now > entry.expires {
            let tick = entry.tick;
            inner.items.remove(key);
            inner.order.remove(&tick);
            return None;
        }
        inner.order.remove(&entry.tick);
        entry.tick = inner.next_tick;
        inner.next_tick += 1;
        inner.order.insert(entry.tick, key.clone());
        Some(entry.value.clone())
    }

    /// Whether `key` is present and not expired, without touching recency.
    pub fn contains(&self, key: &K) -> bool {
        let now = (self.now)();
        self.inner.lock().items.get(key).is_some_and(|e| now <= e.expires)
    }

    /// Insert with the default TTL.
    pub fn set(&self, key: K, value: V) {
        self.set_for(key, value, self.ttl);
    }

    /// Insert with a specific TTL, evicting the least recently used entries
    /// past capacity.
    pub fn set_for(&self, key: K, value: V, ttl: Duration) {
        let expires = (self.now)() + ttl;
        let mut g = self.inner.lock();
        let inner = &mut *g;
        let tick = inner.next_tick;
        inner.next_tick += 1;
        if let Some(old) = inner.items.insert(key.clone(), Entry { value, expires, tick }) {
            inner.order.remove(&old.tick);
        }
        inner.order.insert(tick, key);
        while inner.items.len() > self.cap {
            let Some((_, oldest)) = inner.order.pop_first() else {
                break;
            };
            inner.items.remove(&oldest);
        }
    }

    /// Remove `key`.
    pub fn delete(&self, key: &K) {
        let mut g = self.inner.lock();
        if let Some(e) = g.items.remove(key) {
            g.order.remove(&e.tick);
        }
    }

    /// Number of entries, including expired ones not yet collected.
    pub fn len(&self) -> usize {
        self.inner.lock().items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn fake_clock() -> (Clock, Arc<AtomicU64>) {
        let base = Instant::now();
        let offset = Arc::new(AtomicU64::new(0));
        let o = offset.clone();
        (
            Arc::new(move || base + Duration::from_millis(o.load(Ordering::SeqCst))),
            offset,
        )
    }

    #[test]
    fn evicts_least_recently_used() {
        let c = Lru::new(2, Duration::from_secs(60));
        c.set("a", 1);
        c.set("b", 2);
        assert_eq!(c.get(&"a"), Some(1));
        c.set("c", 3);
        assert_eq!(c.get(&"b"), None, "b was least recently used");
        assert_eq!(c.get(&"a"), Some(1));
        assert_eq!(c.get(&"c"), Some(3));
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn entries_expire_and_set_for_overrides_ttl() {
        let (clock, t) = fake_clock();
        let c = Lru::with_clock(10, Duration::from_secs(10), clock);
        c.set("long", 1);
        c.set_for("short", 2, Duration::from_secs(1));
        t.store(2_000, Ordering::SeqCst);
        assert_eq!(c.get(&"short"), None);
        assert_eq!(c.get(&"long"), Some(1));
        t.store(11_000, Ordering::SeqCst);
        assert!(!c.contains(&"long"));
        assert_eq!(c.get(&"long"), None);
        assert!(c.is_empty());
    }

    #[test]
    fn overwrite_and_delete() {
        let c = Lru::new(2, Duration::from_secs(60));
        c.set("a", 1);
        c.set("a", 2);
        assert_eq!(c.len(), 1);
        assert_eq!(c.get(&"a"), Some(2));
        c.delete(&"a");
        assert_eq!(c.get(&"a"), None);
        let zero = Lru::new(0, Duration::from_secs(1));
        zero.set(1, 1);
        zero.set(2, 2);
        assert_eq!(zero.len(), 1, "capacity is at least one");
    }
}
