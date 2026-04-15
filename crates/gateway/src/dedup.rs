use std::collections::{HashSet, VecDeque};
use std::hash::Hash;

/// Generic bounded deduplicator using a HashSet for O(1) lookups
/// with a VecDeque for LRU eviction order.
pub struct BoundedDedup<K: Hash + Eq + Clone> {
    order: VecDeque<K>,
    set: HashSet<K>,
    capacity: usize,
}

impl<K: Hash + Eq + Clone> BoundedDedup<K> {
    pub fn new(capacity: usize) -> Self {
        Self {
            order: VecDeque::with_capacity(capacity),
            set: HashSet::with_capacity(capacity),
            capacity,
        }
    }

    /// Returns `true` if this key has been seen before.
    /// If new, inserts it and evicts the oldest entry if at capacity.
    pub fn is_duplicate(&mut self, key: &K) -> bool {
        if self.set.contains(key) {
            return true;
        }

        if self.order.len() >= self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.set.remove(&evicted);
            }
        }
        self.set.insert(key.clone());
        self.order.push_back(key.clone());
        false
    }
}

/// Generate a typed deduplicator wrapper around `BoundedDedup`.
///
/// Usage:
/// ```ignore
/// crate::dedup_wrapper!(
///     /// Doc comment for the wrapper.
///     pub struct MyDedup(KeyType, CAPACITY_CONST);
///     is_duplicate(arg_name: ArgType) => convert_expr;
/// );
/// ```
#[macro_export]
macro_rules! dedup_wrapper {
    (
        $(#[$meta:meta])*
        $vis:vis struct $Name:ident($KeyType:ty, $capacity:expr);
        is_duplicate($arg:ident : $ArgType:ty) => $convert:expr;
    ) => {
        $(#[$meta])*
        $vis struct $Name($crate::dedup::BoundedDedup<$KeyType>);

        impl $Name {
            pub fn new() -> Self {
                Self($crate::dedup::BoundedDedup::new($capacity))
            }

            #[cfg(test)]
            #[allow(dead_code)]
            pub(crate) fn with_capacity(capacity: usize) -> Self {
                Self($crate::dedup::BoundedDedup::new(capacity))
            }

            pub fn is_duplicate(&mut self, $arg: $ArgType) -> bool {
                self.0.is_duplicate(&$convert)
            }
        }

        impl Default for $Name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_seen_not_duplicate() {
        let mut dedup = BoundedDedup::new(10);
        assert!(!dedup.is_duplicate(&1));
    }

    #[test]
    fn second_seen_is_duplicate() {
        let mut dedup = BoundedDedup::new(10);
        assert!(!dedup.is_duplicate(&1));
        assert!(dedup.is_duplicate(&1));
    }

    #[test]
    fn different_keys_not_duplicate() {
        let mut dedup = BoundedDedup::new(10);
        assert!(!dedup.is_duplicate(&"a".to_string()));
        assert!(!dedup.is_duplicate(&"b".to_string()));
        assert!(!dedup.is_duplicate(&"c".to_string()));
    }

    #[test]
    fn eviction_at_capacity() {
        let mut dedup = BoundedDedup::new(3);

        assert!(!dedup.is_duplicate(&1));
        assert!(!dedup.is_duplicate(&2));
        assert!(!dedup.is_duplicate(&3));
        // Capacity full — next insert evicts 1
        assert!(!dedup.is_duplicate(&4));
        // 1 was evicted, no longer detected as duplicate
        assert!(!dedup.is_duplicate(&1));
        // After adding 4, order was [2,3,4]. Adding 1 evicts 2 -> [3,4,1]
        assert!(!dedup.is_duplicate(&2));
    }
}
