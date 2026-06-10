//! Tree node representation for the Bε-tree (`crate::betree`).

use std::collections::BTreeMap;

use crate::types::{Key, Message, Value};

/// Index of a node in the arena (`BeTree::nodes`, ADR-0004).
pub(crate) type NodeId = u64;

/// A materialized key/value pair in a leaf, tagged with the seqno of the
/// message that produced it (needed for the I3 freshness check).
#[derive(Debug)]
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
}
