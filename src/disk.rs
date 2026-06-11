//! The disk-backed Bε-tree engine (M1.1): copy-on-write nodes in a single
//! file, dual-superblock atomic commits, recovery — and no write-ahead log
//! by design (ADR-0007).
//!
//! Between commits, [`DiskEngine`] is the in-memory engine: the same
//! greedy-fullest flush policy, message algebra, and reclamation
//! (deliberately mirrored from `src/betree.rs`, slot bookkeeping aside),
//! the same P1–P5/Q1–Q5 semantics under the harnesses. `commit()` makes the
//! current state durable by appending every dirty node to the data region
//! (children before parents), syncing, then publishing the new root through
//! the inactive superblock slot (ADR-0008). `open()` resumes from the
//! newest valid superblock; everything after it on disk is a torn tail and
//! is truncated away.
//!
//! All I/O goes through [`Vfs`] (ADR-0009) so M1.2 can inject faults.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::mem;

use thiserror::Error;

use crate::check::{self, NodeSource};
use crate::engine::{EngineError, KvEngine};
use crate::format::{
    DATA_START, DiskNode, FORMAT_VERSION, MAGIC, RECORD_HEADER_SIZE, SUPERBLOCK_SLOT_SIZE,
    SUPERBLOCK_SLOTS, SlotStatus, Superblock, decode_node, encode_node,
};
use crate::node::{
    Delivery, LeafEntry, Node, NodeId, Outcome, apply_chain, apply_to_leaf, bound_as_slice,
    clip_lower, clip_upper, coalesce_into, partition_sizes, range_is_empty, route,
};
use crate::trace::{OpKind2, Recorder, TraceEvent, TraceEvent2};
use crate::types::{InvariantViolation, Key, Message, Params, UpsertOp, Value};
use crate::vfs::Vfs;

/// A storage-layer failure: real I/O errors, detected corruption, or a
/// file that is not a usable database.
///
/// Corruption is always DETECTED and typed — a [`DiskEngine`] never panics
/// on bad bytes and never returns wrong data (SPEC, "Durability contract").
#[derive(Debug, Error)]
pub enum DiskError {
    /// The underlying [`Vfs`] failed.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// A node record failed validation (bounds, checksum, or decoding).
    /// The committed data at this offset cannot be trusted; the read is
    /// refused rather than answered.
    #[error("corrupt node record at offset {offset}: {reason}")]
    CorruptNode {
        /// File offset of the record that failed validation.
        offset: u64,
        /// What exactly failed, for diagnostics.
        reason: String,
    },

    /// Neither superblock slot holds a valid superblock: the file is not a
    /// database this build can open.
    #[error("no valid superblock in either slot")]
    NoValidSuperblock,

    /// The file is a real database, but of a different on-disk format
    /// version. There is no migration pre-release (ADR-0012).
    #[error("unsupported on-disk format version {found} (this build reads v{FORMAT_VERSION})")]
    UnsupportedVersion {
        /// The format version found in the file's superblock.
        found: u32,
    },

    /// `create()` refuses to clobber an existing non-empty file.
    #[error("refusing to create a database over {len} bytes of existing data")]
    NotEmpty {
        /// Length of the existing file.
        len: u64,
    },

    /// A serialized node exceeded the u32 record-length field (SPEC,
    /// "Node records"). The commit is refused before anything is written:
    /// truncating the length silently would corrupt the record at write
    /// time and only surface at read time, when the data is already lost.
    #[error("a serialized node of {bytes} bytes exceeds the u32 record-length limit")]
    NodeTooLarge {
        /// Size of the offending node payload.
        bytes: usize,
    },

    /// An earlier storage error interrupted a mutation or a commit, so the
    /// in-memory state can no longer be trusted to match any committable
    /// whole (an interrupted flush may have moved committed messages out
    /// of a buffer; a failed commit may have half-published a generation).
    /// Further commits are refused; reads stay best-effort. Reopen the
    /// file to recover the last committed state.
    #[error("engine poisoned by an earlier storage error; reopen to recover")]
    Poisoned,
}

/// What one [`DiskEngine::commit`] wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitStats {
    /// Dirty nodes serialized to new records (copy-on-write).
    pub nodes_written: usize,
    /// Total bytes written: appended node records plus the 4096-byte
    /// superblock slot.
    pub bytes_written: u64,
}

/// One arena slot: a node either lives in memory or is a record offset
/// waiting to be loaded on first access.
enum NodeSlot {
    /// Resident node. `dirty` means it has changed since it was last
    /// written (or has never been written); `disk_offset` is its current
    /// record, if any. A clean loaded node ALWAYS has a disk offset.
    Loaded {
        node: Node,
        dirty: bool,
        disk_offset: Option<u64>,
    },
    /// Not yet loaded; the record lives at this offset.
    OnDisk { offset: u64 },
}

