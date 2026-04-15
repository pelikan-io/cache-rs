//! A hashtable for fast item lookup, adapted from the segcache design.
//!
//! The [`HashTable`] uses bulk chaining with cacheline-aligned buckets to
//! reduce per-item overheads and provide good data locality. Each bucket
//! contains 8 x 64-bit slots. The first slot stores per-bucket metadata
//! (CAS value, chain length), and the remaining slots store item info
//! entries that reference items in the slab.

/// The number of slots within each bucket
const N_BUCKET_SLOT: usize = 8;

/// Maximum number of buckets in a chain. Must be <= 255.
const MAX_CHAIN_LEN: u64 = 16;

use ahash::RandomState;
use core::hash::{BuildHasher, Hasher};

use crate::Slab;

mod hash_bucket;

pub(crate) use hash_bucket::*;

#[cfg(feature = "metrics")]
use crate::metrics::*;

/// Main structure for performing item lookup. Contains a contiguous allocation
/// of [`HashBucket`]s which are used to store item info and metadata.
pub(crate) struct HashTable {
    hash_builder: Box<RandomState>,
    power: u64,
    mask: u64,
    data: Box<[HashBucket]>,
    next_to_chain: u64,
}

impl HashTable {
    /// Creates a new hashtable with a specified power and overflow factor. The
    /// hashtable will have the capacity to store up to
    /// `7 * 2^(power - 3) * (1 + overflow_factor)` items.
    pub fn new(power: u8, overflow_factor: f64) -> HashTable {
        if overflow_factor < 0.0 {
            panic!("hashtable overflow factor must be >= 0.0");
        }

        if overflow_factor > MAX_CHAIN_LEN as f64 {
            panic!("hashtable overflow factor must be <= {MAX_CHAIN_LEN}");
        }

        let slots = 1_u64 << power;
        let buckets = slots / 8;
        let mask = buckets - 1;

        let total_buckets = (buckets as f64 * (1.0 + overflow_factor)).ceil() as usize;

        let mut data = Vec::with_capacity(0);
        data.reserve_exact(total_buckets);
        data.resize(total_buckets, HashBucket::new());
        debug!(
            "hashtable has: {slots} primary slots across {buckets} primary buckets and {total_buckets} total buckets",
        );

        let hash_builder = RandomState::with_seeds(
            0xa3f12c4e8b6d9071,
            0x5e7f1a3b9c8d2e4f,
            0x1d3f5a7b9e0c2d4f,
            0x8b6d4f2e0a1c3e5d,
        );

        Self {
            hash_builder: Box::new(hash_builder),
            power: power.into(),
            mask,
            data: data.into_boxed_slice(),
            next_to_chain: buckets,
        }
    }

    /// Compute the hash of a key
    pub fn hash(&self, key: &[u8]) -> u64 {
        #[cfg(feature = "metrics")]
        HASH_LOOKUP.increment();

        let mut hasher = self.hash_builder.build_hasher();
        hasher.write(key);
        hasher.finish()
    }

    /// Lookup an item by key, returns its slab index
    pub fn get(&self, key: &[u8], hash: u64, slab: &Slab) -> Option<u32> {
        let tag = tag_from_hash(hash);
        let mut bucket_id = (hash & self.mask) as usize;
        let chain_length = chain_len(self.data[bucket_id].data[0]) as usize;

        for chain_idx in 0..=chain_length {
            let start = if chain_idx == 0 { 1 } else { 0 };
            let end = if chain_idx == chain_length {
                N_BUCKET_SLOT
            } else {
                N_BUCKET_SLOT - 1
            };

            for slot in start..end {
                let item_info = self.data[bucket_id].data[slot];
                if item_info == 0 {
                    continue;
                }
                if get_tag(item_info) == tag {
                    let index = get_index(item_info);
                    if slab.is_match(index, key) {
                        return Some(index);
                    }
                    #[cfg(feature = "metrics")]
                    HASH_TAG_COLLISION.increment();
                }
            }

            if chain_idx < chain_length {
                bucket_id = self.data[bucket_id].data[N_BUCKET_SLOT - 1] as usize;
            }
        }

        None
    }

