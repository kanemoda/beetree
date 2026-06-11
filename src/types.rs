//! Core data types: keys, values, messages, structure parameters, and the
//! invariant-violation error reported by `check_invariants`.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Keys are arbitrary byte strings, ordered lexicographically.
pub type Key = Vec<u8>;

/// Values are arbitrary byte strings.
pub type Value = Vec<u8>;

/// A write operation tagged with the global seqno of the public op that
/// produced it.
///
/// Messages live in internal-node buffers and migrate downward via flushes;
/// leaves store materialized entries (`docs/SPEC.md`, "Semantics"). M0 is
/// insert-only, so `Put` is the only message kind; deletes and upserts are
/// future variants.
///
/// Serde derives serve the on-disk node records (`docs/SPEC.md`, "On-disk
/// format v1"): buffered messages persist inside internal-node records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    /// Insert or overwrite the value for a key (last-writer-wins).
    Put {
        /// Global sequence number of the public op that created this message.
        seq: u64,
        /// The value written.
        value: Value,
    },
}

impl Message {
    /// The global sequence number this message carries.
    pub fn seq(&self) -> u64 {
        match self {
            Message::Put { seq, .. } => *seq,
        }
    }
}

/// Structure parameters: count-based capacities (ADR-0001).
///
/// The defaults are the deliberately tiny test values from `docs/SPEC.md`
/// (F=4, B=8, L=8): small capacities force deep trees and frequent
/// structural operations under test. Never "fix" a failing test by enlarging
/// them (`CLAUDE.md`).
///
/// Legal ranges: `fanout >= 2`, `buffer_capacity >= 1`, `leaf_capacity >= 1`.
/// With F < 2, invariants I2 (k pivots ⇒ k+1 children) and I5 (fanout ≤ F)
/// are jointly unsatisfiable for any internal node. Engines may panic on
/// parameters outside these ranges.
///
/// Serde derives serve the superblock (`docs/SPEC.md`, "On-disk format
/// v1"): since M1.1 a database file persists its params; traces still
/// travel them out-of-band (ADR-0006).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Params {
    /// F: max children per internal node.
    pub fanout: usize,
    /// B: max messages per internal buffer.
    pub buffer_capacity: usize,
    /// L: max entries per leaf.
    pub leaf_capacity: usize,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            fanout: 4,
            buffer_capacity: 8,
            leaf_capacity: 8,
        }
    }
}

/// Which capacity bound an [`InvariantViolation::OverCapacity`] exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityKind {
    /// Messages in an internal-node buffer (limit B).
    Buffer,
    /// Entries in a leaf (limit L).
    Leaf,
    /// Children of an internal node (limit F).
    Fanout,
}

impl fmt::Display for CapacityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CapacityKind::Buffer => "buffer (B)",
            CapacityKind::Leaf => "leaf (L)",
            CapacityKind::Fanout => "fanout (F)",
        })
    }
}

/// A violation of one of the structural invariants I1–I6 (`docs/SPEC.md`,
/// "Invariants"), as reported by `KvEngine::check_invariants`.
///
/// `NaiveEngine` has no tree structure and never produces one of these; the
/// variants are defined now so the M0.2 invariant checker has a complete
/// vocabulary from day one.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InvariantViolation {
    /// I1: a buffered message or leaf entry lies outside the key range its
    /// node owns, as induced by ancestor pivots.
    #[error("I1 key ownership: node {node} holds key {key:?} outside its owned range")]
    KeyOutsideOwnedRange {
        /// Identifier of the offending node.
        node: u64,
        /// The key found outside the node's owned range.
        key: Key,
    },

    /// I2: a node's pivots are not strictly increasing.
    #[error("I2 pivot order: node {node} pivots not strictly increasing at index {index}")]
    PivotsOutOfOrder {
        /// Identifier of the offending node.
        node: u64,
        /// Index of the first pivot not greater than its predecessor.
        index: usize,
    },

    /// I2: an internal node with k pivots does not have exactly k+1 children.
    #[error("I2 pivot order: node {node} has {pivots} pivots but {children} children")]
    PivotChildMismatch {
        /// Identifier of the offending node.
        node: u64,
        /// Number of pivots in the node.
        pivots: usize,
        /// Number of children in the node.
        children: usize,
    },

    /// I3: along some root→leaf path, two occurrences of a key do not
    /// strictly decrease in seq.
    #[error(
        "I3 freshness order: key {key:?} has seq {ancestor_seq} at an ancestor, \
         seq {descendant_seq} at a descendant"
    )]
    StaleAncestor {
        /// The key whose occurrences are mis-ordered.
        key: Key,
        /// Seq of the occurrence higher in the tree.
        ancestor_seq: u64,
        /// Seq of the occurrence lower in the tree (must be strictly older).
        descendant_seq: u64,
    },

    /// I4: a buffer holds more than one message for the same key.
    #[error("I4 coalescing: node {node} buffers key {key:?} more than once")]
    UncoalescedBuffer {
        /// Identifier of the offending node.
        node: u64,
        /// The key buffered more than once.
        key: Key,
    },

    /// I5: a capacity bound is exceeded at rest (after a public op returned).
    #[error("I5 capacity at rest: node {node} {kind} holds {occupancy}, limit {limit}")]
    OverCapacity {
        /// Identifier of the offending node.
        node: u64,
        /// Which bound (B, L, or F) was exceeded.
        kind: CapacityKind,
        /// Observed count.
        occupancy: usize,
        /// The configured limit from [`Params`].
        limit: usize,
    },

    /// I6: not all leaves are at the same depth.
    #[error("I6 uniform height: leaves found at depths {shallowest} and {deepest}")]
    UnevenLeafDepth {
        /// Depth of the shallowest leaf found.
        shallowest: usize,
        /// Depth of the deepest leaf found.
        deepest: usize,
    },
}
