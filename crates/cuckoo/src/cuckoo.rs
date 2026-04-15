// Copyright 2025 Pelikan Cache contributors
// Licensed under the MIT and Apache-2.0 licenses

//! Core cuckoo cache implementation.
//!
//! Each item slot has the following layout:
//!
//! ```text
//! ┌────────────┬──────────────────────────────────────────────┐
//! │   EXPIRE   │              Item Data                       │
//! │   (u32)    │  [ItemHeader][optional][key][value]          │
//! │  4 bytes   │  (keyvalue crate format)                     │
//! └────────────┴──────────────────────────────────────────────┘
//! ```
//!
//! A slot is considered empty when the key length in the item header is zero.

use crate::*;
use ahash::RandomState;
use clocksource::coarse::Instant;
use core::hash::{BuildHasher, Hasher};
use keyvalue::{RawItem, Value, ITEM_HDR_SIZE};
use rand::RngExt;

/// Size of the per-slot expiration field in bytes.
const EXPIRE_SIZE: usize = std::mem::size_of::<u32>();

/// Fixed seeds for the four independent hash functions, analogous to the
/// initialization vectors used in the C implementation.
const SEEDS: [[u64; 4]; D] = [
    [0x3ac5_d673, 0x6d78_39d0, 0x2b58_1cf5, 0x4dd2_be0a],
    [0x9e37_79b9, 0x517c_c1b7, 0x27d4_eb2f, 0x3c6e_f372],
    [0xdead_beef, 0xcafe_babe, 0x1234_5678, 0xfeed_face],
    [0xa076_1d64, 0xe703_7ed1, 0x8ebc_6af0, 0x5899_65cd],
];

/// A pre-allocated cache that uses cuckoo hashing with D=4 candidate positions
/// per key. Items are stored inline in fixed-size slots within a contiguous
/// array.
pub struct CuckooCache {
    /// Backing storage: `nitem` slots of `item_size` bytes each.
    data: Box<[u8]>,
    /// Four independent hash builders for cuckoo hashing.
    hashers: Box<[RandomState; D]>,
    /// Bytes per item slot.
    item_size: usize,
    /// Total number of item slots.
    nitem: usize,
    /// Maximum displacement chain depth.
    max_displace: usize,
    /// Maximum TTL in seconds.
    max_ttl: u32,
    /// Eviction policy.
    policy: Policy,
    /// Creation time for computing relative expiration timestamps.
    started: Instant,
}

impl CuckooCache {
    /// Returns a new [`Builder`] for configuring a `CuckooCache`.
    ///
    /// ```
    /// use cuckoo_cache::CuckooCache;
    ///
    /// let cache = CuckooCache::builder()
    ///     .nitem(4096)
    ///     .item_size(64)
    ///     .build();
    /// ```
    pub fn builder() -> Builder {
        Builder::default()
    }

    pub(crate) fn from_builder(b: Builder) -> Self {
        assert!(
            b.item_size > EXPIRE_SIZE + ITEM_HDR_SIZE,
            "item_size must be greater than {} bytes (overhead)",
            EXPIRE_SIZE + ITEM_HDR_SIZE
        );
        assert!(b.nitem > 0, "nitem must be positive");

        let total = b
            .item_size
            .checked_mul(b.nitem)
            .expect("total storage size overflow");

        let data = vec![0u8; total].into_boxed_slice();
        let hashers = Box::new(SEEDS.map(|s| RandomState::with_seeds(s[0], s[1], s[2], s[3])));

        debug!(
            "cuckoo cache: {} items x {} bytes = {} bytes total",
            b.nitem, b.item_size, total,
        );

        Self {
            data,
            hashers,
            item_size: b.item_size,
            nitem: b.nitem,
            max_displace: b.max_displace,
            max_ttl: b.max_ttl,
            policy: b.policy,
            started: Instant::now(),
        }
    }

    // -----------------------------------------------------------------------
    // Slot access helpers
    // -----------------------------------------------------------------------

    /// Byte offset into `self.data` for slot `index`.
    #[inline]
    fn slot_offset(&self, index: usize) -> usize {
        index * self.item_size
    }

    /// Read the expiration timestamp for a slot.
    #[inline]
    fn slot_expire(&self, index: usize) -> u32 {
        let off = self.slot_offset(index);
        u32::from_le_bytes(self.data[off..off + EXPIRE_SIZE].try_into().unwrap())
    }

