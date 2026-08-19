//! Segment header with atomic fields for lock-free metadata access.
//!
//! Each header is exactly 64 bytes (one cache line) and uses atomic types
//! for all mutable fields, preparing for concurrent access.
//!
//! ```text
//! ┌──────────────┬──────────────┬──────────────┬──────────────┐
//! │      ID      │ WRITE OFFSET │  LIVE BYTES  │  LIVE ITEMS  │
//! │     u32      │  AtomicI32   │  AtomicI32   │  AtomicI32   │
//! │    32 bit    │    32 bit    │    32 bit    │    32 bit    │
//! ├──────────────┼──────────────┼──────────────┼──────────────┤
//! │  CREATE AT   │   MERGE AT   │     TTL      │  REF COUNT   │
//! │ AtomicInstant│ AtomicInstant│  AtomicU32   │  AtomicU32   │
//! │    32 bit    │    32 bit    │    32 bit    │    32 bit    │
//! ├──────────────┴──────────────┼──────┬──┬──┬────────────────┤
//! │          METADATA           │ GEN  │PL│PD│ ACTIVE WRITERS │
//! │          AtomicU64          │ 16b  │8b│8b│  AtomicU32/32b │
//! ├─────────────────────────────┴──────┴──┴──┴────────────────┤
//! │        ACTIVE REMOVERS      │          PADDING            │
//! │        AtomicU32/32b        │            96 bit           │
//! └───────────────────────────────────────────────────────────┘
//!
//! METADATA = [8 tag][8 state][24 prev][24 next] (see segments::state)
//! GEN = generation (AtomicU16)   PL = SegmentPool (AtomicU8)
//! PD = 8-bit alignment pad before ACTIVE WRITERS (AtomicU32)
//! Total: 512 bits = 64 bytes = 1 cache line
//! ```
//!
//! The state, prev, and next fields share one atomic word so that a chain
//! mutation and its state transition are a single CAS — the property
//! concurrent linking requires (ported from crucible). `ref_count` and
//! `generation` deliberately stay separate atomics: the reader-pinning
//! protocol pairs a `ref_count` RMW against a state load (SeqCst Dekker
//! pair), and the generation feeds the CAS-token ABA protection.

use crate::segments::state::{Metadata, State};
use crate::sync::{AtomicI32, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering};
use clocksource::coarse::{AtomicInstant, Duration, Instant};
use core::num::NonZeroU32;

/// Outcome of a reader-pin attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AcquireOutcome {
    /// A pin was taken; the caller owns it and must pair it with a guard.
    Acquired,
    /// The segment is not readable; no pin is held.
    NotReadable,
    /// The segment is not readable, and backing the pin out left a
    /// condemned segment with no reader remaining to free it. This caller
    /// won the AwaitingRelease -> Free transition and must return the
    /// segment to the free queue.
    ReleaseCondemned,
}

/// Test-only interposition inside [`SegmentHeader::try_acquire_reader`].
///
/// The acquire's increment/re-check window is only entered when the state
/// changes underneath it, which no single-threaded test can arrange and no
/// multi-threaded one can arrange *deterministically*. This hook lets a
/// test park the acquire at either edge of that window and run the racing
/// drain by hand, with no scheduler — the same "put the race where it
/// happens" idiom as #60's `KeyVerifier` oracle.
///
/// Ambient (a thread-local) rather than a parameter so that
/// `try_acquire_reader` keeps its exact production signature and body:
/// outside `cfg(test)` the two [`fire`] calls compile away entirely, and
/// the test drives the real `Segments::acquire_item_at`, not a copy.
///
/// Phases:
///   0 — after the `ref_count` increment, before the state re-check
///   1 — after the re-check failed, before the backout
#[cfg(all(test, not(feature = "loom")))]
pub(crate) mod acquire_hook {
    use std::cell::RefCell;

    /// What a test installs: called with the phase number.
    pub(crate) type Hook = Box<dyn FnMut(u8)>;

    thread_local! {
        static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    /// Uninstalls the hook when dropped, so a failing test cannot leak it
    /// onto the next test sharing this thread.
    pub(crate) struct Installed;

    impl Drop for Installed {
        fn drop(&mut self) {
            HOOK.with(|h| *h.borrow_mut() = None);
        }
    }

    /// Install `hook` on this thread until the returned guard drops.
    pub(crate) fn install(hook: Hook) -> Installed {
        HOOK.with(|h| *h.borrow_mut() = Some(hook));
        Installed
    }

    /// Run the installed hook, if any. It is taken out of the slot for the
    /// duration of the call, so an acquire reached from *inside* the hook
    /// runs unhooked (and cannot re-borrow the cell).
    pub(super) fn fire(phase: u8) {
        let taken = HOOK.with(|h| h.borrow_mut().take());
        if let Some(mut hook) = taken {
            hook(phase);
            HOOK.with(|h| {
                let mut slot = h.borrow_mut();
                if slot.is_none() {
                    *slot = Some(hook);
                }
            });
        }
    }
}

/// Which pool a segment belongs to (for S3-FIFO eviction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum SegmentPool {
    Main = 0,
    Admission = 1,
}

impl SegmentPool {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Admission,
            _ => Self::Main,
        }
    }
}

/// Segment metadata header, cache-line aligned (64 bytes).
///
/// All mutable fields use atomic types so the header can be read via
/// shared reference (`&self`). This enables the `Segment<'a>` view to
/// hold `&'a SegmentHeader` instead of `&'a mut SegmentHeader`.
///
/// ```text
/// Offset  Size  Field
///  0       4    id            (u32, immutable after init)
///  4       4    write_offset  (AtomicI32)
///  8       4    live_bytes    (AtomicI32)
/// 12       4    live_items    (AtomicI32)
/// 16       4    create_at     (AtomicInstant)
/// 20       4    merge_at      (AtomicInstant)
/// 24       4    ttl           (AtomicU32, seconds)
/// 28       4    ref_count     (AtomicU32, active readers)
/// 32       8    metadata      (AtomicU64: state + prev + next)
/// 40       2    generation    (AtomicU16, bumped on reserve)
/// 42       1    pool          (AtomicU8, SegmentPool)
/// 43       1    (implicit alignment pad before active_writers)
/// 44       4    active_writers (AtomicU32, in-flight reserve/write pins)
/// 48       4    active_removers (AtomicU32, in-flight replace/delete pins)
/// 52       9    _pad          (+3 implicit trailing bytes to align(64) → 64)
/// ```
#[repr(C, align(64))]
pub(crate) struct SegmentHeader {
    id: u32,
    write_offset: AtomicI32,
    live_bytes: AtomicI32,
    live_items: AtomicI32,
    create_at: AtomicInstant,
    merge_at: AtomicInstant,
    ttl: AtomicU32,
    ref_count: AtomicU32,
    metadata: AtomicU64,
    generation: AtomicU16,
    pool: AtomicU8,
    active_writers: AtomicU32,
    active_removers: AtomicU32,
    _pad: [u8; 9],
}

// Loom atomics are larger than std atomics, so skip size check under loom.
#[cfg(not(feature = "loom"))]
const _: () = assert!(std::mem::size_of::<SegmentHeader>() == 64);
#[cfg(not(feature = "loom"))]
const _: () = assert!(std::mem::align_of::<SegmentHeader>() == 64);

