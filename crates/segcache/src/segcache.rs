// Copyright 2021 Twitter, Inc.
// Copyright 2023 Pelikan Cache contributors
// Licensed under the MIT and Apache-2.0 licenses

//! Core datastructure.

use crate::Value;
use crate::*;
use core::num::NonZeroU32;
use std::cmp::min;

const RESERVE_RETRIES: usize = 3;

/// A pre-allocated key-value store with eager expiration. It uses a
/// segment-structured design that stores data in fixed-size segments, grouping
/// objects with nearby expiration time into the same segment, and lifting most
/// per-object metadata into the shared segment header.
pub struct Segcache {
    pub(crate) hashtable: MultiChoiceHashtable,
    pub(crate) segments: Segments,
    pub(crate) ttl_buckets: TtlBuckets,
    pub(crate) time: Instant,
}

impl Segcache {
    /// Returns a new `Builder` which is used to configure and construct a
    /// `Segcache` instance.
    ///
    /// ```
    /// use segcache::{Policy, Segcache};
    ///
    /// const MB: usize = 1024 * 1024;
    ///
    /// // create a heap using 1MB segments
    /// let cache = Segcache::builder()
    ///     .heap_size(64 * MB)
    ///     .segment_size(1 * MB as i32)
    ///     .hash_power(16)
    ///     .eviction(Policy::Random).build().expect("failed to create cache");
    /// ```
    pub fn builder() -> Builder {
        Builder::default()
    }

