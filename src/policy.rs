//! Pluggable flush-child choice (M4.1).
//!
//! Step 1 of the flush-policy program (`docs/analysis/FALSIFICATION.md`):
//! the normative greedy-fullest rule (`docs/SPEC.md`, "Baseline flush
//! policy") is extracted behind the [`FlushPolicy`] trait so alternative
//! policies — the M4.1 rollout oracle first — can be measured against it.
//! Engines take a policy at construction and default to [`GreedyFullest`];
//! the refactor is byte-identical under the default
//! (`tests/policy_regression.rs` proves it against stored pre-refactor
//! hashes). `drain()` stays OUTSIDE the policy: its forced flushes are not
//! policy decisions (SPEC "Observability") and always use the greedy rule.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::types::{Key, Message};

/// Everything an engine knows at a flush decision point, policy-visible.
///
/// One `FlushCtx` is assembled per `TraceEvent::FlushDecision`, BEFORE the
/// batch is extracted; `child_pending` is exactly the trace event's
/// `child_occupancies`. The struct serializes (serde) because it is the
/// per-decision record of the oracle's JSONL decision log (SPEC,
/// "Observability"), which is the future training-data format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlushCtx {
    /// Identifier of the flushing node (arena/slot id — descriptive and
    /// session-local, like the trace event's `node`).
    pub node: u64,
    /// Depth of the flushing node in the tree (0 = root). The flush
    /// cascade always starts at the root, so this is the position in the
    /// cascade stack.
    pub depth: usize,
    /// Pending buffered-message count per child (messages routed by pivot
    /// ranges) — the greedy rule's input.
    pub child_pending: Vec<usize>,
    /// Estimated serialized bytes of the pending messages per child: the
    /// exact bincode record-encoding size of each buffered (key, message)
    /// entry, summed per child (an estimate of flushed PAYLOAD, not of
    /// the children's record sizes).
    pub child_pending_bytes: Vec<u64>,
    /// Per-child dirty flag: would this child be rewritten by a commit
    /// taken right now? On `DiskEngine` this is the slot's CoW dirty bit;
    /// on `BeTree` it is the simulated mirror (`BeTree::simulate_commit`).
    /// Flushing into an already-dirty subtree is the "dirty-spine
    /// discount": the rewrite is a sunk cost this commit window.
    pub child_dirty: Vec<bool>,
    /// Total messages in this node's buffer (the sum of `child_pending`).
    pub buffer_total: usize,
    /// Mutating ops since the last commit boundary (real commits on
    /// `DiskEngine`, simulated boundaries on `BeTree`): the flush's
    /// position in the commit window, on which the CoW-granularity tax —
    /// and therefore the dirty-spine discount — depends.
    pub ops_since_commit: u64,
}

/// A flush-child-choice policy: given the decision context, name the child
/// index to flush toward.
///
/// Contract: the returned index must be in range and name a child with
/// `child_pending > 0` — flushing an empty batch is not a legal engine
/// step, and the engines PANIC on a policy that asks for one. Policies may
/// keep state (`&mut self`); engines consult the policy for every
/// overflow-driven flush decision and emit the unchanged
/// `FlushDecision` trace event with its choice. `drain()` never consults
/// the policy.
pub trait FlushPolicy: fmt::Debug {
    /// Choose the child to flush toward.
    fn choose(&mut self, ctx: &FlushCtx) -> usize;
}

/// The normative baseline (`docs/SPEC.md`, "Baseline flush policy"):
/// flush toward the child with the most pending messages, lowest index on
/// ties. This is the rule every engine used before M4.1, now extracted —
/// and the default for every engine constructor that does not take an
/// explicit policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GreedyFullest;

impl GreedyFullest {
    /// The bare rule over occupancy counts: argmax, lowest index on ties
    /// (the exact strictly-greater scan the engines always used). Public
    /// so analysis code can recompute "what greedy would have done" from
    /// a recorded [`FlushCtx`].
    pub fn pick(child_pending: &[usize]) -> usize {
        let mut chosen = 0;
        for (i, &count) in child_pending.iter().enumerate() {
            if count > child_pending[chosen] {
                chosen = i;
            }
        }
        chosen
    }
}

impl FlushPolicy for GreedyFullest {
    fn choose(&mut self, ctx: &FlushCtx) -> usize {
        GreedyFullest::pick(&ctx.child_pending)
    }
}

/// Exact serialized size of one buffered (key, message) entry inside a
/// node record's buffer map, in the on-disk bincode configuration
/// (little-endian, fixed-int): a map entry is the key then the message
/// with no framing, a `Vec<u8>` is a u64 length plus the bytes, and an
/// enum is a u32 variant tag plus its fields. Pinned to the real encoding
/// by a property test below.
pub(crate) fn entry_bytes(key: &Key, message: &Message) -> u64 {
    let key_bytes = 8 + key.len() as u64;
    let message_bytes = match message {
        Message::Put { value, .. } => 4 + 8 + 8 + value.len() as u64,
        Message::Delete { .. } => 4 + 8,
        Message::Upsert { .. } => 4 + 8 + 4 + 8,
    };
    key_bytes + message_bytes
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use proptest::prelude::*;

    use super::*;
    use crate::format::{DiskNode, encode_node};
    use crate::types::UpsertOp;

    fn ctx(child_pending: Vec<usize>) -> FlushCtx {
        let n = child_pending.len();
        FlushCtx {
            node: 7,
            depth: 0,
            buffer_total: child_pending.iter().sum(),
            child_pending,
            child_pending_bytes: vec![0; n],
            child_dirty: vec![false; n],
            ops_since_commit: 0,
        }
    }

    #[test]
    fn greedy_picks_the_fullest_child() {
        assert_eq!(GreedyFullest.choose(&ctx(vec![1, 4, 2, 0])), 1);
        assert_eq!(GreedyFullest.choose(&ctx(vec![0, 0, 9])), 2);
    }

    #[test]
    fn greedy_breaks_ties_toward_the_lowest_index() {
        assert_eq!(GreedyFullest.choose(&ctx(vec![3, 3, 3])), 0);
        assert_eq!(GreedyFullest.choose(&ctx(vec![1, 5, 5])), 1);
    }

    fn message_strategy() -> impl Strategy<Value = Message> {
        prop_oneof![
            (any::<u64>(), proptest::collection::vec(any::<u8>(), 0..=24))
                .prop_map(|(seq, value)| Message::Put { seq, value }),
            any::<u64>().prop_map(|seq| Message::Delete { seq }),
            (any::<u64>(), any::<i64>()).prop_map(|(seq, d)| Message::Upsert {
                seq,
                op: UpsertOp::Add(d),
            }),
        ]
    }

    proptest! {
        /// `entry_bytes` is EXACT: adding one buffered entry to a record
        /// grows its real bincode encoding by exactly that many bytes.
        #[test]
        fn entry_bytes_matches_the_real_record_encoding(
            key in proptest::collection::vec(any::<u8>(), 0..=16),
            message in message_strategy(),
        ) {
            let node = |buffer: BTreeMap<Key, Message>| DiskNode::Internal {
                pivots: vec![vec![9]],
                children: vec![8192, 8240],
                buffer,
            };
            let empty = encode_node(&node(BTreeMap::new())).unwrap().len() as u64;
            let one = encode_node(&node(BTreeMap::from([(
                key.clone(),
                message.clone(),
            )])))
            .unwrap()
            .len() as u64;
            prop_assert_eq!(one - empty, entry_bytes(&key, &message));
        }
    }
}