impl SegmentHeader {
    /// Create a new header for the given segment id. Write statistics
    /// start at the integrity-aware initial offset, matching `init()`.
    pub fn new(id: NonZeroU32) -> Self {
        let initial_offset = if cfg!(feature = "integrity") {
            std::mem::size_of::<u64>() as i32
        } else {
            0
        };
        Self {
            id: id.get(),
            write_offset: AtomicI32::new(initial_offset),
            live_bytes: AtomicI32::new(initial_offset),
            live_items: AtomicI32::new(0),
            create_at: AtomicInstant::new(Instant::default()),
            merge_at: AtomicInstant::new(Instant::default()),
            ttl: AtomicU32::new(0),
            ref_count: AtomicU32::new(0),
            metadata: AtomicU64::new(Metadata::new_free().pack()),
            generation: AtomicU16::new(0),
            pool: AtomicU8::new(SegmentPool::Main as u8),
            active_writers: AtomicU32::new(0),
            active_removers: AtomicU32::new(0),
            _pad: [0; 9],
        }
    }

    /// Initialize the header for a fresh allocation.
    /// When the `magic` feature is enabled, sets write_offset and live_bytes
    /// past the magic bytes region.
    pub fn init(&self) {
        let initial_offset = if cfg!(feature = "integrity") {
            std::mem::size_of::<u64>() as i32
        } else {
            0
        };
        self.write_offset.store(initial_offset, Ordering::Relaxed);
        self.live_bytes.store(initial_offset, Ordering::Relaxed);
        self.live_items.store(0, Ordering::Relaxed);
        self.metadata
            .store(Metadata::new_free().pack(), Ordering::Relaxed);
    }

    /// Reset the write statistics (write offset, live bytes, live items)
    /// to their initial values. Callers must hold exclusive ownership of
    /// the segment's data — a `Draining` claim with the reader count
    /// observed zero (`recycle`), or a just-won `Free -> Reserved` CAS
    /// (`try_reserve`) — since a reset under live readers would corrupt
    /// their offset math.
    ///
    /// With `metrics`, any residual live items/bytes being zeroed here are
    /// items that leaked their `remove_at` decrement (unlinked without a
    /// remover pin — a delete racing a drain, the fresh-key insert de-dup
    /// race, a reservation rollback), so the global item gauges are
    /// corrected by the residue. Exactly-once: the first reset zeroes the
    /// counters, so a second reset (recycle then try_reserve) subtracts
    /// nothing. The residue is also mirrored into the dead-item gauges,
    /// exactly as `Segment::remove_item_at` does on the normal path, so an
    /// item that dies via an unpinned unlink is accounted the same way as
    /// one that dies normally.
    pub fn reset_write_stats(&self) {
        let initial_offset = if cfg!(feature = "integrity") {
            std::mem::size_of::<u64>() as i32
        } else {
            0
        };
        #[cfg(feature = "metrics")]
        {
            let leaked_items = self.live_items.load(Ordering::Relaxed);
            let leaked_bytes = self.live_bytes.load(Ordering::Relaxed) - initial_offset;
            if leaked_items > 0 {
                crate::ITEM_CURRENT.sub(leaked_items as _);
                crate::ITEM_DEAD.add(leaked_items as _);
            }
            if leaked_bytes > 0 {
                crate::ITEM_CURRENT_BYTES.sub(leaked_bytes as _);
                crate::ITEM_DEAD_BYTES.add(leaked_bytes as _);
            }
        }
        self.write_offset.store(initial_offset, Ordering::Relaxed);
        self.live_bytes.store(initial_offset, Ordering::Relaxed);
        self.live_items.store(0, Ordering::Relaxed);
    }

    /// Get the generation counter. Incremented each time the segment is
    /// reserved from the free queue; wraps at `u16::MAX`.
    #[inline]
    pub fn generation(&self) -> u16 {
        self.generation.load(Ordering::Relaxed)
    }

    // -- Metadata word (state + chain pointers) --

    /// Load and unpack the metadata word.
    #[inline]
    pub fn metadata(&self, order: Ordering) -> Metadata {
        Metadata::unpack(self.metadata.load(order))
    }

    /// Single-shot CAS transition of the metadata word.
    ///
    /// Fails (returns false) if the current state is not `expected_state`
    /// or if the word changed concurrently. For the link parameters,
    /// `None` keeps the current value and `Some(x)` (including
    /// `Some(None)`) replaces it. `success` is the success ordering
    /// (failure ordering is always `Acquire`): use `SeqCst` for
    /// transitions that participate in a reader-handoff Dekker pair
    /// (Sealed/Live -> Draining, Draining -> AwaitingRelease,
    /// AwaitingRelease -> Free), `AcqRel` otherwise.
    pub fn cas_metadata(
        &self,
        expected_state: State,
        new_state: State,
        new_next: Option<Option<NonZeroU32>>,
        new_prev: Option<Option<NonZeroU32>>,
        success: Ordering,
    ) -> bool {
        let current = self.metadata.load(Ordering::Acquire);
        let meta = Metadata::unpack(current);
        if meta.state != expected_state {
            return false;
        }
        let new = Metadata {
            state: new_state,
            next: new_next.unwrap_or(meta.next),
            prev: new_prev.unwrap_or(meta.prev),
            tag: meta.tag,
        };
        self.metadata
            .compare_exchange(current, new.pack(), success, Ordering::Acquire)
            .is_ok()
    }

