//! Tests of drawing a view from last frame around the nested views built
//! again in it.

use crate::fast::splice::kept_keys;
use collections::FxHashSet;
use rand::{Rng as _, SeedableRng as _, rngs::StdRng};

/// What `kept_keys` has to give: every key that is no gap's, in order.
fn kept_keys_naively(keys: &[u64], gaps: &[Vec<u64>]) -> Vec<u64> {
    let gap_keys: FxHashSet<u64> = gaps.iter().flatten().copied().collect();
    keys.iter()
        .copied()
        .filter(|key| !gap_keys.contains(key))
        .collect()
}

#[test]
fn kept_keys_are_the_keys_of_no_gap() {
    let mut scratch = FxHashSet::default();
    for seed in 0..2000 {
        let mut rng = StdRng::seed_from_u64(seed);
        // A small key space, so that keys recorded twice come up.
        let space = rng.random_range(4..200u64);
        let len = rng.random_range(0..60);
        let keys: Vec<u64> = (0..len).map(|_| rng.random_range(0..space)).collect();
        let mut gaps = Vec::new();
        let mut cursor = 0;
        for _ in 0..rng.random_range(0..4) {
            let gap = match rng.random_range(0..4) {
                // A stretch of the view's keys, as a nested view's usually is.
                0 | 1 if cursor < keys.len() => {
                    let start = rng.random_range(cursor..keys.len());
                    let end = rng.random_range(start..=keys.len());
                    cursor = end;
                    keys[start..end].to_vec()
                }
                // Keys the view never had, or had elsewhere.
                2 => (0..rng.random_range(0..8))
                    .map(|_| rng.random_range(0..space * 2))
                    .collect(),
                _ => Vec::new(),
            };
            gaps.push(gap);
        }
        let kept = kept_keys(&keys, gaps.iter().map(Vec::as_slice), &mut scratch);
        assert_eq!(
            kept,
            kept_keys_naively(&keys, &gaps),
            "seed {seed}: {keys:?} without {gaps:?}"
        );
        assert!(scratch.is_empty());
    }
}
