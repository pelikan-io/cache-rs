// Copyright 2021 Twitter, Inc.
// Copyright 2023 Pelikan Cache contributors
// Licensed under the MIT and Apache-2.0 licenses

//! Core datastructure.

use crate::Value;
use crate::*;
use core::num::NonZeroU32;
use crossbeam_utils::Backoff;
use std::cmp::min;

/// Number of eviction passes that free no segment after which
/// `reserve_and_define` returns `NoFreeSegments`; also the attempt cap for the
/// `cas` and `try_into_numeric` freshness retries.
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
/// a different thing (concurrent republications of one key, not eviction
/// passes), and because 3 is far too tight: with retries that CONVERGE on
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
    #[cfg(all(test, not(model_checking)))]
    pub(crate) struct HookGuard(());

    #[cfg(all(test, not(model_checking)))]
    impl Drop for HookGuard {
        fn drop(&mut self) {
            AFTER_LOOKUP.with(|h| *h.borrow_mut() = None);
            BEFORE_REVALIDATE.with(|h| *h.borrow_mut() = None);
        }
    }

    #[cfg(all(test, not(model_checking)))]
    pub(crate) fn on_after_lookup(f: impl Fn() + 'static) -> HookGuard {
        AFTER_LOOKUP.with(|h| *h.borrow_mut() = Some(Rc::new(f)));
        HookGuard(())
    }

    #[cfg(all(test, not(model_checking)))]
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

/// Test-only tally of how often [`Segcache::triage_unknown_location`] has
/// taken its **stale-incarnation** arm and charged the revalidation budget.
///
/// The arm's safety argument is that charging costs a real segment *recycle*,
/// so exhausting `REVALIDATE_RETRIES` inside one lookup -> pin window would
/// take ~16 full segment lifecycles. A test of that argument has to prove it
/// entered the arm at all: a churn test that only republishes the key never
/// invalidates an incarnation, so it never charges, and it would pass while
/// asserting nothing. Counting here (rather than inferring from the fault
/// hooks) is what makes that vacuity impossible to reach silently — if a
/// future change stops routing stale locations through this arm, the count
/// goes to zero and the test fails instead of quietly becoming a no-op.
///
/// Thread-local, like the fault hooks it is used with: the `get` under test
/// runs on the thread that installed them.
#[cfg(all(test, not(model_checking)))]
pub(crate) mod stale_incarnation_charges {
    use std::cell::Cell;

    thread_local! {
        static CHARGES: Cell<usize> = const { Cell::new(0) };
    }

    /// One budget attempt charged for a location whose incarnation is gone.
    #[inline]
    pub(crate) fn record() {
        CHARGES.with(|c| c.set(c.get() + 1));
    }

    /// Read the tally and reset it, so consecutive tests on one thread do not
    /// inherit each other's counts.
    // Used by `revalidation_tests`, which needs `fault-injection`; without
    // that feature the only caller is cfg'd out.
    #[allow(dead_code)]
    pub(crate) fn take() -> usize {
        CHARGES.with(|c| c.replace(0))
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
    /// Test-only count of calls to `wait_while_draining_or_filling`.
    ///
    /// Per-cache, unlike the thread-local `stale_incarnation_charges`, because
    /// the tests read it from a thread other than the waiting writer; a
    /// process-global counter would let two concurrently running tests
    /// satisfy each other's waits.
    #[cfg(all(test, not(model_checking)))]
    pub(crate) insert_waits: std::sync::atomic::AtomicUsize,
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

    /// The verifier `get` uses for its FROM-SCRATCH probe.
    ///
    /// Identical to [`Self::verifier`] except under `fault-injection`, where it
    /// fires the `after_lookup` hook just before each pin attempt — the window
    /// a concurrent writer/evictor gets to stale a slot's location. Keeping it
    /// a separate instance is what preserves the hook's "once per from-scratch
    /// lookup" meaning: the cold path's revalidation lookups use the plain
    /// verifier and do not fire it.
    #[inline]
    fn probe_verifier(&self) -> SegmentsVerifier<'_> {
        #[cfg(feature = "fault-injection")]
        {
            self.segments.verifier().probing()
        }
        #[cfg(not(feature = "fault-injection"))]
        {
            self.segments.verifier()
        }
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
    /// The returned `Item` borrows the cache and pins its segment until it
    /// is dropped; see [`Item`].
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
    pub fn get(&self, key: &[u8]) -> Option<Item<'_>> {
        self.get_pinned(key, true)
    }