    /// Write the expiration timestamp for a slot.
    #[inline]
    fn set_slot_expire(&mut self, index: usize, expire: u32) {
        let off = self.slot_offset(index);
        self.data[off..off + EXPIRE_SIZE].copy_from_slice(&expire.to_le_bytes());
    }

    /// Get a [`RawItem`] view of a slot. The returned item borrows from the
    /// cache's data buffer via a raw pointer.
    fn slot_raw_item(&self, index: usize) -> RawItem {
        let off = self.slot_offset(index) + EXPIRE_SIZE;
        // SAFETY: pointer is within our allocated buffer. The slot layout
        // guarantees at least `item_size - EXPIRE_SIZE` bytes from this offset.
        unsafe { RawItem::from_ptr((self.data.as_ptr() as *mut u8).add(off)) }
    }

    /// Check whether a slot is empty (no item stored).
    #[inline]
    fn slot_is_empty(&self, index: usize) -> bool {
        self.slot_raw_item(index).klen() == 0
    }

    /// Check whether a slot's item has expired.
    fn slot_is_expired(&self, index: usize) -> bool {
        let expire = self.slot_expire(index);
        if expire == 0 {
            return false;
        }
        let elapsed = (Instant::now() - self.started).as_secs();
        elapsed > expire
    }

    /// Clear a slot by zeroing all its bytes.
    fn clear_slot(&mut self, index: usize) {
        let off = self.slot_offset(index);
        self.data[off..off + self.item_size].fill(0);
    }

    /// Copy a slot's contents from `from` to `to` and clear the source.
    fn move_slot(&mut self, from: usize, to: usize) {
        let from_off = self.slot_offset(from);
        let to_off = self.slot_offset(to);
        let size = self.item_size;
        self.data.copy_within(from_off..from_off + size, to_off);
        self.data[from_off..from_off + size].fill(0);
    }

    /// Write an item into a slot.
    fn write_slot(&mut self, index: usize, key: &[u8], value: Value, optional: &[u8], expire: u32) {
        self.clear_slot(index);
        self.set_slot_expire(index, expire);

        let off = self.slot_offset(index) + EXPIRE_SIZE;
        // SAFETY: we have &mut self and the offset is within bounds.
        let ptr = unsafe { self.data.as_mut_ptr().add(off) };
        let mut raw = RawItem::from_ptr(ptr);
        raw.define(key, value, optional);
    }

    // -----------------------------------------------------------------------
    // Hashing
    // -----------------------------------------------------------------------

    /// Compute the D candidate positions for a key.
    fn positions(&self, key: &[u8]) -> [usize; D] {
        let mut positions = [0usize; D];
        for (i, pos) in positions.iter_mut().enumerate() {
            let mut hasher = self.hashers[i].build_hasher();
            hasher.write(key);
            *pos = (hasher.finish() as usize) % self.nitem;
        }
        positions
    }

    /// Compute the expiration timestamp for a given TTL.
    fn compute_expire(&self, ttl: std::time::Duration) -> u32 {
        if ttl.is_zero() {
            return 0;
        }
        let secs = std::cmp::min(ttl.as_secs(), self.max_ttl as u64) as u32;
        let elapsed = (Instant::now() - self.started).as_secs();
        elapsed.saturating_add(secs)
    }

    // -----------------------------------------------------------------------
    // Expiration and eviction helpers
    // -----------------------------------------------------------------------

    /// Handle an expired item: update metrics and clear the slot.
    fn handle_expired(&mut self, index: usize) {
        #[cfg(feature = "metrics")]
        {
            metrics::ITEM_EXPIRE.increment();
            self.decrement_item_metrics(index);
        }
        self.clear_slot(index);
    }

    /// Evict an item to make room: update metrics and clear the slot.
    fn evict_at(&mut self, index: usize) {
        #[cfg(feature = "metrics")]
        {
            metrics::ITEM_EVICT.increment();
            self.decrement_item_metrics(index);
        }
        self.clear_slot(index);
    }

