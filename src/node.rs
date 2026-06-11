//! Tree node representation for the Bε-tree (`crate::betree`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::types::{Key, Message, Value};

/// Index of a node in the arena (`BeTree::nodes`, ADR-0004; `DiskEngine`
/// slots, M1.1).
pub(crate) type NodeId = u64;

/// A materialized key/value pair in a leaf, tagged with the seqno of the
/// message that produced it (needed for the I3 freshness check).
///
/// Serde derives serve the on-disk node records (`docs/SPEC.md`, "On-disk
/// format v1"); `Clone` lets a commit serialize a leaf without taking it
/// apart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LeafEntry {
    pub seq: u64,
    pub value: Value,
}

/// One node of the Bε-tree.
///
/// Internal nodes hold pivots, children, and a message buffer keyed by
/// `Key` — the `BTreeMap` makes invariant I4 (one message per key)
/// structural. Leaves hold materialized entries.
#[derive(Debug)]
pub(crate) enum Node {
    Internal {
        pivots: Vec<Key>,
        children: Vec<NodeId>,
        buffer: BTreeMap<Key, Message>,
    },
    Leaf {
        entries: BTreeMap<Key, LeafEntry>,
    },
}

/// The child index owning `key` under the SPEC pivot convention: child i
/// owns keys in `[p_{i-1}, p_i)`, so a key equal to a pivot routes to the
/// child on the pivot's RIGHT.
pub(crate) fn route(pivots: &[Key], key: &[u8]) -> usize {
    pivots.partition_point(|p| p.as_slice() <= key)
}

/// Sizes of the pieces an overfull collection of `n` items splits into,
/// each piece ≤ `cap`, by repeated halving (so pieces stay balanced and the
/// two-piece case is the classic median split). `n` can far exceed
/// `2 * cap`: batches accumulate down a flush spine (each level adds up to
/// B at-rest messages), so one delivery can carry on the order of
/// height × B messages on top of the receiver's existing contents.
pub(crate) fn partition_sizes(n: usize, cap: usize) -> Vec<usize> {
    if n <= cap {
        vec![n]
    } else {
        let left = n / 2;
        let mut sizes = partition_sizes(left, cap);
        sizes.extend(partition_sizes(n - left, cap));
        sizes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pivots() -> Vec<Key> {
        vec![vec![10], vec![20], vec![30]]
    }

    #[test]
    fn key_below_all_pivots_routes_to_child_0() {
        assert_eq!(route(&pivots(), &[5]), 0);
    }

    #[test]
    fn key_equal_to_a_pivot_routes_right() {
        assert_eq!(route(&pivots(), &[10]), 1);
        assert_eq!(route(&pivots(), &[20]), 2);
        assert_eq!(route(&pivots(), &[30]), 3);
    }

    #[test]
    fn key_between_pivots_routes_to_the_enclosing_range() {
        assert_eq!(route(&pivots(), &[15]), 1);
        assert_eq!(route(&pivots(), &[25]), 2);
    }

    #[test]
    fn smallest_and_largest_keys_route_to_the_outer_children() {
        // The empty key is the smallest byte string; [255, 255] sorts after
        // every single-byte pivot.
        assert_eq!(route(&pivots(), &[]), 0);
        assert_eq!(route(&pivots(), &[255, 255]), 3);
    }

    #[test]
    fn no_pivots_means_a_single_child_owns_everything() {
        assert_eq!(route(&[], &[42]), 0);
    }

    #[test]
    fn partition_sizes_balances_pieces() {
        assert_eq!(partition_sizes(5, 8), vec![5]);
        assert_eq!(partition_sizes(9, 8), vec![4, 5]);
        assert_eq!(partition_sizes(5, 4), vec![2, 3]);
        assert_eq!(partition_sizes(9, 1), vec![1; 9]);
        assert!(partition_sizes(17, 4).iter().all(|&s| (1..=4).contains(&s)));
        assert_eq!(partition_sizes(17, 4).iter().sum::<usize>(), 17);
    }
}
