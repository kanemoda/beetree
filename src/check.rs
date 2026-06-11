//! The invariant checker (I1–I6, `docs/SPEC.md`), shared by every tree
//! engine.
//!
//! The walk is generic over [`NodeSource`] so `BeTree` (M0.2) and
//! `DiskEngine` (M1.1) are checked by byte-for-byte the same logic; the only
//! difference is how a `NodeId` resolves to a [`Node`].

use std::collections::BTreeMap;
use std::ops::Bound;

use crate::node::{Node, NodeId};
use crate::types::{CapacityKind, InvariantViolation, Key, Params};

/// How the checker resolves nodes. Implementations may panic on a `NodeId`
/// that is not resident in memory — the checker itself never reads disk.
pub(crate) trait NodeSource {
    /// The root node's id.
    fn root(&self) -> NodeId;
    /// The structure parameters the tree must respect.
    fn params(&self) -> &Params;
    /// Resolve a node id to the node.
    fn node(&self, id: NodeId) -> &Node;
}

/// Min/max leaf depth seen during an invariant walk (I6).
struct LeafDepths {
    shallowest: usize,
    deepest: usize,
}

/// Is `key` inside `[lower, upper)`? `None` bounds are -inf / +inf.
fn in_range(key: &[u8], lower: Option<&[u8]>, upper: Option<&[u8]>) -> bool {
    lower.is_none_or(|lo| key >= lo) && upper.is_none_or(|hi| key < hi)
}

/// Walk the whole tree and verify invariants I1–I6 (`docs/SPEC.md`).
pub(crate) fn check_invariants(src: &impl NodeSource) -> Result<(), InvariantViolation> {
    let mut depths = LeafDepths {
        shallowest: usize::MAX,
        deepest: 0,
    };
    check_tree(src, &mut depths)?;
    // I6: every leaf at the same depth.
    if depths.shallowest != depths.deepest {
        return Err(InvariantViolation::UnevenLeafDepth {
            shallowest: depths.shallowest,
            deepest: depths.deepest,
        });
    }
    Ok(())
}