    #[cfg(feature = "metrics")]
    fn decrement_item_metrics(&self, index: usize) {
        let raw = self.slot_raw_item(index);
        let klen = raw.klen() as i64;
        let vlen = raw.header().vlen() as i64;
        metrics::ITEM_CURRENT.sub(1);
        metrics::ITEM_KEY_BYTE.sub(klen);
        metrics::ITEM_VAL_BYTE.sub(vlen);
        metrics::ITEM_DATA_BYTE.sub(klen + vlen);
    }

    #[cfg(feature = "metrics")]
    fn increment_item_metrics(&self, index: usize) {
        let raw = self.slot_raw_item(index);
        let klen = raw.klen() as i64;
        let vlen = raw.header().vlen() as i64;
        metrics::ITEM_CURRENT.add(1);
        metrics::ITEM_KEY_BYTE.add(klen);
        metrics::ITEM_VAL_BYTE.add(vlen);
        metrics::ITEM_DATA_BYTE.add(klen + vlen);
    }

    // -----------------------------------------------------------------------
    // Displacement
    // -----------------------------------------------------------------------

    /// Attempt to free one of the candidate positions via displacement.
    /// Returns the index into `candidates` of the freed slot, or `None`.
    fn try_displace(&mut self, candidates: &[usize; D]) -> Option<usize> {
        for (idx, &pos) in candidates.iter().enumerate() {
            if self.displace_from(pos, 0) {
                return Some(idx);
            }
        }
        None
    }

    /// Try to free the slot at `pos` by moving its occupant to one of the
    /// occupant's alternative candidate positions, recursing up to
    /// `max_displace` levels deep. Returns `true` if `pos` was freed.
    fn displace_from(&mut self, pos: usize, depth: usize) -> bool {
        if self.slot_is_empty(pos) {
            return true;
        }
        if self.slot_is_expired(pos) {
            self.handle_expired(pos);
            return true;
        }
        if depth >= self.max_displace {
            return false;
        }

        // Copy the key so we can compute alternative positions while mutating
        // self.data through the displacement chain.
        let key_buf = self.slot_raw_item(pos).key().to_vec();
        let alts = self.positions(&key_buf);

        // Prefer direct moves to empty or expired slots
        for &alt in &alts {
            if alt == pos {
                continue;
            }
            if self.slot_is_empty(alt) {
                self.move_slot(pos, alt);
                #[cfg(feature = "metrics")]
                metrics::CUCKOO_DISPLACE.increment();
                return true;
            }
            if self.slot_is_expired(alt) {
                self.handle_expired(alt);
                self.move_slot(pos, alt);
                #[cfg(feature = "metrics")]
                metrics::CUCKOO_DISPLACE.increment();
                return true;
            }
        }

        // Try deeper displacement: free an alternative slot first, then move
        for &alt in &alts {
            if alt == pos {
                continue;
            }
            if self.displace_from(alt, depth + 1) {
                self.move_slot(pos, alt);
                #[cfg(feature = "metrics")]
                metrics::CUCKOO_DISPLACE.increment();
                return true;
            }
        }

        false
    }

    /// Select a victim candidate index for eviction based on the configured
    /// policy.
    fn select_victim(&self, candidates: &[usize; D]) -> usize {
        match self.policy {
            Policy::Random => rand::rng().random::<u64>() as usize % D,
            Policy::Expire => {
                // Prefer evicting the item with the nearest expiration time.
                // Items with expire=0 (no expiry) are least preferred.
                let mut best = 0;
                let mut best_expire = u32::MAX;
                for (i, &pos) in candidates.iter().enumerate() {
                    let expire = self.slot_expire(pos);
                    if expire != 0 && expire < best_expire {
                        best = i;
                        best_expire = expire;
                    }
                }
                best
            }
        }
    }

    // -----------------------------------------------------------------------
    // Insertion helper
    // -----------------------------------------------------------------------