/// A disk-backed Bε-tree (`docs/SPEC.md`; M1.1).
///
/// ```
/// use beetree::{DiskEngine, KvEngine, Params};
///
/// let dir = tempfile::tempdir().unwrap();
/// let path = dir.path().join("bee.db");
///
/// let mut tree = DiskEngine::create(&path, Params::default()).unwrap();
/// tree.insert(b"k".to_vec(), b"v".to_vec());
/// tree.commit().unwrap();
/// drop(tree);
///
/// let mut tree = DiskEngine::open(&path).unwrap();
/// assert_eq!(tree.get(b"k"), Some(b"v".to_vec()));
/// ```
///
/// The [`KvEngine`] surface treats storage failure as fatal (it panics on
/// I/O errors and detected corruption); callers that must survive a sick
/// disk use the fallible twins [`try_insert`](DiskEngine::try_insert) and
/// [`try_get`](DiskEngine::try_get). A storage error that interrupts a
/// mutation or a commit POISONS the engine: in-memory reads stay
/// best-effort, but further commits refuse with
/// [`DiskError::Poisoned`] — the torn in-memory state must never become
/// durable. The committed on-disk state is unaffected; reopen to recover
/// it.
pub struct DiskEngine<V: Vfs> {
    params: Params,
    vfs: V,
    /// Arena of slots; in-memory child references are [`NodeId`] indices
    /// into this, loaded lazily from their record offsets.
    slots: Vec<NodeSlot>,
    root: NodeId,
    next_seq: u64,
    /// Generation the NEXT commit will write; generation g goes to
    /// superblock slot g mod 2.
    next_generation: u64,
    /// One past the end of valid data; the next commit appends here.
    watermark: u64,
    /// Set when a storage error interrupts a mutation or a commit; from
    /// then on `commit()` refuses with [`DiskError::Poisoned`].
    poisoned: bool,
    trace: Recorder,
}

/// Summarized by hand: the slot arena is far too large to dump, and the
/// [`Vfs`] need not be `Debug`.
impl<V: Vfs> fmt::Debug for DiskEngine<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiskEngine")
            .field("params", &self.params)
            .field("root", &self.root)
            .field("slots", &self.slots.len())
            .field("next_seq", &self.next_seq)
            .field("next_generation", &self.next_generation)
            .field("watermark", &self.watermark)
            .finish_non_exhaustive()
    }
}

#[cfg(unix)]
impl DiskEngine<crate::vfs::FileVfs> {
    /// Create a new database file at `path` with the given parameters and
    /// durably commit generation 0 (an empty tree) before returning.
    /// Errors with [`DiskError::NotEmpty`] if the file exists and is
    /// non-empty.
    ///
    /// # Panics
    ///
    /// On illegal params (need F ≥ 2, B ≥ 1, L ≥ 1), like every engine.
    pub fn create(path: impl AsRef<std::path::Path>, params: Params) -> Result<Self, DiskError> {
        Self::create_on(
            crate::vfs::FileVfs::create(path).map_err(DiskError::Io)?,
            params,
        )
    }

    /// Open an existing database file, resuming from the newest valid
    /// superblock (SPEC, "Durability contract"). The root loads lazily.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, DiskError> {
        Self::open_on(crate::vfs::FileVfs::open(path).map_err(DiskError::Io)?)
    }
}

impl<V: Vfs> DiskEngine<V> {
    /// [`create`](DiskEngine::create) over an arbitrary [`Vfs`] (the M1.2
    /// fault-injection entry point).
    pub fn create_on(vfs: V, params: Params) -> Result<Self, DiskError> {
        assert!(
            params.fanout >= 2 && params.buffer_capacity >= 1 && params.leaf_capacity >= 1,
            "illegal Params (need F >= 2, B >= 1, L >= 1): {params:?}"
        );
        let len = vfs.len()?;
        if len > 0 {
            return Err(DiskError::NotEmpty { len });
        }
        let mut engine = DiskEngine {
            params,
            vfs,
            slots: vec![NodeSlot::Loaded {
                node: Node::Leaf {
                    entries: BTreeMap::new(),
                },
                dirty: true,
                disk_offset: None,
            }],
            root: 0,
            next_seq: 0,
            next_generation: 0,
            watermark: DATA_START,
            poisoned: false,
            trace: Recorder::default(),
        };
        engine.commit()?;
        Ok(engine)
    }

    /// [`open`](DiskEngine::open) over an arbitrary [`Vfs`].
    pub fn open_on(mut vfs: V) -> Result<Self, DiskError> {
        let file_len = vfs.len()?;
        let mut best: Option<Superblock> = None;
        let mut wrong_version: Option<u32> = None;
        for &slot_offset in &SUPERBLOCK_SLOTS {
            if file_len < slot_offset + SUPERBLOCK_SLOT_SIZE {
                continue;
            }
            let mut slot = vec![0u8; SUPERBLOCK_SLOT_SIZE as usize];
            vfs.read_exact_at(slot_offset, &mut slot)?;
            match Superblock::decode_slot(&slot) {
                SlotStatus::Valid(sb) => {
                    // Data is synced before the superblock that points at
                    // it, so an honest slot can never claim more bytes
                    // than the file holds. One that does (external
                    // truncation, crafted bytes) is invalid — picking it
                    // would make the set_len below zero-EXTEND the file
                    // instead of dropping a tail.
                    if sb.watermark <= file_len && best.is_none_or(|b| sb.generation > b.generation)
                    {
                        best = Some(sb);
                    }
                }
                SlotStatus::WrongVersion(found) => wrong_version = Some(found),
                SlotStatus::Invalid => {}
            }
        }
        // The version gate is UNCONDITIONAL: an authenticated
        // other-version slot refuses the whole file even when the other
        // slot holds a valid v2 superblock (ADR-0012). Opening the v2
        // slot instead would silently roll back to a stale generation —
        // and the next commit would overwrite the newer foreign
        // superblock. A torn v2 write can never authenticate as a foreign
        // version (the full-slot crc must check first), so refusing here
        // has no false positives.
        if let Some(found) = wrong_version {
            return Err(DiskError::UnsupportedVersion { found });
        }
        let sb = best.ok_or(DiskError::NoValidSuperblock)?;
        // Drop any torn tail beyond the committed watermark: those bytes
        // were appended by a commit whose superblock never landed.
        vfs.set_len(sb.watermark)?;
        Ok(DiskEngine {
            params: sb.params,
            vfs,
            slots: vec![NodeSlot::OnDisk {
                offset: sb.root_offset,
            }],
            root: 0,
            next_seq: sb.last_seq,
            next_generation: sb.generation + 1,
            watermark: sb.watermark,
            poisoned: false,
            trace: Recorder::default(),
        })
    }