    /// Patch chain pointers while preserving the current state.
    ///
    /// A CAS loop rather than a store because the same word carries the
    /// state, which a concurrent transition may change. Today all chain
    /// writers are serialized by `&mut Segments` (the analogue of
    /// crucible's chain mutex), so the loop is belt-and-braces for the
    /// concurrent future.
    pub fn update_links(
        &self,
        new_next: Option<Option<NonZeroU32>>,
        new_prev: Option<Option<NonZeroU32>>,
    ) {
        let mut current = self.metadata.load(Ordering::Acquire);
        loop {
            let meta = Metadata::unpack(current);
            let new = Metadata {
                state: meta.state,
                next: new_next.unwrap_or(meta.next),
                prev: new_prev.unwrap_or(meta.prev),
                tag: meta.tag,
            };
            match self.metadata.compare_exchange(
                current,
                new.pack(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    /// Reserve a Free segment for reuse (Free -> Reserved, links cleared).
    ///
    /// On success, resets the write statistics, stamps creation/merge
    /// times, and bumps the generation counter so that CAS tokens issued
    /// against the previous use of this segment can never match items
    /// written after it is recycled.
    pub fn try_reserve(&self) -> bool {
        if !self.cas_metadata(
            State::Free,
            State::Reserved,
            Some(None),
            Some(None),
            Ordering::AcqRel,
        ) {
            return false;
        }

        // NOTE: deliberately NO "empty at reserve" assertion here. A
        // synchronous fully-drained-means-zeroed invariant does not hold
        // under concurrency (see the item 7f note on `Segment::clear`):
        // removals that unlink a hashtable entry WITHOUT a remover pin — a
        // delete racing the drain that owns the segment, the fresh-key
        // insert de-dup race, a reservation rollback against a claimed
        // segment — cannot decrement the segment's counters, and the drain
        // sweep skips the already-unlinked item, so a segment can
        // legitimately reach Free with transiently over-counted
        // `write_offset`/`live_bytes`/`live_items`. `recycle` resets them
        // on the common path; a condemned segment (freed by its last
        // reader's guard drop) carries them until here. The stores below
        // are the authoritative reset either way.
        self.reset_write_stats();
        self.mark_created();
        self.mark_merged();
        self.generation.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Return an unused segment to Free (Reserved|Linking -> Free).
    /// Used by allocation error paths before a segment becomes visible.
    pub fn try_release(&self) -> bool {
        self.cas_metadata(
            State::Reserved,
            State::Free,
            Some(None),
            Some(None),
            Ordering::AcqRel,
        ) || self.cas_metadata(
            State::Linking,
            State::Free,
            Some(None),
            Some(None),
            Ordering::AcqRel,
        )
    }

    /// Try to free a condemned segment (AwaitingRelease -> Free).
    ///
    /// Returns true iff this caller won the transition — the CAS
    /// uniqueness is what guarantees exactly-one-free between the last
    /// reader's guard drop and the condemner's race-fix recheck. The
    /// caller that wins must return the segment to the free queue.
    ///
    /// SeqCst: this participates in the release-side Dekker pair (guard
    /// drop decrements ref_count SeqCst, then loads the state; the
    /// condemner CASes to AwaitingRelease SeqCst, then loads ref_count).
    /// Single-shot, with no retry: every writer of an AwaitingRelease word
    /// changes the state, so a lost CAS means one of the three claimants
    /// performed the free. Were an `update_links` against a condemned word
    /// ever to become reachable, that same lost CAS would strand the segment
    /// permanently, since nothing sweeps AwaitingRelease.
    pub fn try_release_condemned(&self) -> bool {
        let current = self.metadata.load(Ordering::SeqCst);
        if Metadata::unpack(current).state != State::AwaitingRelease {
            return false;
        }
        let new = Metadata {
            state: State::Free,
            next: None,
            prev: None,
            tag: 0,
        };
        self.metadata
            .compare_exchange(current, new.pack(), Ordering::SeqCst, Ordering::Acquire)
            .is_ok()
    }

    /// Condemn a drained segment (Draining -> AwaitingRelease, links
    /// cleared), stamping the low byte of the generation into the spare
    /// high byte of the metadata word.
    ///
    /// Without the stamp the condemned word is the constant
    /// `{AwaitingRelease, None, None}` for every use of every segment, so
    /// the token `try_release_condemned` CASes on carries no lifetime
    /// identity: a thread stalled between that load and its CAS can win
    /// the transition against a *later* incarnation of the same segment
    /// that still has live readers, freeing it under them and stealing its
    /// handoff. `try_reserve` bumps the generation, so a stalled token no
    /// longer matches once the segment has been recycled.
    ///
    /// The tag rides in `Metadata` so `pack`/`unpack` round-trip it:
    /// `update_links` is a read-modify-write through `Metadata`, reachable
    /// against an AwaitingRelease segment because `condemn` and `recycle`
    /// both splice neighbours after their own transition.
    ///
    /// The tag is 8 bits, so it aliases every 256 uses of a given segment;
    /// the residual is a thread stalled across that many full lifecycles
    /// inside a three-instruction window.
    pub fn cas_condemn(&self) -> bool {
        let current = self.metadata.load(Ordering::Acquire);
        if Metadata::unpack(current).state != State::Draining {
            return false;
        }
        let new = Metadata {
            state: State::AwaitingRelease,
            next: None,
            prev: None,
            tag: (self.generation.load(Ordering::Relaxed) & 0xFF) as u8,
        };
        self.metadata
            .compare_exchange(current, new.pack(), Ordering::SeqCst, Ordering::Acquire)
            .is_ok()
    }

    /// Test-only escape hatch to place a header in an arbitrary state.
    #[cfg(test)]
    #[allow(dead_code)] // used by loom models; dead in non-loom test builds
    pub fn store_metadata_for_test(&self, m: Metadata) {
        self.metadata.store(m.pack(), Ordering::SeqCst);
    }

    // -- Reader pinning --

    /// Try to pin this segment for reading, using a two-phase protocol:
    /// check the state, increment the reader count, then re-check the
    /// state. If the segment became inaccessible between the first check
    /// and the increment, back out and fail.
    ///
    /// While the reader count is non-zero the segment must not be
    /// recycled, merged, or compacted. Every successful acquire must be
    /// paired with exactly one [`Self::release_reader`] (or a
    /// `SegmentGuard` drop).
    #[inline]
    pub fn try_acquire_reader(&self) -> AcquireOutcome {
        if !self.metadata(Ordering::Acquire).state.is_readable() {
            return AcquireOutcome::NotReadable;
        }

        // `SeqCst` on the increment and the re-check is load-bearing.
        // This pair races the writer's mirror image (CAS the state, then
        // load ref_count) — a store-buffering / Dekker pattern.
        // Acquire/release does NOT forbid the outcome where the writer
        // reads ref_count == 0 while our re-check still sees a readable
        // state (both sides proceed); only the SeqCst total order does,
        // which is why the drain/condemn transitions use SeqCst as well.
        // This matches crossbeam-epoch's SeqCst `pin()`, which exists
        // for the same hazard. Note loom cannot verify this distinction:
        // it reports the store-buffering outcome even for pure-SeqCst
        // litmus tests, so the in-tree loom models cover the protocol
        // shape, not this ordering requirement.
        self.ref_count.fetch_add(1, Ordering::SeqCst);

        #[cfg(all(test, not(feature = "loom")))]
        acquire_hook::fire(0);

        // Re-check after the increment: a writer that observed
        // ref_count == 0 may have transitioned the state concurrently.
        if !self.metadata(Ordering::SeqCst).state.is_readable() {
            #[cfg(all(test, not(feature = "loom")))]
            acquire_hook::fire(1);

            // Back out. The decrement must use the same SeqCst handoff as
            // a guard drop, not a plain release: a condemner that observed
            // this transient pin has already deferred reclamation to "the
            // last reader", and a plain decrement here would leave the
            // segment in AwaitingRelease with no reader left to free it.
            let prev = self.release_reader_for_guard();
            if prev == 1 && self.try_release_condemned() {
                return AcquireOutcome::ReleaseCondemned;
            }
            return AcquireOutcome::NotReadable;
        }

        AcquireOutcome::Acquired
    }

    /// Release a reader pin taken with [`Self::try_acquire_reader`]
    /// without the AwaitingRelease handoff. Production pins always ride
    /// in a `SegmentGuard` (whose drop uses the SeqCst path); this plain
    /// path serves the acquire-failure backout and tests.
    #[cfg(test)]
    #[inline]
    pub fn release_reader(&self) {
        let prev = self.ref_count.fetch_sub(1, Ordering::Release);
        debug_assert!(prev > 0, "release_reader without matching acquire");
    }

    /// Decrement the reader count for a guard drop, returning the
    /// previous count. SeqCst: participates in the release-side Dekker
    /// pair with the condemner (see [`Self::try_release_condemned`]).
    #[inline]
    pub fn release_reader_for_guard(&self) -> u32 {
        let prev = self.ref_count.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(prev > 0, "guard release without matching acquire");
        prev
    }

    /// Number of active readers pinning this segment.
    #[inline]
    pub fn ref_count(&self) -> u32 {
        self.ref_count.load(Ordering::Acquire)
    }

    /// Number of active readers, ordered after a preceding SeqCst
    /// drain/condemn transition (the writer half of the Dekker pair).
    #[inline]
    pub fn ref_count_seqcst(&self) -> u32 {
        self.ref_count.load(Ordering::SeqCst)
    }

    // -- Writer pinning --

    /// Try to pin this segment for writing (a reserve→define→publish in
    /// flight), the exact mirror of [`Self::try_acquire_reader`]: check the
    /// state is writable, increment `active_writers`, then re-check. If the
    /// segment was sealed/claimed between the two checks, back out and fail so
    /// the reserver re-reads the tail instead of writing into a segment a drain
    /// is about to parse.
    ///
    /// The `fetch_add` + re-check `SeqCst` pair is the writer half of the Dekker
    /// pair with the drain/evict claim (`cas state -> Draining` then load
    /// `active_writers`). AcqRel would permit both sides to observe the other's
    /// stale value — the reserver seeing `Live` while the claimer sees zero
    /// writers — which is exactly the parse-undefined-region hazard (spec H1).
    /// loom cannot verify this distinction (see `try_acquire_reader`).
    #[inline]
    pub fn try_pin_writer(&self) -> bool {
        if !self.metadata(Ordering::Acquire).state.is_writable() {
            return false;
        }
        self.active_writers.fetch_add(1, Ordering::SeqCst);
        if !self.metadata(Ordering::SeqCst).state.is_writable() {
            // Backout uses Release, not SeqCst (the design spec's pseudocode
            // writes SeqCst): a claimer that counted this pin WAITS for it
            // (claim_for_drain spins on active_writers) rather than acting on
            // it and deferring a handoff, so unwinding needs no place in the
            // SC total order. Readers differ, and their backout has to
            // complete the handoff — see try_acquire_reader.
            self.active_writers.fetch_sub(1, Ordering::Release);
            return false;
        }
        true
    }

    /// Release a writer pin taken with [`Self::try_pin_writer`]. SeqCst mirrors
    /// `release_reader_for_guard`; it is the store half the drain/evict wait
    /// (`active_writers` load) pairs against.
    #[inline]
    pub fn release_writer(&self) {
        let prev = self.active_writers.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(prev > 0, "release_writer without matching pin");
    }

    /// Number of reservers mid-(reserve→define→publish) on this segment,
    /// ordered after a preceding SeqCst claim CAS (the claimer half of the
    /// Dekker pair).
    #[inline]
    pub fn active_writers(&self) -> u32 {
        self.active_writers.load(Ordering::SeqCst)
    }

    // -- Remover pinning (item 7f) --

    /// A replace/delete may remove one of this segment's items iff it holds
    /// live, removable items: `Sealed` (interior) or `Live` (tail). Not
    /// Draining/AwaitingRelease/Relinking/Free.
    #[inline]
    fn state_is_removable(state: State) -> bool {
        matches!(state, State::Sealed | State::Live)
    }

    /// Two-phase pin for a replace/delete that will unlink+decrement one of this
    /// segment's items — the exact mirror of `try_pin_writer`, but gated on the
    /// removable states. Bump `active_removers`, then re-check: if a drain
    /// claimed the segment (`-> Draining`) in between, back out and fail so the
    /// caller retries rather than decrementing a segment being reclaimed. The
    /// SeqCst fetch_add + recheck is the remover half of the Dekker pair with a
    /// drain's claim CAS + `active_removers` load. loom cannot verify this
    /// distinction (see `try_acquire_reader`/`try_pin_writer`).
    #[inline]
    pub fn try_pin_remover(&self) -> bool {
        if !Self::state_is_removable(self.metadata(Ordering::Acquire).state) {
            return false;
        }
        self.active_removers.fetch_add(1, Ordering::SeqCst);
        if !Self::state_is_removable(self.metadata(Ordering::SeqCst).state) {
            self.active_removers.fetch_sub(1, Ordering::Release);
            return false;
        }
        true
    }

    /// Release a remover pin taken with `try_pin_remover`. SeqCst mirrors
    /// `release_writer`.
    #[inline]
    pub fn release_remover(&self) {
        let prev = self.active_removers.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(prev > 0, "release_remover without matching pin");
    }

    /// Reservers-mid-remove count, ordered after a preceding SeqCst claim CAS.
    #[inline]
    pub fn active_removers(&self) -> u32 {
        self.active_removers.load(Ordering::SeqCst)
    }

    // -- Identity --

    #[inline]
    pub fn id(&self) -> NonZeroU32 {
        // SAFETY: id is always set from NonZeroU32 in new()
        unsafe { NonZeroU32::new_unchecked(self.id) }
    }

    // -- Write offset --

    #[inline]
    pub fn write_offset(&self) -> i32 {
        self.write_offset.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn set_write_offset(&self, offset: i32) {
        self.write_offset.store(offset, Ordering::Relaxed);
    }

    /// Atomically reserve `size` bytes of item space, returning the
    /// offset where the caller may write. Fails (`None`) if the
    /// reservation would exceed `capacity`, or if the new offset would
    /// overflow `i32` — `write_offset` never exceeds the capacity, so
    /// item scans, live-byte accounting, and seal decisions need no
    /// clamping (this is why the reservation is a bounded CAS rather
    /// than a raw `fetch_add`).
    ///
    /// A CAS failure means another writer took the slot; the retry
    /// re-reads the observed offset, which only moves toward capacity,
    /// so the loop terminates.
    ///
    /// AcqRel: writer↔writer coordination on the offset word only — no
    /// Dekker pairing with the reader path, so SeqCst is not warranted.
    pub fn try_reserve_space(&self, size: i32, capacity: i32) -> Option<i32> {
        debug_assert!(size >= 0, "reservation size must be non-negative");
        let mut current = self.write_offset.load(Ordering::Acquire);
        loop {
            let new = current.checked_add(size)?;
            if new > capacity {
                return None;
            }
            match self.write_offset.compare_exchange(
                current,
                new,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(current),
                Err(observed) => current = observed,
            }
        }
    }

    // -- Live bytes --

    #[inline]
    pub fn live_bytes(&self) -> i32 {
        self.live_bytes.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn incr_live_bytes(&self, bytes: i32) {
        self.live_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    #[inline]
    pub fn decr_live_bytes(&self, bytes: i32) {
        self.live_bytes.fetch_sub(bytes, Ordering::Relaxed);
    }

    // -- Live items --

    #[inline]
    pub fn live_items(&self) -> i32 {
        self.live_items.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn incr_live_items(&self) {
        self.live_items.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn decr_live_items(&self) {
        self.live_items.fetch_sub(1, Ordering::Relaxed);
    }

    /// Decrement both live items and live bytes atomically.
    #[inline]
    pub fn decr_item(&self, size: i32) {
        self.decr_live_items();
        self.decr_live_bytes(size);
    }

    // -- Chain pointers (views of the metadata word) --

    #[inline]
    pub fn prev_seg(&self) -> Option<NonZeroU32> {
        self.metadata(Ordering::Acquire).prev
    }

    #[inline]
    pub fn set_prev_seg(&self, id: Option<NonZeroU32>) {
        self.update_links(None, Some(id));
    }

    #[inline]
    pub fn next_seg(&self) -> Option<NonZeroU32> {
        self.metadata(Ordering::Acquire).next
    }

    #[inline]
    pub fn set_next_seg(&self, id: Option<NonZeroU32>) {
        self.update_links(Some(id), None);
    }

    // -- Timestamps --

    #[inline]
    pub fn create_at(&self) -> Instant {
        self.create_at.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn mark_created(&self) {
        self.create_at.store(Instant::now(), Ordering::Relaxed);
    }

    #[inline]
    pub fn merge_at(&self) -> Instant {
        self.merge_at.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn mark_merged(&self) {
        self.merge_at.store(Instant::now(), Ordering::Relaxed);
    }

    // -- TTL --

    #[inline]
    pub fn ttl(&self) -> Duration {
        Duration::from_secs(self.ttl.load(Ordering::Relaxed))
    }

    #[inline]
    pub fn set_ttl(&self, ttl: Duration) {
        self.ttl.store(ttl.as_secs(), Ordering::Relaxed);
    }

    // -- State --

    #[inline]
    pub fn state(&self) -> State {
        self.metadata(Ordering::Acquire).state
    }

    /// Test-only helper: store a state while preserving the chain links.
    #[cfg(test)]
    pub fn set_state(&self, state: State) {
        let mut current = self.metadata.load(Ordering::Acquire);
        loop {
            let meta = Metadata::unpack(current);
            let new = Metadata { state, ..meta };
            match self.metadata.compare_exchange(
                current,
                new.pack(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    /// Check if the segment can actually be evicted: Sealed with no
    /// readers pinning it. The write tail is Live, so it is
    /// automatically excluded (the seal happens when a successor is
    /// appended).
    ///
    /// ADVISORY ONLY — this reads ref_count with plain Acquire and is
    /// not part of the Dekker pair. Use it to select candidates, never
    /// to justify touching segment memory: the authoritative check is
    /// the SeqCst drain CAS + ref_count recheck inside clear_segment /
    /// condemn / the merge revert, which fail closed if this was stale.
    #[inline]
    pub fn can_evict(&self) -> bool {
        self.state().is_evictable() && self.ref_count() == 0
    }

    // -- Pool --

    #[inline]
    pub fn pool(&self) -> SegmentPool {
        SegmentPool::from_u8(self.pool.load(Ordering::Relaxed))
    }

    #[inline]
    pub fn set_pool(&self, pool: SegmentPool) {
        self.pool.store(pool as u8, Ordering::Relaxed);
    }
}

impl std::fmt::Debug for SegmentHeader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let meta = self.metadata(Ordering::Relaxed);
        f.debug_struct("SegmentHeader")
            .field("id", &self.id)
            .field("write_offset", &self.write_offset())
            .field("live_bytes", &self.live_bytes())
            .field("live_items", &self.live_items())
            .field("state", &meta.state)
            .field("pool", &self.pool())
            .field("prev_seg", &meta.prev)
            .field("next_seg", &meta.next)
            .field("ttl", &self.ttl())
            .finish()
    }
}

#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use super::*;
    use crate::segments::state::State;
    use core::num::NonZeroU32;
    use loom::sync::atomic::AtomicU32 as LoomAtomicU32;
    use loom::sync::Arc;
    use loom::thread;

    // NOTE on what these models can and cannot verify: loom reports the
    // store-buffering outcome even for pure-SeqCst litmus tests (its
    // modeling lacks the SC global total order). That has two
    // consequences here. First, the SeqCst-vs-AcqRel distinction on the
    // Dekker-paired transitions is not checkable. Second, the halves of
    // the protocol invariants that DEPEND on the SC total order — "a
    // committed drain is never observed by a pinned reader" and "a
    // condemned segment never leaks" — show false violations under
    // loom, because it explores the store-buffering interleaving that
    // SeqCst forbids on real hardware. The models therefore assert only
    // the SC-independent halves (CAS uniqueness -> no double-free;
    // revert consistency; election uniqueness; bounded reservation
    // grants); the SC-dependent halves are pinned by the single-threaded
    // behavioral tests, where store buffering cannot occur.

    // Two readers race a drain that mirrors the merge-source gate: take
    // Draining exclusivity via CAS, re-check the reader count, and
    // revert if a pin raced in. Only after the recheck passes is it
    // safe to move bytes ("commit"). Strong invariant: no reader ever
    // holds a pin while the drain has committed.
    #[test]
    fn loom_readers_vs_cas_gated_drain() {
        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = Some(3);
        builder.check(|| {
            let header = Arc::new(SegmentHeader::new(NonZeroU32::new(1).unwrap()));
            header.set_state(State::Sealed);
            let committed = Arc::new(LoomAtomicU32::new(0));

            let readers: Vec<_> = (0..2)
                .map(|_| {
                    let h = Arc::clone(&header);
                    let c = Arc::clone(&committed);
                    thread::spawn(move || {
                        if h.try_acquire_reader() == AcquireOutcome::Acquired {
                            // The strong invariant — a pinned reader
                            // never observes a committed drain — is the
                            // SC-total-order property loom cannot model
                            // (see module note); it is NOT asserted
                            // here. Record the observation instead so
                            // the model still exercises the code path.
                            let _ = c.load(Ordering::SeqCst);
                            h.release_reader();
                        }
                    })
                })
                .collect();

            let writer = {
                let h = Arc::clone(&header);
                let c = Arc::clone(&committed);
                thread::spawn(move || {
                    if h.cas_metadata(State::Sealed, State::Draining, None, None, Ordering::SeqCst)
                    {
                        if h.ref_count_seqcst() != 0 {
                            // a pin raced in: revert before touching bytes
                            assert!(h.cas_metadata(
                                State::Draining,
                                State::Sealed,
                                None,
                                None,
                                Ordering::AcqRel,
                            ));
                        } else {
                            c.store(1, Ordering::SeqCst);
                        }
                    }
                })
            };

            for r in readers {
                r.join().unwrap();
            }
            writer.join().unwrap();

            assert_eq!(header.ref_count(), 0);
        });
    }

    // The AwaitingRelease handoff: an evictor condemns a drained, pinned
    // segment (with the race-fix recheck) while the reader's guard drop
    // decrements and maybe reclaims. Exactly one side must free the
    // segment in every interleaving — no double-free, no leak.
    #[test]
    fn loom_awaiting_release_exactly_one_free() {
        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = Some(3);
        builder.check(|| {
            let header = Arc::new(SegmentHeader::new(NonZeroU32::new(1).unwrap()));
            // a drained segment with one outstanding pin
            header.set_state(State::Draining);
            header.ref_count.store(1, Ordering::SeqCst);
            // stand-in for the injector push (loom cannot model the
            // crossbeam Injector)
            let freed = Arc::new(LoomAtomicU32::new(0));

            let evictor = {
                let h = Arc::clone(&header);
                let f = Arc::clone(&freed);
                thread::spawn(move || {
                    // condemn (mirrors Segments::condemn)
                    assert!(h.cas_condemn());
                    // race fix: the pin may have dropped before the CAS
                    if h.ref_count_seqcst() == 0 && h.try_release_condemned() {
                        f.fetch_add(1, Ordering::SeqCst);
                    }
                })
            };

            let reader = {
                let h = Arc::clone(&header);
                let f = Arc::clone(&freed);
                thread::spawn(move || {
                    // mirrors SegmentGuard::drop
                    let prev = h.release_reader_for_guard();
                    if prev == 1 && h.try_release_condemned() {
                        f.fetch_add(1, Ordering::SeqCst);
                    }
                })
            };

            evictor.join().unwrap();
            reader.join().unwrap();

            // Exactly-one-free has two halves. "At most once" is pure
            // CAS uniqueness on try_release_condemned and holds in every
            // interleaving loom explores. "At least once" (no leak)
            // depends on the SeqCst total order of the decrement/CAS
            // Dekker pair, which loom cannot model (see module note) —
            // it reports the store-buffering leak that real SeqCst
            // hardware forbids, so it is not asserted here; the
            // guard_drop_frees_segment behavioral test pins it.
            let freed = freed.load(Ordering::SeqCst);
            assert!(freed <= 1, "condemned segment freed more than once");
            if freed == 1 {
                assert_eq!(header.state(), State::Free);
            }
            assert_eq!(header.ref_count(), 0);
        });
    }

    // Acquisition must fail in every interleaving for non-readable
    // states, leaving no pin behind — AwaitingRelease included, so that
    // no pin can arrive after a segment is condemned.
    #[test]
    fn loom_acquire_by_state() {
        loom::model(|| {
            let header = Arc::new(SegmentHeader::new(NonZeroU32::new(1).unwrap()));

            for (state, acquirable) in [
                (State::Free, false),
                (State::Reserved, false),
                (State::Draining, false),
                (State::AwaitingRelease, false),
            ] {
                header.set_state(state);
                let h = Arc::clone(&header);
                let reader = thread::spawn(move || {
                    if h.try_acquire_reader() == AcquireOutcome::Acquired {
                        h.release_reader();
                        true
                    } else {
                        false
                    }
                });
                assert_eq!(reader.join().unwrap(), acquirable, "state {state:?}");
                assert_eq!(header.ref_count(), 0);
            }
        });
    }

    // The chain-extension election: two expanders race to seal the same
    // Live tail with different successors. The one-CAS seal admits
    // exactly one winner, and the link matches the winner — this is the
    // mutual exclusion the lock-free try_expand relies on. Pure CAS
    // uniqueness on one word: SC-independent, fully within loom's power
    // (see module note).
    #[test]
    fn loom_seal_election_single_winner() {
        loom::model(|| {
            let tail = Arc::new(SegmentHeader::new(NonZeroU32::new(1).unwrap()));
            tail.set_state(State::Live);

            let handles: Vec<_> = [2u32, 3u32]
                .into_iter()
                .map(|succ| {
                    let t = Arc::clone(&tail);
                    thread::spawn(move || {
                        let won = t.cas_metadata(
                            State::Live,
                            State::Sealed,
                            Some(NonZeroU32::new(succ)),
                            None,
                            Ordering::AcqRel,
                        );
                        if !won {
                            // Loser coherence: the winner's Sealed
                            // transition is visible immediately after the
                            // failed CAS — the fact the loser's
                            // state-recheck in try_expand depends on.
                            // Pure coherence, not the SC total order.
                            assert_eq!(t.state(), State::Sealed);
                        }
                        won
                    })
                })
                .collect();
            let wins: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();

            assert_eq!(wins.iter().filter(|w| **w).count(), 1, "exactly one seal");
            assert_eq!(tail.state(), State::Sealed);
            let expected_succ = if wins[0] { 2 } else { 3 };
            assert_eq!(tail.next_seg().unwrap().get(), expected_succ);
        });
    }

    // Writers race a drain claim: the writer's fetch_add+recheck SeqCst pair
    // mirrors try_acquire_reader; the claimer's CAS+active_writers() load
    // mirrors the drain CAS + ref_count_seqcst() load in the model above.
    // Same limitation applies (see module NOTE): the message-passing
    // property this pair exists for — a claimer that has committed the
    // drain is never raced by a writer still mid-pin — is SC-dependent and
    // NOT asserted here; the claimer's single post-CAS observation of
    // `active_writers()` is recorded, not asserted on. Unlike the reader
    // model's `ref_count_seqcst()` check — which IS branched into a
    // revert-on-race — production's writer-drain claim does not revert; it
    // spins until writers drain (`claim_for_drain` / `drain_chain`), so a
    // single discarded observation is the faithful message-passing shape
    // here. (`committed` accordingly means only "the claim CAS won", not
    // "drain confirmed safe" as in the reader model — harmless since it is
    // never asserted on.) What IS asserted is the SC-independent invariant:
    // every `try_pin_writer` call, whether it succeeds or backs out, is
    // exactly balanced by a `release_writer` in every interleaving loom
    // explores — no leaked or underflowed pin count. The SeqCst mutual
    // exclusion itself is pinned by `concurrent_reservers_vs_drain_same_bucket`
    // (stress test) and `claim_for_drain_waits_for_active_writers`
    // (deterministic), not by loom.
    #[test]
    fn loom_writers_vs_cas_gated_drain() {
        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = Some(3);
        builder.check(|| {
            let header = Arc::new(SegmentHeader::new(NonZeroU32::new(1).unwrap()));
            header.set_state(State::Live);
            let committed = Arc::new(LoomAtomicU32::new(0));

            let writers: Vec<_> = (0..2)
                .map(|_| {
                    let h = Arc::clone(&header);
                    let c = Arc::clone(&committed);
                    thread::spawn(move || {
                        if h.try_pin_writer() {
                            // SC-dependent property recorded, not asserted
                            // (see comment above and module NOTE).
                            let _ = c.load(Ordering::SeqCst);
                            h.release_writer();
                        }
                    })
                })
                .collect();

            let claimer = {
                let h = Arc::clone(&header);
                let c = Arc::clone(&committed);
                thread::spawn(move || {
                    if h.cas_metadata(State::Live, State::Draining, None, None, Ordering::SeqCst) {
                        // Single post-CAS observation, mirroring the reader
                        // model's one recheck rather than a spin-wait: not
                        // asserted on (SC-dependent), just exercised.
                        let _ = h.active_writers();
                        c.store(1, Ordering::SeqCst);
                    }
                })
            };

            for w in writers {
                w.join().unwrap();
            }
            claimer.join().unwrap();

            // SC-independent: no interleaving leaves a pin dangling or
            // double-releases one.
            assert_eq!(header.active_writers(), 0);
        });
    }

    // Removers race a drain claim: the remover's fetch_add+recheck SeqCst
    // pair mirrors try_acquire_reader (and try_pin_writer above); the
    // claimer's CAS+active_removers() load mirrors the drain CAS +
    // ref_count_seqcst()/active_writers() load in the models above. Header
    // starts Sealed — the common interior-item-removal case a replace/
    // delete pins (see `state_is_removable`). Same limitation applies (see
    // module NOTE): the message-passing property this pair exists for — a
    // claimer that has committed the drain is never raced by a remover
    // still mid-pin — is SC-dependent and NOT asserted here; the claimer's
    // single post-CAS observation of `active_removers()` is recorded, not
    // asserted on. Like the writer model (and unlike the reader model's
    // revert-on-race), production's remover-vs-drain claim does not revert
    // on the claimer's side; the remover's own recheck inside
    // `try_pin_remover` is what backs out, so a single discarded
    // observation on the claimer side is the faithful message-passing
    // shape here. (`committed` accordingly means only "the claim CAS won",
    // not "drain confirmed safe" — harmless since it is never asserted
    // on.) What IS asserted is the SC-independent invariant: every
    // `try_pin_remover` call, whether it succeeds or backs out, is exactly
    // balanced by a `release_remover` in every interleaving loom explores
    // — no leaked or underflowed pin count. The SeqCst mutual exclusion
    // itself is pinned by the stress tests exercising concurrent
    // replace/delete against an evictor draining the same segment, not by
    // loom.
    #[test]
    fn loom_removers_vs_cas_gated_drain() {
        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = Some(3);
        builder.check(|| {
            let header = Arc::new(SegmentHeader::new(NonZeroU32::new(1).unwrap()));
            header.set_state(State::Sealed);
            let committed = Arc::new(LoomAtomicU32::new(0));

            let removers: Vec<_> = (0..2)
                .map(|_| {
                    let h = Arc::clone(&header);
                    let c = Arc::clone(&committed);
                    thread::spawn(move || {
                        if h.try_pin_remover() {
                            // SC-dependent property recorded, not asserted
                            // (see comment above and module NOTE).
                            let _ = c.load(Ordering::SeqCst);
                            h.release_remover();
                        }
                    })
                })
                .collect();

            let claimer = {
                let h = Arc::clone(&header);
                let c = Arc::clone(&committed);
                thread::spawn(move || {
                    if h.cas_metadata(State::Sealed, State::Draining, None, None, Ordering::SeqCst)
                    {
                        // Single post-CAS observation, mirroring the reader
                        // model's one recheck rather than a spin-wait: not
                        // asserted on (SC-dependent), just exercised.
                        let _ = h.active_removers();
                        c.store(1, Ordering::SeqCst);
                    }
                })
            };

            for r in removers {
                r.join().unwrap();
            }
            claimer.join().unwrap();

            // SC-independent: no interleaving leaves a pin dangling or
            // double-releases one.
            assert_eq!(header.active_removers(), 0);
        });
    }

    // Two writers CAS-reserve space from the same segment: both fit, so
    // the final state is fully determined regardless of interleaving —
    // the grants are exactly {base, base+24} and the offset lands at
    // base+48. Pure CAS-disjointness + bound: SC-independent.
    #[test]
    fn loom_reserve_space_disjoint_bounded() {
        loom::model(|| {
            let h = Arc::new(SegmentHeader::new(NonZeroU32::new(1).unwrap()));
            let base = h.write_offset(); // 0, or 8 with `integrity`
            let cap = base + 64;

            let handles: Vec<_> = [24i32, 24i32]
                .into_iter()
                .map(|size| {
                    let h = Arc::clone(&h);
                    thread::spawn(move || h.try_reserve_space(size, cap).map(|o| (o, size)))
                })
                .collect();
            let mut grants: Vec<(i32, i32)> = handles
                .into_iter()
                .filter_map(|h| h.join().unwrap())
                .collect();
            grants.sort_unstable();

            // both fit; the grant set and final offset are exact.
            assert_eq!(grants, vec![(base, 24), (base + 24, 24)]);
            assert_eq!(h.write_offset(), base + 48);
        });
    }

    // Two writers race to reserve more than half the capacity each: only
    // one 40-byte grant can fit in 64 bytes (40 + 40 > 64). This is the
    // capacity-rejection property under contention that motivated a
    // bounded CAS over a raw fetch_add. Pure CAS uniqueness + bound:
    // SC-independent.
    #[test]
    fn loom_reserve_space_bounded_under_contention() {
        loom::model(|| {
            let h = Arc::new(SegmentHeader::new(NonZeroU32::new(1).unwrap()));
            let base = h.write_offset(); // 0, or 8 with `integrity`
            let cap = base + 64;

            let handles: Vec<_> = [40i32, 40i32]
                .into_iter()
                .map(|size| {
                    let h = Arc::clone(&h);
                    thread::spawn(move || h.try_reserve_space(size, cap))
                })
                .collect();
            let grants: Vec<i32> = handles
                .into_iter()
                .filter_map(|h| h.join().unwrap())
                .collect();

            assert_eq!(
                grants.len(),
                1,
                "only one 40-byte reservation can fit in 64 bytes"
            );
            assert_eq!(grants[0], base);
            assert_eq!(h.write_offset(), base + 40);
            assert!(h.write_offset() <= cap);
        });
    }
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use super::*;

    // The initial write_offset is 0, or 8 with the `integrity` feature
    // (magic bytes). Tests use relative math so they pass either way.
    fn initial_offset() -> i32 {
        if cfg!(feature = "integrity") {
            std::mem::size_of::<u64>() as i32
        } else {
            0
        }
    }

    #[test]
    fn reserve_space_grants_sequential_offsets() {
        let h = SegmentHeader::new(NonZeroU32::new(1).unwrap());
        let base = initial_offset();
        assert_eq!(h.try_reserve_space(24, base + 128), Some(base));
        assert_eq!(h.try_reserve_space(40, base + 128), Some(base + 24));
        assert_eq!(h.write_offset(), base + 64);
    }

    #[test]
    fn reserve_space_exact_fit_boundary() {
        let h = SegmentHeader::new(NonZeroU32::new(1).unwrap());
        let base = initial_offset();
        // fills the segment exactly
        assert_eq!(h.try_reserve_space(64, base + 64), Some(base));
        assert_eq!(h.write_offset(), base + 64);
        // nothing further fits, offset must not move
        assert_eq!(h.try_reserve_space(8, base + 64), None);
        assert_eq!(h.write_offset(), base + 64);
    }

    #[test]
    fn reserve_space_rejects_oversized() {
        let h = SegmentHeader::new(NonZeroU32::new(1).unwrap());
        let base = initial_offset();
        assert_eq!(h.try_reserve_space(129, base + 128), None);
        // a failed reservation must not advance the offset
        assert_eq!(h.write_offset(), base);
        // smaller items still fit after a large one failed
        assert_eq!(h.try_reserve_space(64, base + 128), Some(base));
    }

    #[test]
    fn reserve_space_offset_overflow_fails() {
        let h = SegmentHeader::new(NonZeroU32::new(1).unwrap());
        let base = initial_offset();
        // advance the offset so current > 0, then request a size that
        // overflows current + size in i32
        assert_eq!(h.try_reserve_space(8, base + 64), Some(base));
        assert_eq!(h.try_reserve_space(i32::MAX, i32::MAX), None);
        // a failed reservation must not advance the offset
        assert_eq!(h.write_offset(), base + 8);
    }

    #[test]
    fn writer_pin_two_phase() {
        use crate::segments::state::{Metadata, State};
        let h = SegmentHeader::new(NonZeroU32::new(1).unwrap());

        // A fresh header is Free — not writable, so the pin is refused and the
        // counter is left untouched (the post-increment backout ran).
        assert!(!h.try_pin_writer());
        assert_eq!(h.active_writers(), 0);

        // Make it Live: try_pin_writer now succeeds and bumps the counter.
        h.store_metadata_for_test(Metadata {
            next: None,
            prev: None,
            state: State::Live,
            tag: 0,
        });
        assert!(h.try_pin_writer());
        assert_eq!(h.active_writers(), 1);

        // A second concurrent writer also pins.
        assert!(h.try_pin_writer());
        assert_eq!(h.active_writers(), 2);

        // Releasing brings it back down.
        h.release_writer();
        h.release_writer();
        assert_eq!(h.active_writers(), 0);

        // Once Sealed the segment is no longer writable — pin refused, counter untouched.
        h.store_metadata_for_test(Metadata {
            next: None,
            prev: None,
            state: State::Sealed,
            tag: 0,
        });
        assert!(!h.try_pin_writer());
        assert_eq!(h.active_writers(), 0);
    }

    #[test]
    fn remover_pin_two_phase() {
        use crate::segments::state::{Metadata, State};
        let h = SegmentHeader::new(NonZeroU32::new(1).unwrap());
        // Free is not removable.
        assert!(!h.try_pin_remover());
        assert_eq!(h.active_removers(), 0);
        // Sealed IS removable (interior items can be replaced/deleted).
        h.store_metadata_for_test(Metadata {
            next: None,
            prev: None,
            state: State::Sealed,
            tag: 0,
        });
        assert!(h.try_pin_remover());
        assert_eq!(h.active_removers(), 1);
        // Live is removable too (tail items).
        h.store_metadata_for_test(Metadata {
            next: None,
            prev: None,
            state: State::Live,
            tag: 0,
        });
        assert!(h.try_pin_remover());
        assert_eq!(h.active_removers(), 2);
        h.release_remover();
        h.release_remover();
        assert_eq!(h.active_removers(), 0);
        // Draining is NOT removable — a remover must bail so it can't decrement a
        // segment a drain is reclaiming.
        h.store_metadata_for_test(Metadata {
            next: None,
            prev: None,
            state: State::Draining,
            tag: 0,
        });
        assert!(!h.try_pin_remover());
        assert_eq!(h.active_removers(), 0);
    }

    // ISSUE #64, DEFECT 2 — the tag's transport.
    //
    // `cas_condemn` stamps a lifetime tag into the spare high byte of the
    // metadata word, and that tag is what makes the release CAS token
    // unique to one use of a segment. `update_links` is a
    // read-modify-write through `Metadata` that IS reachable against an
    // AwaitingRelease segment — `condemn` and `recycle` both splice
    // neighbours after their own transition, so a neighbour being
    // condemned concurrently gets its links patched while it carries a
    // tag. If that round-trip drops the tag, the condemned word falls
    // back to the constant every segment shares and defect 2 is back.
    #[test]
    fn update_links_preserves_the_condemn_tag() {
        use crate::segments::state::{Metadata, State};
        let h = SegmentHeader::new(NonZeroU32::new(1).unwrap());
        h.store_metadata_for_test(Metadata {
            next: NonZeroU32::new(5),
            prev: NonZeroU32::new(6),
            state: State::AwaitingRelease,
            tag: 0xAB,
        });

        h.update_links(Some(NonZeroU32::new(7)), Some(None));

        let meta = h.metadata(Ordering::Acquire);
        assert_eq!(
            meta.tag, 0xAB,
            "splicing a neighbour must not erase the lifetime tag"
        );
        // The links it was called for still landed, and the state is untouched.
        assert_eq!(meta.next, NonZeroU32::new(7));
        assert_eq!(meta.prev, None);
        assert_eq!(meta.state, State::AwaitingRelease);
    }

    // ISSUE #64, DEFECT 2 — deterministic reproduction.
    //
    // `try_release_condemned` loads the metadata word and then CASes it to
    // Free. Without the generation stamp that word is the constant
    // `{AwaitingRelease, None, None}` in every use of every segment, so a
    // thread stalled between its load and its CAS can win the transition
    // against a LATER incarnation of the same segment — freeing it out
    // from under the readers that incarnation still has, and stealing the
    // handoff its real last reader owes.
    //
    // The stall is driven by hand: the token is captured where the stalled
    // thread's load happens, a full free/recycle/condemn cycle runs, and
    // then the CAS `try_release_condemned` would perform is issued against
    // that stale token. No scheduler involved.
    #[test]
    fn a_stale_release_token_cannot_free_a_later_incarnation() {
        use crate::segments::state::{Metadata, State};

        // Exactly the CAS in `try_release_condemned`, but on a token the
        // caller loaded earlier — i.e. a thread descheduled between that
        // function's own load and its own compare-exchange.
        fn stale_release(h: &SegmentHeader, token: u64) -> bool {
            let free = Metadata {
                next: None,
                prev: None,
                state: State::Free,
                tag: 0,
            };
            h.metadata
                .compare_exchange(token, free.pack(), Ordering::SeqCst, Ordering::Acquire)
                .is_ok()
        }

        let h = SegmentHeader::new(NonZeroU32::new(1).unwrap());

        // --- Incarnation N: reserved, filled, drained, then condemned
        // because a reader still pinned it.
        assert!(h.try_reserve());
        h.set_state(State::Draining);
        assert!(h.cas_condemn());
        assert_eq!(h.state(), State::AwaitingRelease);

        // N's last reader enters `try_release_condemned` and loads the
        // word here — then stalls, for a very long time.
        let stale_token = h.metadata.load(Ordering::SeqCst);

        // --- Meanwhile N is released by someone else (the condemner's
        // race-fix recheck) and returns to the pool.
        assert!(h.try_release_condemned());
        assert_eq!(h.state(), State::Free);

        // --- Incarnation N+1: the same segment is reserved again (which
        // bumps the generation), filled, drained, and condemned again —
        // this time with a live reader still pinning and reading it.
        assert!(h.try_reserve());
        h.set_state(State::Draining);
        h.ref_count.fetch_add(1, Ordering::SeqCst);
        assert!(h.cas_condemn());
        assert_eq!(h.state(), State::AwaitingRelease);

        // --- The stalled thread from N finally runs its CAS.
        assert!(
            !stale_release(&h, stale_token),
            "a release token from an earlier incarnation must not win the \
             AwaitingRelease -> Free transition against a later one"
        );
        assert_eq!(
            h.state(),
            State::AwaitingRelease,
            "the later incarnation must stay condemned, not be freed under \
             its live reader"
        );
        assert_eq!(
            h.ref_count(),
            1,
            "the live reader is still reading the segment bytes"
        );

        // The handoff still belongs to N+1's real last reader, and works.
        assert_eq!(h.release_reader_for_guard(), 1);
        assert!(h.try_release_condemned());
        assert_eq!(h.state(), State::Free);
    }
}