    /// Find a slot for inserting a key following the cuckoo insertion
    /// algorithm. Returns `(slot_index, is_update)`.
    ///
    /// The search order matches the C reference implementation:
    /// 1. Existing (non-expired) matching key → update in place
    /// 2. Empty slot
    /// 3. Expired slot
    /// 4. Displacement
    /// 5. Eviction by policy
    fn find_slot_for_insert(&mut self, key: &[u8], positions: &[usize; D]) -> (usize, bool) {
        // Pass 1: look for existing non-expired key
        for &pos in positions {
            if self.slot_is_empty(pos) || self.slot_is_expired(pos) {
                continue;
            }
            if self.slot_raw_item(pos).key() == key {
                return (pos, true);
            }
        }

        // Pass 2: use an empty slot
        for &pos in positions {
            if self.slot_is_empty(pos) {
                return (pos, false);
            }
        }

        // Pass 3: use an expired slot
        for &pos in positions {
            if self.slot_is_expired(pos) {
                self.handle_expired(pos);
                return (pos, false);
            }
        }

        // Pass 4: try displacement
        if let Some(freed_idx) = self.try_displace(positions) {
            return (positions[freed_idx], false);
        }

        // Pass 5: evict according to policy
        let victim_idx = self.select_victim(positions);
        let pos = positions[victim_idx];
        self.evict_at(pos);
        (pos, false)
    }

    // -----------------------------------------------------------------------
    // Public API
    // -----------------------------------------------------------------------

    /// Look up an item by key. Returns `None` if the item is not found or has
    /// expired. Expired items are lazily cleared on access.
    ///
    /// ```
    /// use cuckoo_cache::CuckooCache;
    /// use std::time::Duration;
    ///
    /// let mut cache = CuckooCache::builder().build();
    /// assert!(cache.get(b"coffee").is_none());
    ///
    /// cache.insert(b"coffee", b"strong", None, Duration::ZERO).unwrap();
    /// let item = cache.get(b"coffee").unwrap();
    /// assert_eq!(item.value(), b"strong");
    /// ```
    pub fn get(&mut self, key: &[u8]) -> Option<Item> {
        #[cfg(feature = "metrics")]
        metrics::CUCKOO_GET.increment();

        let positions = self.positions(key);

        for &pos in &positions {
            if self.slot_is_empty(pos) {
                continue;
            }

            if self.slot_raw_item(pos).key() == key {
                if self.slot_is_expired(pos) {
                    self.handle_expired(pos);
                    #[cfg(feature = "metrics")]
                    metrics::CUCKOO_GET_KEY_MISS.increment();
                    return None;
                }

                #[cfg(feature = "metrics")]
                metrics::CUCKOO_GET_KEY_HIT.increment();

                let expire = self.slot_expire(pos);
                return Some(Item::new(self.slot_raw_item(pos), expire));
            }
        }

        #[cfg(feature = "metrics")]
        metrics::CUCKOO_GET_KEY_MISS.increment();

        None
    }

    /// Insert an item into the cache. If the key already exists, its value and
    /// TTL are updated. When all candidate positions are occupied, items may be
    /// displaced or evicted according to the configured policy.
    ///
    /// ```
    /// use cuckoo_cache::CuckooCache;
    /// use std::time::Duration;
    ///
    /// let mut cache = CuckooCache::builder().build();
    /// cache.insert(b"drink", b"coffee", None, Duration::ZERO).unwrap();
    ///
    /// let item = cache.get(b"drink").unwrap();
    /// assert_eq!(item.value(), b"coffee");
    ///
    /// // Overwrite with a new value
    /// cache.insert(b"drink", b"whisky", None, Duration::ZERO).unwrap();
    /// let item = cache.get(b"drink").unwrap();
    /// assert_eq!(item.value(), b"whisky");
    /// ```
    pub fn insert<'a, T: Into<Value<'a>>>(
        &mut self,
        key: &[u8],
        value: T,
        optional: Option<&[u8]>,
        ttl: std::time::Duration,
    ) -> Result<(), CuckooCacheError> {
        let value: Value = value.into();
        let optional = optional.unwrap_or(&[]);

        let required =
            EXPIRE_SIZE + ITEM_HDR_SIZE + optional.len() + key.len() + keyvalue::size_of(&value);
        if required > self.item_size {
            #[cfg(feature = "metrics")]
            metrics::CUCKOO_INSERT_EX.increment();
            return Err(CuckooCacheError::ItemOversized {
                size: required,
                max: self.item_size,
            });
        }

        debug_assert!(!key.is_empty(), "empty keys are not supported");
        debug_assert!(
            key.len() <= u8::MAX as usize,
            "key length exceeds maximum of 255"
        );

        #[cfg(feature = "metrics")]
        metrics::CUCKOO_INSERT.increment();

        let expire = self.compute_expire(ttl);
        let positions = self.positions(key);
        let (pos, is_update) = self.find_slot_for_insert(key, &positions);

        if is_update {
            #[cfg(feature = "metrics")]
            {
                metrics::CUCKOO_UPDATE.increment();
                self.decrement_item_metrics(pos);
            }
        }

        self.write_slot(pos, key, value, optional, expire);

        #[cfg(feature = "metrics")]
        self.increment_item_metrics(pos);

        Ok(())
    }

