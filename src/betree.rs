//! The real Bε-tree engine (M0.2).
//!
//! Writes become messages that settle into internal-node buffers and
//! migrate toward the leaves in batches; reads walk root→leaf and the
//! topmost buffer hit wins. Flushes follow the normative greedy-fullest
//! policy (`docs/SPEC.md`, "Baseline flush policy"). Splits propagate via
//! return values (ADR-0005); nodes live in an append-only arena (ADR-0004).

use std::collections::BTreeMap;
use std::mem;
use std::ops::Bound;

use crate::engine::KvEngine;
use crate::node::{LeafEntry, Node, NodeId, route};
use crate::trace::{OpKind, TraceEvent};
use crate::types::{CapacityKind, InvariantViolation, Key, Message, Params, Value};

/// A Bε-tree key-value engine (`docs/SPEC.md`).
///
/// ```
/// use beetree::{BeTree, KvEngine, Params};
///
/// let mut tree = BeTree::new(Params::default());
/// for b in 0..=255u8 {
///     tree.insert(vec![b], vec![b]);
/// }
/// assert_eq!(tree.get(&[7]), Some(vec![7]));
/// tree.check_invariants().unwrap();
/// assert!(tree.height() > 1);
/// ```
#[derive(Debug)]
pub struct BeTree {
    params: Params,
    /// Arena: nodes are addressed by index and never freed in M0
    /// (insert-only ⇒ no merges; ADR-0004).
    nodes: Vec<Node>,
    root: NodeId,
    next_seq: u64,
    trace: Vec<TraceEvent>,
}

/// Min/max leaf depth seen during an invariant walk (I6).
struct LeafDepths {
    shallowest: usize,
    deepest: usize,
}

/// Sizes of the pieces an overfull collection of `n` items splits into,
/// each piece ≤ `cap`, by repeated halving (so pieces stay balanced and the
/// two-piece case is the classic median split). `n` can far exceed
/// `2 * cap`: batches accumulate down a flush spine (each level adds up to
/// B at-rest messages), so one delivery can carry on the order of
/// height × B messages on top of the receiver's existing contents.
fn partition_sizes(n: usize, cap: usize) -> Vec<usize> {
    if n <= cap {
        vec![n]
    } else {
        let left = n / 2;
        let mut sizes = partition_sizes(left, cap);
        sizes.extend(partition_sizes(n - left, cap));
        sizes
    }
}

/// Is `key` inside `[lower, upper)`? `None` bounds are -inf / +inf.
fn in_range(key: &[u8], lower: Option<&[u8]>, upper: Option<&[u8]>) -> bool {
    lower.is_none_or(|lo| key >= lo) && upper.is_none_or(|hi| key < hi)
}

impl BeTree {
    /// Number of levels in the tree; a lone root leaf has height 1.
    pub fn height(&self) -> usize {
        let mut height = 1;
        let mut id = self.root;
        while let Node::Internal { children, .. } = &self.nodes[id as usize] {
            height += 1;
            id = children[0];
        }
        height
    }

