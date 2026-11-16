//! Bounded LRU cache of one connection's metadata: a limit on entries and a memory budget,
//! evicting the least recently used entry first.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use ninesql_driver_api::metadata::ObjectPath;

use super::model::is_under;
use super::{NodeChildren, NodeKey, ObjectDdl};

/// What an entry is keyed by.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum CacheKey {
    /// The children of one kind under a parent.
    Children(NodeKey),
    /// The DDL of an object.
    Ddl(ObjectPath),
}

impl CacheKey {
    /// `true` when the entry lies at or under `prefix`: children whose parent is under it, DDL
    /// of an object under it.
    pub(crate) fn is_under(&self, prefix: &ObjectPath) -> bool {
        match self {
            Self::Children(k) => is_under(&k.parent, prefix),
            Self::Ddl(p) => is_under(p, prefix),
        }
    }
}

/// A cached value.
#[derive(Debug, Clone)]
pub(crate) enum CacheValue {
    Children(Arc<NodeChildren>),
    Ddl(Arc<ObjectDdl>),
}

impl CacheValue {
    pub(crate) fn bytes(&self) -> usize {
        match self {
            Self::Children(c) => c.bytes,
            Self::Ddl(d) => d.bytes,
        }
    }
}

struct Slot {
    value: CacheValue,
    tick: u64,
}

/// The cache. Not thread-safe by itself: the service keeps it behind its lock.
pub(crate) struct LruCache {
    map: HashMap<CacheKey, Slot>,
    order: BTreeMap<u64, CacheKey>,
    bytes: usize,
    tick: u64,
    max_entries: usize,
    max_bytes: usize,
    evictions: u64,
    /// Bumped on every change, so readers can tell a snapshot is stale.
    generation: u64,
}

impl LruCache {
    pub(crate) fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: BTreeMap::new(),
            bytes: 0,
            tick: 0,
            max_entries: max_entries.max(1),
            max_bytes: max_bytes.max(1),
            evictions: 0,
            generation: 0,
        }
    }

    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// The value, marked as recently used.
    pub(crate) fn get(&mut self, key: &CacheKey) -> Option<CacheValue> {
        let tick = self.next_tick();
        let slot = self.map.get_mut(key)?;
        self.order.remove(&slot.tick);
        slot.tick = tick;
        self.order.insert(tick, key.clone());
        Some(slot.value.clone())
    }

    /// The value, without touching its recency.
    #[cfg(test)]
    pub(crate) fn peek(&self, key: &CacheKey) -> Option<&CacheValue> {
        self.map.get(key).map(|s| &s.value)
    }

    /// Inserts or replaces an entry, then evicts least recently used entries until both limits
    /// hold. An entry larger than the whole budget is not kept (returns `false`).
    pub(crate) fn insert(&mut self, key: CacheKey, value: CacheValue) -> bool {
        self.remove(&key);
        let size = value.bytes();
        if size > self.max_bytes {
            return false;
        }
        while !self.map.is_empty()
            && (self.map.len() >= self.max_entries || self.bytes + size > self.max_bytes)
        {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            if let Some(slot) = self.map.remove(&oldest) {
                self.bytes -= slot.value.bytes();
                self.evictions += 1;
            }
        }
        let tick = self.next_tick();
        self.order.insert(tick, key.clone());
        self.bytes += size;
        self.map.insert(key, Slot { value, tick });
        self.generation += 1;
        true
    }

    /// Removes one entry.
    pub(crate) fn remove(&mut self, key: &CacheKey) -> Option<CacheValue> {
        let slot = self.map.remove(key)?;
        self.order.remove(&slot.tick);
        self.bytes -= slot.value.bytes();
        self.generation += 1;
        Some(slot.value)
    }

    /// Removes every entry at or under `prefix`; returns how many.
    pub(crate) fn remove_under(&mut self, prefix: &ObjectPath) -> usize {
        self.remove_where(|k| k.is_under(prefix))
    }

    /// Removes every entry whose key matches; returns how many.
    pub(crate) fn remove_where(&mut self, pred: impl Fn(&CacheKey) -> bool) -> usize {
        let keys: Vec<CacheKey> = self.map.keys().filter(|k| pred(k)).cloned().collect();
        for k in &keys {
            self.remove(k);
        }
        keys.len()
    }

    /// Every value (for search and completion), in no particular order.
    pub(crate) fn values(&self) -> impl Iterator<Item = &CacheValue> {
        self.map.values().map(|s| &s.value)
    }

    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    pub(crate) fn evictions(&self) -> u64 {
        self.evictions
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use std::time::Instant;

    use ninesql_driver_api::ObjectKind;

    use super::*;

    fn node(name: &str, bytes: usize) -> (CacheKey, CacheValue) {
        let key = NodeKey::new(
            ObjectPath::root().child(ObjectKind::Schema, name),
            ObjectKind::Table,
        );
        let value = CacheValue::Children(Arc::new(NodeChildren {
            key: key.clone(),
            items: Vec::new(),
            complete: true,
            hidden_by_filter: 0,
            next_cursor: None,
            bytes,
            loaded_at: Instant::now(),
        }));
        (CacheKey::Children(key), value)
    }

    #[test]
    fn evicts_least_recently_used_by_count() {
        let mut c = LruCache::new(2, 1_000);
        let (a, va) = node("a", 10);
        let (b, vb) = node("b", 10);
        let (d, vd) = node("d", 10);
        assert!(c.insert(a.clone(), va));
        assert!(c.insert(b.clone(), vb));
        assert!(c.get(&a).is_some());
        assert!(c.insert(d.clone(), vd));
        assert!(c.peek(&a).is_some());
        assert!(c.peek(&b).is_none());
        assert!(c.peek(&d).is_some());
        assert_eq!(c.evictions(), 1);
        assert_eq!(c.len(), 2);
        assert_eq!(c.bytes(), 20);
    }

    #[test]
    fn evicts_by_bytes_and_refuses_oversized_entries() {
        let mut c = LruCache::new(100, 100);
        let (a, va) = node("a", 60);
        let (b, vb) = node("b", 60);
        let (huge, vh) = node("huge", 101);
        assert!(c.insert(a.clone(), va));
        assert!(c.insert(b.clone(), vb));
        assert!(c.peek(&a).is_none());
        assert_eq!(c.bytes(), 60);
        assert!(!c.insert(huge.clone(), vh));
        assert!(c.peek(&huge).is_none());
        assert!(c.peek(&b).is_some());
    }

    #[test]
    fn removes_a_subtree() {
        let mut c = LruCache::new(100, 10_000);
        let (a, va) = node("a", 10);
        let (b, vb) = node("b", 10);
        c.insert(a.clone(), va);
        c.insert(b.clone(), vb);
        let g = c.generation();
        assert_eq!(
            c.remove_under(&ObjectPath::root().child(ObjectKind::Schema, "a")),
            1
        );
        assert!(c.generation() > g);
        assert!(c.peek(&b).is_some());
        assert_eq!(c.remove_under(&ObjectPath::root()), 1);
        assert_eq!(c.len(), 0);
        assert_eq!(c.bytes(), 0);
    }
}
