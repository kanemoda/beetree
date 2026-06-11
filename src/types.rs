//! Core data types: keys, values, messages, structure parameters, and the
//! invariant-violation error reported by `check_invariants`.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Keys are arbitrary byte strings, ordered lexicographically.
pub type Key = Vec<u8>;

/// Values are arbitrary byte strings.
pub type Value = Vec<u8>;

/// A blind update: pure DATA describing how to transform a value — never
/// user code, so traces stay deterministic and replayable (ADR-0011;
/// contrast RocksDB merge operators).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpsertOp {
    /// Wrapping 64-bit addition over the little-endian numeric reading of
    /// the value (`docs/SPEC.md`, "Upsert semantics").
    Add(i64),
}

impl UpsertOp {
    /// Apply this upsert to an existing value (`docs/SPEC.md`, normative):
    /// base = if the existing value is exactly 8 bytes,
    /// `i64::from_le_bytes(it)`, else 0 (including absent and deleted).
    /// The result is always the 8-byte LE encoding of
    /// `base.wrapping_add(delta)` — wrapping, never panicking.
    pub fn apply(&self, existing: Option<&[u8]>) -> Value {
        let UpsertOp::Add(delta) = self;
        let base = match existing {
            Some(bytes) if bytes.len() == 8 => {
                i64::from_le_bytes(bytes.try_into().expect("checked: exactly 8 bytes"))
            }
            _ => 0,
        };
        base.wrapping_add(*delta).to_le_bytes().to_vec()
    }
}

/// A write operation tagged with the global seqno of the public op that
/// produced it.
///
/// Messages live in internal-node buffers and migrate downward via flushes;
/// leaves store materialized entries (`docs/SPEC.md`, "Semantics"): `Put`
/// sets the entry, `Delete` removes it outright (leaves never store
/// tombstones — the leaf is the authoritative bottom), `Upsert`
/// materializes per [`UpsertOp::apply`].
///
/// Serde derives serve the on-disk node records (`docs/SPEC.md`, "On-disk
/// format v2"): buffered messages persist inside internal-node records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    /// Insert or overwrite the value for a key (last-writer-wins).
    Put {
        /// Global sequence number of the public op that created this message.
        seq: u64,
        /// The value written.
        value: Value,
    },
    /// Remove the key (a tombstone while in transit; annihilates at the
    /// leaf).
    Delete {
        /// Global sequence number of the public op that created this message.
        seq: u64,
    },
    /// Blindly transform the value, whatever it currently is.
    Upsert {
        /// Global sequence number of the public op that created this message.
        seq: u64,
        /// The transformation to apply.
        op: UpsertOp,
    },
}

impl Message {
    /// The global sequence number this message carries.
    pub fn seq(&self) -> u64 {
        match self {
            Message::Put { seq, .. } | Message::Delete { seq } | Message::Upsert { seq, .. } => {
                *seq
            }
        }
    }