    /// Create a SegmentsVerifier for the current segments state.
    #[inline]
    fn verifier(&self) -> SegmentsVerifier<'_> {
        self.segments.verifier()
    }

    /// Gets a count of items in the `Segcache` instance. This is an expensive
    /// operation and is only enabled for tests and builds with the `debug`
    /// feature enabled.
    ///
    /// ```
    /// use segcache::{Policy, Segcache};
    ///
    /// let mut cache = Segcache::builder().build().expect("failed to create cache");
    /// assert_eq!(cache.items(), 0);
    /// ```
    #[cfg(any(test, feature = "debug"))]
    pub fn items(&mut self) -> usize {
        trace!("getting segment item counts");
        self.segments.items()
    }

    /// Get the item in the `Segcache` with the provided key
    ///
    /// ```
    /// use segcache::{Policy, Segcache};
    /// use std::time::Duration;
    ///
    /// let mut cache = Segcache::builder().build().expect("failed to create cache");
    /// assert!(cache.get(b"coffee").is_none());
    ///
    /// cache.insert(b"coffee", b"strong", None, Duration::ZERO);
    /// let item = cache.get(b"coffee").expect("didn't get item back");
    /// assert_eq!(item.value(), b"strong");
    /// ```
    pub fn get(&mut self, key: &[u8]) -> Option<Item> {
        let verifier = self.verifier();
        let (location, _freq) = self.hashtable.lookup(key, &verifier)?;
        let (seg_id, offset) = unpack_location(location);
        let seg_id = NonZeroU32::new(seg_id)?;
        let (raw, guard) = self.segments.acquire_item_at(seg_id, offset)?;
        raw.check_magic();

        let cas = Self::token_for(&raw, location, self.segments.generation(seg_id));
        Some(Item::new(raw, cas, guard))
    }

    /// Build the CAS token for an item: location + segment generation,
    /// with a numeric item's seqlock version folded in so that in-place
    /// increments bump the token (memcached's incr/decr assign a fresh
    /// cas unique).
    #[inline]
    fn token_for(raw: &RawItem, location: Location, generation: u16) -> u64 {
        let base = CasToken::new(location, generation).as_raw();
        match raw.numeric_version() {
            Some(version) => crate::cas::mix_version(base, version),
            None => base,
        }
    }

    /// Get the item in the `Segcache` with the provided key without
    /// increasing the item frequency - useful for combined operations that
    /// check for presence - eg replace is a get + set
    /// ```
    /// use segcache::{Policy, Segcache};
    ///
    /// let mut cache = Segcache::builder().build().expect("failed to create cache");
    /// assert!(cache.get_no_freq_incr(b"coffee").is_none());
    /// ```
    pub fn get_no_freq_incr(&mut self, key: &[u8]) -> Option<Item> {
        let verifier = self.verifier();
        let (location, _freq) = self.hashtable.lookup_no_freq_update(key, &verifier)?;
        let (seg_id, offset) = unpack_location(location);
        let seg_id = NonZeroU32::new(seg_id)?;
        let (raw, guard) = self.segments.acquire_item_at(seg_id, offset)?;
        raw.check_magic();

        let cas = Self::token_for(&raw, location, self.segments.generation(seg_id));
        Some(Item::new(raw, cas, guard))
    }

    /// Insert a new item into the cache. May return an error indicating that
    /// the insert was not successful.
    /// ```
    /// use segcache::{Policy, Segcache};
    /// use std::time::Duration;
    ///
    /// let mut cache = Segcache::builder().build().expect("failed to create cache");
    /// assert!(cache.get(b"drink").is_none());
    ///
    /// cache.insert(b"drink", b"coffee", None, Duration::ZERO);
    /// let item = cache.get(b"drink").expect("didn't get item back");
    /// assert_eq!(item.value(), b"coffee");
    ///
    /// cache.insert(b"drink", b"whisky", None, Duration::ZERO);
    /// let item = cache.get(b"drink").expect("didn't get item back");
    /// assert_eq!(item.value(), b"whisky");
    /// ```
    pub fn insert<'a, T: Into<Value<'a>>>(
        &mut self,
        key: &'a [u8],
        value: T,
        optional: Option<&[u8]>,
        ttl: std::time::Duration,
    ) -> Result<(), SegcacheError> {
        let value: Value = value.into();

        // default optional data is empty
        let optional = optional.unwrap_or(&[]);

        let ttl = Duration::from_secs(min(u32::MAX as u64, ttl.as_secs()) as u32);

        let reserved = self.reserve_and_define(key, value, optional, ttl)?;

        let location = pack_location(reserved.seg(), reserved.offset() as u64);
        let verifier = self.verifier();

        match self
            .hashtable
            .insert(reserved.item().key(), location, &verifier)
        {
            Ok(Some(old_location)) => {
                // Replaced existing key — remove old item from segment
                let (old_seg_id, old_offset) = unpack_location(old_location);
                if let Some(old_seg_id) = NonZeroU32::new(old_seg_id) {
                    #[cfg(feature = "metrics")]
                    ITEM_REPLACE.increment();

                    let _ = self.segments.remove_at(
                        old_seg_id,
                        old_offset,
                        &mut self.ttl_buckets,
                        &self.hashtable,
                    );
                }
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(()) => {
                // Hashtable full — roll back the segment allocation
                let _ = self.segments.remove_at(
                    reserved.seg(),
                    reserved.offset(),
                    &mut self.ttl_buckets,
                    &self.hashtable,
                );
                Err(SegcacheError::HashTableInsertEx)
            }
        }
    }

    /// Reserve segment space for an item and write its bytes, without
    /// publishing it in the hashtable. Handles S3-FIFO pool targeting and
    /// runs eviction (with retries) when no free segment is available.
    // inline(always) is measured, not cargo-cult: the extraction from
    // insert() cost ~2.6ns (+6%) on the set benchmark until the call
    // boundary was forced away; #[inline] alone did not recover it.
    #[inline(always)]
    fn reserve_and_define(
        &mut self,
        key: &[u8],
        value: Value,
        optional: &[u8],
        ttl: Duration,
    ) -> Result<ReservedItem, SegcacheError> {
        // calculate size for item (numeric items carry an alignment pad
        // and a seqlock version word — reservation and the segment scan
        // must agree, so both use keyvalue's item_size)
        let size = keyvalue::item_size(key.len(), &value, optional.len());

        // For S3-FIFO: determine target pool based on ghost queue
        let target_pool = if matches!(self.segments.evict_policy(), Policy::S3Fifo { .. }) {
            let hash = {
                let mut hasher = self.hashtable.hash_builder().build_hasher();
                hasher.write(key);
                hasher.finish()
            };
            if self.segments.ghost_contains(hash) {
                self.segments.ghost_remove(hash);
                SegmentPool::Main
            } else {
                SegmentPool::Admission
            }
        } else {
            SegmentPool::Main
        };

        // For S3-FIFO: ensure the target pool has room by evicting from it
        // if it's at capacity. This enforces the small/main ratio computed
        // at construction time.
        if matches!(self.segments.evict_policy(), Policy::S3Fifo { .. })
            && !self.segments.pool_has_room(target_pool)
        {
            let _ = self.segments.evict(&mut self.ttl_buckets, &self.hashtable);
        }

        let mut retries = RESERVE_RETRIES;
        loop {
            match self
                .ttl_buckets
                .get_bucket(ttl)
                .reserve(size, &self.segments)
            {
                Ok(mut reserved_item) => {
                    reserved_item.define(key, value, optional);
                    // Set the segment pool for S3-FIFO (only transitions
                    // Main→Admission need a counter update; fresh segments
                    // default to Main)
                    if let Ok(seg) = self.segments.get_mut(reserved_item.seg()) {
                        if target_pool == SegmentPool::Admission
                            && seg.pool() != SegmentPool::Admission
                        {
                            seg.set_pool(target_pool);
                            self.segments.incr_pool(SegmentPool::Admission);
                        }
                    }
                    return Ok(reserved_item);
                }
                Err(TtlBucketsError::ItemOversized { size }) => {
                    return Err(SegcacheError::ItemOversized { size });
                }
                Err(TtlBucketsError::NoFreeSegments) => {
                    if self
                        .segments
                        .evict(&mut self.ttl_buckets, &self.hashtable)
                        .is_err()
                    {
                        retries -= 1;
                    } else {
                        // we successfully evicted a segment, return to start of
                        // loop to reserve the item
                        continue;
                    }
                }
            }
            if retries == 0 {
                // segment acquire failed, increment the stats and return with
                // an error

                #[cfg(feature = "metrics")]
                {
                    SEGMENT_REQUEST.increment();
                    SEGMENT_REQUEST_FAILURE.increment();
                }

                return Err(SegcacheError::NoFreeSegments);
            }
            retries -= 1;
        }
    }

    /// Publish a reserved item by swapping the hashtable slot from
    /// `old_location` to the reserved item's location — the linearization
    /// point for CAS-style replacement. On success the old item is
    /// removed from its segment; if the entry no longer maps to
    /// `old_location`, the reservation is rolled back and `Exists` is
    /// returned.
    fn replace_at(
        &mut self,
        key: &[u8],
        old_location: Location,
        reserved: ReservedItem,
    ) -> Result<(), SegcacheError> {
        let new_location = pack_location(reserved.seg(), reserved.offset() as u64);
        let (old_seg_id, old_offset) = unpack_location(old_location);
        let old_seg_id = match NonZeroU32::new(old_seg_id) {
            Some(id) => id,
            None => {
                // invalid old location: roll back the reservation
                let _ = self.segments.remove_at(
                    reserved.seg(),
                    reserved.offset(),
                    &mut self.ttl_buckets,
                    &self.hashtable,
                );
                return Err(SegcacheError::NotFound);
            }
        };

        loop {
            if self
                .hashtable
                .cas_location(key, old_location, new_location, true)
            {
                #[cfg(feature = "metrics")]
                ITEM_REPLACE.increment();

                let _ = self.segments.remove_at(
                    old_seg_id,
                    old_offset,
                    &mut self.ttl_buckets,
                    &self.hashtable,
                );
                return Ok(());
            }

            if self
                .hashtable
                .get_item_frequency(key, old_location)
                .is_none()
            {
                // The entry genuinely no longer maps to old_location
                // (replaced, relocated, or removed) — roll back.
                let _ = self.segments.remove_at(
                    reserved.seg(),
                    reserved.offset(),
                    &mut self.ttl_buckets,
                    &self.hashtable,
                );
                return Err(SegcacheError::Exists);
            }

            // The entry is still at old_location: the exchange failed
            // spuriously (a concurrent reader bumped the frequency bits
            // in the packed slot mid-exchange). Unreachable under &mut
            // today; retry for the concurrent future.
        }
    }

    /// Remaining TTL for an item's segment — the time until its expiry
    /// deadline. Numeric rewrites reserve with this so the item's
    /// absolute expiration is preserved, matching memcached: incr/decr
    /// keep the original exptime (do_add_delta passes `it->exptime` even
    /// when it must reallocate). An already-elapsed deadline returns
    /// `NotFound`, matching memcached's treatment of expired keys.
    ///
    /// Note there is no true "no expiry" in segcache: `Duration::ZERO`
    /// maps to the last TTL bucket (representative TTL ~97 days), and a
    /// large remaining TTL clamps back to that same bucket, so
    /// effectively-non-expiring counters stay effectively non-expiring.
    /// The zero check below is defensive (linked segments always carry a
    /// bucket TTL >= 1s).
    fn remaining_ttl(&mut self, seg_id: NonZeroU32) -> Result<Duration, SegcacheError> {
        let (create_at, ttl) = self.segments.expiry_info(seg_id);
        if ttl.as_secs() == 0 {
            return Ok(Duration::from_secs(0));
        }
        let now = Instant::now();
        let expires_at = create_at + ttl;
        if expires_at <= now {
            return Err(SegcacheError::NotFound);
        }
        Ok(expires_at - now)
    }

    /// Performs a CAS operation, inserting the item only if the CAS value
    /// matches the current value for that item.
    ///
    /// ```
    /// use segcache::{Policy, Segcache, SegcacheError};
    /// use std::time::Duration;
    ///
    /// let mut cache = Segcache::builder().build().expect("failed to create cache");
    ///
    /// // If the item is not in the cache, CAS will fail as 'NotFound'
    /// assert_eq!(
    ///     cache.cas(b"drink", b"coffee", None, Duration::ZERO, 0),
    ///     Err(SegcacheError::NotFound)
    /// );
    ///
    /// // If a stale CAS value is provided, CAS will fail as 'Exists'
    /// cache.insert(b"drink", b"coffee", None, Duration::ZERO);
    /// assert_eq!(
    ///     cache.cas(b"drink", b"coffee", None, Duration::ZERO, 0),
    ///     Err(SegcacheError::Exists)
    /// );
    ///
    /// // Getting the CAS value and then performing the operation ensures
    /// // success in absence of a race with another client
    /// let current = cache.get(b"drink").expect("not found");
    /// assert!(cache.cas(b"drink", b"whisky", None, Duration::ZERO, current.cas()).is_ok());
    /// let item = cache.get(b"drink").expect("not found");
    /// assert_eq!(item.value(), b"whisky"); // item is updated
    /// ```
    pub fn cas<'a, T: Into<Value<'a>>>(
        &mut self,
        key: &'a [u8],
        value: T,
        optional: Option<&[u8]>,
        ttl: std::time::Duration,
        cas: u64,
    ) -> Result<(), SegcacheError> {
        // Look up the current item to check its CAS token
        let verifier = self.verifier();
        let (location, _freq) = self
            .hashtable
            .lookup_no_freq_update(key, &verifier)
            .ok_or(SegcacheError::NotFound)?;

        let (seg_id, offset) = unpack_location(location);
        let seg_id = NonZeroU32::new(seg_id).ok_or(SegcacheError::NotFound)?;
        let current_cas = {
            // Pin briefly to read the item's seqlock version (numeric
            // items fold it into the token); drop the pin before the
            // reservation below — pinned segments are unevictable.
            let (raw, _guard) = self
                .segments
                .acquire_item_at(seg_id, offset)
                .ok_or(SegcacheError::NotFound)?;
            Self::token_for(&raw, location, self.segments.generation(seg_id))
        };
        if current_cas != cas {
            return Err(SegcacheError::Exists);
        }

        let value: Value = value.into();
        let optional = optional.unwrap_or(&[]);
        let ttl = Duration::from_secs(min(u32::MAX as u64, ttl.as_secs()) as u32);

        // Publish by swapping the hashtable slot only if it still holds
        // the token-checked location — the linearization point. A plain
        // insert would replace whatever entry is current, silently losing
        // a write that landed between the token check and the publish.
        //
        // Behavior note: eviction triggered by this reservation can
        // relocate or evict the checked item, in which case the CAS now
        // fails with `Exists` (fail-safe) where it previously succeeded
        // through the plain insert.
        let reserved = self.reserve_and_define(key, value, optional, ttl)?;
        self.replace_at(key, location, reserved)
    }

    /// Remove the item with the given key, returns a bool indicating if it was
    /// removed.
    /// ```
    /// use segcache::{Policy, Segcache, SegcacheError};
    /// use std::time::Duration;
    ///
    /// let mut cache = Segcache::builder().build().expect("failed to create cache");
    ///
    /// // If the item is not in the cache, delete will return false
    /// assert_eq!(cache.delete(b"coffee"), false);
    ///
    /// // And will return true on success
    /// cache.insert(b"coffee", b"strong", None, Duration::ZERO);
    /// assert!(cache.get(b"coffee").is_some());
    /// assert_eq!(cache.delete(b"coffee"), true);
    /// assert!(cache.get(b"coffee").is_none());
    /// ```
    // TODO(bmartin): a result would be better here
    pub fn delete(&mut self, key: &[u8]) -> bool {
        // Look up the item to get its location
        let verifier = self.verifier();
        let (location, _freq) = match self.hashtable.lookup_no_freq_update(key, &verifier) {
            Some(result) => result,
            None => return false,
        };

        // Remove from hashtable
        if !self.hashtable.remove(key, location) {
            return false;
        }

        #[cfg(feature = "metrics")]
        {
            HASH_REMOVE.increment();
            ITEM_DELETE.increment();
        }

        // Remove from segment
        let (seg_id, offset) = unpack_location(location);
        if let Some(seg_id) = NonZeroU32::new(seg_id) {
            if let Some(mut item) = self.segments.get_item_at(Some(seg_id), offset) {
                item.set_deleted(true);
            }
            let _ = self
                .segments
                .remove_at(seg_id, offset, &mut self.ttl_buckets, &self.hashtable);
        }

        true
    }

    /// Loops through the TTL Buckets to handle eager expiration, returns the
    /// number of segments expired
    /// ```
    /// use segcache::{Policy, Segcache, SegcacheError};
    /// use std::time::Duration;
    ///
    /// let mut cache = Segcache::builder().build().expect("failed to create cache");
    ///
    /// // Insert an item with a short ttl
    /// cache.insert(b"coffee", b"strong", None, Duration::from_secs(5));
    ///
    /// // The item is still in the cache
    /// assert!(cache.get(b"coffee").is_some());
    ///
    /// // Delay and then trigger expiration
    /// std::thread::sleep(Duration::from_secs(6));
    /// cache.expire();
    ///
    /// // And the expired item is not in the cache
    /// assert!(cache.get(b"coffee").is_none());
    /// ```
    /// Returns the number of segments actually freed. Segments pinned by
    /// outstanding [`Item`]s are drained from the hashtable but not freed
    /// (and not counted) until a later pass runs after the pins drop.
    pub fn expire(&mut self) -> usize {
        self.time = Instant::now();
        self.ttl_buckets.expire(&self.hashtable, &mut self.segments)
    }

    /// Clear the cache, draining every segment from the hashtable.
    ///
    /// Returns the number of segments actually freed. Segments pinned by
    /// outstanding [`Item`]s are drained but not freed (and not counted)
    /// until a later pass runs after the pins drop.
    pub fn clear(&mut self) -> usize {
        self.time = Instant::now();
        self.ttl_buckets.clear(&self.hashtable, &mut self.segments)
    }

    /// Checks the integrity of all segments
    /// *NOTE*: this operation is relatively expensive
    #[cfg(feature = "debug")]
    pub fn check_integrity(&mut self) -> Result<(), SegcacheError> {
        if self.segments.check_integrity(&self.hashtable) {
            Ok(())
        } else {
            Err(SegcacheError::DataCorrupted)
        }
    }

    /// Perform a wrapping addition on the value stored at the supplied key.
    /// Returns an error if the key is invalid, the item is not found, or the
    /// stored value is not a numeric type.
    ///
    /// The update happens IN PLACE under the item's seqlock: no item or
    /// segment churn, the expiration deadline is untouched (memcached's
    /// incr/decr preserve exptime), and the item's seqlock version —
    /// folded into its CAS token — bumps on every update, so tokens
    /// observe increments exactly as memcached's do_add_delta assigns a
    /// fresh cas unique. An already-expired counter returns `NotFound`.
    ///
    /// Returns the new value, as memcached's incr does. Held `Item`s
    /// alias the same memory and observe updates (seqlock-consistent,
    /// never torn).
    pub fn wrapping_add(&mut self, key: &[u8], rhs: u64) -> Result<u64, SegcacheError> {
        self.numeric_update(key, |raw| raw.fetch_wrapping_add(rhs))
    }

    /// Perform a saturating subtraction on the value stored at the supplied
    /// key. Returns an error if the key is invalid, the item is not found, or
    /// the stored value is not a numeric type.
    ///
    /// See [`Self::wrapping_add`] for the update and CAS-token semantics.
    /// Returns the new value.
    pub fn saturating_sub(&mut self, key: &[u8], rhs: u64) -> Result<u64, SegcacheError> {
        self.numeric_update(key, |raw| raw.fetch_saturating_sub(rhs))
    }

    /// Shared in-place update for the numeric operations.
    ///
    /// Looks up the key, checks its segment deadline (memcached lazily
    /// treats expired keys as missing — increments must not resurrect
    /// them), pins the segment, and performs the seqlocked in-place
    /// update through the pinned item. The pin is the load-bearing
    /// safety argument for mutating in place: eviction\'s byte-copy
    /// paths (merge prune/copy_into) only proceed on segments
    /// whose reader count is zero (the Sealed -> Draining CAS plus the
    /// SeqCst recheck-and-revert from the drain protocol), so segment
    /// memory cannot be moved or reused out from under the update.
    ///
    /// Returns the value this call published.
    fn numeric_update(
        &mut self,
        key: &[u8],
        op: impl Fn(&RawItem) -> Result<u64, keyvalue::NotNumericError>,
    ) -> Result<u64, SegcacheError> {
        loop {
            let verifier = self.verifier();
            let (location, _freq) = self
                .hashtable
                .lookup(key, &verifier)
                .ok_or(SegcacheError::NotFound)?;
            let (seg_id, offset) = unpack_location(location);
            let seg_id = NonZeroU32::new(seg_id).ok_or(SegcacheError::NotFound)?;

            // Lazy expiry: a counter past its segment deadline is
            // treated as missing, matching memcached, even before
            // expire() reclaims the segment.
            let (create_at, ttl) = self.segments.expiry_info(seg_id);
            if ttl.as_secs() != 0 && create_at + ttl <= Instant::now() {
                return Err(SegcacheError::NotFound);
            }

            match self.segments.acquire_item_at(seg_id, offset) {
                // Segment not readable (draining; a relocation is in
                // flight) — retry from the lookup.
                None => continue,
                Some((raw, _guard)) => {
                    return op(&raw).map_err(|_| SegcacheError::NotNumeric);
                }
            }
        }
    }

    /// Ensure the value stored at `key` is numeric.
    ///
    /// - key missing: creates a numeric item with `initial`, using `ttl`
    /// - existing numeric value: no-op success (`ttl` unused)
    /// - existing bytes value that is a canonical ASCII `u64` (see
    ///   [`keyvalue::numeric::parse_simple_numeric`]): converts it to a
    ///   numeric item with the SAME value and the REMAINING TTL of the
    ///   existing item (its absolute expiration is preserved) — the
    ///   caller's `ttl` is deliberately unused
    /// - any other value: `Err(NotNumeric)`, item untouched
    ///
    /// Composes with [`Self::wrapping_add`]/[`Self::saturating_sub`] to
    /// implement memcached-style incr-with-initial at a protocol layer.
    pub fn try_into_numeric(
        &mut self,
        key: &[u8],
        initial: u64,
        ttl: std::time::Duration,
    ) -> Result<(), SegcacheError> {
        let verifier = self.verifier();
        let Some((location, _freq)) = self.hashtable.lookup_no_freq_update(key, &verifier) else {
            // Missing: create with the caller's ttl. NOTE for the
            // concurrent future: this publishes via plain insert, which
            // would overwrite a concurrently created value; revisit with
            // insert-if-absent when the API goes concurrent.
            return self.insert(key, initial, None, ttl);
        };

        let (seg_id, offset) = unpack_location(location);
        let seg_id = NonZeroU32::new(seg_id).ok_or(SegcacheError::NotFound)?;

        let (parsed, opt_buf, olen, seg_ttl) = {
            let (raw, _guard) = self
                .segments
                .acquire_item_at(seg_id, offset)
                .ok_or(SegcacheError::NotFound)?;
            let parsed = match raw.value() {
                Value::U64(_) => return Ok(()),
                Value::Bytes(b) => {
                    keyvalue::numeric::parse_simple_numeric(b).ok_or(SegcacheError::NotNumeric)?
                }
            };
            let mut opt_buf = [0u8; 63];
            let olen = raw.optional().map_or(0, |o| {
                opt_buf[..o.len()].copy_from_slice(o);
                o.len()
            });
            let seg_ttl = self.remaining_ttl(seg_id)?;
            (parsed, opt_buf, olen, seg_ttl)
        };

        let reserved =
            self.reserve_and_define(key, Value::U64(parsed), &opt_buf[..olen], seg_ttl)?;
        self.replace_at(key, location, reserved)
    }
}