    /// Total number of nodes in the arena. The arena is append-only and
    /// every node stays reachable (ADR-0004), so this is also the number of
    /// live tree nodes.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    fn alloc(&mut self, node: Node) -> NodeId {
        let id = self.nodes.len() as NodeId;
        self.nodes.push(node);
        id
    }

    /// Re-establish capacity invariants at the root after an insert,
    /// growing the tree upward as needed. Every promoted piece sits at the
    /// same depth as the old root, so a new root above them keeps all
    /// leaves at uniform depth (I6).
    fn restore_root(&mut self) {
        loop {
            let root_is_internal = matches!(&self.nodes[self.root as usize], Node::Internal { .. });
            let promoted = if root_is_internal {
                self.flush_overfull(self.root)
            } else {
                self.split_if_needed(self.root)
            };
            if promoted.is_empty() {
                return;
            }
            let mut pivots = Vec::with_capacity(promoted.len());
            let mut children = Vec::with_capacity(promoted.len() + 1);
            children.push(self.root);
            for (pivot, child) in promoted {
                pivots.push(pivot);
                children.push(child);
            }
            self.root = self.alloc(Node::Internal {
                pivots,
                children,
                buffer: BTreeMap::new(),
            });
            // The new root may itself exceed F children (a leaf can shatter
            // into many pieces under tiny L); the next iteration splits it
            // again, growing the height by one more level.
        }
    }

    /// Flush this internal node's buffer until it holds ≤ B messages, then
    /// split the node itself if its fanout overflowed. Returns the promoted
    /// (pivot, new-right-sibling) pairs the caller must integrate
    /// (ADR-0005).
    ///
    /// The cascade only ever pushes messages strictly downward, so it walks
    /// root→leaf and never revisits a level — but it is processed with an
    /// explicit frame stack, NOT machine recursion: the stack depth equals
    /// tree height, and legal-but-degenerate parameters make height linear
    /// in the number of inserts (F=2 under sorted insertion; `docs/SPEC.md`,
    /// "Structure parameters"), which would overflow the call stack.
    fn flush_overfull(&mut self, id: NodeId) -> Vec<(Key, NodeId)> {
        // Nodes currently flushing, cascade root first. A node below the
        // top of the stack is waiting for its overfull child above it.
        let mut flushing: Vec<NodeId> = vec![id];
        loop {
            let top = *flushing
                .last()
                .expect("the flush stack only empties via the return below");
            if self.buffer_len(top) <= self.params.buffer_capacity {
                // `top` is done: split it if its fanout overflowed and hand
                // the promoted pairs to the frame below (its parent in the
                // cascade), or to the caller for the cascade root.
                let promoted = self.split_if_needed(top);
                flushing.pop();
                match flushing.last() {
                    Some(&parent) => self.integrate_splits(parent, promoted),
                    None => return promoted,
                }
                continue;
            }
            let (chosen, child_id, child_occupancies, batch) = self.pick_and_extract(top);
            self.trace.push(TraceEvent::FlushDecision {
                node: top,
                child_occupancies,
                chosen,
            });
            if self.apply_batch(child_id, batch) {
                // The child's buffer now overflows too: flush it to
                // completion before continuing with `top`.
                flushing.push(child_id);
            } else {
                // Integrate split pieces immediately: child indices shift,
                // so the next iteration recomputes routing from scratch
                // rather than caching it across iterations.
                let promoted = self.split_if_needed(child_id);
                self.integrate_splits(top, promoted);
            }
        }
    }

    fn buffer_len(&self, id: NodeId) -> usize {
        match &self.nodes[id as usize] {
            Node::Internal { buffer, .. } => buffer.len(),
            Node::Leaf { .. } => unreachable!("flush_overfull requires an internal node"),
        }
    }

    /// Greedy-fullest choice (`docs/SPEC.md`, "Baseline flush policy"):
    /// pick the child with the most pending messages (lowest index on
    /// ties) and remove every buffered message destined for it.
    fn pick_and_extract(
        &mut self,
        id: NodeId,
    ) -> (usize, NodeId, Vec<usize>, BTreeMap<Key, Message>) {
        let Node::Internal {
            pivots,
            children,
            buffer,
        } = &mut self.nodes[id as usize]
        else {
            unreachable!("flush_overfull requires an internal node")
        };
        let mut occupancies = vec![0usize; children.len()];
        for key in buffer.keys() {
            occupancies[route(pivots, key)] += 1;
        }
        // Strictly-greater scan keeps the lowest index on ties.
        let mut chosen = 0;
        for (i, &count) in occupancies.iter().enumerate() {
            if count > occupancies[chosen] {
                chosen = i;
            }
        }
        // The chosen child owns [pivots[chosen-1], pivots[chosen]).
        let mut batch = match chosen.checked_sub(1) {
            Some(i) => buffer.split_off(pivots[i].as_slice()),
            None => mem::take(buffer),
        };
        if let Some(hi) = pivots.get(chosen) {
            let mut tail = batch.split_off(hi.as_slice());
            buffer.append(&mut tail);
        }
        (chosen, children[chosen], occupancies, batch)
    }

    /// Apply a flushed batch to `child_id` (leaf: materialize entries;
    /// internal: coalesce into the buffer). Returns true iff the child is
    /// internal and its buffer now exceeds B, i.e. it must flush next.
    fn apply_batch(&mut self, child_id: NodeId, batch: BTreeMap<Key, Message>) -> bool {
        match &mut self.nodes[child_id as usize] {
            Node::Leaf { entries } => {
                for (key, message) in batch {
                    let Message::Put { seq, value } = message;
                    let old = entries.insert(key, LeafEntry { seq, value });
                    debug_assert!(
                        old.is_none_or(|e| e.seq < seq),
                        "I3: an incoming message must be newer than the leaf entry it replaces"
                    );
                }
                false
            }
            Node::Internal { buffer, .. } => {
                for (key, message) in batch {
                    let seq = message.seq();
                    let old = buffer.insert(key, message);
                    debug_assert!(
                        old.is_none_or(|m| m.seq() < seq),
                        "I3: an incoming message must be newer than the buffered one it coalesces"
                    );
                }
                buffer.len() > self.params.buffer_capacity
            }
        }
    }

    fn integrate_splits(&mut self, id: NodeId, promoted: Vec<(Key, NodeId)>) {
        if promoted.is_empty() {
            return;
        }
        let Node::Internal {
            pivots, children, ..
        } = &mut self.nodes[id as usize]
        else {
            unreachable!("split pairs are only ever integrated into internal nodes")
        };
        for (pivot, new_child) in promoted {
            let pos = pivots.partition_point(|p| *p < pivot);
            pivots.insert(pos, pivot);
            children.insert(pos + 1, new_child);
        }
    }

    /// Split `id` into capacity-respecting pieces if it overflowed and
    /// return the promoted (pivot, right-piece) pairs in ascending order;
    /// empty if the node fits. Handles arbitrary overflow in one pass
    /// (with L=1 a single delivery can force many splits).
    fn split_if_needed(&mut self, id: NodeId) -> Vec<(Key, NodeId)> {
        match &self.nodes[id as usize] {
            Node::Leaf { entries } if entries.len() > self.params.leaf_capacity => {
                self.split_leaf(id)
            }
            Node::Internal { children, .. } if children.len() > self.params.fanout => {
                self.split_internal(id)
            }
            _ => Vec::new(),
        }
    }

    fn split_leaf(&mut self, id: NodeId) -> Vec<(Key, NodeId)> {
        let Node::Leaf { entries } = &mut self.nodes[id as usize] else {
            unreachable!("split_leaf requires a leaf")
        };
        let items: Vec<(Key, LeafEntry)> = mem::take(entries).into_iter().collect();
        let sizes = partition_sizes(items.len(), self.params.leaf_capacity);
        let mut iter = items.into_iter();
        let mut pieces = sizes
            .iter()
            .map(|&size| iter.by_ref().take(size).collect::<BTreeMap<_, _>>())
            .collect::<Vec<_>>()
            .into_iter();

        let first = pieces.next().expect("a split yields at least one piece");
        let Node::Leaf { entries } = &mut self.nodes[id as usize] else {
            unreachable!("split_leaf requires a leaf")
        };
        *entries = first;

        let mut promoted = Vec::new();
        for piece in pieces {
            // The separator equals the smallest key of the right piece
            // (SPEC pivot convention).
            let pivot = piece
                .first_key_value()
                .expect("split pieces are non-empty")
                .0
                .clone();
            let new_id = self.alloc(Node::Leaf { entries: piece });
            promoted.push((pivot, new_id));
        }
        promoted
    }

    fn split_internal(&mut self, id: NodeId) -> Vec<(Key, NodeId)> {
        let Node::Internal {
            pivots,
            children,
            buffer,
        } = &mut self.nodes[id as usize]
        else {
            unreachable!("split_internal requires an internal node")
        };
        let pivots = mem::take(pivots);
        let children = mem::take(children);
        let mut buffer = mem::take(buffer);

        // Piece j takes children [start_j, start_j + size_j); the pivot at
        // each piece boundary moves UP (kept in neither piece) — for two
        // pieces this is exactly the classic median split.
        let sizes = partition_sizes(children.len(), self.params.fanout);
        let mut starts = Vec::with_capacity(sizes.len());
        let mut acc = 0;
        for &size in &sizes {
            starts.push(acc);
            acc += size;
        }

        // Peel the buffer right-to-left: each split_off at a promoted pivot
        // takes exactly the rightmost remaining piece's key range.
        let mut pieces_rev = Vec::with_capacity(sizes.len() - 1);
        for j in (1..sizes.len()).rev() {
            let start = starts[j];
            let size = sizes[j];
            let piece_pivots = pivots[start..start + size - 1].to_vec();
            let piece_children = children[start..start + size].to_vec();
            let piece_buffer = buffer.split_off(pivots[start - 1].as_slice());
            pieces_rev.push((
                pivots[start - 1].clone(),
                piece_pivots,
                piece_children,
                piece_buffer,
            ));
        }

        let first_pivots = pivots[..sizes[0] - 1].to_vec();
        let first_children = children[..sizes[0]].to_vec();
        let Node::Internal {
            pivots: node_pivots,
            children: node_children,
            buffer: node_buffer,
        } = &mut self.nodes[id as usize]
        else {
            unreachable!("split_internal requires an internal node")
        };
        *node_pivots = first_pivots;
        *node_children = first_children;
        *node_buffer = buffer;

        let mut promoted = Vec::new();
        for (pivot, piece_pivots, piece_children, piece_buffer) in pieces_rev.into_iter().rev() {
            let new_id = self.alloc(Node::Internal {
                pivots: piece_pivots,
                children: piece_children,
                buffer: piece_buffer,
            });
            promoted.push((pivot, new_id));
        }
        promoted
    }

    /// Worker for `check_invariants`: verify I1–I5 at every node, refining
    /// key bounds downward and carrying the I3 "newest seq seen above" map
    /// (restored via undo frames once a subtree completes).
    ///
    /// The walk uses an explicit frame stack, NOT machine recursion: a
    /// VALID tree can be linearly tall under legal-but-degenerate
    /// parameters (F=2 with sorted insertion; `docs/SPEC.md`, "Structure
    /// parameters"), and the checker must report — never crash — on
    /// anything the engine can legally build.
    fn check_tree(&self, depths: &mut LeafDepths) -> Result<(), InvariantViolation> {
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

        let mut newest_above: BTreeMap<Key, u64> = BTreeMap::new();
        let mut stack = vec![Frame::Enter {
            id: self.root,
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

            match &self.nodes[id as usize] {
                Node::Leaf { entries } => {
                    if entries.len() > self.params.leaf_capacity {
                        return Err(InvariantViolation::OverCapacity {
                            node: id,
                            kind: CapacityKind::Leaf,
                            occupancy: entries.len(),
                            limit: self.params.leaf_capacity,
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
                            return Err(InvariantViolation::PivotsOutOfOrder {
                                node: id,
                                index: i,
                            });
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
                    if children.len() > self.params.fanout {
                        return Err(InvariantViolation::OverCapacity {
                            node: id,
                            kind: CapacityKind::Fanout,
                            occupancy: children.len(),
                            limit: self.params.fanout,
                        });
                    }
                    if buffer.len() > self.params.buffer_capacity {
                        return Err(InvariantViolation::OverCapacity {
                            node: id,
                            kind: CapacityKind::Buffer,
                            occupancy: buffer.len(),
                            limit: self.params.buffer_capacity,
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
}

impl KvEngine for BeTree {
    fn new(params: Params) -> Self {
        assert!(
            params.fanout >= 2 && params.buffer_capacity >= 1 && params.leaf_capacity >= 1,
            "illegal Params (need F >= 2, B >= 1, L >= 1): {params:?}"
        );
        BeTree {
            params,
            nodes: vec![Node::Leaf {
                entries: BTreeMap::new(),
            }],
            root: 0,
            next_seq: 0,
            trace: Vec::new(),
        }
    }

    fn insert(&mut self, key: Key, value: Value) {
        self.next_seq += 1;
        let seq = self.next_seq;
        self.trace.push(TraceEvent::Op {
            seq,
            op: OpKind::Insert {
                key: key.clone(),
                value: value.clone(),
            },
        });
        match &mut self.nodes[self.root as usize] {
            Node::Leaf { entries } => {
                let old = entries.insert(key, LeafEntry { seq, value });
                debug_assert!(
                    old.is_none_or(|e| e.seq < seq),
                    "seqnos are monotonic, so a replaced root-leaf entry must be older"
                );
            }
            Node::Internal { buffer, .. } => {
                // Coalesce into the root buffer (ADR-0003): newest wins.
                let old = buffer.insert(key, Message::Put { seq, value });
                debug_assert!(
                    old.is_none_or(|m| m.seq() < seq),
                    "seqnos are monotonic, so a coalesced-away message must be older"
                );
            }
        }
        self.restore_root();
    }

    fn get(&mut self, key: &[u8]) -> Option<Value> {
        self.trace.push(TraceEvent::Get { key: key.to_vec() });
        let mut id = self.root;
        loop {
            match &self.nodes[id as usize] {
                Node::Internal {
                    pivots,
                    children,
                    buffer,
                } => {
                    // A buffer hit can be returned immediately: by I3
                    // (freshness order), the topmost occurrence of a key on
                    // the root→leaf path is the newest.
                    if let Some(Message::Put { value, .. }) = buffer.get(key) {
                        return Some(value.clone());
                    }
                    id = children[route(pivots, key)];
                }
                Node::Leaf { entries } => return entries.get(key).map(|e| e.value.clone()),
            }
        }
    }

    fn check_invariants(&self) -> Result<(), InvariantViolation> {
        let mut depths = LeafDepths {
            shallowest: usize::MAX,
            deepest: 0,
        };
        self.check_tree(&mut depths)?;
        // I6: every leaf at the same depth.
        if depths.shallowest != depths.deepest {
            return Err(InvariantViolation::UnevenLeafDepth {
                shallowest: depths.shallowest,
                deepest: depths.deepest,
            });
        }
        Ok(())
    }

    fn trace(&self) -> &[TraceEvent] {
        &self.trace
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_sizes_balances_pieces() {
        assert_eq!(partition_sizes(5, 8), vec![5]);
        assert_eq!(partition_sizes(9, 8), vec![4, 5]);
        assert_eq!(partition_sizes(5, 4), vec![2, 3]);
        assert_eq!(partition_sizes(9, 1), vec![1; 9]);
        assert!(partition_sizes(17, 4).iter().all(|&s| (1..=4).contains(&s)));
        assert_eq!(partition_sizes(17, 4).iter().sum::<usize>(), 17);
    }

    /// Overwriting a key that is also a pivot must route the new message to
    /// the subtree on the pivot's RIGHT (SPEC pivot convention). If it were
    /// routed left, the message would land in a node whose owned range is
    /// `[.., pivot)` and the I1 checker — whose bounds logic is independent
    /// of `route` — would flag it.
    #[test]
    fn overwriting_every_pivot_key_keeps_invariants_and_freshness() {
        let mut tree = BeTree::new(Params::default());
        for b in 0..=255u8 {
            tree.insert(vec![b], vec![b]);
        }
        let all_pivots: Vec<Key> = tree
            .nodes
            .iter()
            .filter_map(|node| match node {
                Node::Internal { pivots, .. } => Some(pivots.clone()),
                Node::Leaf { .. } => None,
            })
            .flatten()
            .collect();
        assert!(
            !all_pivots.is_empty(),
            "256 inserts under F=4/B=8/L=8 must produce internal nodes"
        );

        for pivot in &all_pivots {
            tree.insert(pivot.clone(), b"updated".to_vec());
            tree.check_invariants().unwrap();
        }
        for pivot in &all_pivots {
            assert_eq!(tree.get(pivot), Some(b"updated".to_vec()));
        }
    }
}
