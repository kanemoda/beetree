//! The real Bε-tree engine (M0.2; full message algebra since M2.1).
//!
//! Mutations become messages — puts, tombstones, upserts — that settle
//! into internal-node buffers and migrate toward the leaves in batches;
//! reads walk root→leaf folding the pending upsert chain over the first
//! Put/Delete/leaf terminal (`docs/SPEC.md`, "Reads"). Flushes follow the
//! normative greedy-fullest policy ("Baseline flush policy") and reclaim
//! emptied leaves as they go ("Reclamation v1"). Splits propagate via
//! return values (ADR-0005); nodes live in an append-only arena (ADR-0004).

use std::collections::BTreeMap;
use std::mem;
use std::ops::Bound;

use crate::check::{self, NodeSource};
use crate::engine::{EngineError, KvEngine};
use crate::node::{
    Delivery, LeafEntry, Node, NodeId, Outcome, apply_chain, apply_to_leaf, bound_as_slice,
    clip_lower, clip_upper, coalesce_into, partition_sizes, range_is_empty, route,
};
use crate::trace::{OpKind2, Recorder, TraceEvent, TraceEvent2};
use crate::types::{InvariantViolation, Key, Message, Params, UpsertOp, Value};

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
    /// Arena: nodes are addressed by index and never freed — unlinked
    /// nodes (emptied leaves, collapsed roots; M2.1) simply leak here
    /// (ADR-0004).
    nodes: Vec<Node>,
    root: NodeId,
    next_seq: u64,
    trace: Recorder,
}

/// The invariant checker resolves nodes straight out of the arena
/// (`src/check.rs` holds the shared walk).
impl NodeSource for BeTree {
    fn root(&self) -> NodeId {
        self.root
    }

    fn params(&self) -> &Params {
        &self.params
    }

    fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }
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

    /// Force-flush every buffer until the whole tree is message-free
    /// (SPEC "Observability"): a benchmarking/analysis utility OUTSIDE
    /// the performance model. Its internal flushes are NOT traced — no
    /// FlushDecision events (docs/findings.md) — so never call it
    /// mid-workload when recording traces for policy analysis.
    pub fn drain(&mut self) {
        self.restore_root_with(0, false);
    }

    fn alloc(&mut self, node: Node) -> NodeId {
        let id = self.nodes.len() as NodeId;
        self.nodes.push(node);
        id
    }

    /// Re-establish capacity invariants at the root after a public
    /// mutating op, growing the tree upward (splits) or shrinking it
    /// (reclamation; SPEC "Reclamation v1") as needed. Every promoted
    /// piece sits at the same depth as the old root, so a new root above
    /// them keeps all leaves at uniform depth (I6).
    fn restore_root(&mut self) {
        self.restore_root_with(self.params.buffer_capacity, true);
    }

    /// The settling loop behind both public-op tails (`threshold` = B,
    /// traced) and [`BeTree::drain`] (`threshold` = 0, untraced).
    fn restore_root_with(&mut self, threshold: usize, trace_flushes: bool) {
        loop {
            let root_is_internal = matches!(&self.nodes[self.root as usize], Node::Internal { .. });
            let outcome = if root_is_internal {
                self.flush_with(self.root, threshold, trace_flushes)
            } else {
                Outcome::Splits(self.split_if_needed(self.root))
            };
            let promoted = match outcome {
                Outcome::Removed => {
                    // Every key range beneath the root emptied out: the
                    // tree is the empty tree again — its initial state, a
                    // single empty leaf. The old root leaks (ADR-0004).
                    self.root = self.alloc(Node::Leaf {
                        entries: BTreeMap::new(),
                    });
                    break;
                }
                Outcome::Splits(promoted) => promoted,
            };
            if promoted.is_empty() {
                break;
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
        // Root collapse (SPEC "Reclamation v1"): promote a lone child
        // while the root's buffer is empty. A non-empty buffer is NOT
        // force-flushed — a stale tall root is harmless and collapses on
        // a later op once its buffer drains.
        loop {
            match &self.nodes[self.root as usize] {
                Node::Internal {
                    children, buffer, ..
                } if children.len() == 1 && buffer.is_empty() => {
                    self.root = children[0];
                }
                _ => break,
            }
        }
    }

    /// Flush this internal node's buffer until it holds ≤ B messages, then
    /// settle the node itself: split it if its fanout overflowed, or
    /// signal [`Outcome::Removed`] if deliveries emptied every child out
    /// from under it. The caller integrates splits or unlinks the node
    /// (ADR-0005; SPEC "Reclamation v1").
    ///
    /// The cascade only ever pushes messages strictly downward, so it walks
    /// root→leaf and never revisits a level — but it is processed with an
    /// explicit frame stack, NOT machine recursion: the stack depth equals
    /// tree height, and legal-but-degenerate parameters make height linear
    /// in the number of inserts (F=2 under sorted insertion; `docs/SPEC.md`,
    /// "Structure parameters"), which would overflow the call stack.
    fn flush_with(&mut self, id: NodeId, threshold: usize, trace_flushes: bool) -> Outcome {
        // Nodes currently flushing, cascade root first. A node below the
        // top of the stack is waiting for its overfull child above it.
        let mut flushing: Vec<NodeId> = vec![id];
        loop {
            let top = *flushing
                .last()
                .expect("the flush stack only empties via the return below");
            if self.buffer_len(top) <= threshold {
                if threshold == 0 {
                    // Drain mode: a delivery-driven cascade never visits a
                    // child the parent holds no messages for, so descend
                    // into any child whose SUBTREE still holds resting
                    // messages (the child's own buffer may be empty above
                    // a buffered descendant); the flushing stack wires its
                    // outcome to `top` as usual.
                    let buffered_child = match &self.nodes[top as usize] {
                        Node::Internal { children, .. } => children
                            .iter()
                            .copied()
                            .find(|&c| self.subtree_has_messages(c)),
                        Node::Leaf { .. } => None,
                    };
                    if let Some(child) = buffered_child {
                        flushing.push(child);
                        continue;
                    }
                }
                // `top` is done: hand its outcome to the frame below (its
                // parent in the cascade), or to the caller for the
                // cascade root.
                let outcome = self.settle(top);
                flushing.pop();
                match flushing.last() {
                    Some(&parent) => match outcome {
                        Outcome::Splits(promoted) => self.integrate_splits(parent, promoted),
                        Outcome::Removed => self.remove_child(parent, top),
                    },
                    None => return outcome,
                }
                continue;
            }
            let (chosen, child_id, child_occupancies, batch) = self.pick_and_extract(top);
            if trace_flushes {
                self.trace.flush_decision(top, child_occupancies, chosen);
            }
            match self.apply_batch(child_id, batch, threshold) {
                Delivery::Internal { overflowed: true } => {
                    // The child's buffer now overflows too: flush it to
                    // completion before continuing with `top`.
                    flushing.push(child_id);
                }
                Delivery::Internal { overflowed: false } | Delivery::Leaf { emptied: false } => {
                    // Integrate split pieces immediately: child indices
                    // shift, so the next iteration recomputes routing from
                    // scratch rather than caching it across iterations.
                    let promoted = self.split_if_needed(child_id);
                    self.integrate_splits(top, promoted);
                }
                Delivery::Leaf { emptied: true } => {
                    // The delivery annihilated the leaf's last entries:
                    // reclaim it right now (I7) — a delivery cannot both
                    // split and empty.
                    self.remove_child(top, child_id);
                }
            }
        }
    }

    /// Drain-mode probe: does any buffer in `id`'s subtree hold messages?
    fn subtree_has_messages(&self, id: NodeId) -> bool {
        let mut stack = vec![id];
        while let Some(n) = stack.pop() {
            if let Node::Internal {
                buffer, children, ..
            } = &self.nodes[n as usize]
            {
                if !buffer.is_empty() {
                    return true;
                }
                stack.extend(children.iter().copied());
            }
        }
        false
    }

    /// A flushed node's parting word to its parent: [`Outcome::Removed`]
    /// for an internal node whose children were all reclaimed (its buffer
    /// is necessarily empty: the messages went down with the deliveries
    /// that emptied them), else its split pieces.
    fn settle(&mut self, id: NodeId) -> Outcome {
        if let Node::Internal {
            children, buffer, ..
        } = &self.nodes[id as usize]
        {
            if children.is_empty() {
                debug_assert!(
                    buffer.is_empty(),
                    "a node that lost every child has delivered every message"
                );
                return Outcome::Removed;
            }
        }
        Outcome::Splits(self.split_if_needed(id))
    }

    /// Unlink a reclaimed child (SPEC "Reclamation v1"): drop it and its
    /// adjacent pivot — the left pivot if one exists, else the right one —
    /// so the neighbor absorbs the emptied key range. A parent reduced to
    /// a single child PERSISTS (fanout-1 internals are legal; same
    /// degeneracy class as F=2); one reduced to zero children signals
    /// [`Outcome::Removed`] when it settles.
    fn remove_child(&mut self, parent: NodeId, child: NodeId) {
        let Node::Internal {
            pivots, children, ..
        } = &mut self.nodes[parent as usize]
        else {
            unreachable!("children are only ever removed from internal nodes")
        };
        let at = children
            .iter()
            .position(|&c| c == child)
            .expect("the removed node is a child of this parent");
        children.remove(at);
        if at > 0 {
            pivots.remove(at - 1);
        } else if !pivots.is_empty() {
            pivots.remove(0);
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

    /// Apply a flushed batch to `child_id` (leaf: materialize entries per
    /// the message kinds — tombstones annihilate; internal: coalesce into
    /// the buffer per the normative table) and report what the delivery
    /// did, so the flush loop can split, recurse, or reclaim.
    fn apply_batch(
        &mut self,
        child_id: NodeId,
        batch: BTreeMap<Key, Message>,
        threshold: usize,
    ) -> Delivery {
        match &mut self.nodes[child_id as usize] {
            Node::Leaf { entries } => {
                for (key, message) in batch {
                    apply_to_leaf(entries, key, message);
                }
                Delivery::Leaf {
                    emptied: entries.is_empty(),
                }
            }
            Node::Internal { buffer, .. } => {
                for (key, message) in batch {
                    coalesce_into(buffer, key, message);
                }
                Delivery::Internal {
                    overflowed: buffer.len() > threshold,
                }
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
}

impl BeTree {
    /// The shared tail of every public mutating op: route the message at
    /// the root (leaf root: materialize; internal root: coalesce into the
    /// buffer, ADR-0003) and re-settle the tree.
    fn apply_root(&mut self, key: Key, message: Message) {
        match &mut self.nodes[self.root as usize] {
            Node::Leaf { entries } => apply_to_leaf(entries, key, message),
            Node::Internal { buffer, .. } => coalesce_into(buffer, key, message),
        }
        self.restore_root();
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
            trace: Recorder::default(),
        }
    }

    fn insert(&mut self, key: Key, value: Value) {
        self.next_seq += 1;
        let seq = self.next_seq;
        self.trace.op(
            seq,
            OpKind2::Insert {
                key: key.clone(),
                value: value.clone(),
            },
        );
        self.apply_root(key, Message::Put { seq, value });
    }

    fn delete(&mut self, key: Key) {
        self.next_seq += 1;
        let seq = self.next_seq;
        self.trace.op(seq, OpKind2::Delete { key: key.clone() });
        self.apply_root(key, Message::Delete { seq });
    }

    fn upsert(&mut self, key: Key, op: UpsertOp) {
        self.next_seq += 1;
        let seq = self.next_seq;
        self.trace.op(
            seq,
            OpKind2::Upsert {
                key: key.clone(),
                op,
            },
        );
        self.apply_root(key, Message::Upsert { seq, op });
    }

    fn scan(
        &mut self,
        lo: Bound<Vec<u8>>,
        hi: Bound<Vec<u8>>,
    ) -> Result<Vec<(Key, Value)>, EngineError> {
        self.trace.scan(&lo, &hi);
        if range_is_empty(&lo, &hi) {
            return Ok(Vec::new());
        }
        // Root leaf: the entries are the terminal resolutions.
        if let Node::Leaf { entries } = &self.nodes[self.root as usize] {
            return Ok(entries
                .range::<[u8], _>((bound_as_slice(&lo), bound_as_slice(&hi)))
                .map(|(k, e)| (k.clone(), e.value.clone()))
                .collect());
        }
        // Bottom-up application (SPEC "Range scans", normative): recurse
        // top-down clipping the range per child, but APPLY upward — each
        // internal node folds its in-range buffered messages onto the
        // union of its children's (disjoint, ordered) results. I3
        // guarantees every message here is strictly newer than anything
        // produced below, so plain overwrite/transform in depth order IS
        // seq order. Explicit frames, not machine recursion: degenerate
        // F=2 trees are linearly tall.
        struct Frame {
            id: NodeId,
            lo: Bound<Key>,
            hi: Bound<Key>,
            next_child: usize,
            acc: BTreeMap<Key, Value>,
        }
        let mut stack = vec![Frame {
            id: self.root,
            lo,
            hi,
            next_child: 0,
            acc: BTreeMap::new(),
        }];
        loop {
            let frame = stack.len() - 1;
            let Node::Internal {
                pivots,
                children,
                buffer,
            } = &self.nodes[stack[frame].id as usize]
            else {
                unreachable!("only internal nodes get frames; leaf children fold inline")
            };
            let i = stack[frame].next_child;
            if i < children.len() {
                stack[frame].next_child += 1;
                let c_lo = if i == 0 {
                    stack[frame].lo.clone()
                } else {
                    clip_lower(&stack[frame].lo, &pivots[i - 1])
                };
                let c_hi = if i == pivots.len() {
                    stack[frame].hi.clone()
                } else {
                    clip_upper(&stack[frame].hi, &pivots[i])
                };
                if range_is_empty(&c_lo, &c_hi) {
                    continue;
                }
                match &self.nodes[children[i] as usize] {
                    Node::Leaf { entries } => {
                        for (k, e) in
                            entries.range::<[u8], _>((bound_as_slice(&c_lo), bound_as_slice(&c_hi)))
                        {
                            stack[frame].acc.insert(k.clone(), e.value.clone());
                        }
                    }
                    Node::Internal { .. } => {
                        let id = children[i];
                        stack.push(Frame {
                            id,
                            lo: c_lo,
                            hi: c_hi,
                            next_child: 0,
                            acc: BTreeMap::new(),
                        });
                    }
                }
            } else {
                // Children done: fold this node's in-range messages onto
                // their union (newer-over-older by I3).
                let mut acc = mem::take(&mut stack[frame].acc);
                let clip = (
                    bound_as_slice(&stack[frame].lo),
                    bound_as_slice(&stack[frame].hi),
                );
                for (k, message) in buffer.range::<[u8], _>(clip) {
                    match message {
                        Message::Put { value, .. } => {
                            acc.insert(k.clone(), value.clone());
                        }
                        Message::Delete { .. } => {
                            acc.remove(k);
                        }
                        Message::Upsert { op, .. } => {
                            let value = op.apply(acc.get(k).map(|v| v.as_slice()));
                            acc.insert(k.clone(), value);
                        }
                    }
                }
                stack.pop();
                match stack.last_mut() {
                    Some(parent) => parent.acc.append(&mut acc),
                    None => return Ok(acc.into_iter().collect()),
                }
            }
        }
    }

    fn get(&mut self, key: &[u8]) -> Option<Value> {
        self.trace.get(key);
        // Walk root→leaf accumulating the pending-upsert chain (SPEC,
        // "Reads"): by I3 every occurrence above is newer, so a Put or
        // Delete terminates the walk (everything below is shadowed) while
        // upserts stack — a running wrapping sum suffices for Add.
        let mut chain: Option<i64> = None;
        let mut id = self.root;
        loop {
            match &self.nodes[id as usize] {
                Node::Internal {
                    pivots,
                    children,
                    buffer,
                } => {
                    match buffer.get(key) {
                        Some(Message::Put { value, .. }) => {
                            return apply_chain(chain, Some(value));
                        }
                        Some(Message::Delete { .. }) => return apply_chain(chain, None),
                        Some(Message::Upsert {
                            op: UpsertOp::Add(delta),
                            ..
                        }) => {
                            chain = Some(chain.unwrap_or(0).wrapping_add(*delta));
                        }
                        None => {}
                    }
                    id = children[route(pivots, key)];
                }
                Node::Leaf { entries } => {
                    return apply_chain(chain, entries.get(key).map(|e| e.value.as_slice()));
                }
            }
        }
    }

    fn check_invariants(&self) -> Result<(), InvariantViolation> {
        check::check_invariants(self)
    }

    fn trace(&self) -> &[TraceEvent] {
        self.trace.v1()
    }

    fn trace2(&self) -> &[TraceEvent2] {
        self.trace.v2()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

#[cfg(test)]
mod m21_tests {
    use super::*;

    fn le(n: i64) -> Value {
        n.to_le_bytes().to_vec()
    }

    /// Node ids along `key`'s root→leaf route.
    fn route_path(tree: &BeTree, key: &[u8]) -> Vec<NodeId> {
        let mut path = vec![tree.root];
        let mut id = tree.root;
        while let Node::Internal {
            pivots, children, ..
        } = &tree.nodes[id as usize]
        {
            id = children[route(pivots, key)];
            path.push(id);
        }
        path
    }

    /// Every reachable node, root first.
    fn reachable(tree: &BeTree) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack = vec![tree.root];
        while let Some(id) = stack.pop() {
            out.push(id);
            if let Node::Internal { children, .. } = &tree.nodes[id as usize] {
                stack.extend(children.iter().copied());
            }
        }
        out
    }

    /// Assert `key` appears in NO reachable buffer and NO reachable leaf.
    fn assert_annihilated(tree: &BeTree, key: &[u8]) {
        for id in reachable(tree) {
            match &tree.nodes[id as usize] {
                Node::Leaf { entries } => assert!(
                    !entries.contains_key(key),
                    "leaf {id} still holds an entry for {key:?}"
                ),
                Node::Internal { buffer, .. } => assert!(
                    !buffer.contains_key(key),
                    "node {id} still buffers a message for {key:?}"
                ),
            }
        }
    }

    /// Drain every internal buffer on `key`'s path (white-box scaffolding:
    /// makes the next flush decisions involving `key` deterministic; the
    /// discarded messages belong to other keys and no invariant requires
    /// their presence).
    fn drain_path_buffers(tree: &mut BeTree, key: &[u8]) {
        for id in route_path(tree, key) {
            if let Node::Internal { buffer, .. } = &mut tree.nodes[id as usize] {
                buffer.clear();
            }
        }
    }

    /// Tombstone lifecycle: a key is inserted and flushed down to a leaf;
    /// its delete then travels the same path and ANNIHILATES at the leaf —
    /// entry gone, no resident tombstone anywhere (the leaf is the
    /// authoritative bottom), I1–I7 green throughout.
    #[test]
    fn tombstone_annihilates_at_the_leaf() {
        let params = Params {
            fanout: 4,
            buffer_capacity: 1,
            leaf_capacity: 4,
        };
        let mut tree = BeTree::new(params);
        let mut i = 0u16;
        while tree.height() < 2 || i < 24 {
            tree.insert((i * 4).to_be_bytes().to_vec(), le(i as i64));
            i += 1;
            assert!(i < 500, "build phase failed to grow the tree");
        }
        // Pick a leaf with at least two entries: the victim plus the twin
        // that pumps the tombstone down without emptying the leaf.
        let (victim, twin) = reachable(&tree)
            .into_iter()
            .find_map(|id| match &tree.nodes[id as usize] {
                Node::Leaf { entries } if entries.len() >= 2 => {
                    let mut keys = entries.keys();
                    Some((
                        keys.next().unwrap().clone(),
                        keys.next_back().unwrap().clone(),
                    ))
                }
                _ => None,
            })
            .expect("some leaf holds two entries");

        drain_path_buffers(&mut tree, &victim);
        tree.delete(victim.clone());
        // The tombstone sits buffered somewhere on the path; pump its
        // full-path twin so the cascade carries both to the leaf.
        tree.insert(twin.clone(), b"pump".to_vec());
        tree.insert(twin.clone(), b"pump2".to_vec());

        assert_eq!(tree.get(&victim), None);
        assert_annihilated(&tree, &victim);
        tree.check_invariants().unwrap();
    }

    /// Reclamation v1 end-to-end: build a multi-level tree, delete
    /// EVERYTHING, and the tree must collapse back to a single empty root
    /// leaf (the initial state) — then keep working. Prints the numbers
    /// the M2.1 report wants.
    #[test]
    fn delete_everything_collapses_to_the_empty_tree() {
        let mut tree = BeTree::new(Params::default());
        for b in 0..=255u8 {
            tree.insert(vec![b], vec![b]);
        }
        let height_before = tree.height();
        let arena_before = tree.node_count();
        let live_before = reachable(&tree).len();
        assert!(height_before >= 3, "256 keys must build a real tree");

        for b in 0..=255u8 {
            tree.delete(vec![b]);
            tree.check_invariants().unwrap();
        }
        // One pass leaves up to B resting tombstones per level: buffers
        // only move on overflow and the root is never force-flushed (SPEC,
        // "Reclamation v1" — a stale tall root is harmless). Further
        // delete passes are semantic no-ops (every key already reads
        // None) whose tombstones drive the resting ones down by ordinary
        // overflow until the structure fully collapses.
        let mut extra_rounds = 0;
        while tree.height() > 1 {
            extra_rounds += 1;
            assert!(extra_rounds <= 8, "reclamation failed to converge");
            for b in 0..=255u8 {
                tree.delete(vec![b]);
            }
            tree.check_invariants().unwrap();
        }
        let height_after = tree.height();
        let arena_after = tree.node_count();
        let live_after = reachable(&tree).len();

        assert_eq!(height_after, 1, "the tree must collapse to a root leaf");
        assert_eq!(live_after, 1, "exactly the empty root leaf remains live");
        match &tree.nodes[tree.root as usize] {
            Node::Leaf { entries } => assert!(entries.is_empty(), "the root leaf must be empty"),
            Node::Internal { .. } => panic!("the root must be a leaf again"),
        }
        for b in 0..=255u8 {
            assert_eq!(tree.get(&[b]), None);
        }
        println!(
            "delete-all experiment: height {height_before} -> {height_after}, \
             live nodes {live_before} -> {live_after}, \
             arena slots {arena_before} -> {arena_after} (leaked by design, ADR-0004); \
             extra idempotent delete passes to drive resting tombstones: {extra_rounds}"
        );

        // The emptied tree is a normal engine again.
        for b in 0..=255u8 {
            tree.insert(vec![b], vec![b, b]);
        }
        tree.check_invariants().unwrap();
        for b in 0..=255u8 {
            assert_eq!(tree.get(&[b]), Some(vec![b, b]));
        }
    }

    /// Stacked upserts across three tree levels, built via controlled
    /// flushes, resolving on a single get: root buffer Upsert(+30), mid
    /// buffer Upsert(+20), leaf entry 10 — get folds them to 60. Prints
    /// the placement and the trace excerpt for the M2.1 report.
    #[test]
    fn upsert_stack_across_three_levels_resolves_on_get() {
        let params = Params {
            fanout: 4,
            buffer_capacity: 1,
            leaf_capacity: 4,
        };
        let mut tree = BeTree::new(params);
        let mut i = 0u16;
        while tree.height() < 3 {
            tree.insert((i * 4).to_be_bytes().to_vec(), le(i as i64));
            i += 1;
            assert!(i < 2000, "build phase failed to reach height 3");
        }

        // K: an odd (never-inserted) key whose depth-2 path node has a
        // lower sibling child to flush decoys into.
        let (key, mid) = (1u16..2000)
            .step_by(2)
            .find_map(|k| {
                let key = k.to_be_bytes().to_vec();
                let path = route_path(&tree, &key);
                let mid = path[1];
                let Node::Internal { pivots, .. } = &tree.nodes[mid as usize] else {
                    return None;
                };
                (route(pivots, &key) >= 1).then_some((key, mid))
            })
            .expect("some key has a lower sibling at depth 2");

        // W: an existing key sharing K's ENTIRE path (a co-tenant of K's
        // leaf) — pumping it cascades a batch all the way down. W2: an
        // existing key under `mid`'s next-lower child — same root child as
        // K, lower child inside `mid`, so the tie-break flushes W2's range
        // and K's upsert PARKS at `mid`.
        let path = route_path(&tree, &key);
        let leaf = *path.last().unwrap();
        let Node::Leaf { entries } = &tree.nodes[leaf as usize] else {
            unreachable!()
        };
        let w = entries.keys().next().expect("leaves are non-empty").clone();
        let Node::Internal {
            pivots, children, ..
        } = &tree.nodes[mid as usize]
        else {
            unreachable!()
        };
        let lower_sibling = children[route(pivots, &key) - 1];
        let mut probe = lower_sibling;
        let w2 = loop {
            match &tree.nodes[probe as usize] {
                Node::Internal { children, .. } => probe = children[0],
                Node::Leaf { entries } => {
                    break entries.keys().next().expect("leaves are non-empty").clone();
                }
            }
        };

        // Level 3: drive the first upsert all the way to the leaf.
        drain_path_buffers(&mut tree, &key);
        tree.upsert(key.clone(), UpsertOp::Add(10));
        tree.insert(w.clone(), b"pump".to_vec());
        let path = route_path(&tree, &key);
        let leaf = *path.last().unwrap();
        match &tree.nodes[leaf as usize] {
            Node::Leaf { entries } => assert_eq!(
                entries.get(&key).map(|e| e.value.clone()),
                Some(le(10)),
                "the first upsert must have materialized at the leaf"
            ),
            Node::Internal { .. } => unreachable!(),
        }

        // Level 2: park the second upsert at `mid`.
        drain_path_buffers(&mut tree, &key);
        tree.upsert(key.clone(), UpsertOp::Add(20));
        tree.insert(w2.clone(), b"pump".to_vec());
        let mid = route_path(&tree, &key)[1];
        match &tree.nodes[mid as usize] {
            Node::Internal { buffer, .. } => assert!(
                matches!(
                    buffer.get(&key),
                    Some(Message::Upsert {
                        op: UpsertOp::Add(20),
                        ..
                    })
                ),
                "the second upsert must be parked in the mid-level buffer, found {:?}",
                buffer.get(&key)
            ),
            Node::Leaf { .. } => unreachable!(),
        }

        // Level 1: the third upsert rests in the root buffer.
        tree.upsert(key.clone(), UpsertOp::Add(30));
        match &tree.nodes[tree.root as usize] {
            Node::Internal { buffer, .. } => assert!(
                matches!(
                    buffer.get(&key),
                    Some(Message::Upsert {
                        op: UpsertOp::Add(30),
                        ..
                    })
                ),
                "the third upsert must rest in the root buffer"
            ),
            Node::Leaf { .. } => unreachable!(),
        }

        // One get folds the whole stack: 10 + 20 + 30 — and one scan
        // folds it identically (M2.2, bottom-up application).
        assert_eq!(tree.get(&key), Some(le(60)));
        let scanned = tree.scan(Bound::Unbounded, Bound::Unbounded).unwrap();
        assert_eq!(
            scanned.iter().find(|(k, _)| k == &key).map(|(_, v)| v),
            Some(&le(60)),
            "scan must fold the same 3-level upsert stack"
        );
        tree.check_invariants().unwrap();

        println!(
            "3-level upsert stack on key {key:?}: root buffer Add(30) @ node {}, \
             mid buffer Add(20) @ node {mid}, leaf entry 10 @ node {leaf}; \
             get folded them to 60",
            tree.root
        );
        let trace = tree.trace2();
        println!("trace excerpt (last 8 events):");
        for event in &trace[trace.len().saturating_sub(8)..] {
            println!("  {event:?}");
        }
    }
}

#[cfg(test)]
mod m22_tests {
    use super::*;

    fn le(n: i64) -> Value {
        n.to_le_bytes().to_vec()
    }

    /// Node ids along `key`'s root→leaf route.
    fn route_path(tree: &BeTree, key: &[u8]) -> Vec<NodeId> {
        let mut path = vec![tree.root];
        let mut id = tree.root;
        while let Node::Internal {
            pivots, children, ..
        } = &tree.nodes[id as usize]
        {
            id = children[route(pivots, key)];
            path.push(id);
        }
        path
    }

    /// Every reachable node, root first.
    fn reachable(tree: &BeTree) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack = vec![tree.root];
        while let Some(id) = stack.pop() {
            out.push(id);
            if let Node::Internal { children, .. } = &tree.nodes[id as usize] {
                stack.extend(children.iter().copied());
            }
        }
        out
    }

    /// Drain every internal buffer on `key`'s path (white-box scaffolding;
    /// see `m21_tests`).
    fn drain_path_buffers(tree: &mut BeTree, key: &[u8]) {
        for id in route_path(tree, key) {
            if let Node::Internal { buffer, .. } = &mut tree.nodes[id as usize] {
                buffer.clear();
            }
        }
    }

    /// Build a height-3 tree under B=1 (every flush controllable) over
    /// 4-spaced u16 keys with LE values.
    fn build_controlled() -> BeTree {
        let params = Params {
            fanout: 4,
            buffer_capacity: 1,
            leaf_capacity: 4,
        };
        let mut tree = BeTree::new(params);
        let mut i = 0u16;
        while tree.height() < 3 {
            tree.insert((i * 4).to_be_bytes().to_vec(), le(i as i64));
            i += 1;
            assert!(i < 2000, "build phase failed to reach height 3");
        }
        tree
    }

    /// Whether `key` is buffered (as any message kind) at node `id`.
    fn buffered_at(tree: &BeTree, id: NodeId, key: &[u8]) -> bool {
        match &tree.nodes[id as usize] {
            Node::Internal { buffer, .. } => buffer.contains_key(key),
            Node::Leaf { .. } => false,
        }
    }

    fn scan_all(tree: &mut BeTree) -> Vec<(Key, Value)> {
        tree.scan(Bound::Unbounded, Bound::Unbounded).unwrap()
    }

    fn scan_has(scanned: &[(Key, Value)], key: &[u8]) -> bool {
        scanned.iter().any(|(k, _)| k == key)
    }

    /// A tombstone in transit must suppress its key from scans at EVERY
    /// level it rests at: root buffer, mid buffer, and finally the leaf
    /// (where it annihilates).
    #[test]
    fn scan_suppresses_a_tombstone_at_every_level() {
        let mut tree = build_controlled();

        // The victim and two co-tenants of its leaf (pump fuel), plus a
        // key under the mid node's next-lower child (parking fuel).
        let victim_path = route_path(&tree, &40u16.to_be_bytes());
        let leaf = *victim_path.last().unwrap();
        let mid = victim_path[1];
        let Node::Leaf { entries } = &tree.nodes[leaf as usize] else {
            unreachable!()
        };
        assert!(entries.len() >= 3, "need a victim plus two pump keys");
        let mut keys = entries.keys().cloned();
        let victim = keys.next().unwrap();
        let pump_a = keys.next().unwrap();
        let pump_b = keys.next().unwrap();
        let Node::Internal {
            pivots, children, ..
        } = &tree.nodes[mid as usize]
        else {
            unreachable!()
        };
        let g = route(pivots, &victim);
        assert!(g >= 1, "victim must have a lower sibling at the mid node");
        let mut probe = children[g - 1];
        let parking_fuel = loop {
            match &tree.nodes[probe as usize] {
                Node::Internal { children, .. } => probe = children[0],
                Node::Leaf { entries } => break entries.keys().next().unwrap().clone(),
            }
        };

        // Level 1: tombstone resting in the ROOT buffer.
        drain_path_buffers(&mut tree, &victim);
        tree.delete(victim.clone());
        assert!(buffered_at(&tree, tree.root, &victim));
        let scanned = scan_all(&mut tree);
        assert!(!scan_has(&scanned, &victim), "root tombstone must suppress");
        assert_eq!(tree.get(&victim), None);

        // Level 2: parked in the MID buffer (the parking fuel flushes
        // first on the tie-break, stranding the tombstone).
        tree.insert(parking_fuel.clone(), b"fuel".to_vec());
        let mid = route_path(&tree, &victim)[1];
        assert!(
            buffered_at(&tree, mid, &victim),
            "the tombstone must be parked at the mid level"
        );
        let scanned = scan_all(&mut tree);
        assert!(!scan_has(&scanned, &victim), "mid tombstone must suppress");
        assert_eq!(tree.get(&victim), None);

        // Level 3: driven into the leaf, where it annihilates.
        tree.insert(pump_a.clone(), b"fuel2".to_vec());
        tree.insert(pump_b.clone(), b"fuel3".to_vec());
        let path = route_path(&tree, &victim);
        let leaf = *path.last().unwrap();
        assert!(
            !path.iter().any(|&id| buffered_at(&tree, id, &victim)),
            "no tombstone may remain buffered on the path"
        );
        match &tree.nodes[leaf as usize] {
            Node::Leaf { entries } => assert!(!entries.contains_key(&victim)),
            Node::Internal { .. } => unreachable!(),
        }
        let scanned = scan_all(&mut tree);
        assert!(!scan_has(&scanned, &victim), "annihilated key stays gone");
        tree.check_invariants().unwrap();
    }

    /// The report excerpt: ONE scan resolving both a 3-level upsert stack
    /// and a tombstone-in-transit in a single pass, printed with the
    /// trace2 tail (`--nocapture`).
    #[test]
    fn scan_resolves_stack_and_tombstone_in_one_pass() {
        let mut tree = build_controlled();

        // A 3-level upsert stack on a never-inserted key (as in
        // m21_tests::upsert_stack_across_three_levels_resolves_on_get).
        let (key, mid) = (1u16..2000)
            .step_by(2)
            .find_map(|k| {
                let key = k.to_be_bytes().to_vec();
                let path = route_path(&tree, &key);
                let mid = path[1];
                let Node::Internal { pivots, .. } = &tree.nodes[mid as usize] else {
                    return None;
                };
                (route(pivots, &key) >= 1).then_some((key, mid))
            })
            .expect("some key has a lower sibling at depth 2");
        let path = route_path(&tree, &key);
        let leaf = *path.last().unwrap();
        let Node::Leaf { entries } = &tree.nodes[leaf as usize] else {
            unreachable!()
        };
        let w = entries.keys().next().expect("leaves are non-empty").clone();
        let Node::Internal {
            pivots, children, ..
        } = &tree.nodes[mid as usize]
        else {
            unreachable!()
        };
        let mut probe = children[route(pivots, &key) - 1];
        let w2 = loop {
            match &tree.nodes[probe as usize] {
                Node::Internal { children, .. } => probe = children[0],
                Node::Leaf { entries } => break entries.keys().next().unwrap().clone(),
            }
        };
        drain_path_buffers(&mut tree, &key);
        tree.upsert(key.clone(), UpsertOp::Add(10));
        tree.insert(w.clone(), b"pump".to_vec());
        drain_path_buffers(&mut tree, &key);
        tree.upsert(key.clone(), UpsertOp::Add(20));
        tree.insert(w2.clone(), b"pump".to_vec());
        tree.upsert(key.clone(), UpsertOp::Add(30));

        // A tombstone in transit: delete an existing key far from `key`'s
        // subtree; it rests in the root buffer.
        let victim = {
            let Node::Internal { children, .. } = &tree.nodes[tree.root as usize] else {
                unreachable!()
            };
            let other = *children
                .iter()
                .find(|&&c| c != route_path(&tree, &key)[1])
                .expect("the root has more than one child");
            let mut probe = other;
            loop {
                match &tree.nodes[probe as usize] {
                    Node::Internal { children, .. } => probe = children[0],
                    Node::Leaf { entries } => break entries.keys().next().unwrap().clone(),
                }
            }
        };
        tree.delete(victim.clone());
        assert!(buffered_at(&tree, tree.root, &victim));

        // ONE scan resolves both in a single bottom-up pass. The trace
        // excerpt is captured immediately, before the verification gets
        // append their own events.
        let scanned = scan_all(&mut tree);
        println!(
            "one-pass scan: upsert stack on {key:?} folded to 60; \
             in-transit tombstone on {victim:?} suppressed; \
             {} keys returned",
            scanned.len()
        );
        let trace = tree.trace2();
        println!("trace2 excerpt (last 7 events, ending in the scan):");
        for event in &trace[trace.len().saturating_sub(7)..] {
            println!("  {event:?}");
        }

        assert_eq!(
            scanned.iter().find(|(k, _)| k == &key).map(|(_, v)| v),
            Some(&le(60)),
            "the scan must fold the 3-level upsert stack"
        );
        assert!(
            !scan_has(&scanned, &victim),
            "the scan must suppress the in-transit tombstone"
        );
        // Scan agrees with get on every scanned key.
        for (k, v) in &scanned {
            assert_eq!(tree.get(k), Some(v.clone()), "scan/get diverge at {k:?}");
        }
        assert_eq!(tree.get(&victim), None);
        tree.check_invariants().unwrap();
    }

    /// Regression (M3.1 review): drain must reach resting messages that
    /// sit BELOW an internal node whose own buffer is empty — a
    /// delivery-driven descent alone never visits such a child.
    #[test]
    fn drain_reaches_messages_below_empty_buffers() {
        let mut tree = BeTree::new(Params::default());
        for b in 0..=255u8 {
            tree.insert(vec![b], vec![b]);
        }
        assert!(tree.height() >= 3, "need an intermediate level");
        // Empty every internal buffer, then plant one message at a
        // leaf-parent: its ancestors all have EMPTY buffers.
        for id in reachable(&tree) {
            if let Node::Internal { buffer, .. } = &mut tree.nodes[id as usize] {
                buffer.clear();
            }
        }
        let path = route_path(&tree, &[0]);
        let leaf_parent = path[path.len() - 2];
        let key = vec![0u8];
        tree.next_seq += 1;
        let seq = tree.next_seq;
        let Node::Internal { buffer, .. } = &mut tree.nodes[leaf_parent as usize] else {
            unreachable!("the second-to-last path node is internal")
        };
        buffer.insert(
            key.clone(),
            Message::Put {
                seq,
                value: b"deep".to_vec(),
            },
        );
        tree.check_invariants().unwrap();

        tree.drain();

        for id in reachable(&tree) {
            if let Node::Internal { buffer, .. } = &tree.nodes[id as usize] {
                assert!(
                    buffer.is_empty(),
                    "node {id} still buffers after drain — the planted message survived"
                );
            }
        }
        assert_eq!(tree.get(&key), Some(b"deep".to_vec()));
        tree.check_invariants().unwrap();
    }

    /// drain() force-flushes every buffer to the leaves: afterwards the
    /// tree is message-free, invariants hold, reads are unchanged, and —
    /// because drain is OUTSIDE the performance model — the trace gained
    /// NOTHING (no FlushDecision events; SPEC "Observability").
    #[test]
    fn drain_leaves_a_message_free_tree_and_no_trace() {
        let mut tree = BeTree::new(Params::default());
        let mut oracle: BTreeMap<Key, Value> = BTreeMap::new();
        for i in 0..600u16 {
            let key = vec![(i % 200) as u8];
            tree.insert(key.clone(), le(i as i64));
            oracle.insert(key, le(i as i64));
        }
        for b in (0..200u8).step_by(3) {
            tree.delete(vec![b]);
            oracle.remove(&vec![b]);
        }
        for b in (0..200u8).step_by(5) {
            tree.upsert(vec![b], UpsertOp::Add(7));
            let base = match oracle.get(&vec![b]) {
                Some(v) if v.len() == 8 => i64::from_le_bytes(v.as_slice().try_into().unwrap()),
                _ => 0,
            };
            oracle.insert(vec![b], le(base.wrapping_add(7)));
        }
        // Some buffers must be non-empty pre-drain or the test is vacuous.
        let buffered_pre: usize = reachable(&tree)
            .iter()
            .map(|&id| match &tree.nodes[id as usize] {
                Node::Internal { buffer, .. } => buffer.len(),
                Node::Leaf { .. } => 0,
            })
            .sum();
        assert!(buffered_pre > 0, "the workload must leave resting messages");
        let trace_len_pre = tree.trace2().len();

        tree.drain();

        for id in reachable(&tree) {
            if let Node::Internal { buffer, .. } = &tree.nodes[id as usize] {
                assert!(buffer.is_empty(), "node {id} still buffers after drain");
            }
        }
        tree.check_invariants().unwrap();
        assert_eq!(
            tree.trace2().len(),
            trace_len_pre,
            "drain must be invisible to traces"
        );
        let scanned = tree.scan(Bound::Unbounded, Bound::Unbounded).unwrap();
        let expected: Vec<(Key, Value)> =
            oracle.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(scanned, expected, "drain must not change the contents");
    }

    /// Scan across a leaf boundary and a reclamation-produced gap: empty
    /// out a middle leaf, let reclamation absorb its range, then scan
    /// straddling the gap — neighbors only, no phantoms, no misses.
    #[test]
    fn scan_spans_leaf_boundaries_and_reclamation_gaps() {
        let mut tree = build_controlled();
        let mut oracle: BTreeMap<Key, Value> = BTreeMap::new();
        for id in 0..tree.nodes.len() as NodeId {
            if let Node::Leaf { entries } = &tree.nodes[id as usize] {
                if route_path(&tree, entries.keys().next().unwrap()).last() == Some(&id) {
                    for (k, e) in entries {
                        oracle.insert(k.clone(), e.value.clone());
                    }
                }
            }
        }

        // The middle leaf of `mid`'s children empties and gets absorbed.
        let probe_key = 40u16.to_be_bytes().to_vec();
        let mid = route_path(&tree, &probe_key)[1];
        let Node::Internal { children, .. } = &tree.nodes[mid as usize] else {
            unreachable!()
        };
        assert!(children.len() >= 3, "need a genuine middle leaf");
        let gap_leaf = children[1];
        let Node::Leaf { entries } = &tree.nodes[gap_leaf as usize] else {
            unreachable!()
        };
        let gap_keys: Vec<Key> = entries.keys().cloned().collect();
        for k in &gap_keys {
            oracle.remove(k);
            tree.delete(k.clone());
        }
        // Drive the resting tombstones down until the leaf is reclaimed
        // (idempotent re-deletes; SPEC "Reclamation v1" caveat).
        let mut rounds = 0;
        while route_path(&tree, &gap_keys[0]).contains(&gap_leaf) {
            rounds += 1;
            assert!(rounds <= 8, "reclamation failed to converge");
            for k in &gap_keys {
                tree.delete(k.clone());
            }
        }
        tree.check_invariants().unwrap();

        // Full scan equals the oracle exactly (boundary keys included).
        let scanned = scan_all(&mut tree);
        let expected: Vec<(Key, Value)> =
            oracle.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(scanned, expected, "full scan across the gap diverged");

        // A narrow scan straddling the absorbed range: from the last
        // surviving key below the gap to the first above it.
        let below = oracle.range(..gap_keys[0].clone()).next_back().unwrap();
        let above = oracle
            .range(gap_keys.last().unwrap().clone()..)
            .next()
            .unwrap();
        let narrow = tree
            .scan(
                Bound::Included(below.0.clone()),
                Bound::Included(above.0.clone()),
            )
            .unwrap();
        assert_eq!(
            narrow,
            vec![
                (below.0.clone(), below.1.clone()),
                (above.0.clone(), above.1.clone())
            ],
            "the straddling scan must return exactly the gap's neighbors"
        );
    }
}