    /// Coalesce `self` (the NEWER message) onto `older`, per the normative
    /// table (`docs/SPEC.md`, "Coalescing"; newer ∘ older = result):
    ///
    /// ```text
    /// Put(v)    ∘ anything      = Put(v)
    /// Delete    ∘ anything      = Delete
    /// Upsert(d) ∘ Put(v)        = Put(apply(v, d))       [folds immediately]
    /// Upsert(d) ∘ Delete        = Put(encode(d))         [base 0; folds]
    /// Upsert(d) ∘ Upsert(d_old) = Upsert(Add(d_old + d)) [wrapping]
    /// ```
    ///
    /// The result always carries the NEWER seq, so invariant I4 keeps
    /// holding: one effective message per key per buffer.
    pub fn coalesce(self, older: &Message) -> Message {
        debug_assert!(
            older.seq() < self.seq(),
            "coalescing requires self to be the newer message"
        );
        match self {
            Message::Put { .. } | Message::Delete { .. } => self,
            Message::Upsert { seq, op } => match older {
                Message::Put { value, .. } => Message::Put {
                    seq,
                    value: op.apply(Some(value)),
                },
                Message::Delete { .. } => Message::Put {
                    seq,
                    value: op.apply(None),
                },
                Message::Upsert {
                    op: UpsertOp::Add(d_old),
                    ..
                } => {
                    let UpsertOp::Add(d) = op;
                    Message::Upsert {
                        seq,
                        op: UpsertOp::Add(d_old.wrapping_add(d)),
                    }
                }
            },
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

/// A violation of one of the structural invariants I1–I7 (`docs/SPEC.md`,
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

    /// I7: a non-root leaf is empty at rest (an emptied leaf must be
    /// reclaimed by its parent the moment a flush empties it; SPEC,
    /// "Reclamation v1").
    #[error("I7 no empty leaves: non-root leaf {node} is empty")]
    EmptyNonRootLeaf {
        /// Identifier of the offending leaf.
        node: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le(n: i64) -> Value {
        n.to_le_bytes().to_vec()
    }

    // ------------------------------------------------------------------
    // The Add semantics (SPEC, "Upsert semantics"), edge by edge.

    #[test]
    fn add_reads_exactly_8_byte_values_as_le_i64() {
        assert_eq!(UpsertOp::Add(5).apply(Some(&le(10))), le(15));
        assert_eq!(UpsertOp::Add(-20).apply(Some(&le(10))), le(-10));
    }

    #[test]
    fn add_treats_absent_and_non_8_byte_values_as_base_0() {
        assert_eq!(UpsertOp::Add(7).apply(None), le(7));
        assert_eq!(UpsertOp::Add(7).apply(Some(b"xyz")), le(7));
        assert_eq!(UpsertOp::Add(7).apply(Some(b"")), le(7));
        assert_eq!(UpsertOp::Add(7).apply(Some(&[0u8; 9])), le(7));
    }

    #[test]
    fn add_wraps_never_panics() {
        let once = UpsertOp::Add(i64::MAX).apply(None);
        assert_eq!(once, le(i64::MAX));
        let twice = UpsertOp::Add(i64::MAX).apply(Some(&once));
        assert_eq!(twice, le(i64::MAX.wrapping_add(i64::MAX)));
        assert_eq!(
            UpsertOp::Add(-1).apply(Some(&le(i64::MIN))),
            le(i64::MIN.wrapping_add(-1))
        );
    }

    // ------------------------------------------------------------------
    // Every cell of the coalescing table (newer ∘ older = result), with
    // the newer seq carried by the result.

    fn put(seq: u64, n: i64) -> Message {
        Message::Put { seq, value: le(n) }
    }

    fn delete(seq: u64) -> Message {
        Message::Delete { seq }
    }

    fn upsert(seq: u64, d: i64) -> Message {
        Message::Upsert {
            seq,
            op: UpsertOp::Add(d),
        }
    }

    #[test]
    fn put_absorbs_older_put() {
        assert_eq!(put(2, 9).coalesce(&put(1, 5)), put(2, 9));
    }

    #[test]
    fn put_absorbs_older_delete() {
        assert_eq!(put(2, 9).coalesce(&delete(1)), put(2, 9));
    }

    #[test]
    fn put_absorbs_older_upsert() {
        assert_eq!(put(2, 9).coalesce(&upsert(1, 5)), put(2, 9));
    }

    #[test]
    fn delete_absorbs_older_put() {
        assert_eq!(delete(2).coalesce(&put(1, 5)), delete(2));
    }

    #[test]
    fn delete_absorbs_older_delete() {
        assert_eq!(delete(2).coalesce(&delete(1)), delete(2));
    }

    #[test]
    fn delete_absorbs_older_upsert() {
        assert_eq!(delete(2).coalesce(&upsert(1, 5)), delete(2));
    }

    #[test]
    fn upsert_folds_into_older_put() {
        // Upsert(d) ∘ Put(v) = Put(apply(v, d)), newer seq.
        assert_eq!(upsert(2, 5).coalesce(&put(1, 10)), put(2, 15));
        // A non-numeric older value folds from base 0.
        let odd = Message::Put {
            seq: 1,
            value: b"xyz".to_vec(),
        };
        assert_eq!(upsert(2, 5).coalesce(&odd), put(2, 5));
    }

    #[test]
    fn upsert_over_older_delete_folds_from_base_0() {
        // Upsert(d) ∘ Delete = Put(encode(d)), newer seq.
        assert_eq!(upsert(2, 5).coalesce(&delete(1)), put(2, 5));
    }

    #[test]
    fn upserts_compose_with_wrapping_addition() {
        assert_eq!(upsert(2, 5).coalesce(&upsert(1, 10)), upsert(2, 15));
        assert_eq!(
            upsert(2, i64::MAX).coalesce(&upsert(1, i64::MAX)),
            upsert(2, i64::MAX.wrapping_add(i64::MAX))
        );
    }
}