    /// Shared lookup for [`Self::get`]/[`Self::get_no_freq_incr`]: resolve
    /// the key, hand out the pinned item. `update_freq` selects whether the
    /// lookup bumps the item's frequency counter.
    ///
    /// # The shape, and why it is two paths rather than one
    ///
    /// The verifier compares key bytes **under a reader pin whose incarnation
    /// tag it checked** (`SegmentsVerifier`), so a `Lookup::Found` already
    /// carries the pinned item: there is nothing left to establish about
    /// *which item these bytes are*. What a pin does NOT establish is whether
    /// the entry is still **published** — a reader that loaded the slot word,
    /// was descheduled, and resumed after a `delete` would pin a perfectly
    /// live segment, match the tag, compare the key equal (a delete flips one
    /// header bit), and hand back a deleted item. Linearizable, but with a
    /// staleness window bounded by thread scheduling rather than by protocol.
    ///
    /// So freshness is a second, separate question, and it is answered on the
    /// hot path by **re-reading the same slot word** and comparing the
    /// location field ([`MultiChoiceHashtable::slot_publishes`]) — one
    /// `Acquire` load of a line the scan just touched, exact by the
    /// CAS-in-place argument written out there.
    ///
    /// On a mismatch the read falls through to **exactly the pre-#91 code**: a
    /// full `lookup_no_freq_update` re-probe, `follow_republished`, and
    /// `REVALIDATE_RETRIES`. That is not conservatism. The full re-probe is
    /// what `follow_republished`, the `before_revalidate` fault hook,
    /// `revalidation_tests` and #68's loom lookup bound are about; a design
    /// that removes the fast path's *need* for them must not remove the path
    /// they test. What changes is that it stops being the default cost of a
    /// hit — the second full hashtable probe every hit used to pay — and
    /// becomes the cold fallback.
    // inline(always) is measured, not cargo-cult (same story as
    // reserve_and_define): the extraction from get() cost ~3ns on the 255b
    // get benchmark until the call boundary was forced away. It also lets
    // the constant `update_freq` fold at each call site.
    #[inline(always)]
    fn get_pinned(&self, key: &[u8], update_freq: bool) -> Option<Item<'_>> {
        let probe = self.probe_verifier();
        let verifier = self.verifier();
        let backoff = Backoff::new();
        let mut attempts = 0;

        'resolve: loop {
            let hit = match self.lookup_hit(key, &probe, update_freq) {
                Lookup::Found(hit) => hit,
                Lookup::Absent => return None,
                // A candidate slot could not be pinned. Reporting that as a
                // miss is the false absent this arm exists to prevent.
                Lookup::Unknown(location) => {
                    self.triage_unknown_location(location, &backoff, &mut attempts)?;
                    continue 'resolve;
                }
            };

            #[cfg(feature = "fault-injection")]
            revalidation_fault::before_revalidate();

            if self.hashtable.slot_publishes(hit.slot, hit.location) {
                return self.item_from_pin(hit.location, hit.pin);
            }