    /// Insert a new entry into the hashtable. If an entry with the same key
    /// exists, it is replaced and the old slab index is returned.
    #[allow(clippy::result_unit_err)]
    pub fn insert(
        &mut self,
        hash: u64,
        index: u32,
        key: &[u8],
        slab: &Slab,
    ) -> Result<Option<u32>, ()> {
        #[cfg(feature = "metrics")]
        HASH_INSERT.increment();

        let tag = tag_from_hash(hash);
        let new_item_info = build_item_info(tag, index);
        let mut first_empty: Option<(usize, usize)> = None;
        let mut bucket_id = (hash & self.mask) as usize;
        let chain_length = chain_len(self.data[bucket_id].data[0]) as usize;

        for chain_idx in 0..=chain_length {
            let start = if chain_idx == 0 { 1 } else { 0 };
            let end = if chain_idx == chain_length {
                N_BUCKET_SLOT
            } else {
                N_BUCKET_SLOT - 1
            };

            for slot in start..end {
                let item_info = self.data[bucket_id].data[slot];
                if item_info == 0 {
                    if first_empty.is_none() {
                        first_empty = Some((bucket_id, slot));
                    }
                    continue;
                }
                if get_tag(item_info) == tag {
                    let existing_index = get_index(item_info);
                    if slab.is_match(existing_index, key) {
                        // Replace existing entry
                        self.data[bucket_id].data[slot] = new_item_info;
                        let primary = (hash & self.mask) as usize;
                        self.data[primary].data[0] += 1 << CAS_BIT_SHIFT;

                        #[cfg(feature = "metrics")]
                        crate::ITEM_REPLACE.increment();

                        return Ok(Some(existing_index));
                    }
                    #[cfg(feature = "metrics")]
                    HASH_TAG_COLLISION.increment();
                }
            }

            if chain_idx < chain_length {
                bucket_id = self.data[bucket_id].data[N_BUCKET_SLOT - 1] as usize;
            }
        }

        // Try to insert in the first empty slot found
        if let Some((bid, slot)) = first_empty {
            self.data[bid].data[slot] = new_item_info;
            let primary = (hash & self.mask) as usize;
            self.data[primary].data[0] += 1 << CAS_BIT_SHIFT;
            return Ok(None);
        }

        // No empty slot, try to chain a new bucket
        let primary = (hash & self.mask) as usize;
        if chain_length < MAX_CHAIN_LEN as usize && (self.next_to_chain as usize) < self.data.len()
        {
            // Chase to end of chain
            let mut last_bucket = primary;
            for _ in 0..chain_length {
                last_bucket = self.data[last_bucket].data[N_BUCKET_SLOT - 1] as usize;
            }

            let next_id = self.next_to_chain as usize;
            self.next_to_chain += 1;

            self.data[next_id].data[0] = self.data[last_bucket].data[N_BUCKET_SLOT - 1];
            self.data[next_id].data[1] = new_item_info;
            self.data[last_bucket].data[N_BUCKET_SLOT - 1] = next_id as u64;

            self.data[primary].data[0] += 1 << BUCKET_CHAIN_LEN_BIT_SHIFT;
            self.data[primary].data[0] += 1 << CAS_BIT_SHIFT;

            return Ok(None);
        }

        #[cfg(feature = "metrics")]
        HASH_INSERT_EX.increment();

        Err(())
    }

    /// Remove an entry by its hash and slab index. Used during eviction when
    /// we already know the exact item info.
    pub fn remove_by_index(&mut self, hash: u64, index: u32) -> bool {
        let tag = tag_from_hash(hash);
        let target = build_item_info(tag, index);
        let mut bucket_id = (hash & self.mask) as usize;
        let chain_length = chain_len(self.data[bucket_id].data[0]) as usize;

        for chain_idx in 0..=chain_length {
            let start = if chain_idx == 0 { 1 } else { 0 };
            let end = if chain_idx == chain_length {
                N_BUCKET_SLOT
            } else {
                N_BUCKET_SLOT - 1
            };

            for slot in start..end {
                if self.data[bucket_id].data[slot] == target {
                    self.data[bucket_id].data[slot] = 0;

                    #[cfg(feature = "metrics")]
                    HASH_REMOVE.increment();

                    return true;
                }
            }

            if chain_idx < chain_length {
                bucket_id = self.data[bucket_id].data[N_BUCKET_SLOT - 1] as usize;
            }
        }

        false
    }

    /// Remove an entry by key. Returns the slab index of the removed entry.
    pub fn delete(&mut self, key: &[u8], hash: u64, slab: &Slab) -> Option<u32> {
        let tag = tag_from_hash(hash);
        let mut bucket_id = (hash & self.mask) as usize;
        let chain_length = chain_len(self.data[bucket_id].data[0]) as usize;

        for chain_idx in 0..=chain_length {
            let start = if chain_idx == 0 { 1 } else { 0 };
            let end = if chain_idx == chain_length {
                N_BUCKET_SLOT
            } else {
                N_BUCKET_SLOT - 1
            };

            for slot in start..end {
                let item_info = self.data[bucket_id].data[slot];
                if item_info == 0 {
                    continue;
                }
                if get_tag(item_info) == tag {
                    let index = get_index(item_info);
                    if slab.is_match(index, key) {
                        self.data[bucket_id].data[slot] = 0;

                        #[cfg(feature = "metrics")]
                        HASH_REMOVE.increment();

                        return Some(index);
                    }
                    #[cfg(feature = "metrics")]
                    HASH_TAG_COLLISION.increment();
                }
            }

            if chain_idx < chain_length {
                bucket_id = self.data[bucket_id].data[N_BUCKET_SLOT - 1] as usize;
            }
        }

        None
    }

    /// Get the CAS value for the bucket associated with a hash
    pub fn get_cas(&self, hash: u64) -> u32 {
        let bucket_id = (hash & self.mask) as usize;
        get_cas(self.data[bucket_id].data[0])
    }

    /// Clear all entries from the hashtable
    pub fn clear(&mut self) {
        let buckets = (1 << self.power) / 8;
        for bucket in self.data.iter_mut() {
            *bucket = HashBucket::new();
        }
        self.next_to_chain = buckets;
    }
}