    /// Remove the item with the given key. Returns `true` if the item was
    /// found and removed, `false` if not found (or expired).
    ///
    /// ```
    /// use cuckoo_cache::CuckooCache;
    /// use std::time::Duration;
    ///
    /// let mut cache = CuckooCache::builder().build();
    /// assert!(!cache.delete(b"coffee"));
    ///
    /// cache.insert(b"coffee", b"strong", None, Duration::ZERO).unwrap();
    /// assert!(cache.delete(b"coffee"));
    /// assert!(cache.get(b"coffee").is_none());
    /// ```
    pub fn delete(&mut self, key: &[u8]) -> bool {
        #[cfg(feature = "metrics")]
        metrics::CUCKOO_DELETE.increment();

        let positions = self.positions(key);

        for &pos in &positions {
            if self.slot_is_empty(pos) {
                continue;
            }
            if self.slot_raw_item(pos).key() == key {
                if self.slot_is_expired(pos) {
                    self.handle_expired(pos);
                    return false;
                }
                #[cfg(feature = "metrics")]
                self.decrement_item_metrics(pos);
                self.clear_slot(pos);
                return true;
            }
        }

        false
    }

    /// Clear all items from the cache.
    pub fn clear(&mut self) {
        self.data.fill(0);
    }

    /// Perform a wrapping addition on the numeric value stored at the given
    /// key. Returns the updated item, or an error if the key is not found or
    /// the value is not numeric.
    pub fn wrapping_add(&mut self, key: &[u8], rhs: u64) -> Result<Item, CuckooCacheError> {
        let positions = self.positions(key);

        for &pos in &positions {
            if self.slot_is_empty(pos) || self.slot_is_expired(pos) {
                continue;
            }
            if self.slot_raw_item(pos).key() == key {
                let off = self.slot_offset(pos) + EXPIRE_SIZE;
                // SAFETY: we have &mut self and the offset is within bounds.
                let ptr = unsafe { self.data.as_mut_ptr().add(off) };
                let mut raw = RawItem::from_ptr(ptr);
                raw.wrapping_add(rhs)
                    .map_err(|_| CuckooCacheError::NotNumeric)?;
                return Ok(Item::new(raw, self.slot_expire(pos)));
            }
        }

        Err(CuckooCacheError::NotFound)
    }

    /// Perform a saturating subtraction on the numeric value stored at the
    /// given key. Returns the updated item, or an error if the key is not
    /// found or the value is not numeric.
    pub fn saturating_sub(&mut self, key: &[u8], rhs: u64) -> Result<Item, CuckooCacheError> {
        let positions = self.positions(key);

        for &pos in &positions {
            if self.slot_is_empty(pos) || self.slot_is_expired(pos) {
                continue;
            }
            if self.slot_raw_item(pos).key() == key {
                let off = self.slot_offset(pos) + EXPIRE_SIZE;
                // SAFETY: we have &mut self and the offset is within bounds.
                let ptr = unsafe { self.data.as_mut_ptr().add(off) };
                let mut raw = RawItem::from_ptr(ptr);
                raw.saturating_sub(rhs)
                    .map_err(|_| CuckooCacheError::NotNumeric)?;
                return Ok(Item::new(raw, self.slot_expire(pos)));
            }
        }

        Err(CuckooCacheError::NotFound)
    }

    /// Get a count of live (non-expired) items. This is an expensive operation
    /// and is only enabled for tests and builds with the `debug` feature.
    ///
    /// ```
    /// use cuckoo_cache::CuckooCache;
    ///
    /// let cache = CuckooCache::builder().build();
    /// assert_eq!(cache.items(), 0);
    /// ```
    #[cfg(any(test, feature = "debug"))]
    pub fn items(&self) -> usize {
        (0..self.nitem)
            .filter(|&i| !self.slot_is_empty(i) && !self.slot_is_expired(i))
            .count()
    }
}
