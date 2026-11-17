//! Deterministic hash collections.
//!
//! `std` hash maps are banned (their hasher is randomly seeded, so iteration order changes between runs). These
//! aliases use a fixed hasher, so the same sequence of operations always yields the same iteration order. Prefer
//! `BTreeMap` when order matters semantically.

use rustc_hash::FxBuildHasher;

/// A hash map with a fixed hasher.
pub type DetMap<K, V> = hashbrown::HashMap<K, V, FxBuildHasher>;

/// A hash set with a fixed hasher.
pub type DetSet<T> = hashbrown::HashSet<T, FxBuildHasher>;

/// An empty [`DetMap`].
pub fn det_map<K, V>() -> DetMap<K, V> {
    DetMap::with_hasher(FxBuildHasher)
}

/// An empty [`DetMap`] with room for `capacity` entries.
pub fn det_map_with_capacity<K, V>(capacity: usize) -> DetMap<K, V> {
    DetMap::with_capacity_and_hasher(capacity, FxBuildHasher)
}

/// An empty [`DetSet`].
pub fn det_set<T>() -> DetSet<T> {
    DetSet::with_hasher(FxBuildHasher)
}

/// An empty [`DetSet`] with room for `capacity` entries.
pub fn det_set_with_capacity<T>(capacity: usize) -> DetSet<T> {
    DetSet::with_capacity_and_hasher(capacity, FxBuildHasher)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iteration_order_is_reproducible() {
        let build = || {
            let mut m = det_map();
            for k in [9u64, 3, 1_000_003, 7, 42, 5] {
                m.insert(k, k * 2);
            }
            m.into_iter().collect::<Vec<_>>()
        };
        assert_eq!(build(), build());
        let mut s = det_set_with_capacity(4);
        s.insert("a");
        assert!(s.contains("a"));
        assert!(det_map_with_capacity::<u8, u8>(8).capacity() >= 8);
    }
}
