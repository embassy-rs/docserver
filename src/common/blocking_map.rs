use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::Mutex;
use std::thread::{self, Thread};

/// A fixed-size synchronized map with blocking, wait-free semantics.
///
/// * `remove` waits until a key exists, then removes and returns it.
/// * `insert` waits until its target slot is free **and** the key is not
///   present anywhere in the map, then inserts.
///
/// The structure is `Sync` when `K: Send` and `V: Send`.
pub struct BlockingSlotMap<K, V> {
    inner: Mutex<Inner<K, V>>,
}

struct Slot<V> {
    value: Option<V>,
    /// Threads waiting for this specific slot to become free.
    waiters: VecDeque<Thread>,
}

struct Inner<K, V> {
    slots: Vec<Slot<V>>,
    /// key → slot index
    key_to_slot: HashMap<K, usize>,
    /// Threads waiting for a specific key to appear (blocked in `remove`).
    remove_waiters: HashMap<K, VecDeque<Thread>>,
    /// Threads waiting for a specific key to be absent (blocked in `insert`).
    key_waiters: HashMap<K, VecDeque<Thread>>,
}

impl<K, V> BlockingSlotMap<K, V> {
    /// Creates a new map with `num_slots` slots (indexed `0 .. num_slots-1`).
    ///
    /// # Panics
    /// Panics if `num_slots` is zero.
    pub fn new(num_slots: usize) -> Self {
        assert!(num_slots > 0, "num_slots must be > 0");
        let mut slots = Vec::with_capacity(num_slots);
        for _ in 0..num_slots {
            slots.push(Slot {
                value: None,
                waiters: VecDeque::new(),
            });
        }
        Self {
            inner: Mutex::new(Inner {
                slots,
                key_to_slot: HashMap::new(),
                remove_waiters: HashMap::new(),
                key_waiters: HashMap::new(),
            }),
        }
    }
}

impl<K: Hash + Eq + Clone + Send, V: Send> BlockingSlotMap<K, V> {
    /// Inserts `value` at `slot` with `key`.
    ///
    /// Blocks until:
    /// 1. `slot` is free, **and**
    /// 2. `key` is not present in any slot.
    ///
    /// After a successful insert, any thread blocked in `remove` waiting for
    /// `key` is woken.
    ///
    /// # Panics
    /// Panics if `slot` is out of bounds.
    pub fn insert(&self, slot: usize, key: K, value: V) {
        let mut inner = self.inner.lock().unwrap();
        loop {
            assert!(slot < inner.slots.len(), "slot index out of bounds");

            let slot_free = inner.slots[slot].value.is_none();
            let key_absent = !inner.key_to_slot.contains_key(&key);

            // Fast path: both conditions satisfied.
            if slot_free && key_absent {
                inner.slots[slot].value = Some(value);
                inner.key_to_slot.insert(key.clone(), slot);

                // Hand-off to one remover waiting for this key.
                if let Some(waiters) = inner.remove_waiters.get_mut(&key)
                    && let Some(t) = waiters.pop_front()
                {
                    if waiters.is_empty() {
                        inner.remove_waiters.remove(&key);
                    }
                    drop(inner); // unlock before unpark
                    t.unpark();
                }
                return;
            }

            // Slow path: register in every queue whose condition we still need.
            if !slot_free {
                inner.slots[slot].waiters.push_back(thread::current());
            }
            if !key_absent {
                inner
                    .key_waiters
                    .entry(key.clone())
                    .or_default()
                    .push_back(thread::current());
            }

            drop(inner);
            thread::park();

            // Woken up: remove ourselves from queues and re-evaluate.
            inner = self.inner.lock().unwrap();
            remove_current_thread(&mut inner.slots[slot].waiters);
            if let Some(waiters) = inner.key_waiters.get_mut(&key) {
                remove_current_thread(waiters);
                if waiters.is_empty() {
                    inner.key_waiters.remove(&key);
                }
            }
        }
    }

    /// Removes and returns the value for `key`.
    ///
    /// If `key` is not present, blocks until another thread inserts it,
    /// then removes it and returns.
    pub fn remove(&self, key: &K) -> V {
        let mut inner = self.inner.lock().unwrap();
        loop {
            // Fast path: key exists.
            if let Some(&slot) = inner.key_to_slot.get(key) {
                let value = inner.slots[slot].value.take().unwrap();
                inner.key_to_slot.remove(key);

                // Collect every thread that might now be able to proceed.
                let slot_waiters: Vec<Thread> = inner.slots[slot].waiters.drain(..).collect();
                let key_waiters: Vec<Thread> = inner
                    .key_waiters
                    .remove(key)
                    .map(|q| q.into_iter().collect())
                    .unwrap_or_default();

                drop(inner); // unlock before waking

                for t in slot_waiters {
                    t.unpark();
                }
                for t in key_waiters {
                    t.unpark();
                }
                return value;
            }

            // Slow path: key absent → park and retry.
            inner
                .remove_waiters
                .entry(key.clone())
                .or_default()
                .push_back(thread::current());

            drop(inner);
            thread::park();

            inner = self.inner.lock().unwrap();
            if let Some(waiters) = inner.remove_waiters.get_mut(key) {
                remove_current_thread(waiters);
                if waiters.is_empty() {
                    inner.remove_waiters.remove(key);
                }
            }
        }
    }
}

/// O(n) in the number of waiters for that queue (n is tiny because N is small).
/// Optimised for the common FIFO case.
fn remove_current_thread(queue: &mut VecDeque<Thread>) {
    let id = thread::current().id();
    if queue.front().map(|t| t.id()) == Some(id) {
        queue.pop_front();
    } else if let Some(pos) = queue.iter().position(|t| t.id() == id) {
        queue.remove(pos);
    }
}
