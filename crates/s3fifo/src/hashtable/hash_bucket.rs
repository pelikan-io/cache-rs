//! Hash buckets store a group of item entries with shared metadata.
//!
//! The bucket layout matches the cacheline-aligned design from segcache,
//! adapted for S3-FIFO's slab-based item storage.
//!
//! Bucket Info:
//! ```text
//! ┌──────────────────────────────┬──────┬──────┬──────────────┐
//! │             CAS              │ ---- │CHAIN │   RESERVED   │
//! │                              │      │ LEN  │              │
//! │            32 bit            │8 bit │8 bit │    16 bit    │
//! │                              │      │      │              │
//! │0                           31│32  39│40  47│48          63│
//! └──────────────────────────────┴──────┴──────┴──────────────┘
//! ```
//!
//! Item Info:
//! ```text
//! ┌──────────────────────────────┬──────────────────────────────┐
//! │             TAG              │          SLAB INDEX           │
//! │                              │                              │
//! │            32 bit            │            32 bit             │
//! │                              │                              │
//! │0                           31│32                          63│
//! └──────────────────────────────┴──────────────────────────────┘
//! ```

use super::*;

// bucket info masks and shifts

/// A mask to get the CAS value from the bucket info
pub(crate) const CAS_MASK: u64 = 0xFFFF_FFFF_0000_0000;
/// Number of bits to shift to get the CAS value
pub(crate) const CAS_BIT_SHIFT: u64 = 32;
/// A mask to get the chain length from the bucket info
pub(crate) const BUCKET_CHAIN_LEN_MASK: u64 = 0x0000_0000_00FF_0000;
/// Number of bits to shift to get the chain length
pub(crate) const BUCKET_CHAIN_LEN_BIT_SHIFT: u64 = 16;

// item info masks and shifts

/// A mask to get the tag from the item info
pub(crate) const TAG_MASK: u64 = 0xFFFF_FFFF_0000_0000;
/// A mask to get the slab index from the item info
pub(crate) const INDEX_MASK: u64 = 0x0000_0000_FFFF_FFFF;

#[derive(Copy, Clone)]
pub(crate) struct HashBucket {
    pub(super) data: [u64; N_BUCKET_SLOT],
}

impl HashBucket {
    pub fn new() -> Self {
        Self {
            data: [0; N_BUCKET_SLOT],
        }
    }
}

/// Calculate an item's tag from the hash value. The MSB is always set to
/// ensure valid item info entries are never zero.
#[inline]
pub const fn tag_from_hash(hash: u64) -> u64 {
    (hash & TAG_MASK) | 0x8000_0000_0000_0000
}

/// Get the tag from the item info
#[inline]
pub const fn get_tag(item_info: u64) -> u64 {
    item_info & TAG_MASK
}

/// Get the slab index from the item info
#[inline]
pub const fn get_index(item_info: u64) -> u32 {
    (item_info & INDEX_MASK) as u32
}

/// Get the CAS value from the bucket info
#[inline]
pub const fn get_cas(bucket_info: u64) -> u32 {
    ((bucket_info & CAS_MASK) >> CAS_BIT_SHIFT) as u32
}

/// Get the chain length from the bucket info
#[inline]
pub const fn chain_len(bucket_info: u64) -> u64 {
    (bucket_info & BUCKET_CHAIN_LEN_MASK) >> BUCKET_CHAIN_LEN_BIT_SHIFT
}

/// Build an item info entry from a tag and slab index
#[inline]
pub const fn build_item_info(tag: u64, index: u32) -> u64 {
    tag | (index as u64)
}