            // ── Cold fallback ────────────────────────────────────────────
            // Entered holding a pin on `hit.location`, which the same-slot
            // compare just said is no longer published there.
            let (mut location, mut pin) = (hit.location, hit.pin);
            loop {
                // A fresh hashtable lookup is the soundness argument. It only
                // ever reads currently-published items — stale entries are
                // removed from the hashtable BEFORE a segment is recycled — so
                // it is authoritative in both directions: resolving to this
                // exact `location` means the (pinned, hence un-recyclable)
                // segment genuinely holds the item we want, and resolving
                // ELSEWHERE hands us a location that is itself currently
                // published. See `follow_republished` for why following the
                // second is the same argument rather than a weakening of it.
                let current = match self.hashtable.lookup_no_freq_update(key, &verifier) {
                    // The revalidation's own pin is dropped with `other`: this
                    // path is asking WHERE the key is, not for its bytes.
                    Lookup::Found(other) => Some(other.location),
                    Lookup::Absent => None,
                    Lookup::Unknown(unknown) => {
                        drop(pin);
                        self.triage_unknown_location(unknown, &backoff, &mut attempts)?;
                        continue 'resolve;
                    }
                };

                if current == Some(location) {
                    return self.item_from_pin(location, pin);
                }

                drop(pin);
                location = Self::follow_republished(current, &mut attempts)?;

                // Pin the location the revalidation just handed back, rather
                // than looking the key up again — the #65 convergence.
                let Some(next) = self.segments.acquire_item_at(location) else {
                    self.triage_unknown_location(location, &backoff, &mut attempts)?;
                    continue 'resolve;
                };
                pin = next;

                #[cfg(feature = "fault-injection")]
                revalidation_fault::before_revalidate();
            }
        }
    }

    /// Resolve `key` from scratch, honouring `update_freq`.
    #[inline(always)]
    fn lookup_hit<'s>(
        &'s self,
        key: &[u8],
        verifier: &SegmentsVerifier<'s>,
        update_freq: bool,
    ) -> Lookup<Hit<(RawItem, SegmentGuard<'s>)>> {
        if update_freq {
            self.hashtable.lookup(key, verifier)
        } else {
            self.hashtable.lookup_no_freq_update(key, verifier)
        }
    }

    /// Turn a pinned, verified, still-published entry into an [`Item`].
    ///
    /// The segment is pinned here, so its header's `create_at`/`ttl` are
    /// authoritative and cannot be recycled under us. Lazy expiry: an item past
    /// its segment deadline is treated as missing, matching memcached, even
    /// before the segment is reclaimed.
    #[inline]
    fn item_from_pin<'s>(
        &'s self,
        location: Location,
        pin: (RawItem, SegmentGuard<'s>),
    ) -> Option<Item<'s>> {
        let (raw, guard) = pin;
        let (seg_id, _offset) = unpack_location(location);
        let seg_id = NonZeroU32::new(seg_id)?;
        if self.remaining_ttl(seg_id).is_err() {
            drop(guard);
            return None;
        }
        raw.check_magic();
        let cas = Self::token_for(&raw, location, self.segments.generation(seg_id));
        Some(Item::new(raw, cas, guard))
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

    /// A candidate location could not be pinned. Decide how long to wait for
    /// it, and whether the wait is charged against the read budget.
    ///
    /// `Some(())` means "wait done, retry the resolve"; `None` means the budget
    /// is spent and the `get` is over. Nothing is looked up here — the caller's
    /// loop re-resolves — but the triage is unchanged, because
    /// `acquire_item_at` still refuses a pin for exactly two reasons and they
    /// still want different answers:
    ///
    /// **Transient (`resolve` still says `Some`)** — the segment is in a
    /// non-readable state: a drain owns it (Draining) or it is mid linking.
    /// Under merge eviction a drain RETAINS live items (they are relocated into
    /// the copy destination and republished), so an unreadable segment does NOT
    /// mean the key is gone: returning `None` here is a false miss — the key
    /// "reappears" once the merge publishes the relocation, breaking
    /// read-your-writes and the add/replace semantics built on a get. Retry
    /// instead (the same protocol as `numeric_update`): the owning drain is
    /// bounded, straight-line work that either republishes the item at a new
    /// location (the fresh lookup resolves there, in a readable segment) or
    /// removes the entry (the lookup returns `Absent` and we exit). This retry
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
    /// gone — and it is retried under a BOUND, which is the whole reason the tag
    /// is consulted here rather than left to the revalidation (that would also
    /// reject the location, but only after routing it through the unbounded arm
    /// above).
    ///
    /// **What the bound is actually for.** The design justified it as "a stale
    /// tag must be retried under a bound or a permanently stale entry spins that
    /// arm forever". No such entry is reachable through the public API, by two
    /// invariants that meet:
    ///
    /// - *nothing publishes at a dead generation.* Every location the hashtable
    ///   ever holds is packed from a generation read while holding something
    ///   that blocks the two `-> Free` transitions: `try_alloc_item` reads it
    ///   after `try_pin_writer` and hands it out inside the `ReservedItem`,
    ///   whose `WriterPin` is held across the publish (`insert`, `replace_at`);
    ///   the two relink sites read it off a segment they have claimed
    ///   (`Draining` source, `Relinking` destination — `pack_location`'s stated
    ///   precondition).
    /// - *nothing survives the bump.* Both bumps are reached only through
    ///   `finalize_drained`, which runs `Segment::clear` first, and `clear`
    ///   sweeps every item boundary in `[0, write_offset)` — exactly the set of
    ///   offsets a published location can name — unlinking each entry still
    ///   there. (`try_unlink_in_bucket` retries its slot across a racing
    ///   frequency bump for precisely this reason: a spurious `false` would
    ///   "recycle the segment with the entry still published".) And
    ///   `claim_for_drain` waits out every writer and remover before the sweep,
    ///   so no in-flight publish can land behind it.
    ///
    /// So at the instant a generation advances, no entry names the outgoing
    /// incarnation, and a fresh lookup can only hand back a location that was
    /// live when it was read. Each firing of this arm is therefore PAID FOR by a
    /// drain+recycle completing inside one resolve window — real system-wide
    /// progress, the same termination argument as the revalidation mismatch it
    /// shares a budget with. The loop is lock-free and terminates without the
    /// bound; the bound buys STARVATION-freedom (a `get` costs at most
    /// `REVALIDATE_RETRIES` pins under any recycle storm), and it is what makes
    /// a directly PLANTED stale entry terminate — a state reachable only from
    /// inside the crate, which is how
    /// `incarnation_tests::stale_location_is_rejected_by_every_consumer` gets to
    /// assert this arm's policy at all.
    ///
    /// The write-path loops that also retry an unpinnable candidate
    /// (`cas`, `numeric_update`, `try_into_numeric`) do NOT share this bound and
    /// do not triage the two failures at all — they retry both unboundedly. That
    /// is sound for the reason above and NOT parity with this function; each says
    /// so at its own snooze. `insert` cannot wait while it holds its
    /// reservation; it rolls back first and then waits in
    /// `wait_while_draining_or_filling`.
    ///
    /// It shares `attempts` — and therefore `REVALIDATE_RETRIES` — with
    /// `follow_republished` rather than carrying `RESERVE_RETRIES`. The two
    /// bound the same thing (how many times ONE `get` re-attempts because the
    /// world moved under it, one pin apiece), so a single counter is what caps
    /// a `get` at `REVALIDATE_RETRIES` pins no matter how an adversary mixes the
    /// arms. `RESERVE_RETRIES` bounds the write path's eviction passes that free no segment;
    /// #68 split the read path's budget out of it precisely because 3 is far too
    /// tight for a live key, and re-coupling this arm to it would re-introduce
    /// the false miss on the other face of the same window.
    ///
    /// Charging costs a real segment RECYCLE, so exhausting the budget here
    /// needs ~16 full segment lifecycles inside one resolve window.
    /// `revalidation_tests::budget_absorbs_recycled_incarnations_without_a_false_absent`
    /// drives 15 of them deterministically and counts the charges, so that
    /// argument is tested rather than merely stated.
    #[cold]
    #[inline(never)]
    fn triage_unknown_location(
        &self,
        location: Location,
        backoff: &Backoff,
        attempts: &mut usize,
    ) -> Option<()> {
        if self.segments.resolve(location).is_none() {
            #[cfg(all(test, not(model_checking)))]
            stale_incarnation_charges::record();
            *attempts += 1;
            if *attempts >= REVALIDATE_RETRIES {
                return None;
            }
        }
        backoff.snooze();
        Some(())
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
    ///
    /// The returned `Item` borrows the cache and pins its segment until it
    /// is dropped; see [`Item`].
    ///
    /// ```
    /// use segcache::{Policy, Segcache};
    ///
    /// let cache = Segcache::builder().build().expect("failed to create cache");
    /// assert!(cache.get_no_freq_incr(b"coffee").is_none());
    /// ```
    pub fn get_no_freq_incr(&self, key: &[u8]) -> Option<Item<'_>> {
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

        // The whole reserve→publish operation rolls back, waits in
        // `wait_while_draining_or_filling`, and restarts with a fresh
        // reservation when the key's current entry cannot be verified or
        // pinned; see the arms below.
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
                    // A candidate slot could not be verified, so whether this
                    // key already has an entry is UNKNOWN. Roll back and
                    // restart — unconditionally, which is the established
                    // `old_seg_id == new_seg` arm below and the #54 argument
                    // transfers exactly. `Unknown` means a candidate segment is
                    // unpinnable, i.e. a drain owns it, and that drain may be
                    // waiting on `active_writers` — the WriterPin inside our
                    // own reservation. Spinning in place cannot resolve;
                    // rolling back drops the pin and unblocks the drain; the
                    // thread then waits for the drain to finish and reserves
                    // again.
                    //
                    // Treating `Unknown` as absent is the failure to avoid:
                    // insert would take the fresh-key arm below and publish a
                    // DUPLICATE entry for a key that already has one (#46).
                    Lookup::Unknown(location) => {
                        self.rollback_reservation(reserved, new_location);
                        self.wait_while_draining_or_filling(location);
                        continue 'operation;
                    }
                    Lookup::Found((old_location, slot)) => {
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
                        // 7f). The pin fails unless the segment is `Live` or
                        // `Sealed`: a drain has claimed it (`Draining`, then
                        // `AwaitingRelease` or `Free`; the drain owns the
                        // item's removal), or it is a copy destination still
                        // being filled (`Relinking`). How to wait depends on
                        // which segment it is:
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
                        //   the drain — wait for the drain to finish, and
                        //   restart the whole operation; the retry reserves in
                        //   a fresh tail because this segment is no longer
                        //   writable.
                        //
                        // - `old_seg_id != new_seg`: that drain is not waiting
                        //   on OUR pin and normally finishes on its own, so a
                        //   brief spin-and-relookup is productive (the entry is
                        //   swept or republished elsewhere). But it can be
                        //   waiting on ANOTHER writer's pin whose owner is
                        //   symmetrically blocked on a drain of OUR segment (a
                        //   cross-thread cycle), so the spin is bounded: once
                        //   the backoff is exhausted, roll back — releasing our
                        //   pin breaks any such cycle — then wait for the drain
                        //   or fill to finish and restart. If the segment is a
                        //   `Relinking` destination, the merge filling it may
                        //   claim our reservation's segment (sealed since we
                        //   reserved) and wait on our `WriterPin`; the same
                        //   bounded spin, rollback and wait handle that case.
                        let Some(pin) = self.segments.try_pin_remover(old_seg_id) else {
                            if old_seg_id == new_seg || backoff.is_completed() {
                                self.rollback_reservation(reserved, new_location);
                                self.wait_while_draining_or_filling(old_location);
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
                    Lookup::Absent => {
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
                            // Same rollback-restart as the `lookup_slot` arm
                            // above, for the same reason: the upsert could not
                            // establish whether the key already has an entry,
                            // and publishing on a guess duplicates it.
                            Ok(Insert::Unknown(location)) => {
                                self.rollback_reservation(reserved, new_location);
                                self.wait_while_draining_or_filling(location);
                                continue 'operation;
                            }
                            Ok(Insert::Created) => {
                                #[cfg(feature = "metrics")]
                                HASH_INSERT.increment();
                                return Ok(());
                            }
                            Ok(Insert::Replaced(raced_old)) => {
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

    /// Wait while the segment `location` points into is `Draining` or
    /// `Relinking` under the same incarnation. `insert` calls this after
    /// rolling back its reservation, so it holds no writer or remover pin, in
    /// two cases:
    ///
    /// - a candidate slot named a location it could not verify (`Unknown`):
    ///   its segment was not readable, usually because a drain owns it, or the
    ///   location's incarnation is gone;
    /// - the key's current entry is in a segment it cannot take a remover pin
    ///   on: a drain claimed it, or it is a merge or S3-FIFO copy destination
    ///   still being filled.
    ///
    /// # Why the wait comes after the rollback
    ///
    /// `insert` cannot wait while it holds its reservation: the reservation
    /// carries a `WriterPin`, and the drain, or the owner of the fill, may be
    /// waiting on that pin, so waiting in place deadlocks both threads (#54).
    ///
    /// Re-reserving immediately after the rollback takes a fresh reservation
    /// per retry, and against a drain or fill that has not finished, that
    /// empties the free pool and `insert` returns `NoFreeSegments`.
    ///
    /// `delete`, `cas`, `numeric_update` and `try_into_numeric` hold no pin
    /// when they see `Unknown`, and retry the lookup after a snooze. `insert`
    /// reaches the same position only after the rollback, and polls the
    /// segment's state instead.
    ///
    /// # Termination
    ///
    /// A drain waits on the writer and remover pins of the segment it claimed.
    /// A merge fill waits on the same pins of each candidate it claims, and on
    /// leaf locks (the eviction-policy mutex, an item's version lock). An
    /// S3-FIFO fill claims its source before the destination is `Relinking`;
    /// after that it waits only on an item's version lock. After the rollback
    /// this thread holds no writer or remover pin and no lock, so the wait
    /// ends when the drain or the fill does. When a drain finishes, the segment
    /// is `Free` (`resolve` says `None` once the generation is bumped) or
    /// condemned to its readers (`AwaitingRelease`); either way the drain has
    /// swept or relinked the entry. When a fill finishes, the destination is
    /// `Sealed`, which `try_pin_remover` accepts. A fresh lookup then resolves
    /// the key at its current location or reports it absent.
    ///
    /// An `AwaitingRelease` segment ends the wait. It keeps its generation and
    /// refuses new pins until its last reader drops its `Item`, and that
    /// reader can be the waiting thread.
    #[cold]
    #[inline(never)]
    fn wait_while_draining_or_filling(&self, location: Location) {
        #[cfg(all(test, not(model_checking)))]
        self.insert_waits
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let backoff = Backoff::new();
        while self.segments.resolve(location).is_some_and(|(seg_id, _)| {
            matches!(
                self.segments.header(seg_id).state(),
                State::Draining | State::Relinking
            )
        }) {
            backoff.snooze();
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
        // has room by evicting from it if it's at capacity — this keeps the
        // admission pool near the cap computed at construction time.
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
                    // Label the segment admission-pool for S3-FIFO. Fresh
                    // segments are labelled Main, and several inserts can
                    // land in the same one, so only the insert whose CAS
                    // changes the label counts it. This runs while
                    // `reserved_item` holds the segment's writer pin;
                    // `recycle` and `condemn` run only after the drain claim
                    // sees no writers, so they read this label.
                    if target_pool == SegmentPool::Admission
                        && self
                            .segments
                            .header(reserved_item.seg())
                            .cas_pool(SegmentPool::Main, SegmentPool::Admission)
                    {
                        self.segments.incr_pool(SegmentPool::Admission);
                    }
                    return Ok(reserved_item);
                }
                Err(TtlBucketsError::ItemOversized { size }) => {
                    return Err(SegcacheError::ItemOversized { size });
                }
                Err(TtlBucketsError::NoFreeSegments) => {
                    // Evict, then retry without spending a retry if any
                    // segment was freed since `freed_before`, on this thread
                    // or another, including one another reserve has since
                    // taken. Otherwise wait for the evictions counted as
                    // running (see `wait_for_running_evictions`), and spend a
                    // retry if no segment was freed by the time they finish. A pass that only
                    // refills the merge spare frees nothing to the free
                    // queue. The number of passes is not bounded while other
                    // threads keep freeing segments.
                    let freed_before = self.segments.freed_count();
                    let _ = self.segments.evict(&self.ttl_buckets, &self.hashtable);
                    if self.segments.freed_count() != freed_before
                        || self.segments.wait_for_running_evictions(freed_before)
                    {
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
        let now = crate::clock::now();
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
            // ONE probe. Before #91 this was three: `lookup_slot` (unpinned),
            // then `acquire_item_at` to pin the location it returned, then a
            // full `lookup_no_freq_update` to prove the pinned bytes were
            // still this key's. The verifier pins in order to compare, so the
            // first two collapse into one another, and the third is answered
            // by re-reading the slot the lookup already found.
            let hit = match self.hashtable.lookup_no_freq_update(key, &verifier) {
                Lookup::Found(hit) => hit,
                Lookup::Absent => return Err(SegcacheError::NotFound),
                // Transient drain window, or a location whose incarnation is
                // gone; either way the fresh lookup on retry resolves the live
                // location or reports the key gone.
                //
                // NOT counted against `attempts`, and — unlike `get_pinned` —
                // not counted against anything else either: this loop does not
                // triage the two failures, so it has no bounded arm to charge.
                // Sound because neither can spin. A drain is bounded,
                // straight-line work; and a fresh lookup cannot keep handing
                // back a dead incarnation, because no hashtable entry survives
                // its segment's generation bump (the reachability argument is
                // written out on `triage_unknown_location`), so every stale pin
                // failure is paid for by a real recycle. `attempts` /
                // `RESERVE_RETRIES` below bounds the OTHER face of this window
                // — a key being republished under us.
                Lookup::Unknown(_location) => {
                    backoff.snooze();
                    continue;
                }
            };
            let (raw, guard) = hit.pin;
            let (seg_id, _offset) = unpack_location(hit.location);
            let seg_id = NonZeroU32::new(seg_id).ok_or(SegcacheError::NotFound)?;

            // Lazy expiry: memcached returns NOT_FOUND for a cas on an expired
            // key, even before the segment is reclaimed. Read under the pin, so
            // unlike the pre-#91 form it is not merely a semantic filter racing
            // a recycle — the header cannot be recycled while the guard is
            // held.
            if let Err(error) = self.remaining_ttl(seg_id) {
                drop(guard);
                return Err(error);
            }

            // Freshness: is the entry we pinned still the PUBLISHED one? The
            // pin settles which item these bytes are; it says nothing about
            // whether a racing writer has since superseded them, and a token
            // minted from a superseded item is a bad CAS token — a correctness
            // question here, not merely a staleness one. Exact by the
            // CAS-in-place argument (`MultiChoiceHashtable::slot_publishes`).
            //
            // A bounded number of mismatches means the key is churning under
            // us, and any relocation/replacement has already staled the
            // caller's location-bearing token: fail `Exists` (never a false
            // `NotFound` for a live key).
            if !self.hashtable.slot_publishes(hit.slot, hit.location) {
                drop(guard);
                attempts += 1;
                if attempts >= RESERVE_RETRIES {
                    return Err(SegcacheError::Exists);
                }
                continue;
            }
            let token = Self::token_for(&raw, hit.location, self.segments.generation(seg_id));
            // The pin is released HERE, before the reservation below: a verify
            // pin is never held across a wait, and `reserve_and_define` can
            // drive an eviction.
            drop(guard);
            break (hit.location, hit.slot, token);
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
        // Set the moment this call unlinks an entry for `key`. From then on the
        // ack is OWED, even if the loop goes round again — and it can, because
        // the post-unlink re-check below is allowed to answer "could not look"
        // rather than "confirmed gone". Re-deriving the answer from a later
        // iteration instead loses it: the key is absent by then precisely
        // BECAUSE this call removed it, and `delete` would report NOT_FOUND for
        // a key it had just deleted.
        let mut unlinked = false;
        loop {
            // Look up the item to get its location
            // The verify pin the lookup took is dropped with `hit`: this path
            // wants the location, not the bytes, and the pin that matters below
            // is the REMOVER pin, which is the one a drain waits out.
            let location = match self.hashtable.lookup_no_freq_update(key, &verifier) {
                Lookup::Found(hit) => hit.location,
                Lookup::Absent => return unlinked,
                // Unbounded retry, as at the refused remover pin below: a
                // draining segment does not mean the key is gone, and an acked
                // `false` for a live key is a lost delete.
                Lookup::Unknown(_location) => {
                    backoff.snooze();
                    continue;
                }
            };

            let (seg_id, offset) = unpack_location(location);
            let Some(seg_id) = NonZeroU32::new(seg_id) else {
                // Not expected — `lookup_no_freq_update` only returns real
                // (non-ghost) entries, and `verify` rejects an out-of-range
                // location before pinning — but stay defensive: nothing to pin.
                //
                // `unlinked ||` so that EVERY exit from this loop honours an
                // unlink an earlier iteration already performed. That is the
                // whole content of the lost-ack fix, and an exit that quietly
                // opts out of it is how the bug comes back.
                return unlinked || self.hashtable.remove(key, location);
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
                return unlinked;
            }

            // Pin the item's segment BEFORE unlinking it (item 7f): the pin
            // brackets both the hashtable unlink below and the `remove_at`
            // decrement, so a concurrent drain of this segment cannot
            // interleave with the decrement.
            //
            // If the pin FAILS, the segment is Relinking — a merge/promotion
            // copy destination mid-fill — or it was claimed for drain in the
            // instant since the lookup. The `Relinking` case is why the unlink
            // below happens WITHOUT a pin rather than waiting: a copy
            // destination is never swept by anyone, so "the drain will remove
            // it" does not hold there and an acked delete that left the entry
            // behind would resurrect.
            //
            // A segment already `Draining` when the lookup ran never reaches
            // here: `Draining` is not readable, so the pinned verify (#91)
            // answers `Unknown` and the loop above waits it out instead. That
            // is the right answer for a drain — a merge drain RETAINS live
            // items and republishes them, so the retry unlinks the entry at
            // its new location, and a clear/expire drain sweeps it, so the
            // retry reports the key gone. Neither can resurrect, and the wait
            // cannot wedge: `delete` holds no pin while it waits, so it can
            // never be what a drain is waiting on.
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
            // ABA guard: `location` is (segment, incarnation tag, offset),
            // and `hashtable.remove` matches (hash tag, location) — the whole
            // 44-bit location word, incarnation included — without
            // re-verifying key bytes. The pinned path below is exempt only
            // because its pin freezes the segment against recycling (the
            // location-uniqueness precondition documented on
            // `cas_location_at`). Unpinned, the segment could have been
            // drained, recycled, and refilled since the lookup, with a
            // colliding-tag key freshly written at this exact offset. Three
            // defenses, the first structural: (0) the incarnation tag — a
            // refill republishes at the NEXT generation, so `remove`'s
            // location compare simply does not match it, and an ALIASING
            // republish needs 64 full lifecycles of this id rather than one
            // (the tag is 6 bits); (1) refuse the unpinned unlink when the
            // generation moved since the lookup — the entry is stale either
            // way; (2) after a successful unlink, re-verify the key stopped
            // resolving, retrying if it did not — so an acked delete NEVER
            // leaves the key reachable (a retry against a concurrent
            // re-insert deletes the newer value: a legal linearization of
            // concurrent set+delete).
            //
            // The residual below predates (0), so it is now CONSERVATIVE
            // rather than tight — kept because its conclusion is the part
            // that matters. The residual window (generation load to
            // remove-CAS) requires a full drain+recycle+refill+publish
            // plus a 12-bit tag collision in an overlapping bucket to land
            // within a few instructions — and, with (0), 64 such lifecycles
            // rather than one. Its worst case is a spurious unlink of ONE
            // colliding key — observably an eviction, which a cache may
            // always perform — never corruption (the unlink touches no
            // segment state). `table.rs`'s
            // `loom_stale_incarnation_unlink_cannot_take_the_refilled_entry`
            // models exactly this race with defense (1) deliberately absent,
            // so (0)'s own contribution is asserted rather than assumed.
            let Some(pin) = self.segments.try_pin_remover(seg_id) else {
                if self.segments.generation(seg_id) == observed_gen
                    && self.hashtable.remove(key, location)
                {
                    // The unlink LANDED. Record it before asking whether the
                    // key is gone, because that question has three answers and
                    // only one of them is "yes".
                    unlinked = true;
                    // Counted PER UNLINK, matching the pinned path below. That
                    // is a deliberate change: before the lost-ack fix these
                    // sat behind the whole `&& ...Absent` conjunction, so this
                    // path under-counted whenever the re-check did not confirm
                    // — including every case where it was the retry, not this
                    // iteration, that finished the job. A `delete` that unlinks
                    // twice (ours, then a racing re-insert's) now counts twice,
                    // because two entries really were removed.
                    #[cfg(feature = "metrics")]
                    {
                        HASH_REMOVE.increment();
                        ITEM_DELETE.increment();
                    }
                    // Defense (2): an acked delete must NEVER leave the key
                    // reachable. `Absent` is the only answer that proves that.
                    // `Unknown` means a candidate slot could not be verified —
                    // a 12-bit-tag-colliding entry in a segment a drain owns —
                    // and proves nothing, so it retries rather than acking. The
                    // ack itself is not lost by that: `unlinked` carries it.
                    if matches!(
                        self.hashtable.lookup_no_freq_update(key, &verifier),
                        Lookup::Absent
                    ) {
                        return true;
                    }
                }
                // The entry moved (a merge republished it elsewhere), was
                // removed concurrently, or the key still resolves — or could
                // not be shown not to — after the unlink. Retry from the
                // lookup, which resolves the fresh location or reports the key
                // gone.
                backoff.snooze();
                continue;
            };

            // Remove from hashtable
            if !self.hashtable.remove(key, location) {
                drop(pin);
                return unlinked;
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
        let verifier = self.verifier();
        let backoff = Backoff::new();
        loop {
            let hit = match self.hashtable.lookup(key, &verifier) {
                Lookup::Found(hit) => hit,
                Lookup::Absent => return Err(SegcacheError::NotFound),
                // Segment not readable (draining; a relocation is in flight),
                // or the location's incarnation is gone — back off and retry
                // from the lookup, giving the drain a chance to finish instead
                // of busy-waiting through its whole window.
                //
                // Unbounded, and NOT the bounded arm `get_pinned` gives a
                // stale incarnation: this loop does not triage the two. Safe
                // for both — a drain is bounded work, and a fresh lookup
                // cannot keep resolving to a dead incarnation, because no
                // hashtable entry survives its segment's generation bump (see
                // `triage_unknown_location` for the invariants).
                Lookup::Unknown(_location) => {
                    backoff.snooze();
                    continue;
                }
            };
            // The lookup's OWN pin. Before #91 this path looked the key up
            // unpinned and then pinned the location it got back; the verifier
            // now pins in order to compare at all, so re-pinning here would be
            // a second `SeqCst` pair for a guarantee already in hand. The pin
            // is also what establishes `raw` as a REAL item of this
            // incarnation, which is what makes the version-word access below
            // sound.
            let (raw, _guard) = hit.pin;
            let (seg_id, _offset) = unpack_location(hit.location);
            let seg_id = NonZeroU32::new(seg_id).ok_or(SegcacheError::NotFound)?;

            // Lazy expiry: a counter past its segment deadline is
            // treated as missing, matching memcached, even before
            // expire() reclaims the segment.
            self.remaining_ttl(seg_id)?;

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
            //     PUBLISHED: the re-check below sees the slot moved,
            //     we drop the lock unchanged and retry — the
            //     increment applies (once) to the NEW item. Acked
            //     only after it is visible.
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
            //
            // The check itself is the SAME-SLOT compare, not a fresh probe.
            // It is exact for "still published" by the CAS-in-place argument
            // (`MultiChoiceHashtable::slot_publishes`): every party that could
            // supersede or move this item — a cas publish, a relocation, an
            // unlink — CASes the slot the entry occupies, which is the slot
            // the lookup above found it in. A full re-probe would answer the
            // same question by re-hashing the key and rescanning its buckets,
            // and would take a pin of its own to do it.
            let vguard = match raw.lock_numeric_version() {
                Ok(vguard) => vguard,
                // NOT-NUMERIC IS ALSO A FRESHNESS QUESTION. `raw` is a real
                // item of this incarnation (the pin and the tag say so), but it
                // may have been SUPERSEDED — a racing `try_into_numeric` can
                // have converted this key and published the numeric copy
                // elsewhere, leaving the bytes we pinned as the old
                // non-numeric value. Returning `NotNumeric` on that would be a
                // spurious error for a key that is numeric right now, and it
                // would break the `try_into_numeric` + `wrapping_add`
                // composition this API documents.
                //
                // Before the write-path collapse a full re-probe ran BEFORE
                // this lock and caught it; the freshness check now lives after
                // it, so this arm has to consult it too rather than escaping
                // through `?`.
                Err(_) => {
                    if self.hashtable.slot_publishes(hit.slot, hit.location) {
                        // Still published, and genuinely not numeric.
                        return Err(SegcacheError::NotNumeric);
                    }
                    continue;
                }
            };
            if !self.hashtable.slot_publishes(hit.slot, hit.location) {
                drop(vguard);
                continue;
            }
            return Ok(vguard.update(&op));
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
            // ONE probe, same collapse as `cas` above: the verifier pins in
            // order to compare, so the lookup already hands back the pinned
            // item, and freshness is the same-slot re-read rather than a
            // second full probe.
            let hit = match self.hashtable.lookup_no_freq_update(key, &verifier) {
                Lookup::Found(hit) => hit,
                // Transient drain window, or a stale incarnation whose pin is
                // refused — retry from the lookup.
                //
                // Unbounded, for both, and NOT parity with `get_pinned` (which
                // bounds its stale arm): this loop does not triage the two
                // failures. Neither can spin — a drain is bounded work, and no
                // hashtable entry survives its segment's generation bump, so a
                // fresh lookup cannot keep returning a dead incarnation (see
                // `triage_unknown_location`). `attempts` below bounds the
                // separate churn face of this window.
                Lookup::Unknown(_location) => {
                    backoff.snooze();
                    continue;
                }
                Lookup::Absent => {
                    // Missing: create with the caller's ttl. NOTE for the
                    // concurrent future: this publishes via plain insert, which
                    // would overwrite a concurrently created value; revisit with
                    // insert-if-absent when the API goes concurrent.
                    return self.insert(key, initial, None, ttl);
                }
            };
            let (raw, guard) = hit.pin;
            let (seg_id, _offset) = unpack_location(hit.location);
            let seg_id = NonZeroU32::new(seg_id).ok_or(SegcacheError::NotFound)?;

            // Freshness (see `cas`): a superseded entry means the bytes below
            // are no longer the published value. Retry, and after a bounded
            // number of mismatches report the churn as `Exists` (the same
            // outcome `replace_at` gives a concurrent replacement), never a
            // false `NotFound`.
            if !self.hashtable.slot_publishes(hit.slot, hit.location) {
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
            // `guard` is released at the break, before the reservation below.
            break (hit.location, hit.slot, parsed, opt_buf, olen, seg_ttl);
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
    #[cfg(all(test, not(model_checking)))]
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
