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

/// How many post-pin revalidation mismatches `get_pinned` tolerates before it
/// reports a miss (#65).
///
/// A mismatch means another thread published a NEW location for this exact key
/// between our pin and our revalidation, so every retry is paid for by real
/// system-wide progress — the same termination argument `cas_location` and
/// `try_unlink_in_bucket` rely on. That makes an UNBOUNDED loop lock-free but
/// not starvation-free: unlike a drain (bounded, straight-line work that must
/// finish), nothing bounds how long a stream of writers keeps rewriting a hot
/// key, and this is the library's hottest path. So the loop keeps a hard cap.
///
/// The cap is its own constant rather than `RESERVE_RETRIES` because it bounds
/// a different thing (concurrent republications of one key, not reservation
/// attempts), and because 3 is far too tight: with retries that CONVERGE on
/// the location the revalidation just returned, exhausting the budget needs 16
/// consecutive republications each landing inside a pin+lookup window. At the
/// ~1% per-attempt mismatch rate measured on the worst realistic workload
/// (#65: one key rewritten by 24 oversubscribed `cas` threads while read) that
/// is ~1e-32, versus ~1e-6 at 3 — while still capping a `get` at 16 pins.
pub(crate) const REVALIDATE_RETRIES: usize = 16;

/// Fault injection for `get_pinned`'s lookup/revalidate window. Compiled only
/// under `feature = "fault-injection"`; a normal build has no trace of it.
///
/// The revalidation race is a two-thread interleaving that no unit test can
/// schedule directly: a writer must republish the key in the window between a
/// reader's lookup and its revalidation. These two hooks let a single-threaded
/// test stand in for that writer at exactly the two points that matter, which
/// is what makes the #65 coverage deterministic instead of a stress run.
///
/// - [`after_lookup`] fires after a FROM-SCRATCH lookup resolves a location.
///   The converging retry loop does one of these per `get`, so a hook that
///   republishes on every firing loops forever against the pre-#65 code (which
///   re-looked-up on every attempt) and fires exactly once against this one.
/// - [`before_revalidate`] fires after the pin, before the revalidation
///   lookup — i.e. inside the window the retry budget exists to survive.
#[cfg(feature = "fault-injection")]
pub(crate) mod revalidation_fault {
    use std::cell::RefCell;
    use std::rc::Rc;

    type Hook = Rc<dyn Fn()>;

    thread_local! {
        static AFTER_LOOKUP: RefCell<Option<Hook>> = const { RefCell::new(None) };
        static BEFORE_REVALIDATE: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    /// Uninstalls both hooks when dropped, so a panicking test cannot leak one
    /// into whatever else runs on this thread.
    #[cfg(all(test, not(feature = "loom")))]
    pub(crate) struct HookGuard(());

    #[cfg(all(test, not(feature = "loom")))]
    impl Drop for HookGuard {
        fn drop(&mut self) {
            AFTER_LOOKUP.with(|h| *h.borrow_mut() = None);
            BEFORE_REVALIDATE.with(|h| *h.borrow_mut() = None);
        }
    }

    #[cfg(all(test, not(feature = "loom")))]
    pub(crate) fn on_after_lookup(f: impl Fn() + 'static) -> HookGuard {
        AFTER_LOOKUP.with(|h| *h.borrow_mut() = Some(Rc::new(f)));
        HookGuard(())
    }

    #[cfg(all(test, not(feature = "loom")))]
    pub(crate) fn on_before_revalidate(f: impl Fn() + 'static) -> HookGuard {
        BEFORE_REVALIDATE.with(|h| *h.borrow_mut() = Some(Rc::new(f)));
        HookGuard(())
    }

    // The hook is cloned out before it is called: the callback re-enters the
    // cache (that is the point — it republishes the key), and a live
    // `RefCell` borrow across that call would be a re-entrancy panic waiting
    // for the first hook that also reads a hook.
    #[inline]
    pub(crate) fn after_lookup() {
        if let Some(hook) = AFTER_LOOKUP.with(|h| h.borrow().clone()) {
            hook();
        }
    }

    #[inline]
    pub(crate) fn before_revalidate() {
        if let Some(hook) = BEFORE_REVALIDATE.with(|h| h.borrow().clone()) {
            hook();
        }
    }
}

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