/// Worker for [`check_invariants`]: verify I1–I5 at every node, refining
/// key bounds downward and carrying the I3 "newest seq seen above" map
/// (restored via undo frames once a subtree completes).
///
/// The walk uses an explicit frame stack, NOT machine recursion: a VALID
/// tree can be linearly tall under legal-but-degenerate parameters (F=2
/// with sorted insertion; `docs/SPEC.md`, "Structure parameters"), and the
/// checker must report — never crash — on anything an engine can legally
/// build.
fn check_tree(src: &impl NodeSource, depths: &mut LeafDepths) -> Result<(), InvariantViolation> {
    /// One unit of pending walk work.
    enum Frame<'a> {
        /// Visit a node: apply `overlay` (the parent's buffered
        /// messages for this node's key range) to the I3 map, check
        /// the node, queue its children.
        Enter {
            id: NodeId,
            lower: Option<&'a [u8]>,
            upper: Option<&'a [u8]>,
            depth: usize,
            overlay: Vec<(Key, u64)>,
        },
        /// Restore the I3 map after a subtree is fully checked.
        Undo { entries: Vec<(Key, Option<u64>)> },
    }

    let params = src.params();
    let mut newest_above: BTreeMap<Key, u64> = BTreeMap::new();
    let mut stack = vec![Frame::Enter {
        id: src.root(),
        lower: None,
        upper: None,
        depth: 1,
        overlay: Vec::new(),
    }];

    while let Some(frame) = stack.pop() {
        let (id, lower, upper, depth, overlay) = match frame {
            Frame::Undo { entries } => {
                for (key, previous) in entries.into_iter().rev() {
                    match previous {
                        Some(seq) => newest_above.insert(key, seq),
                        None => newest_above.remove(&key),
                    };
                }
                continue;
            }
            Frame::Enter {
                id,
                lower,
                upper,
                depth,
                overlay,
            } => (id, lower, upper, depth, overlay),
        };

        // Overlay the parent's messages for this subtree onto the I3
        // map. The matching Undo frame sits BELOW everything this node
        // pushes, so it runs exactly when the subtree completes.
        let mut undo = Vec::with_capacity(overlay.len());
        for (key, seq) in overlay {
            let previous = newest_above.insert(key.clone(), seq);
            undo.push((key, previous));
        }
        stack.push(Frame::Undo { entries: undo });

        match src.node(id) {
            Node::Leaf { entries } => {
                if entries.len() > params.leaf_capacity {
                    return Err(InvariantViolation::OverCapacity {
                        node: id,
                        kind: CapacityKind::Leaf,
                        occupancy: entries.len(),
                        limit: params.leaf_capacity,
                    });
                }
                for (key, entry) in entries {
                    if !in_range(key, lower, upper) {
                        return Err(InvariantViolation::KeyOutsideOwnedRange {
                            node: id,
                            key: key.clone(),
                        });
                    }
                    if let Some(&above) = newest_above.get(key) {
                        if entry.seq >= above {
                            return Err(InvariantViolation::StaleAncestor {
                                key: key.clone(),
                                ancestor_seq: above,
                                descendant_seq: entry.seq,
                            });
                        }
                    }
                }
                depths.shallowest = depths.shallowest.min(depth);
                depths.deepest = depths.deepest.max(depth);
            }
            Node::Internal {
                pivots,
                children,
                buffer,
            } => {
                // I2: pivots strictly increasing.
                for i in 1..pivots.len() {
                    if pivots[i - 1] >= pivots[i] {
                        return Err(InvariantViolation::PivotsOutOfOrder { node: id, index: i });
                    }
                }
                // I2: k pivots ⇒ exactly k+1 children.
                if children.len() != pivots.len() + 1 {
                    return Err(InvariantViolation::PivotChildMismatch {
                        node: id,
                        pivots: pivots.len(),
                        children: children.len(),
                    });
                }
                // I5: fanout and buffer capacity at rest.
                if children.len() > params.fanout {
                    return Err(InvariantViolation::OverCapacity {
                        node: id,
                        kind: CapacityKind::Fanout,
                        occupancy: children.len(),
                        limit: params.fanout,
                    });
                }
                if buffer.len() > params.buffer_capacity {
                    return Err(InvariantViolation::OverCapacity {
                        node: id,
                        kind: CapacityKind::Buffer,
                        occupancy: buffer.len(),
                        limit: params.buffer_capacity,
                    });
                }
                // Pivots must lie in the node's own range; this also
                // keeps the child range bounds below well-formed even
                // on a corrupt tree (the checker must report, never
                // panic).
                for pivot in pivots {
                    if !in_range(pivot, lower, upper) {
                        return Err(InvariantViolation::KeyOutsideOwnedRange {
                            node: id,
                            key: pivot.clone(),
                        });
                    }
                }
                // I4 is structural here: the buffer is a BTreeMap keyed
                // by Key, so it cannot hold two messages for one key.
                // I1 + I3 for the buffered messages themselves.
                for (key, message) in buffer {
                    if !in_range(key, lower, upper) {
                        return Err(InvariantViolation::KeyOutsideOwnedRange {
                            node: id,
                            key: key.clone(),
                        });
                    }
                    if let Some(&above) = newest_above.get(key) {
                        if message.seq() >= above {
                            return Err(InvariantViolation::StaleAncestor {
                                key: key.clone(),
                                ancestor_seq: above,
                                descendant_seq: message.seq(),
                            });
                        }
                    }
                }
                // Queue the children with refined bounds and their
                // slice of this buffer as the I3 overlay. Sibling order
                // is immaterial: each child's Undo frame restores the
                // map before the next sibling's Enter frame runs.
                for (i, &child) in children.iter().enumerate() {
                    let child_lower = if i == 0 {
                        lower
                    } else {
                        Some(pivots[i - 1].as_slice())
                    };
                    let child_upper = pivots.get(i).map(|p| p.as_slice()).or(upper);
                    let range = (
                        child_lower.map_or(Bound::Unbounded, Bound::Included),
                        child_upper.map_or(Bound::Unbounded, Bound::Excluded),
                    );
                    let overlay = buffer
                        .range::<[u8], _>(range)
                        .map(|(key, message)| (key.clone(), message.seq()))
                        .collect();
                    stack.push(Frame::Enter {
                        id: child,
                        lower: child_lower,
                        upper: child_upper,
                        depth: depth + 1,
                        overlay,
                    });
                }
            }
        }
    }
    Ok(())
}
