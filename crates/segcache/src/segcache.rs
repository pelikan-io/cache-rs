// Copyright 2021 Twitter, Inc.
// Copyright 2023 Pelikan Cache contributors
// Licensed under the MIT and Apache-2.0 licenses

//! Core datastructure.

use crate::Value;
use crate::*;
use core::num::NonZeroU32;
use crossbeam_utils::Backoff;
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
}

// Compile-time guard: Segcache must be Send + Sync so Arc<Segcache> can be
// shared across threads for concurrent reads AND writes (item 7e). This
// relies on auto-derive — the hashtable carries its own `unsafe impl Send +
// Sync` for its raw-pointer internals, and every other field is a Send + Sync
// type (anonymous mmap, atomic headers, lock-free Injector queues, Xoshiro
// RNG, atomic TTL-bucket links). A future !Send or !Sync field breaks the
// build here rather than silently at 7e.
const _: () = {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    let _ = assert_send::<Segcache>;
    let _ = assert_sync::<Segcache>;
};

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

    /// Clamp a caller-supplied TTL into the coarse-clock seconds range.
    #[inline]
    fn coarse_ttl(ttl: std::time::Duration) -> Duration {
        Duration::from_secs(min(u32::MAX as u64, ttl.as_secs()) as u32)
    }

    /// Gets a count of items in the `Segcache` instance. This is an expensive
    /// operation and is only enabled for tests and builds with the `debug`
    /// feature enabled.
    ///
    /// ```
    /// use segcache::{Policy, Segcache};
    ///
    /// let cache = Segcache::builder().build().expect("failed to create cache");
    /// assert_eq!(cache.items(), 0);
    /// ```
    #[cfg(any(test, feature = "debug"))]
    pub fn items(&self) -> usize {
        trace!("getting segment item counts");
        self.segments.items()
    }

    /// Get the item in the `Segcache` with the provided key
    ///
    /// ```
    /// use segcache::{Policy, Segcache};
    /// use std::time::Duration;
    ///
    /// let cache = Segcache::builder().build().expect("failed to create cache");
    /// assert!(cache.get(b"coffee").is_none());
    ///
    /// cache.insert(b"coffee", b"strong", None, Duration::ZERO);
    /// let item = cache.get(b"coffee").expect("didn't get item back");
    /// assert_eq!(item.value(), b"strong");
    /// ```
    pub fn get(&self, key: &[u8]) -> Option<Item> {
        self.get_pinned(key, true)
    }

    /// Shared lookup for [`Self::get`]/[`Self::get_no_freq_incr`]: resolve
    /// the key, pin the item's segment, re-validate, and hand out the item.
    /// `update_freq` selects whether the initial lookup bumps the item's
    /// frequency counter.
    // inline(always) is measured, not cargo-cult (same story as
    // reserve_and_define): the extraction from get() cost ~3ns on the 255b
    // get benchmark until the call boundary was forced away. It also lets
    // the constant `update_freq` fold at each call site.
    #[inline(always)]
    fn get_pinned(&self, key: &[u8], update_freq: bool) -> Option<Item> {
        let verifier = self.verifier();
        let mut attempts = 0;
        loop {
            let (location, _freq) = if update_freq {
                self.hashtable.lookup(key, &verifier)?
            } else {
                self.hashtable.lookup_no_freq_update(key, &verifier)?
            };
            let (seg_id, offset) = unpack_location(location);
            let seg_id = NonZeroU32::new(seg_id)?;
            let (raw, guard) = self.segments.acquire_item_at(seg_id, offset)?;
            // Re-validate AFTER pinning (concurrent-write reader safety, item
            // 7f). Between the lookup and the pin, `location`'s segment can be
            // drained, recycled, and REUSED (a different item written at this
            // offset), so `raw` may be an aliased/torn read. A fresh lookup only
            // ever reads currently-published items — stale entries are removed
            // from the hashtable BEFORE a segment is recycled — so it is safe
            // and authoritative: if the key still resolves to this exact
            // `location`, the (now pinned, hence un-recyclable) segment genuinely
            // holds the item we want. If it no longer does, drop the pin and
            // retry; give up after a few attempts (a key churning under us).
            if self
                .hashtable
                .lookup_no_freq_update(key, &verifier)
                .map(|(l, _)| l)
                == Some(location)
            {
                raw.check_magic();
                let cas = Self::token_for(&raw, location, self.segments.generation(seg_id));
                return Some(Item::new(raw, cas, guard));
            }
            drop(guard);
            attempts += 1;
            if attempts >= RESERVE_RETRIES {
                return None;
            }
        }
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
    /// let cache = Segcache::builder().build().expect("failed to create cache");
    /// assert!(cache.get_no_freq_incr(b"coffee").is_none());
    /// ```
    pub fn get_no_freq_incr(&self, key: &[u8]) -> Option<Item> {
        self.get_pinned(key, false)
    }

    /// Insert a new item into the cache. May return an error indicating that
    /// the insert was not successful.
    /// ```
    /// use segcache::{Policy, Segcache};
    /// use std::time::Duration;
    ///
    /// let cache = Segcache::builder().build().expect("failed to create cache");
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
        &self,
        key: &'a [u8],
        value: T,
        optional: Option<&[u8]>,
        ttl: std::time::Duration,
    ) -> Result<(), SegcacheError> {
        let value: Value = value.into();

        // default optional data is empty
        let optional = optional.unwrap_or(&[]);

        let ttl = Self::coarse_ttl(ttl);

        let reserved = self.reserve_and_define(key, value, optional, ttl)?;

        let new_location = pack_location(reserved.seg(), reserved.offset() as u64);
        let (new_seg, new_offset) = (reserved.seg(), reserved.offset());
        let verifier = self.verifier();

        // Publish under the pin: `reserved` (and its WriterPin) is held across
        // the hashtable op(s) below so a concurrent drain cannot recycle the
        // segment between define and publish (item 7d, H2). It is dropped the
        // instant publish succeeds, on every path below, BEFORE any
        // `remove_at` — which can take a bucket `chain_lock` (empty-segment
        // drain / merge-compact) and would deadlock a drainer that waits on
        // `active_writers` WHILE holding that same `chain_lock` (lock-order
        // inversion). Invariant: never hold a WriterPin across a `chain_lock`
        // acquisition.
        //
        // Replace is now "lookup -> pinned cas_location-replace, else
        // insert-if-absent" rather than one atomic hashtable upsert (item 7f,
        // F2): the old item's location must be known BEFORE it is unlinked so
        // its segment can be pinned (`try_pin_remover`) across the unlink AND
        // the `remove_at` decrement — closing the window where a concurrent
        // eviction drain of that segment could interleave with the decrement.
        //
        // `lookup_slot` (item 7f perf follow-up) returns the slot the old
        // entry was found in alongside its location, so the publish below
        // uses `cas_location_at` to CAS that exact slot directly instead of
        // `cas_location` re-probing the key's candidate buckets from
        // scratch — the hashtable does one hash per op regardless, so this
        // only elides the redundant second bucket scan/verify.
        let backoff = Backoff::new();
        loop {
            match self.hashtable.lookup_slot(key, &verifier) {
                Some((old_location, slot)) => {
                    if old_location == new_location {
                        // Already published (a prior loop iteration's
                        // fresh-key upsert below raced another insert of this
                        // same reservation) — nothing left to unlink/decrement.
                        return Ok(());
                    }

                    let (old_seg_raw, old_offset) = unpack_location(old_location);
                    let Some(old_seg_id) = NonZeroU32::new(old_seg_raw) else {
                        // Not expected — `lookup_slot` only returns real
                        // (non-ghost) entries — but stay defensive and fall
                        // through to the same rollback used below.
                        break;
                    };

                    // Pin the OLD item's segment BEFORE unlinking it (item
                    // 7f). If a drain has already claimed the segment, it
                    // owns the item's removal — bail and retry the lookup.
                    let Some(pin) = self.segments.try_pin_remover(old_seg_id) else {
                        backoff.snooze();
                        continue;
                    };

                    if self
                        .hashtable
                        .cas_location_at(slot, old_location, new_location, true)
                    {
                        #[cfg(feature = "metrics")]
                        ITEM_REPLACE.increment();

                        drop(reserved);
                        let _ = self.segments.remove_at(
                            old_seg_id,
                            old_offset,
                            &self.ttl_buckets,
                            &self.hashtable,
                            pin,
                        );
                        return Ok(());
                    }

                    // Lost the unlink race — release the pin and retry.
                    drop(pin);
                }
                None => {
                    // Fresh key: insert. `hashtable.insert()` is itself an
                    // atomic upsert, so if a racing writer's insert of this
                    // same previously-absent key beat us here, our call
                    // unlinks THEIRS (not ours) as a side effect and returns
                    // it as `Ok(Some(raced_old))`. F4's matching-slot retry
                    // means this always resolves to exactly one hashtable
                    // entry (ours), but that racer's segment accounting is
                    // now our responsibility to decrement — same as any
                    // other replace, just with the unlink already done by
                    // the call above rather than by a pin-first
                    // `cas_location` (a narrow, accepted gap: if a drain
                    // claims that segment in the instant between the unlink
                    // above and the pin attempt below, the pin fails and
                    // that decrement is missed — concurrent fresh-insert
                    // de-dup racing a same-instant drain is a known separate
                    // follow-up).
                    match self
                        .hashtable
                        .insert(reserved.item().key(), new_location, &verifier)
                    {
                        Ok(None) => {
                            #[cfg(feature = "metrics")]
                            HASH_INSERT.increment();
                            return Ok(());
                        }
                        Ok(Some(raced_old)) => {
                            #[cfg(feature = "metrics")]
                            HASH_INSERT.increment();
                            drop(reserved);
                            let (raced_seg, raced_offset) = unpack_location(raced_old);
                            if let Some(raced_seg) = NonZeroU32::new(raced_seg) {
                                if let Some(pin) = self.segments.try_pin_remover(raced_seg) {
                                    let _ = self.segments.remove_at(
                                        raced_seg,
                                        raced_offset,
                                        &self.ttl_buckets,
                                        &self.hashtable,
                                        pin,
                                    );
                                }
                            }
                            return Ok(());
                        }
                        Err(()) => {
                            // Hashtable full — roll back the (unpublished)
                            // reservation.
                            #[cfg(feature = "metrics")]
                            HASH_INSERT_EX.increment();
                            self.rollback_reservation(reserved, new_seg, new_offset);
                            return Err(SegcacheError::HashTableInsertEx);
                        }
                    }
                }
            }
        }

        // Defensive fallback for the "invalid old location" break above.
        self.rollback_reservation(reserved, new_seg, new_offset);
        Err(SegcacheError::HashTableInsertEx)
    }

    /// Roll back an unpublished reservation: release its `WriterPin` (item
    /// 7d — always BEFORE `remove_at`, never across a `chain_lock`
    /// acquisition), then best-effort pin (item 7f) and decrement its
    /// segment. Used by `insert`/`replace_at` error paths that must discard
    /// a reserved-but-never-published item. If the pin fails (the segment is
    /// concurrently being drained), the drain owns the item's accounting —
    /// nothing further to do.
    fn rollback_reservation(&self, reserved: ReservedItem, seg: NonZeroU32, offset: usize) {
        drop(reserved);
        if let Some(pin) = self.segments.try_pin_remover(seg) {
            let _ = self
                .segments
                .remove_at(seg, offset, &self.ttl_buckets, &self.hashtable, pin);
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
        &self,
        key: &[u8],
        value: Value,
        optional: &[u8],
        ttl: Duration,
    ) -> Result<ReservedItem, SegcacheError> {
        // calculate size for item (numeric items carry an alignment pad
        // and a seqlock version word — reservation and the segment scan
        // must agree, so both use keyvalue's item_size)
        let size = keyvalue::item_size(key.len(), &value, optional.len());

        // For S3-FIFO: route the item by ghost-queue membership (a recently
        // evicted key skips the admission pool), then ensure the target pool
        // has room by evicting from it if it's at capacity — this enforces
        // the small/main ratio computed at construction time.
        let mut target_pool = SegmentPool::Main;
        if matches!(self.segments.evict_policy(), Policy::S3Fifo { .. }) {
            let hash = {
                let mut hasher = self.hashtable.hash_builder().build_hasher();
                hasher.write(key);
                hasher.finish()
            };
            if self.segments.ghost_contains(hash) {
                self.segments.ghost_remove(hash);
            } else {
                target_pool = SegmentPool::Admission;
            }

            if !self.segments.pool_has_room(target_pool) {
                let _ = self.segments.evict(&self.ttl_buckets, &self.hashtable);
            }
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
                    if let Ok(seg) = self.segments.segment(reserved_item.seg()) {
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
                    // Try to make room. Count a retry unless eviction actually
                    // raised the general free queue — i.e. produced a segment a
                    // reserve can use. An `evict()` that returns Ok but frees
                    // nothing usable (e.g. a merge pass that only refills the
                    // spare) must NOT grant an unbounded free retry, or this
                    // loop livelocks; bounding it turns "can't make room" into a
                    // NoFreeSegments error instead of a hang.
                    let before = self.segments.free_queue_len();
                    let evicted = self
                        .segments
                        .evict(&self.ttl_buckets, &self.hashtable)
                        .is_ok();
                    if evicted && self.segments.free_queue_len() > before {
                        // A segment became available to normal writes — retry
                        // the reserve without spending a retry.
                        continue;
                    }

                    retries -= 1;
                    if retries == 0 {
                        // couldn't make room: count the failed request and
                        // return with an error
                        #[cfg(feature = "metrics")]
                        {
                            SEGMENT_REQUEST.increment();
                            SEGMENT_REQUEST_FAILURE.increment();
                        }

                        return Err(SegcacheError::NoFreeSegments);
                    }
                }
            }
        }
    }

    /// Publish a reserved item by swapping the hashtable slot from
    /// `old_location` to the reserved item's location — the linearization
    /// point for CAS-style replacement. On success the old item is
    /// removed from its segment; if the entry no longer maps to
    /// `old_location`, the reservation is rolled back and `Exists` is
    /// returned.
    ///
    /// `old_slot` is the `SlotRef` the caller's `lookup_slot` found
    /// `old_location` at; it lets the publish below CAS that slot
    /// directly via `cas_location_at` instead of re-probing the key's
    /// candidate buckets (item 7f perf follow-up). Reused unchanged across
    /// retries in the loop below: `old_location` only ever lives in one
    /// slot at a time, so as long as `get_item_frequency` still finds
    /// `old_location` under `key`, it is still at `old_slot`.
    fn replace_at(
        &self,
        key: &[u8],
        old_location: Location,
        old_slot: SlotRef,
        reserved: ReservedItem,
    ) -> Result<(), SegcacheError> {
        let new_location = pack_location(reserved.seg(), reserved.offset() as u64);
        // Capture the reservation's own location up front so the rollback paths
        // can reclaim it AFTER the pin is released (see the drop-before-remove_at
        // invariant below), without borrowing `reserved`.
        let (new_seg, new_offset) = (reserved.seg(), reserved.offset());
        let (old_seg_id, old_offset) = unpack_location(old_location);
        let Some(old_seg_id) = NonZeroU32::new(old_seg_id) else {
            // invalid old location: roll back the (unpublished) reservation.
            self.rollback_reservation(reserved, new_seg, new_offset);
            return Err(SegcacheError::NotFound);
        };

        let backoff = Backoff::new();
        loop {
            // Pin the OLD item's segment BEFORE unlinking it (item 7f): the
            // pin brackets both the `cas_location` unlink below and the
            // `remove_at` decrement on success, so a concurrent drain of
            // `old_seg_id` cannot interleave with the decrement. If a drain
            // has already claimed the segment, check whether the entry still
            // resolves to `old_location`: if not, it was already moved or
            // removed — roll back and report `Exists` (same as the
            // post-CAS-failure check below); if it does, the drain claimed
            // the segment but has not yet drained this hashtable entry —
            // retry until it does.
            let pin = match self.segments.try_pin_remover(old_seg_id) {
                Some(pin) => pin,
                None => {
                    if self
                        .hashtable
                        .get_item_frequency(key, old_location)
                        .is_none()
                    {
                        self.rollback_reservation(reserved, new_seg, new_offset);
                        return Err(SegcacheError::Exists);
                    }
                    backoff.snooze();
                    continue;
                }
            };

            // Publish under the pin: `reserved` (and its WriterPin) is held
            // across the exchange so a concurrent drain cannot recycle the
            // segment between define and publish (item 7d, H2).
            if self
                .hashtable
                .cas_location_at(old_slot, old_location, new_location, true)
            {
                #[cfg(feature = "metrics")]
                ITEM_REPLACE.increment();

                // Release the WriterPin the instant publish succeeds, BEFORE
                // remove_at (which can take a bucket `chain_lock`; holding a
                // WriterPin across a `chain_lock` acquisition deadlocks a
                // drainer waiting on `active_writers` under that lock — item
                // 7d lock-order invariant). The remover `pin` above brackets
                // the unlink just performed and the decrement below (item
                // 7f); `remove_at` releases it, also before any `chain_lock`.
                drop(reserved);
                let _ = self.segments.remove_at(
                    old_seg_id,
                    old_offset,
                    &self.ttl_buckets,
                    &self.hashtable,
                    pin,
                );
                return Ok(());
            }

            // The exchange failed while pinned — release the remover pin.
            drop(pin);

            if self
                .hashtable
                .get_item_frequency(key, old_location)
                .is_none()
            {
                // The entry genuinely no longer maps to old_location
                // (replaced, relocated, or removed) — roll back the
                // (unpublished) reservation.
                self.rollback_reservation(reserved, new_seg, new_offset);
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
    fn remaining_ttl(&self, seg_id: NonZeroU32) -> Result<Duration, SegcacheError> {
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
    /// let cache = Segcache::builder().build().expect("failed to create cache");
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
        &self,
        key: &'a [u8],
        value: T,
        optional: Option<&[u8]>,
        ttl: std::time::Duration,
        cas: u64,
    ) -> Result<(), SegcacheError> {
        // Look up the current item to check its CAS token
        let verifier = self.verifier();
        let (location, slot) = self
            .hashtable
            .lookup_slot(key, &verifier)
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
        let ttl = Self::coarse_ttl(ttl);

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
        // `reserved` (and its WriterPin) is handed to `replace_at` by value and
        // stays alive there until publish — never dropped/destructured here
        // before the hashtable exchange (item 7d, H2).
        self.replace_at(key, location, slot, reserved)
    }

    /// Remove the item with the given key, returns a bool indicating if it was
    /// removed.
    /// ```
    /// use segcache::{Policy, Segcache, SegcacheError};
    /// use std::time::Duration;
    ///
    /// let cache = Segcache::builder().build().expect("failed to create cache");
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
    pub fn delete(&self, key: &[u8]) -> bool {
        // Look up the item to get its location
        let verifier = self.verifier();
        let (location, _freq) = match self.hashtable.lookup_no_freq_update(key, &verifier) {
            Some(result) => result,
            None => return false,
        };

        let (seg_id, offset) = unpack_location(location);
        let Some(seg_id) = NonZeroU32::new(seg_id) else {
            // Not expected — `lookup_no_freq_update` only returns real
            // (non-ghost) entries — but stay defensive: nothing to pin.
            return self.hashtable.remove(key, location);
        };

        // Pin the item's segment BEFORE unlinking it (item 7f): the pin
        // brackets both the hashtable unlink below and the `remove_at`
        // decrement, so a concurrent drain of this segment cannot interleave
        // with the decrement. If a drain has already claimed the segment, it
        // owns this item's removal — the key is going away either way (the
        // drain's own hashtable sweep will unlink it), so report success.
        let Some(pin) = self.segments.try_pin_remover(seg_id) else {
            return true;
        };

        // Remove from hashtable
        if !self.hashtable.remove(key, location) {
            drop(pin);
            return false;
        }

        #[cfg(feature = "metrics")]
        {
            HASH_REMOVE.increment();
            ITEM_DELETE.increment();
        }

        // Remove from segment
        if let Some(mut item) = self.segments.get_item_at(Some(seg_id), offset) {
            item.set_deleted(true);
        }
        let _ = self
            .segments
            .remove_at(seg_id, offset, &self.ttl_buckets, &self.hashtable, pin);

        true
    }

    /// Loops through the TTL Buckets to handle eager expiration, returns the
    /// number of segments expired
    /// ```
    /// use segcache::{Policy, Segcache, SegcacheError};
    /// use std::time::Duration;
    ///
    /// let cache = Segcache::builder().build().expect("failed to create cache");
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
    pub fn expire(&self) -> usize {
        self.ttl_buckets.expire(&self.hashtable, &self.segments)
    }

    /// Clear the cache, draining every segment from the hashtable.
    ///
    /// Returns the number of segments actually freed. Segments pinned by
    /// outstanding [`Item`]s are drained but not freed (and not counted)
    /// until a later pass runs after the pins drop.
    pub fn clear(&self) -> usize {
        self.ttl_buckets.clear(&self.hashtable, &self.segments)
    }

    /// Checks the integrity of all segments
    /// *NOTE*: this operation is relatively expensive
    #[cfg(feature = "debug")]
    pub fn check_integrity(&self) -> Result<(), SegcacheError> {
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
    pub fn wrapping_add(&self, key: &[u8], rhs: u64) -> Result<u64, SegcacheError> {
        self.numeric_update(key, |raw| raw.fetch_wrapping_add(rhs))
    }

    /// Perform a saturating subtraction on the value stored at the supplied
    /// key. Returns an error if the key is invalid, the item is not found, or
    /// the stored value is not a numeric type.
    ///
    /// See [`Self::wrapping_add`] for the update and CAS-token semantics.
    /// Returns the new value.
    pub fn saturating_sub(&self, key: &[u8], rhs: u64) -> Result<u64, SegcacheError> {
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
        &self,
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
            self.remaining_ttl(seg_id)?;

            match self.segments.acquire_item_at(seg_id, offset) {
                // Segment not readable (draining; a relocation is in
                // flight) — retry from the lookup.
                None => continue,
                Some((raw, _guard)) => {
                    // Re-validate after pinning (see `get`): if the key no longer
                    // resolves to this exact location, the segment was
                    // recycled+reused between lookup and pin and `raw` is a
                    // stale/aliased item — retry rather than update the WRONG
                    // item in place (item 7f). The fresh lookup only reads
                    // currently-published items, so it is safe and authoritative.
                    if self.hashtable.lookup(key, &verifier).map(|(l, _)| l) != Some(location) {
                        continue;
                    }
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
        &self,
        key: &[u8],
        initial: u64,
        ttl: std::time::Duration,
    ) -> Result<(), SegcacheError> {
        let verifier = self.verifier();
        let Some((location, slot)) = self.hashtable.lookup_slot(key, &verifier) else {
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
        self.replace_at(key, location, slot, reserved)
    }

    /// Test-only access to the segment collection, for asserting on segment
    /// headers (e.g. `active_writers()`) after write operations return.
    #[cfg(test)]
    pub(crate) fn segments_for_test(&self) -> &Segments {
        &self.segments
    }
}
