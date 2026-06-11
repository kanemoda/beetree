//! On-disk format v1 (`docs/SPEC.md`, "On-disk format v1"; ADR-0008).
//!
//! One database is one file: two 4096-byte superblock slots at offsets 0
//! and 4096, then an append-only data region of variable-length node
//! records from offset 8192. Everything is bincode in the little-endian,
//! fixed-int configuration, so every encoding is byte-stable, and every
//! superblock and record carries a CRC-32 so corruption is DETECTED, never
//! returned as data.

use std::collections::BTreeMap;
use std::io;

use bincode::config::Configuration;
use bincode::config::{Fixint, LittleEndian};
use serde::{Deserialize, Serialize};

use crate::node::LeafEntry;
use crate::types::{Key, Message, Params};

/// Size of one superblock slot.
pub(crate) const SUPERBLOCK_SLOT_SIZE: u64 = 4096;

/// File offsets of the two superblock slots; generation g lives in slot
/// g mod 2 (the active slot flips by generation, not by position).
pub(crate) const SUPERBLOCK_SLOTS: [u64; 2] = [0, SUPERBLOCK_SLOT_SIZE];

/// First byte of the data region: node records are appended from here.
pub(crate) const DATA_START: u64 = 2 * SUPERBLOCK_SLOT_SIZE;

/// `[len: u32][crc32: u32]` precede every node record payload.
pub(crate) const RECORD_HEADER_SIZE: u64 = 8;

/// Magic bytes opening every superblock.
pub(crate) const MAGIC: [u8; 4] = *b"BEET";

/// On-disk format version this build reads and writes.
pub(crate) const FORMAT_VERSION: u32 = 1;

/// The bincode configuration every on-disk byte goes through:
/// little-endian, fixed-width integers (usize as u64).
fn config() -> Configuration<LittleEndian, Fixint> {
    bincode::config::standard()
        .with_little_endian()
        .with_fixed_int_encoding()
}

/// One committed root: everything `open()` needs to resume a database.
///
/// A slot stores the bincode-serialized fields zero-padded to 4092 bytes,
/// then the CRC-32 of those 4092 bytes in the slot's final 4 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Superblock {
    /// Always [`MAGIC`].
    pub magic: [u8; 4],
    /// Always [`FORMAT_VERSION`] in files this build writes.
    pub format_version: u32,
    /// Structure parameters; persisted so `open()` needs no out-of-band
    /// params (SPEC "On-disk format v1"; amends ADR-0006's note).
    pub params: Params,
    /// Seqno of the newest committed op. Reopening continues the global
    /// seqno sequence from here, keeping cross-session last-writer-wins
    /// ordering and invariant I3 sound.
    pub last_seq: u64,
    /// Commit counter, starting at 0 for `create()`.
    pub generation: u64,
    /// File offset of the root node record.
    pub root_offset: u64,
    /// One past the end of valid data; `open()` truncates here.
    pub watermark: u64,
}

impl Superblock {
    /// Serialize into a full 4096-byte slot image (payload, zero padding,
    /// trailing crc).
    pub fn encode_slot(&self) -> io::Result<Vec<u8>> {
        let mut slot = vec![0u8; SUPERBLOCK_SLOT_SIZE as usize];
        let crc_at = slot.len() - 4;
        let written = bincode::serde::encode_into_slice(self, &mut slot[..crc_at], config())
            .map_err(io::Error::other)?;
        debug_assert!(written < crc_at, "superblock payload must fit the slot");
        let crc = crc32fast::hash(&slot[..crc_at]);
        slot[crc_at..].copy_from_slice(&crc.to_le_bytes());
        Ok(slot)
    }

    /// Parse a 4096-byte slot image; `None` if the slot is not a valid
    /// superblock (bad crc, magic, version, or insane geometry) — open()
    /// then falls back to the other slot.
    pub fn decode_slot(slot: &[u8]) -> Option<Superblock> {
        if slot.len() != SUPERBLOCK_SLOT_SIZE as usize {
            return None;
        }
        let crc_at = slot.len() - 4;
        let stored = u32::from_le_bytes(slot[crc_at..].try_into().expect("4 bytes"));
        if crc32fast::hash(&slot[..crc_at]) != stored {
            return None;
        }
        let (sb, _): (Superblock, usize) =
            bincode::serde::decode_from_slice(&slot[..crc_at], config()).ok()?;
        // Conjunct order matters: `watermark >= DATA_START` first makes
        // the subtraction below safe, and comparing instead of adding to
        // `root_offset` keeps offsets near u64::MAX from wrapping a check
        // whose whole purpose is rejecting insane geometry.
        let geometry_ok = sb.watermark >= DATA_START
            && sb.root_offset >= DATA_START
            && sb.root_offset <= sb.watermark - RECORD_HEADER_SIZE;
        let params_ok =
            sb.params.fanout >= 2 && sb.params.buffer_capacity >= 1 && sb.params.leaf_capacity >= 1;
        (sb.magic == MAGIC && sb.format_version == FORMAT_VERSION && geometry_ok && params_ok)
            .then_some(sb)
    }
}