    /// The structure parameters this database runs under (persisted in the
    /// superblock since M1.1).
    pub fn params(&self) -> Params {
        self.params
    }

    /// The generation of the newest committed (or recovered) state — a
    /// non-normative observability helper, like `BeTree::height()`.
    /// `create()` leaves it at 0; every successful commit increments it;
    /// `open()` reports the generation it recovered.
    pub fn generation(&self) -> u64 {
        self.next_generation - 1
    }

    /// Current length of the backing file in bytes.
    pub fn file_len(&self) -> Result<u64, DiskError> {
        Ok(self.vfs.len()?)
    }

    /// Make everything since the last commit durable (SPEC, "Durability
    /// contract"): append all dirty nodes as new records — children before
    /// parents, so every stored pointer leads to already-written bytes —
    /// sync, publish the new root via the inactive superblock slot, sync
    /// again.
    ///
    /// A commit that errors POISONS the engine: the previously committed
    /// generation is intact, but whether the failed one became durable is
    /// unknowable (the lost-ack window), and retrying in place could
    /// overwrite records a durable superblock already points at. Further
    /// commits refuse with [`DiskError::Poisoned`]; reopen to recover.
    pub fn commit(&mut self) -> Result<CommitStats, DiskError> {
        if self.poisoned {
            return Err(DiskError::Poisoned);
        }
        let result = self.commit_inner();
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    fn commit_inner(&mut self) -> Result<CommitStats, DiskError> {
        // Dirty nodes form a connected subtree at the root (every mutation
        // path dirties everything above it), so walking dirty-only edges
        // finds them all. `pre` lists parents before descendants; an
        // explicit stack, as everywhere: degenerate F=2 trees are linearly
        // tall.
        let mut pre: Vec<NodeId> = Vec::new();
        if self.is_dirty(self.root) {
            let mut visit = vec![self.root];
            while let Some(id) = visit.pop() {
                pre.push(id);
                if let Node::Internal { children, .. } = self.loaded(id) {
                    visit.extend(children.iter().copied().filter(|&c| self.is_dirty(c)));
                }
            }
        }

        // 1. Serialize children before parents (reverse of `pre`),
        //    resolving child NodeIds to record offsets: clean children keep
        //    their stored offset, dirty children take the offset assigned
        //    moments ago in this very loop.
        let base = self.watermark;
        let mut buf: Vec<u8> = Vec::new();
        let mut assigned: BTreeMap<NodeId, u64> = BTreeMap::new();
        for &id in pre.iter().rev() {
            let offset = base + buf.len() as u64;
            let payload = self.node_payload(id, &assigned)?;
            if payload.len() > u32::MAX as usize {
                // Refuse rather than silently truncate the len field: the
                // record would checksum-fail on every future read, turning
                // a "successful" commit into data loss (SPEC, "Node
                // records"). Nothing has been written yet at this point.
                return Err(DiskError::NodeTooLarge {
                    bytes: payload.len(),
                });
            }
            buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            buf.extend_from_slice(&crc32fast::hash(&payload).to_le_bytes());
            buf.extend_from_slice(&payload);
            assigned.insert(id, offset);
        }
        if !buf.is_empty() {
            self.vfs.write_all_at(base, &buf)?;
        }
        // 2. Data durable before any pointer to it exists.
        self.vfs.sync()?;

        // 3. Publish through the INACTIVE slot; the active slot flips by
        //    generation, not by position.
        let watermark = base + buf.len() as u64;
        let sb = Superblock {
            magic: MAGIC,
            format_version: FORMAT_VERSION,
            params: self.params,
            last_seq: self.next_seq,
            generation: self.next_generation,
            root_offset: self.resolve_offset(self.root, &assigned),
            watermark,
        };
        let slot = SUPERBLOCK_SLOTS[(self.next_generation % 2) as usize];
        self.vfs.write_all_at(slot, &sb.encode_slot()?)?;
        // 4. The new generation is durable; only now adopt it in memory.
        self.vfs.sync()?;

        self.watermark = watermark;
        self.next_generation += 1;
        let nodes_written = assigned.len();
        for (id, off) in assigned {
            match &mut self.slots[id as usize] {
                NodeSlot::Loaded {
                    dirty, disk_offset, ..
                } => {
                    *dirty = false;
                    *disk_offset = Some(off);
                }
                NodeSlot::OnDisk { .. } => unreachable!("only resident nodes are written"),
            }
        }
        Ok(CommitStats {
            nodes_written,
            bytes_written: buf.len() as u64 + SUPERBLOCK_SLOT_SIZE,
        })
    }

    /// Insert or overwrite, reporting storage failures instead of
    /// panicking. An error AFTER the mutation began (a load failure inside
    /// the flush cascade) poisons the engine — an interrupted flush can
    /// have moved committed messages out of a buffer, so committing the
    /// torn in-memory state would lose them; reopen to recover. An error
    /// loading the root happens before anything mutates and is clean.
    pub fn try_insert(&mut self, key: Key, value: Value) -> Result<(), DiskError> {
        self.ensure_loaded(self.root)?;
        self.next_seq += 1;
        let seq = self.next_seq;
        self.trace.op(
            seq,
            OpKind2::Insert {
                key: key.clone(),
                value: value.clone(),
            },
        );
        self.apply_root(key, Message::Put { seq, value })
    }

    /// Remove a key, reporting storage failures instead of panicking; the
    /// same error/poisoning semantics as [`DiskEngine::try_insert`].
    pub fn try_delete(&mut self, key: Key) -> Result<(), DiskError> {
        self.ensure_loaded(self.root)?;
        self.next_seq += 1;
        let seq = self.next_seq;
        self.trace.op(seq, OpKind2::Delete { key: key.clone() });
        self.apply_root(key, Message::Delete { seq })
    }

    /// Blindly transform a value, reporting storage failures instead of
    /// panicking; the same error/poisoning semantics as
    /// [`DiskEngine::try_insert`].
    pub fn try_upsert(&mut self, key: Key, op: UpsertOp) -> Result<(), DiskError> {
        self.ensure_loaded(self.root)?;
        self.next_seq += 1;
        let seq = self.next_seq;
        self.trace.op(
            seq,
            OpKind2::Upsert {
                key: key.clone(),
                op,
            },
        );
        self.apply_root(key, Message::Upsert { seq, op })
    }

    /// The shared tail of every public mutating op: route the message at
    /// the (already loaded) root and re-settle the tree, poisoning the
    /// engine if the cascade errors mid-mutation.
    fn apply_root(&mut self, key: Key, message: Message) -> Result<(), DiskError> {
        match self.loaded_mut(self.root) {
            Node::Leaf { entries } => apply_to_leaf(entries, key, message),
            Node::Internal { buffer, .. } => coalesce_into(buffer, key, message),
        }
        let result = self.restore_root();
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    /// The newest value for `key`, reporting storage failures — including
    /// detected corruption — instead of panicking. Never wrong data: a
    /// record that fails validation is an error, not an answer.
    pub fn try_get(&mut self, key: &[u8]) -> Result<Option<Value>, DiskError> {
        self.trace.get(key);
        // Walk root→leaf accumulating the pending-upsert chain (SPEC,
        // "Reads"): by I3 every occurrence above is newer, so a Put or
        // Delete terminates the walk (everything below is shadowed) while
        // upserts stack — a running wrapping sum suffices for Add.
        let mut chain: Option<i64> = None;
        let mut id = self.root;
        loop {
            self.ensure_loaded(id)?;
            match self.loaded(id) {
                Node::Internal {
                    pivots,
                    children,
                    buffer,
                } => {
                    match buffer.get(key) {
                        Some(Message::Put { value, .. }) => {
                            return Ok(apply_chain(chain, Some(value)));
                        }
                        Some(Message::Delete { .. }) => return Ok(apply_chain(chain, None)),
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
                    return Ok(apply_chain(
                        chain,
                        entries.get(key).map(|e| e.value.as_slice()),
                    ));
                }
            }
        }
    }

    /// Every key in the range with its resolved value, in ascending key
    /// order, reporting storage failures instead of panicking — the
    /// fallible twin of [`KvEngine::scan`]. Loads nodes lazily as the
    /// walk reaches them (the cold-cache path).
    pub fn try_scan(
        &mut self,
        lo: std::ops::Bound<Vec<u8>>,
        hi: std::ops::Bound<Vec<u8>>,
    ) -> Result<Vec<(Key, Value)>, DiskError> {
        self.trace.scan(&lo, &hi);
        if range_is_empty(&lo, &hi) {
            return Ok(Vec::new());
        }
        self.ensure_loaded(self.root)?;
        // Root leaf: the entries are the terminal resolutions.
        if let Node::Leaf { entries } = self.loaded(self.root) {
            return Ok(entries
                .range::<[u8], _>((bound_as_slice(&lo), bound_as_slice(&hi)))
                .map(|(k, e)| (k.clone(), e.value.clone()))
                .collect());
        }
        // Bottom-up application (SPEC "Range scans", normative), exactly
        // as in `src/betree.rs`: clip top-down, apply upward — I3 makes
        // every buffered message strictly newer than anything produced
        // below, so depth order IS seq order. Explicit frames: degenerate
        // F=2 trees are linearly tall.
        struct Frame {
            id: NodeId,
            lo: std::ops::Bound<Key>,
            hi: std::ops::Bound<Key>,
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
            let i = stack[frame].next_child;
            let Node::Internal {
                pivots, children, ..
            } = self.loaded(stack[frame].id)
            else {
                unreachable!("only internal nodes get frames; leaf children fold inline")
            };
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
                let child = children[i];
                self.ensure_loaded(child)?;
                match self.loaded(child) {
                    Node::Leaf { entries } => {
                        for (k, e) in
                            entries.range::<[u8], _>((bound_as_slice(&c_lo), bound_as_slice(&c_hi)))
                        {
                            stack[frame].acc.insert(k.clone(), e.value.clone());
                        }
                    }
                    Node::Internal { .. } => {
                        stack.push(Frame {
                            id: child,
                            lo: c_lo,
                            hi: c_hi,
                            next_child: 0,
                            acc: BTreeMap::new(),
                        });
                    }
                }
            } else {
                let Node::Internal { buffer, .. } = self.loaded(stack[frame].id) else {
                    unreachable!("checked above")
                };
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

    /// Fault the entire committed tree into memory. Nothing evicts in M1,
    /// so afterwards the tree stays fully resident — the precondition for
    /// [`KvEngine::check_invariants`], which cannot read disk through
    /// `&self`.
    pub fn load_all(&mut self) -> Result<(), DiskError> {
        let mut stack = vec![self.root];
        while let Some(id) = stack.pop() {
            self.ensure_loaded(id)?;
            if let Node::Internal { children, .. } = self.loaded(id) {
                stack.extend(children.iter().copied());
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Slot bookkeeping.

    fn alloc_dirty(&mut self, node: Node) -> NodeId {
        let id = self.slots.len() as NodeId;
        self.slots.push(NodeSlot::Loaded {
            node,
            dirty: true,
            disk_offset: None,
        });
        id
    }

    fn alloc_on_disk(&mut self, offset: u64) -> NodeId {
        let id = self.slots.len() as NodeId;
        self.slots.push(NodeSlot::OnDisk { offset });
        id
    }

    fn is_dirty(&self, id: NodeId) -> bool {
        matches!(
            self.slots[id as usize],
            NodeSlot::Loaded { dirty: true, .. }
        )
    }

    /// The resident node at `id`. Every caller sits behind an
    /// `ensure_loaded` on the same id; hitting an unloaded slot here is an
    /// engine bug, not a storage condition.
    fn loaded(&self, id: NodeId) -> &Node {
        match &self.slots[id as usize] {
            NodeSlot::Loaded { node, .. } => node,
            NodeSlot::OnDisk { offset } => {
                unreachable!("engine bug: node {id} (record at {offset}) accessed before loading")
            }
        }
    }

    /// Mutable access to the resident node at `id`, marking it dirty:
    /// every mutation site goes through here, so nothing escapes the next
    /// commit (insert path, flush cascade, splits alike).
    fn loaded_mut(&mut self, id: NodeId) -> &mut Node {
        match &mut self.slots[id as usize] {
            NodeSlot::Loaded { node, dirty, .. } => {
                *dirty = true;
                node
            }
            NodeSlot::OnDisk { offset } => {
                unreachable!("engine bug: node {id} (record at {offset}) mutated before loading")
            }
        }
    }

    /// Load the record behind `id` if it is still on disk: read, verify
    /// length and checksum (typed [`DiskError::CorruptNode`] on mismatch —
    /// never a panic, never wrong data), deserialize, and turn each child
    /// offset into a fresh `OnDisk` slot.
    fn ensure_loaded(&mut self, id: NodeId) -> Result<(), DiskError> {
        let offset = match &self.slots[id as usize] {
            NodeSlot::Loaded { .. } => return Ok(()),
            NodeSlot::OnDisk { offset } => *offset,
        };
        let node = match self.read_record(offset)? {
            DiskNode::Leaf { entries } => Node::Leaf { entries },
            DiskNode::Internal {
                pivots,
                children,
                buffer,
            } => {
                // Structural validation up front: the tree code indexes
                // children by routing over pivots, so a record that passed
                // its crc but breaks the I2 arity (crafted bytes, checksum
                // collision) must become a typed error here — not an
                // index-out-of-bounds panic in the first get through it.
                if children.len() != pivots.len() + 1 {
                    return Err(DiskError::CorruptNode {
                        offset,
                        reason: format!(
                            "internal node with {} pivots and {} children",
                            pivots.len(),
                            children.len()
                        ),
                    });
                }
                // Children are always written before their parent, so a
                // valid child offset is strictly below the parent's. This
                // also rules out reference cycles, keeping every walk over
                // a crc-collision-corrupt file finite.
                for &child in &children {
                    if child < DATA_START || child >= offset {
                        return Err(DiskError::CorruptNode {
                            offset,
                            reason: format!("child offset {child} not in [{DATA_START}, {offset})"),
                        });
                    }
                }
                let children = children
                    .into_iter()
                    .map(|off| self.alloc_on_disk(off))
                    .collect();
                Node::Internal {
                    pivots,
                    children,
                    buffer,
                }
            }
        };
        self.slots[id as usize] = NodeSlot::Loaded {
            node,
            dirty: false,
            disk_offset: Some(offset),
        };
        Ok(())
    }

    /// Read and validate the node record at `offset`.
    fn read_record(&self, offset: u64) -> Result<DiskNode, DiskError> {
        let corrupt = |reason: String| DiskError::CorruptNode { offset, reason };
        // Compare instead of adding to `offset`: a corrupt offset near
        // u64::MAX must fail the bounds check, not wrap around it.
        if offset < DATA_START || offset > self.watermark.saturating_sub(RECORD_HEADER_SIZE) {
            return Err(corrupt(format!(
                "record header outside the data region [{DATA_START}, {})",
                self.watermark
            )));
        }
        let mut header = [0u8; RECORD_HEADER_SIZE as usize];
        self.vfs.read_exact_at(offset, &mut header)?;
        let len = u32::from_le_bytes(header[..4].try_into().expect("4 bytes")) as u64;
        let stored_crc = u32::from_le_bytes(header[4..].try_into().expect("4 bytes"));
        // `offset + RECORD_HEADER_SIZE` cannot overflow: the check above
        // bounded it by the watermark.
        if len > self.watermark - (offset + RECORD_HEADER_SIZE) {
            return Err(corrupt(format!(
                "payload of {len} bytes runs past the watermark {}",
                self.watermark
            )));
        }
        let mut payload = vec![0u8; len as usize];
        self.vfs
            .read_exact_at(offset + RECORD_HEADER_SIZE, &mut payload)?;
        let computed = crc32fast::hash(&payload);
        if computed != stored_crc {
            return Err(corrupt(format!(
                "checksum mismatch (stored {stored_crc:#010x}, computed {computed:#010x})"
            )));
        }
        decode_node(&payload).map_err(corrupt)
    }

    /// Serialize the resident node at `id` with child ids resolved to
    /// record offsets.
    fn node_payload(
        &self,
        id: NodeId,
        assigned: &BTreeMap<NodeId, u64>,
    ) -> Result<Vec<u8>, DiskError> {
        let disk_node = match self.loaded(id) {
            Node::Leaf { entries } => DiskNode::Leaf {
                entries: entries.clone(),
            },
            Node::Internal {
                pivots,
                children,
                buffer,
            } => DiskNode::Internal {
                pivots: pivots.clone(),
                children: children
                    .iter()
                    .map(|&c| self.resolve_offset(c, assigned))
                    .collect(),
                buffer: buffer.clone(),
            },
        };
        Ok(encode_node(&disk_node)?)
    }

    /// The record offset a node will have after the running commit: clean
    /// nodes keep their stored offset, dirty nodes were assigned one
    /// earlier in the children-first serialization order.
    fn resolve_offset(&self, id: NodeId, assigned: &BTreeMap<NodeId, u64>) -> u64 {
        match &self.slots[id as usize] {
            NodeSlot::OnDisk { offset } => *offset,
            NodeSlot::Loaded {
                dirty: false,
                disk_offset,
                ..
            } => disk_offset.expect("a clean resident node always has a disk offset"),
            NodeSlot::Loaded { dirty: true, .. } => *assigned
                .get(&id)
                .expect("children are serialized before their parents"),
        }
    }

    // ------------------------------------------------------------------
    // The M0.2 tree machinery over slots. Mirrors `src/betree.rs`
    // deliberately — the in-memory semantics between commits are normative
    // and identical; only residency and dirty tracking differ.

    /// Re-establish capacity invariants at the root after a public
    /// mutating op, growing the tree upward (splits) or shrinking it
    /// (reclamation; SPEC "Reclamation v1") as needed. Every promoted
    /// piece sits at the same depth as the old root, so a new root above
    /// them keeps all leaves at uniform depth (I6).
    fn restore_root(&mut self) -> Result<(), DiskError> {
        loop {
            let root_is_internal = matches!(self.loaded(self.root), Node::Internal { .. });
            let outcome = if root_is_internal {
                self.flush_overfull(self.root)?
            } else {
                Outcome::Splits(self.split_if_needed(self.root))
            };
            let promoted = match outcome {
                Outcome::Removed => {
                    // Every key range beneath the root emptied out: the
                    // tree is the empty tree again — its initial state, a
                    // single (dirty) empty leaf. The old root's slot
                    // leaks, and its records go unreferenced by the next
                    // commit — CoW handles reclamation on disk.
                    self.root = self.alloc_dirty(Node::Leaf {
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
            self.root = self.alloc_dirty(Node::Internal {
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
        // a later op once its buffer drains. The promoted child needs no
        // load: it stays a NodeId either way, and the next commit's
        // superblock simply points at it (a clean child keeps its record;
        // the abandoned root chain leaks by design).
        loop {
            match self.loaded(self.root) {
                Node::Internal {
                    children, buffer, ..
                } if children.len() == 1 && buffer.is_empty() => {
                    self.root = children[0];
                    self.ensure_loaded(self.root)?;
                }
                _ => break,
            }
        }
        Ok(())
    }

    /// Flush this internal node's buffer until it holds ≤ B messages, then
    /// settle the node itself: split it if its fanout overflowed, or
    /// signal [`Outcome::Removed`] if deliveries emptied every child out
    /// from under it (ADR-0005; SPEC "Reclamation v1"). Errors only on
    /// storage failure while loading a flush target.
    ///
    /// Explicit frame stack, NOT machine recursion, exactly as in
    /// `src/betree.rs`: degenerate F=2 trees are linearly tall.
    fn flush_overfull(&mut self, id: NodeId) -> Result<Outcome, DiskError> {
        // Nodes currently flushing, cascade root first. A node below the
        // top of the stack is waiting for its overfull child above it.
        let mut flushing: Vec<NodeId> = vec![id];
        loop {
            let top = *flushing
                .last()
                .expect("the flush stack only empties via the return below");
            if self.buffer_len(top) <= self.params.buffer_capacity {
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
                    None => return Ok(outcome),
                }
                continue;
            }
            let (chosen, child_id, child_occupancies, batch) = self.pick_and_extract(top);
            self.trace.flush_decision(top, child_occupancies, chosen);
            match self.apply_batch(child_id, batch)? {
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

    /// A flushed node's parting word to its parent: [`Outcome::Removed`]
    /// for an internal node whose children were all reclaimed (its buffer
    /// is necessarily empty: the messages went down with the deliveries
    /// that emptied them), else its split pieces.
    fn settle(&mut self, id: NodeId) -> Outcome {
        if let Node::Internal {
            children, buffer, ..
        } = self.loaded(id)
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
    /// [`Outcome::Removed`] when it settles. The reclaimed child's slot
    /// leaks, and its records go unreferenced by the next commit.
    fn remove_child(&mut self, parent: NodeId, child: NodeId) {
        let Node::Internal {
            pivots, children, ..
        } = self.loaded_mut(parent)
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
        match self.loaded(id) {
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
        } = self.loaded_mut(id)
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
    /// the buffer per the normative table), loading the child first if it
    /// is still on disk, and report what the delivery did.
    fn apply_batch(
        &mut self,
        child_id: NodeId,
        batch: BTreeMap<Key, Message>,
    ) -> Result<Delivery, DiskError> {
        self.ensure_loaded(child_id)?;
        let buffer_capacity = self.params.buffer_capacity;
        Ok(match self.loaded_mut(child_id) {
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
                    overflowed: buffer.len() > buffer_capacity,
                }
            }
        })
    }

    fn integrate_splits(&mut self, id: NodeId, promoted: Vec<(Key, NodeId)>) {
        if promoted.is_empty() {
            return;
        }
        let Node::Internal {
            pivots, children, ..
        } = self.loaded_mut(id)
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
    /// (with L=1 a single delivery can force many splits). The node is
    /// always resident here: it was just flushed into or out of.
    fn split_if_needed(&mut self, id: NodeId) -> Vec<(Key, NodeId)> {
        match self.loaded(id) {
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
        let Node::Leaf { entries } = self.loaded_mut(id) else {
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
        let Node::Leaf { entries } = self.loaded_mut(id) else {
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
            let new_id = self.alloc_dirty(Node::Leaf { entries: piece });
            promoted.push((pivot, new_id));
        }
        promoted
    }

    fn split_internal(&mut self, id: NodeId) -> Vec<(Key, NodeId)> {
        let Node::Internal {
            pivots,
            children,
            buffer,
        } = self.loaded_mut(id)
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
        } = self.loaded_mut(id)
        else {
            unreachable!("split_internal requires an internal node")
        };
        *node_pivots = first_pivots;
        *node_children = first_children;
        *node_buffer = buffer;

        let mut promoted = Vec::new();
        for (pivot, piece_pivots, piece_children, piece_buffer) in pieces_rev.into_iter().rev() {
            let new_id = self.alloc_dirty(Node::Internal {
                pivots: piece_pivots,
                children: piece_children,
                buffer: piece_buffer,
            });
            promoted.push((pivot, new_id));
        }
        promoted
    }
}

/// The invariant checker resolves nodes out of the slot arena. It can only
/// see resident nodes — `check_invariants` documents the `load_all`
/// precondition.
impl<V: Vfs> NodeSource for DiskEngine<V> {
    fn root(&self) -> NodeId {
        self.root
    }

    fn params(&self) -> &Params {
        &self.params
    }

    fn node(&self, id: NodeId) -> &Node {
        match &self.slots[id as usize] {
            NodeSlot::Loaded { node, .. } => node,
            NodeSlot::OnDisk { .. } => panic!(
                "check_invariants requires a fully resident tree: \
                 call load_all() first (node {id} is still on disk)"
            ),
        }
    }
}

/// The frozen M0 engine surface (P1–P5 semantics, unchanged between
/// commits). This surface treats storage failure as fatal — see the
/// type-level docs; `try_insert` / `try_get` are the fallible twins.
impl<V: Vfs> KvEngine for DiskEngine<V> {
    /// Unsupported: a disk engine cannot exist without backing storage.
    /// Use [`DiskEngine::create`] / [`DiskEngine::open`] (tests wrap them
    /// to mount the frozen harness). Panics unconditionally.
    fn new(_params: Params) -> Self {
        panic!("DiskEngine has no storage-free constructor; use create() or open()")
    }

    fn insert(&mut self, key: Key, value: Value) {
        self.try_insert(key, value)
            .expect("storage failure during insert (use try_insert to handle it)")
    }

    fn delete(&mut self, key: Key) {
        self.try_delete(key)
            .expect("storage failure during delete (use try_delete to handle it)")
    }

    fn upsert(&mut self, key: Key, op: UpsertOp) {
        self.try_upsert(key, op)
            .expect("storage failure during upsert (use try_upsert to handle it)")
    }

    fn get(&mut self, key: &[u8]) -> Option<Value> {
        self.try_get(key)
            .expect("storage failure during get (use try_get to handle it)")
    }

    fn scan(
        &mut self,
        lo: std::ops::Bound<Vec<u8>>,
        hi: std::ops::Bound<Vec<u8>>,
    ) -> Result<Vec<(Key, Value)>, EngineError> {
        self.try_scan(lo, hi).map_err(EngineError::from)
    }

    /// Checks I1–I6 over the resident tree with the same shared checker as
    /// `BeTree`. Requires the tree to be fully resident — guaranteed for
    /// an engine that has only ever been `create()`d (nothing evicts in
    /// M1); after `open()`, call [`DiskEngine::load_all`] first or this
    /// panics.
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

    /// The version gate is UNCONDITIONAL (SPEC "On-disk format"): a file
    /// carrying an authenticated foreign-version slot is refused even when
    /// its other slot holds a valid v2 superblock. Opening the stale v2
    /// generation instead would silently roll back — and the next commit
    /// would overwrite the newer foreign superblock (the
    /// downgrade-then-upgrade hazard found by the M2.1 review).
    #[test]
    fn mixed_version_slots_refuse_the_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed.db");
        // A real v2 database in slot 0 (generation 0 from create()).
        drop(DiskEngine::create(&path, Params::default()).unwrap());
        // A crafted, authenticated v1 superblock in slot 1, claiming a
        // NEWER generation.
        let foreign = Superblock {
            magic: MAGIC,
            format_version: 1,
            params: Params::default(),
            last_seq: 9,
            generation: 1,
            root_offset: DATA_START,
            watermark: DATA_START + 64,
        };
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[SUPERBLOCK_SLOT_SIZE as usize..2 * SUPERBLOCK_SLOT_SIZE as usize]
            .copy_from_slice(&foreign.encode_slot().unwrap());
        std::fs::write(&path, &bytes).unwrap();

        let err = DiskEngine::open(&path).expect_err("mixed versions must refuse");
        assert!(
            matches!(err, DiskError::UnsupportedVersion { found: 1 }),
            "got {err:?}"
        );
    }

    /// A crafted v1 superblock — authenticated but of the previous format
    /// version — must be refused with exactly `UnsupportedVersion`, not
    /// `NoValidSuperblock`: the file IS a database, just not one this
    /// build reads (ADR-0012; no migration pre-release).
    #[test]
    fn v1_superblock_is_rejected_as_unsupported_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v1.db");
        let sb = Superblock {
            magic: MAGIC,
            format_version: 1,
            params: Params::default(),
            last_seq: 7,
            generation: 0,
            root_offset: DATA_START,
            watermark: DATA_START + 64,
        };
        let mut file = sb.encode_slot().unwrap();
        file.resize((DATA_START + 64) as usize, 0);
        std::fs::write(&path, &file).unwrap();

        let err = DiskEngine::open(&path).expect_err("v1 must be refused");
        assert!(
            matches!(err, DiskError::UnsupportedVersion { found: 1 }),
            "got {err:?}"
        );
    }

    /// A record can pass its CRC and still be malformed (a crafted file,
    /// or the one-in-2^32 checksum collision). An internal node whose
    /// arity breaks I2 must surface as a typed `CorruptNode` on load — not
    /// as an index-out-of-bounds panic in the first get routed through it.
    #[test]
    fn crc_valid_malformed_arity_is_corrupt_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let leaf_payload = encode_node(&DiskNode::Leaf {
            entries: BTreeMap::new(),
        })
        .unwrap();
        let roots = [
            // Zero children at all.
            DiskNode::Internal {
                pivots: Vec::new(),
                children: Vec::new(),
                buffer: BTreeMap::new(),
            },
            // Two pivots but a single (otherwise valid) child.
            DiskNode::Internal {
                pivots: vec![vec![5], vec![9]],
                children: vec![DATA_START],
                buffer: BTreeMap::new(),
            },
        ];
        for (i, root) in roots.into_iter().enumerate() {
            let path = dir.path().join(format!("crafted{i}.db"));
            let root_offset = DATA_START + RECORD_HEADER_SIZE + leaf_payload.len() as u64;
            let root_payload = encode_node(&root).unwrap();
            let watermark = root_offset + RECORD_HEADER_SIZE + root_payload.len() as u64;

            let mut file = Superblock {
                magic: MAGIC,
                format_version: FORMAT_VERSION,
                params: Params::default(),
                last_seq: 0,
                generation: 0,
                root_offset,
                watermark,
            }
            .encode_slot()
            .unwrap();
            file.resize(DATA_START as usize, 0);
            for payload in [&leaf_payload, &root_payload] {
                file.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                file.extend_from_slice(&crc32fast::hash(payload).to_le_bytes());
                file.extend_from_slice(payload);
            }
            std::fs::write(&path, &file).unwrap();

            let mut engine = DiskEngine::open(&path).unwrap();
            let err = engine
                .try_get(&[6])
                .expect_err("malformed arity must be detected, not panicked on");
            assert!(
                matches!(err, DiskError::CorruptNode { .. }),
                "case {i}: got {err:?}"
            );
        }
    }
}