    /// Get the item in the `Segcache` with the provided key.
    ///
    /// Expiry is lazy on access: an item past its TTL deadline returns
    /// `None`, matching memcached, even before `expire()` or eviction
    /// pressure reclaims its segment. Items stored with `Duration::ZERO`
    /// never expire.
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
        let backoff = Backoff::new();
        let mut attempts = 0;
        let mut location = self.lookup_location(key, &verifier, update_freq)?;
        loop {
            let (seg_id, _offset) = unpack_location(location);
            let seg_id = NonZeroU32::new(seg_id)?;
            // The incarnation check lives INSIDE the pin, not in front of it:
            // `acquire_item_at` takes the whole `Location` and compares its tag
            // against the segment's generation under the reader guard, which is
            // what freezes the generation while it is read. A stale incarnation
            // therefore fails the pin, and the triage of *why* a pin failed is
            // `relookup_after_pin_failure`'s job — off the hot path, behind a
            // cold edge, so the fast path pays for exactly one generation load
            // (the one inside the pin) and no `resolve` of its own.
            let Some((raw, guard)) = self.segments.acquire_item_at(location) else {
                location = self.relookup_after_pin_failure(
                    key,
                    &verifier,
                    update_freq,
                    &backoff,
                    location,
                    &mut attempts,
                )?;
                continue;
            };
            #[cfg(feature = "fault-injection")]
            revalidation_fault::before_revalidate();
            // Re-validate AFTER pinning (concurrent-write reader safety, item
            // 7f). Between the lookup and the pin, `location`'s segment can be
            // drained, recycled, and REUSED (a different item written at this
            // offset), so `raw` may be an aliased/torn read — the CI-caught bug
            // this protocol exists to prevent was a reader handing back ANOTHER
            // KEY'S VALUE, not a miss.
            //
            // A fresh hashtable lookup is the soundness argument. It only ever
            // reads currently-published items — stale entries are removed from
            // the hashtable BEFORE a segment is recycled — so it is
            // authoritative in both directions: resolving to this exact
            // `location` means the (now pinned, hence un-recyclable) segment
            // genuinely holds the item we want, and resolving ELSEWHERE hands
            // us a location that is itself currently published. See
            // `follow_republished` for why following the second is the same
            // argument rather than a weakening of it.
            let current = self
                .hashtable
                .lookup_no_freq_update(key, &verifier)
                .map(|(l, _)| l);
            if current == Some(location) {
                // Lazy expiry: an item past its segment deadline is treated
                // as missing, matching memcached, even before the segment is
                // reclaimed. The segment is pinned here, so its header's
                // create_at/ttl are authoritative and cannot be recycled
                // under us.
                if self.remaining_ttl(seg_id).is_err() {
                    drop(guard);
                    return None;
                }
                raw.check_magic();
                let cas = Self::token_for(&raw, location, self.segments.generation(seg_id));
                return Some(Item::new(raw, cas, guard));
            }
            drop(guard);
            location = Self::follow_republished(current, &mut attempts)?;
        }
    }

    /// Resolve `key` to a location from scratch, honouring `update_freq`.
    #[inline(always)]
    fn lookup_location(
        &self,
        key: &[u8],
        verifier: &SegmentsVerifier<'_>,
        update_freq: bool,
    ) -> Option<Location> {
        let (location, _freq) = if update_freq {
            self.hashtable.lookup(key, verifier)?
        } else {
            self.hashtable.lookup_no_freq_update(key, verifier)?
        };
        #[cfg(feature = "fault-injection")]
        revalidation_fault::after_lookup();
        Some(location)
    }

    /// The revalidation lookup disagreed with the pinned location: `current` is
    /// where `key` is published NOW (or `None` if it is published nowhere).
    ///
    /// Pinning THAT rather than looking the key up again is what makes these
    /// retries converge (#65). It is sound by the revalidation's own argument —
    /// `current` came out of a fresh lookup, so it is currently published — and
    /// it is not the rejected "trust the pinned location" shape: the next thing
    /// that happens to it is another pin AND another full revalidation lookup.
    ///
    /// Before this, every attempt re-raced from scratch, and each attempt's
    /// vulnerable window spanned lookup + pin + lookup; now it spans only
    /// pin + lookup, and the second lookup per attempt is gone from the `get`
    /// path entirely (a cost flagged as "optimize later" when 7f landed).
    ///
    /// `None` out means the `get` is over: either the key is genuinely
    /// unpublished, or the budget is spent. Spending it is a false absent —
    /// the key is live and we know where — but the alternative, spinning until
    /// the writers stop, is a starvation hazard on the hottest path. The budget
    /// picks the first, sized so that reaching it is not a rate anyone will
    /// observe (see `REVALIDATE_RETRIES`).
    #[cold]
    #[inline(never)]
    fn follow_republished(current: Option<Location>, attempts: &mut usize) -> Option<Location> {
        *attempts += 1;
        if *attempts >= REVALIDATE_RETRIES {
            return None;
        }
        current
    }

    /// The reader pin failed. Both answers are a fresh lookup; they differ only
    /// in whether the retry is bounded, and the incarnation tag is what tells
    /// them apart. `acquire_item_at` refuses a pin for exactly two reasons, and
    /// this is the one place that has to distinguish them:
    ///
    /// **Transient (`resolve` still says `Some`)** — the segment is in a
    /// non-readable state: a drain owns it (Draining) or it is mid linking.
    /// Under merge eviction a drain RETAINS live items (they are relocated into
    /// the copy destination and republished), so an unreadable segment does NOT
    /// mean the key is gone: returning `None` here is a false miss — the key
    /// "reappears" once the merge publishes the relocation, breaking
    /// read-your-writes and the add/replace semantics built on a get. Look the
    /// key up again instead (the same protocol as `numeric_update`): the owning
    /// drain is bounded, straight-line work that either republishes the item at
    /// a new location (the fresh lookup resolves there, in a readable segment)
    /// or removes the entry (the lookup returns `None` and we exit). This retry
    /// is deliberately NOT counted against the revalidation budget: a drain
    /// window is far longer than a few spins, so a bounded retry would still
    /// report false misses. Termination relies on writers/drains never wedging
    /// — see the replace-vs-drain rollback in `insert`/`replace_at`, which
    /// guarantees drains cannot block forever on a writer pin.
    ///
    /// **Stale incarnation (`resolve` says `None`)** — the segment is perfectly
    /// readable; it is `location` that is dead. Its tag no longer matches the
    /// segment's generation, so it names an item that was reclaimed, and a hit
    /// on whatever occupies those bytes now would be another key's value. That
    /// is a MISS, and nothing about it is transient: no drain is going to finish
    /// and fix it. Retrying is still right — the entry is stale by definition,
    /// so either a writer has already published a fresh location or the key is
    /// gone — but it MUST be bounded, or a hashtable entry that somehow stays
    /// stale (nothing left to republish it, nothing left to unlink it) spins the
    /// unbounded arm above forever. Bounding it is the whole reason the tag is
    /// consulted here rather than left to the post-pin revalidation, which would
    /// also reject the location but only after routing it through this arm.
    ///
    /// It shares `attempts` — and therefore `REVALIDATE_RETRIES` — with
    /// `follow_republished` rather than carrying `RESERVE_RETRIES`. The two
    /// bound the same thing (how many times ONE `get` re-attempts because the
    /// world moved under it, one pin apiece), so a single counter is what caps
    /// a `get` at `REVALIDATE_RETRIES` pins no matter how an adversary mixes the
    /// arms. `RESERVE_RETRIES` bounds reservation attempts on the write path;
    /// #68 split the read path's budget out of it precisely because 3 is far too
    /// tight for a live key, and re-coupling this arm to it would re-introduce
    /// the false miss on the other face of the same window.
    #[cold]
    #[inline(never)]
    fn relookup_after_pin_failure(
        &self,
        key: &[u8],
        verifier: &SegmentsVerifier<'_>,
        update_freq: bool,
        backoff: &Backoff,
        location: Location,
        attempts: &mut usize,
    ) -> Option<Location> {
        if self.segments.resolve(location).is_none() {
            *attempts += 1;
            if *attempts >= REVALIDATE_RETRIES {
                return None;
            }
        }
        backoff.snooze();
        self.lookup_location(key, verifier, update_freq)
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

        // The whole reserve→publish operation restarts (fresh reservation)
        // when publishing would deadlock against a drain of the reservation's
        // own segment — see the replace arm's pin-failure handler below.
        'operation: loop {
            // `Value` is a borrowed enum without `Copy`; re-borrow it for this
            // attempt so a restart can consume it again.
            let attempt_value = match &value {
                Value::Bytes(b) => Value::Bytes(b),
                Value::U64(v) => Value::U64(*v),
            };
            let reserved = self.reserve_and_define(key, attempt_value, optional, ttl)?;

            // Fresh publish: the generation of the incarnation we reserved in,
            // captured under the reservation's WriterPin.
            let new_location = pack_location(
                reserved.seg(),
                reserved.generation(),
                reserved.offset() as u64,
            );
            let new_seg = reserved.seg();
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

                        // Address only — the incarnation check happens inside
                        // `remove_at`, under the remover pin taken below (an
                        // unpinned check here could go stale before the pin).
                        let (old_seg_raw, _old_offset) = unpack_location(old_location);
                        let Some(old_seg_id) = NonZeroU32::new(old_seg_raw) else {
                            // Not expected — `lookup_slot` only returns real
                            // (non-ghost) entries — but stay defensive and fall
                            // through to the same rollback used below.
                            break;
                        };

                        // Pin the OLD item's segment BEFORE unlinking it (item
                        // 7f). If a drain has already claimed the segment, the
                        // drain owns the item's removal; how to wait for it
                        // depends on WHICH segment it is:
                        //
                        // - `old_seg_id == new_seg` (common: the old value and
                        //   our new reservation co-locate in the Live tail —
                        //   e.g. a re-set of a recently written key): the drain
                        //   that claimed the segment is now waiting for
                        //   `active_writers == 0`, i.e. for the WriterPin held
                        //   inside `reserved`. It can never sweep the old entry
                        //   while we hold that pin, so a spin-and-relookup here
                        //   NEVER resolves — both threads wedge at 100% CPU
                        //   (and the drainer holds the bucket `chain_lock`,
                        //   wedging every writer of the TTL bucket). Roll the
                        //   reservation back — dropping the WriterPin unblocks
                        //   the drain — and restart the whole operation; the
                        //   retry reserves in a fresh tail because this segment
                        //   is no longer writable.
                        //
                        // - `old_seg_id != new_seg`: that drain is not waiting
                        //   on OUR pin and normally finishes on its own, so a
                        //   brief spin-and-relookup is productive (the entry is
                        //   swept or republished elsewhere). But it can be
                        //   waiting on ANOTHER writer's pin whose owner is
                        //   symmetrically blocked on a drain of OUR segment (a
                        //   cross-thread cycle), so the spin is bounded: once
                        //   the backoff is exhausted, roll back and restart
                        //   here too — releasing our pin breaks any such cycle.
                        let Some(pin) = self.segments.try_pin_remover(old_seg_id) else {
                            if old_seg_id == new_seg || backoff.is_completed() {
                                self.rollback_reservation(reserved, new_location);
                                continue 'operation;
                            }
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
                            // `remove_at` re-validates `old_location`'s
                            // incarnation under `pin` and skips the decrement
                            // if the entry we just replaced was published by an
                            // incarnation that is already gone.
                            let _ = self.segments.remove_at(
                                old_location,
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
                        // Fresh key: `hashtable.insert()` is an atomic upsert
                        // whose entry CREATION is serialized per key-hash
                        // stripe (table.rs), so concurrent fresh inserts of
                        // one key can never publish duplicate entries. If a
                        // racing writer published this key between our
                        // `lookup_slot` miss and here, our call resolves to a
                        // replace under the stripe's re-check and returns the
                        // racer's location as `Ok(Some(raced_old))` — that
                        // racer's segment accounting is then ours to
                        // decrement, with the unlink already done by the call
                        // above rather than by a pin-first `cas_location` (a
                        // narrow, accepted gap: if a drain claims that
                        // segment between the unlink and the pin attempt
                        // below, the pin fails and the drain owns the
                        // segment's accounting wholesale). The gap's second
                        // face — the pin SUCCEEDING on a recycled-and-reused
                        // incarnation of that segment id, landing the
                        // decrement on the wrong incarnation — is now closed:
                        // `raced_old` carries its incarnation tag, and
                        // `remove_at` re-checks it under the pin and skips the
                        // decrement on a mismatch.
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
                                let (raced_seg, _raced_offset) = unpack_location(raced_old);
                                if let Some(raced_seg) = NonZeroU32::new(raced_seg) {
                                    if let Some(pin) = self.segments.try_pin_remover(raced_seg) {
                                        let _ = self.segments.remove_at(
                                            raced_old,
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
                                self.rollback_reservation(reserved, new_location);
                                return Err(SegcacheError::HashTableInsertEx);
                            }
                        }
                    }
                }
            }

            // Defensive fallback for the "invalid old location" break above.
            self.rollback_reservation(reserved, new_location);
            return Err(SegcacheError::HashTableInsertEx);
        }
    }

    /// Roll back an unpublished reservation: release its `WriterPin` (item
    /// 7d — always BEFORE `remove_at`, never across a `chain_lock`
    /// acquisition), then best-effort pin (item 7f) and decrement its
    /// segment. Used by `insert`/`replace_at` error paths that must discard
    /// a reserved-but-never-published item. If the pin fails (the segment is
    /// concurrently being drained), the drain owns the item's accounting —
    /// nothing further to do.
    ///
    /// `location` is the reservation's own location, built under the
    /// `WriterPin` that is dropped on the first line here. Dropping it opens a
    /// window in which the segment can be drained and recycled before the
    /// remover pin below succeeds — on a DIFFERENT incarnation. `remove_at`
    /// re-checks the tag under that pin and skips the decrement if so.
    fn rollback_reservation(&self, reserved: ReservedItem, location: Location) {
        drop(reserved);
        let (seg, _offset) = unpack_location(location);
        let Some(seg) = NonZeroU32::new(seg) else {
            return;
        };
        if let Some(pin) = self.segments.try_pin_remover(seg) {
            let _ = self
                .segments
                .remove_at(location, &self.ttl_buckets, &self.hashtable, pin);
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
    ///
    /// `expected_token`, when given (the `cas` path), is the caller's
    /// full CAS token: it is RE-VERIFIED under the old segment's remover
    /// pin immediately before the publish, with a numeric item's seqlock
    /// writer lock held across both the re-verify and the slot CAS. The
    /// slot CAS alone only observes the LOCATION — an in-place
    /// `wrapping_add`/`saturating_sub` changes the item's version (which
    /// the token folds in) without moving it, so without this gate an
    /// increment landing in the token-check -> publish window (which
    /// spans `reserve_and_define`, possibly a whole eviction pass) would
    /// be silently overwritten by a cas that still reports success. On a
    /// token mismatch the reservation is rolled back and `Exists` is
    /// returned, exactly as a token-check failure would have.
    fn replace_at(
        &self,
        key: &[u8],
        old_location: Location,
        old_slot: SlotRef,
        reserved: ReservedItem,
        expected_token: Option<u64>,
    ) -> Result<(), SegcacheError> {
        // Fresh publish: the generation of the incarnation we reserved in,
        // captured under the reservation's WriterPin.
        let new_location = pack_location(
            reserved.seg(),
            reserved.generation(),
            reserved.offset() as u64,
        );
        // Capture the reservation's own segment up front so the rollback paths
        // can reclaim it AFTER the pin is released (see the drop-before-remove_at
        // invariant below), without borrowing `reserved`.
        let new_seg = reserved.seg();
        // Only the segment id is taken from the raw unpack — it is all the pin
        // below needs. The offset is deliberately NOT taken here: it is only
        // addressable once the location's incarnation has been validated under
        // the pin (see the `resolve` gate inside the loop).
        let (old_seg_id, _) = unpack_location(old_location);
        let Some(old_seg_id) = NonZeroU32::new(old_seg_id) else {
            // invalid old location: roll back the (unpublished) reservation.
            self.rollback_reservation(reserved, new_location);
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
            // post-CAS-failure check below). If it does, the drain claimed
            // the segment but has not yet drained this hashtable entry;
            // whether waiting can ever succeed depends on WHICH segment it
            // is (the same deadlock analysis as `insert`'s replace arm):
            //
            // - `old_seg_id == new_seg` (the checked item and our new
            //   reservation co-locate in the Live tail): the drain is
            //   waiting for the WriterPin held inside `reserved`, so it can
            //   never sweep the entry while we spin — a guaranteed
            //   two-thread wedge. Roll back (releasing the pin unblocks the
            //   drain) and fail safe with `Exists`: the caller's token is
            //   about to be invalidated anyway (tokens encode location +
            //   generation, and the drain relocates or removes the item),
            //   so a retry through get-then-cas observes the settled state.
            //   This mirrors delete's drain-owns-the-segment reasoning.
            //
            // - `old_seg_id != new_seg`: the drain is not waiting on OUR
            //   pin and normally finishes on its own — spin briefly. The
            //   spin is still bounded (cross-thread pin cycles, see
            //   `insert`): once the backoff is exhausted, roll back and
            //   fail safe with `Exists` as well. The bound also covers the
            //   `Relinking` case (the checked item lives in a mid-fill
            //   merge/promotion DESTINATION): pre-fix the spin waited for
            //   `publish_dest_sealed` and then succeeded, but that wait is
            //   not deadlock-free — the fill's owner can simultaneously be
            //   claiming OUR (concurrently sealed) reservation segment and
            //   waiting on our WriterPin — so a fill longer than the
            //   backoff now surfaces as a spurious-but-safe `Exists` on an
            //   unmodified token; a get-then-cas retry succeeds once the
            //   fill seals.
            let pin = match self.segments.try_pin_remover(old_seg_id) {
                Some(pin) => pin,
                None => {
                    if self
                        .hashtable
                        .get_item_frequency(key, old_location)
                        .is_none()
                        || old_seg_id == new_seg
                        || backoff.is_completed()
                    {
                        self.rollback_reservation(reserved, new_location);
                        return Err(SegcacheError::Exists);
                    }
                    backoff.snooze();
                    continue;
                }
            };

            // Incarnation gate, under the pin. A remover pin freezes the
            // generation FROM THE MOMENT IT IS TAKEN; it does NOT prove the
            // segment it froze is the incarnation `old_location` names. The
            // segment could have been drained, recycled (generation bumped)
            // and refilled between the caller's lookup and this pin, with the
            // hashtable slot still carrying the stale-tagged `old_location` —
            // `get_item_frequency` matches on (tag, location) alone and would
            // happily report it present. `old_offset` would then be an offset
            // into a DIFFERENT incarnation, not necessarily an item boundary
            // at all, so the token re-verify below would build a `RawItem`
            // over foreign bytes and read a garbage `is_numeric` bit — and if
            // that bit read true, CAS a version word into another
            // incarnation's live payload.
            //
            // `resolve` compares the location's tag against the (now frozen)
            // generation, so a `Some` here holds for the rest of this
            // iteration. `delete` gates its own `get_item_at`/`set_deleted`
            // under its remover pin exactly the same way, and `remove_at`
            // re-checks once more before decrementing. `None` is not an error:
            // the entry no longer names anything we may touch, which is the
            // same situation as losing the publish race — roll the
            // (unpublished) reservation back and report `Exists`.
            let Some((old_seg_id, old_offset)) = self.segments.resolve(old_location) else {
                drop(pin);
                self.rollback_reservation(reserved, new_location);
                return Err(SegcacheError::Exists);
            };

            // Token re-verify under the remover pin (cas path only; see the
            // doc comment). Ordering of the safety argument:
            //
            // 1. Location-uniqueness before touching item bytes: the entry
            //    must still map key -> old_location, AND `old_location` must
            //    still name the pinned segment's current incarnation (the
            //    `resolve` gate above — `get_item_frequency` matches on
            //    (tag, location) alone, so it alone does not establish
            //    this). Together with the frozen generation those give a real
            //    item starting at `old_offset`: the segment cannot drain or
            //    recycle under the pin, a drain must unlink entries BEFORE
            //    its segment is recycled, and a location surviving from an
            //    earlier incarnation was rejected above. (The ABA where the
            //    same key was re-inserted at this exact location after a full
            //    drain+recycle is a real item too — and the recycle bumped
            //    the generation, which the token compare below catches.)
            // 2. For a numeric item, take its seqlock WRITER lock
            //    (`lock_numeric_version`) and hold it across the re-verify
            //    AND the slot CAS. In-place numeric writers serialize
            //    their own check-linkage-then-write step on that same lock
            //    (`numeric_update`), and merge/s3fifo relocation holds it
            //    across its byte copy + relink (`copy_into`,
            //    `s3fifo_promote_from`), so no two of these critical
            //    sections can interleave; whichever completes first
            //    decides:
            //      - increment first: its bumped version fails the compare
            //        below — `Exists`, the increment's ack survives (and a
            //        cas whose token was read after the increment
            //        legitimately carries it forward);
            //      - this publish first: the increment's in-lock linkage
            //        re-check (its lock acquire synchronizes-with our
            //        unlock) observes the published NEW location and
            //        retries against the new item before acking.
            //    Residual window: none — every lost-acked-write
            //    interleaving requires an increment and a token-gated
            //    publish inside one another's critical sections, which the
            //    shared lock forbids.
            // 3. The generation is re-read under the pin (frozen), so the
            //    recomputed token is exact, not racy.
            let old_raw;
            let version_guard = if let Some(expected) = expected_token {
                if self
                    .hashtable
                    .get_item_frequency(key, old_location)
                    .is_none()
                {
                    drop(pin);
                    self.rollback_reservation(reserved, new_location);
                    return Err(SegcacheError::Exists);
                }
                old_raw = self.segments.get_item_at(Some(old_seg_id), old_offset);
                let raw = old_raw.as_ref().expect("pinned segment id is valid");
                let base =
                    CasToken::new(old_location, self.segments.generation(old_seg_id)).as_raw();
                match raw.lock_numeric_version() {
                    Ok(guard) => {
                        if crate::cas::mix_version(base, guard.version()) != expected {
                            drop(guard);
                            drop(pin);
                            self.rollback_reservation(reserved, new_location);
                            return Err(SegcacheError::Exists);
                        }
                        Some(guard)
                    }
                    Err(_) => {
                        // Non-numeric item: the token is bare
                        // location + generation.
                        if base != expected {
                            drop(pin);
                            self.rollback_reservation(reserved, new_location);
                            return Err(SegcacheError::Exists);
                        }
                        None
                    }
                }
            } else {
                None
            };

            // Publish under the pin: `reserved` (and its WriterPin) is held
            // across the exchange so a concurrent drain cannot recycle the
            // segment between define and publish (item 7d, H2).
            if self
                .hashtable
                .cas_location_at(old_slot, old_location, new_location, true)
            {
                // Unlock the old item's seqlock the instant the publish
                // resolves — numeric writers spinning on it re-validate
                // and follow the new location.
                drop(version_guard);

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
                // `remove_at` re-validates `old_location`'s incarnation under
                // `pin` before decrementing (see `Segments::resolve`).
                let _ =
                    self.segments
                        .remove_at(old_location, &self.ttl_buckets, &self.hashtable, pin);
                return Ok(());
            }

            // The exchange failed while pinned — release the seqlock (if
            // held) and the remover pin.
            drop(version_guard);
            drop(pin);

            if self
                .hashtable
                .get_item_frequency(key, old_location)
                .is_none()
            {
                // The entry genuinely no longer maps to old_location
                // (replaced, relocated, or removed) — roll back the
                // (unpublished) reservation.
                self.rollback_reservation(reserved, new_location);
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
    /// Expiry is lazy on access: a cas against an item past its TTL
    /// deadline fails with `NotFound`, matching memcached, even before
    /// its segment is reclaimed.
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
        // Look up the current item to check its CAS token. The lookup+pin
        // retries through transient drain windows, exactly like
        // `get_pinned`: a reader-pin failure means a drain owns the
        // segment, and under merge eviction a drain RETAINS live items
        // (they are relocated and republished) — so failing here with
        // `NotFound` would report a LIVE key missing (memcached: a live
        // key can only fail a cas with EXISTS). Termination mirrors
        // `get_pinned`'s argument: the owning drain either republishes the
        // entry (the fresh lookup resolves it in a readable segment) or
        // removes it (the lookup returns `None` and we exit `NotFound`,
        // now truthfully).
        let verifier = self.verifier();
        let backoff = Backoff::new();
        let mut attempts = 0;
        let (location, slot, current_cas) = loop {
            let (location, slot) = self
                .hashtable
                .lookup_slot(key, &verifier)
                .ok_or(SegcacheError::NotFound)?;

            let (seg_id, _offset) = unpack_location(location);
            let seg_id = NonZeroU32::new(seg_id).ok_or(SegcacheError::NotFound)?;

            // Lazy expiry: memcached returns NOT_FOUND for a cas on an expired
            // key, even before the segment is reclaimed. The header is read
            // unpinned here, so this is a semantic filter, not a safety
            // mechanism — if the segment races a recycle, the token/generation
            // check below still protects correctness.
            self.remaining_ttl(seg_id)?;

            // Pin briefly to read the item's seqlock version (numeric
            // items fold it into the token); the pin drops before the
            // reservation below — pinned segments are unevictable.
            let Some((raw, guard)) = self.segments.acquire_item_at(location) else {
                // Transient drain window, or a location whose incarnation is
                // gone (`acquire_item_at` refuses the pin — see
                // `Segments::resolve`); either way the fresh lookup on retry
                // resolves the live location or reports the key gone. Not
                // counted against `attempts`, same as `get_pinned`.
                backoff.snooze();
                continue;
            };
            // Re-validate after pinning (see `get_pinned`): if the key no
            // longer resolves to this exact location, the entry moved (or
            // the segment recycled) between lookup and pin — the token we
            // would mint from `raw` could be an aliased read. A bounded
            // number of mismatches means the key is churning under us, and
            // any relocation/replacement has already staled the caller's
            // location-bearing token: fail `Exists` (never a false
            // `NotFound` for a live key).
            if self
                .hashtable
                .lookup_no_freq_update(key, &verifier)
                .map(|(l, _)| l)
                != Some(location)
            {
                drop(guard);
                attempts += 1;
                if attempts >= RESERVE_RETRIES {
                    return Err(SegcacheError::Exists);
                }
                continue;
            }
            let token = Self::token_for(&raw, location, self.segments.generation(seg_id));
            break (location, slot, token);
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
        //
        // The token is passed down for a second, pinned verification
        // right before the publish: the slot CAS inside `replace_at`
        // only observes the LOCATION, so an in-place numeric increment
        // landing after the check above (the window spans
        // `reserve_and_define`, possibly a whole eviction pass) would
        // otherwise be invisible to it — a false STORED that destroys an
        // acked increment.
        self.replace_at(key, location, slot, reserved, Some(cas))
    }

    /// Remove the item with the given key, returns a bool indicating if it was
    /// removed.
    ///
    /// Expiry is lazy on access: deleting an item past its TTL deadline
    /// returns `false`, matching memcached's NOT_FOUND, even before its
    /// segment is reclaimed.
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
        let verifier = self.verifier();
        let backoff = Backoff::new();
        loop {
            // Look up the item to get its location
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

            // Capture the segment generation before anything else: the
            // unpinned-unlink path below uses it to detect a recycle of
            // `seg_id` between this lookup and its remove (see the ABA note
            // there).
            let observed_gen = self.segments.generation(seg_id);

            // Lazy expiry: DELETE on an expired key reports NOT_FOUND (false),
            // matching memcached, even before the segment is reclaimed. The
            // stale hashtable entry is left for expire()/eviction pressure to
            // sweep.
            if self.remaining_ttl(seg_id).is_err() {
                return false;
            }

            // Pin the item's segment BEFORE unlinking it (item 7f): the pin
            // brackets both the hashtable unlink below and the `remove_at`
            // decrement, so a concurrent drain of this segment cannot
            // interleave with the decrement.
            //
            // If the pin FAILS, the segment is Draining (a drain owns it) or
            // Relinking (a merge/promotion copy destination mid-fill). The
            // delete must still unlink the hashtable entry itself: a merge
            // drain RETAINS live items — `copy_into` relocates every item
            // still present in the hashtable — and a Relinking destination is
            // never swept at all, so "the drain will remove it" does NOT
            // hold; an acked delete that leaves the entry behind resurrects.
            //
            // Doing the unlink WITHOUT the pin is safe: `hashtable.remove`
            // only CASes the hashtable slot — it touches neither segment
            // bytes nor the live-item/live-byte counters, so it cannot race
            // the drain's exclusive access to the segment. What is skipped is
            // only `remove_at`'s accounting decrement: the segment's owner
            // covers it — every parse path (`clear`, `prune`, `copy_into`,
            // `s3fifo_promote_from`) consults `get_item_frequency` and treats
            // the unlinked item as dead (no relocation, no double remove),
            // and the counters are reset wholesale when the segment is
            // recycled/re-reserved (`reset_write_stats`), the same accepted
            // transient over-count documented in `Segment::clear` (item 7f). If the unlink races `copy_into`
            // after its liveness check, the relink CAS simply fails and the
            // copy aborts — an eviction-legal drop, not corruption.
            //
            // ABA guard: `location` is (segment, offset) with NO generation,
            // and `hashtable.remove` matches (tag, location) without
            // re-verifying key bytes — the pinned path below is exempt only
            // because its pin freezes the segment against recycling (the
            // location-uniqueness precondition documented on
            // `cas_location_at`). Unpinned, the segment could have been
            // drained, recycled, and refilled since the lookup, with a
            // colliding-tag key freshly written at this exact offset. Two
            // defenses: (1) refuse the unpinned unlink when the generation
            // moved since the lookup — the entry is stale either way; (2)
            // after a successful unlink, re-verify the key stopped
            // resolving, retrying if it did not — so an acked delete NEVER
            // leaves the key reachable (a retry against a concurrent
            // re-insert deletes the newer value: a legal linearization of
            // concurrent set+delete). The residual window (generation load
            // to remove-CAS) requires a full drain+recycle+refill+publish
            // plus a 12-bit tag collision in an overlapping bucket to land
            // within a few instructions; its worst case is a spurious
            // unlink of ONE colliding key — observably an eviction, which a
            // cache may always perform — never corruption (the unlink
            // touches no segment state).
            let Some(pin) = self.segments.try_pin_remover(seg_id) else {
                if self.segments.generation(seg_id) == observed_gen
                    && self.hashtable.remove(key, location)
                    && self
                        .hashtable
                        .lookup_no_freq_update(key, &verifier)
                        .is_none()
                {
                    #[cfg(feature = "metrics")]
                    {
                        HASH_REMOVE.increment();
                        ITEM_DELETE.increment();
                    }
                    return true;
                }
                // The entry moved (a merge republished it elsewhere), was
                // removed concurrently, or the key still resolves after the
                // unlink — retry from the lookup, which resolves the fresh
                // location or reports the key gone.
                backoff.snooze();
                continue;
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

            // Remove from segment. Both steps are incarnation-gated: under the
            // remover pin the generation is frozen, so a tag mismatch here
            // means the entry we just unlinked was published by an incarnation
            // that is already gone — its bytes belong to a different
            // incarnation now, and marking THEM deleted would destroy a live
            // item. `remove_at` re-checks the same way before decrementing.
            if self.segments.resolve(location).is_some() {
                if let Some(mut item) = self.segments.get_item_at(Some(seg_id), offset) {
                    item.set_deleted(true);
                }
            }
            let _ = self
                .segments
                .remove_at(location, &self.ttl_buckets, &self.hashtable, pin);

            return true;
        }
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
        self.numeric_update(key, |v| v.wrapping_add(rhs))
    }

    /// Perform a saturating subtraction on the value stored at the supplied
    /// key. Returns an error if the key is invalid, the item is not found, or
    /// the stored value is not a numeric type.
    ///
    /// See [`Self::wrapping_add`] for the update and CAS-token semantics.
    /// Returns the new value.
    pub fn saturating_sub(&self, key: &[u8], rhs: u64) -> Result<u64, SegcacheError> {
        self.numeric_update(key, |v| v.saturating_sub(rhs))
    }

    /// Shared in-place update for the numeric operations.
    ///
    /// Looks up the key, checks its segment deadline (memcached lazily
    /// treats expired keys as missing — increments must not resurrect
    /// them), pins the segment, and performs the seqlocked in-place
    /// update through the pinned item. Mutating in place under only a
    /// reader pin is safe by two mechanisms working together:
    ///
    /// - the reader pin keeps the segment's MEMORY alive: a drained
    ///   segment is recycled or condemned-and-released only once its
    ///   reader count is observed zero, so the item bytes cannot be
    ///   reused out from under the write. The pin does NOT stop a drain
    ///   from claiming the segment or relocating the item — drains wait
    ///   on writers/removers, not readers;
    /// - the item's seqlock version lock serializes the write against
    ///   every party that can supersede or MOVE the item: cas publishes
    ///   re-verify their token under it (`replace_at`), and merge/s3fifo
    ///   relocation holds it across its byte copy and relink CAS
    ///   (`copy_into`, `s3fifo_promote_from`). Linkage is re-validated
    ///   INSIDE the lock, so a write can never land on an item that was
    ///   superseded or relocated first — the re-check observes the new
    ///   location and retries against the live item before acking.
    ///
    /// Returns the value this call published.
    fn numeric_update(&self, key: &[u8], op: impl Fn(u64) -> u64) -> Result<u64, SegcacheError> {
        let backoff = Backoff::new();
        loop {
            let verifier = self.verifier();
            let (location, _freq) = self
                .hashtable
                .lookup(key, &verifier)
                .ok_or(SegcacheError::NotFound)?;
            let (seg_id, _offset) = unpack_location(location);
            let seg_id = NonZeroU32::new(seg_id).ok_or(SegcacheError::NotFound)?;

            // Lazy expiry: a counter past its segment deadline is
            // treated as missing, matching memcached, even before
            // expire() reclaims the segment.
            self.remaining_ttl(seg_id)?;

            match self.segments.acquire_item_at(location) {
                // Segment not readable (draining; a relocation is in
                // flight), or the location's incarnation is gone (the pin
                // is refused — see `Segments::resolve`) — back off and retry
                // from the lookup, giving the drain a chance to finish
                // instead of busy-waiting through its whole window.
                None => {
                    backoff.snooze();
                    continue;
                }
                Some((raw, _guard)) => {
                    // Re-validate after pinning (see `get`): if the key no longer
                    // resolves to this exact location, the segment was
                    // recycled+reused between lookup and pin and `raw` is a
                    // stale/aliased item — retry rather than update the WRONG
                    // item in place (item 7f). The fresh lookup only reads
                    // currently-published items, so it is safe and authoritative.
                    // It also establishes `raw` as a REAL item, making the
                    // version-word access below sound.
                    if self.hashtable.lookup(key, &verifier).map(|(l, _)| l) != Some(location) {
                        continue;
                    }

                    // Take the item's seqlock writer lock, then re-validate
                    // linkage INSIDE it, so the "still the published item"
                    // check and the value write are one atomic step with
                    // respect to every party that serializes on this lock —
                    // a cas publish, which re-verifies its token and swaps
                    // the hashtable slot while holding it (`replace_at`),
                    // and a merge/s3fifo relocation, which byte-copies the
                    // item and relinks its location while holding it
                    // (`copy_into`, `s3fifo_promote_from`; a relocation
                    // that completed first is seen by the re-check below as
                    // a new location, and we retry against the
                    // destination). Interleavings:
                    //
                    //   - cas critical section completed first and
                    //     PUBLISHED: the re-check below sees the new
                    //     location, we drop the lock unchanged and retry —
                    //     the increment applies (once) to the NEW item.
                    //     Acked only after it is visible.
                    //   - cas critical section completed first but FAILED
                    //     (token stale): slot unchanged, we update in
                    //     place. Correct.
                    //   - our update completes first: the cas's in-lock
                    //     token re-verify sees our bumped version and
                    //     fails `Exists` — our acked increment survives on
                    //     the still-linked item. A cas whose token was
                    //     read AFTER our update legitimately carries our
                    //     increment forward in the value it publishes.
                    //
                    // A checked-then-written window simply cannot contain
                    // a token-gated publish, and non-token-gated writes
                    // (set/delete/convert) owe no preservation to a
                    // concurrent increment — losing to them is a legal
                    // linearization. This is why the validation must sit
                    // inside the lock: a post-write re-check variant
                    // double-applies when a fresh-token cas lands between
                    // the write and the re-check.
                    let vguard = raw
                        .lock_numeric_version()
                        .map_err(|_| SegcacheError::NotNumeric)?;
                    if self.hashtable.lookup(key, &verifier).map(|(l, _)| l) != Some(location) {
                        drop(vguard);
                        continue;
                    }
                    return Ok(vguard.update(&op));
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
    /// - key churning under concurrent relocation/replacement: may fail
    ///   safe with `Err(Exists)` after bounded retries — retryable, never
    ///   a false `NotFound`
    ///
    /// Composes with [`Self::wrapping_add`]/[`Self::saturating_sub`] to
    /// implement memcached-style incr-with-initial at a protocol layer.
    pub fn try_into_numeric(
        &self,
        key: &[u8],
        initial: u64,
        ttl: std::time::Duration,
    ) -> Result<(), SegcacheError> {
        // Lookup+pin retries through transient drain windows, exactly like
        // `get_pinned`/`cas`: a reader-pin failure means a drain owns the
        // segment, and a merge drain RETAINS live items — reporting
        // `NotFound` here was a false miss on a live key (a
        // #51-acknowledged follow-up, fixed alongside `cas`).
        let verifier = self.verifier();
        let backoff = Backoff::new();
        let mut attempts = 0;
        let (location, slot, parsed, opt_buf, olen, seg_ttl) = loop {
            let Some((location, slot)) = self.hashtable.lookup_slot(key, &verifier) else {
                // Missing: create with the caller's ttl. NOTE for the
                // concurrent future: this publishes via plain insert, which
                // would overwrite a concurrently created value; revisit with
                // insert-if-absent when the API goes concurrent.
                return self.insert(key, initial, None, ttl);
            };

            let (seg_id, _offset) = unpack_location(location);
            let seg_id = NonZeroU32::new(seg_id).ok_or(SegcacheError::NotFound)?;

            let Some((raw, guard)) = self.segments.acquire_item_at(location) else {
                // Transient drain window, or a stale incarnation whose pin is
                // refused (`Segments::resolve`) — retry from the lookup.
                backoff.snooze();
                continue;
            };
            // Re-validate after pinning (see `get_pinned`): a moved entry
            // means `raw` may be an aliased read; retry, and after a
            // bounded number of mismatches report the churn as `Exists`
            // (the same outcome `replace_at` gives a concurrent
            // replacement), never a false `NotFound`.
            if self
                .hashtable
                .lookup_no_freq_update(key, &verifier)
                .map(|(l, _)| l)
                != Some(location)
            {
                drop(guard);
                attempts += 1;
                if attempts >= RESERVE_RETRIES {
                    return Err(SegcacheError::Exists);
                }
                continue;
            }
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
            break (location, slot, parsed, opt_buf, olen, seg_ttl);
        };

        let reserved =
            self.reserve_and_define(key, Value::U64(parsed), &opt_buf[..olen], seg_ttl)?;
        // No caller token here (this is a convert-in-place, not a cas):
        // location-only publish semantics are the intent — any
        // concurrent replacement fails the slot CAS and surfaces as
        // `Exists`.
        self.replace_at(key, location, slot, reserved, None)
    }

    /// Test-only access to the segment collection, for asserting on segment
    /// headers (e.g. `active_writers()`) after write operations return.
    #[cfg(test)]
    pub(crate) fn segments_for_test(&self) -> &Segments {
        &self.segments
    }

    /// Test-only: reserve a fresh item and drive [`Self::replace_at`] against a
    /// caller-supplied `old_location`/`old_slot`.
    ///
    /// The public `cas`/`try_into_numeric` entry points derive those two from a
    /// lookup that pins the old item first, so they can only ever hand
    /// `replace_at` a location that was live a moment ago; the state a
    /// concurrent recycle produces — a slot still carrying a location whose
    /// incarnation is already gone — is not constructible through them without
    /// a real race. This hook plants that state directly so `replace_at`'s
    /// incarnation gate can be tested deterministically.
    #[cfg(all(test, not(feature = "loom")))]
    pub(crate) fn replace_at_for_test(
        &self,
        key: &[u8],
        old_location: Location,
        old_slot: SlotRef,
        value: &[u8],
        expected_token: Option<u64>,
    ) -> Result<(), SegcacheError> {
        let reserved = self.reserve_and_define(
            key,
            Value::Bytes(value),
            &[],
            Self::coarse_ttl(std::time::Duration::from_secs(3600)),
        )?;
        self.replace_at(key, old_location, old_slot, reserved, expected_token)
    }
}
