//! Internal module providing a concurrent set that never grows past a
//! configured capacity.

use std::hash;

/// A concurrent set that forgets everything it knows once it grows past
/// `capacity`.
///
/// This is used to remember which track descriptors have already been written.
/// Emitting a descriptor twice is harmless — Perfetto keys them by uuid — so
/// trading exactness for a memory bound is a good deal, and without a bound
/// the bookkeeping for short-lived tracks (most obviously Tokio tasks, of
/// which a long-lived process spawns an unlimited number) grows forever.
pub struct BoundedSet<T>
where
    T: Eq + hash::Hash,
{
    entries: dashmap::DashSet<T>,
    capacity: usize,
}

impl<T> BoundedSet<T>
where
    T: Eq + hash::Hash,
{
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: dashmap::DashSet::new(),
            capacity,
        }
    }

    /// Records `value` as seen, returning whether it was newly inserted.
    ///
    /// Callers may see `true` for a value that was inserted earlier, if the
    /// set was cleared in between to stay within capacity.
    pub fn insert(&self, value: T) -> bool {
        if self.entries.len() >= self.capacity {
            self.entries.clear();
        }
        self.entries.insert(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_first_insert_only() {
        let set = BoundedSet::new(4);
        assert!(set.insert(1));
        assert!(!set.insert(1));
    }

    #[test]
    fn stays_within_capacity() {
        let set = BoundedSet::new(4);
        for value in 0..1_000 {
            set.insert(value);
        }
        assert!(set.entries.len() <= 4);
    }
}