/// A node as it lies in a record: identical to the in-memory
/// [`Node`](crate::node::Node) except that children are u64 FILE OFFSETS
/// of the children's records instead of arena ids (ADR-0008).
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum DiskNode {
    /// Internal node: pivots, child record offsets, message buffer.
    Internal {
        /// Pivot keys, exactly as in memory.
        pivots: Vec<Key>,
        /// File offsets of the children's records.
        children: Vec<u64>,
        /// The message buffer, exactly as in memory.
        buffer: BTreeMap<Key, Message>,
    },
    /// Leaf node: materialized entries, exactly as in memory.
    Leaf {
        /// The leaf entries.
        entries: BTreeMap<Key, LeafEntry>,
    },
}

/// Serialize a node record payload (the bytes the record crc covers).
pub(crate) fn encode_node(node: &DiskNode) -> io::Result<Vec<u8>> {
    bincode::serde::encode_to_vec(node, config()).map_err(io::Error::other)
}

/// Deserialize a crc-verified record payload; the error is a human-readable
/// corruption reason (a crc collision with a still-undecodable payload).
pub(crate) fn decode_node(payload: &[u8]) -> Result<DiskNode, String> {
    let (node, read): (DiskNode, usize) =
        bincode::serde::decode_from_slice(payload, config()).map_err(|e| e.to_string())?;
    if read != payload.len() {
        return Err(format!(
            "record decodes in {read} bytes but carries {}",
            payload.len()
        ));
    }
    Ok(node)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn superblock() -> Superblock {
        Superblock {
            magic: MAGIC,
            format_version: FORMAT_VERSION,
            params: Params::default(),
            last_seq: 41,
            generation: 7,
            root_offset: DATA_START,
            watermark: DATA_START + 99,
        }
    }

    #[test]
    fn superblock_slot_round_trips() {
        let sb = superblock();
        let slot = sb.encode_slot().unwrap();
        assert_eq!(slot.len() as u64, SUPERBLOCK_SLOT_SIZE);
        assert_eq!(Superblock::decode_slot(&slot), Some(sb));
    }

    /// Geometry validation must hold at the u64 boundary: a CRC-valid slot
    /// carrying offsets near u64::MAX once overflowed the
    /// `root + header <= watermark` check — a panic in debug builds, a
    /// wrap-around ACCEPT in release. Both variants are simply invalid.
    #[test]
    fn insane_geometry_is_rejected_not_panicked() {
        let mut sb = superblock();
        sb.root_offset = u64::MAX;
        sb.watermark = u64::MAX;
        assert_eq!(Superblock::decode_slot(&sb.encode_slot().unwrap()), None);

        let mut sb = superblock();
        sb.root_offset = u64::MAX - 4; // the sum wraps to a tiny value
        sb.watermark = DATA_START + 99;
        assert_eq!(Superblock::decode_slot(&sb.encode_slot().unwrap()), None);
    }

    #[test]
    fn any_flipped_slot_byte_invalidates() {
        let slot = superblock().encode_slot().unwrap();
        // Payload, padding, and the stored crc itself are all covered.
        for &at in &[0usize, 5, 40, 2048, 4091, 4092, 4095] {
            let mut bad = slot.clone();
            bad[at] ^= 0x01;
            assert_eq!(Superblock::decode_slot(&bad), None, "flip at {at}");
        }
    }

    #[test]
    fn node_payload_round_trips() {
        let node = DiskNode::Internal {
            pivots: vec![vec![9]],
            children: vec![8192, 8240],
            buffer: BTreeMap::from([(
                vec![3],
                Message::Put {
                    seq: 5,
                    value: vec![1, 2],
                },
            )]),
        };
        let payload = encode_node(&node).unwrap();
        let back = decode_node(&payload).unwrap();
        match back {
            DiskNode::Internal {
                pivots, children, ..
            } => {
                assert_eq!(pivots, vec![vec![9]]);
                assert_eq!(children, vec![8192, 8240]);
            }
            DiskNode::Leaf { .. } => panic!("kind flipped in round trip"),
        }
    }

    #[test]
    fn trailing_garbage_is_reported() {
        let payload = encode_node(&DiskNode::Leaf {
            entries: BTreeMap::new(),
        })
        .unwrap();
        let mut long = payload.clone();
        long.push(0);
        assert!(decode_node(&long).is_err());
    }
}
