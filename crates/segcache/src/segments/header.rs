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
//! │   PREV SEG   │   NEXT SEG   │  CREATE AT   │  MERGE AT    │
//! │  AtomicU32   │  AtomicU32   │ AtomicInstant│ AtomicInstant│
//! │    32 bit    │    32 bit    │    32 bit    │    32 bit    │
//! ├──────────────┼──┬──┬──────┬─┴────────────┬─┴──────────────┤
//! │     TTL      │ST│PL│ GEN  │  REF COUNT   │    PADDING     │
//! │  AtomicU32   │8b│8b│ 16b  │  AtomicU32   │     32 bit     │
//! ├──────────────┴──┴──┴──────┴──────────────┴────────────────┤
//! │                        PADDING                            │
//! │                       128 bit                             │
//! └───────────────────────────────────────────────────────────┘
//!
//! ST = SegmentState (AtomicU8)   PL = SegmentPool (AtomicU8)
//! GEN = generation (AtomicU16)
//! Total: 512 bits = 64 bytes = 1 cache line
//! ```

use crate::sync::{AtomicI32, AtomicU16, AtomicU32, AtomicU8, Ordering};
use clocksource::coarse::{AtomicInstant, Duration, Instant};
use core::num::NonZeroU32;

/// Segment lifecycle state.
///
/// Replaces the old `accessible`/`evictable` boolean pair with a single
/// enum that can be extended to more states (e.g. crucible's 9-state
/// machine) when concurrent access is implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum SegmentState {
    /// On the free queue, not accessible or evictable.
    Free = 0,
    /// Tail of a TTL bucket chain, accepting writes, not yet evictable.
    Filling = 1,
    /// Accessible and evictable (sealed for writes, eligible for eviction).
    Active = 2,
    /// Being cleared or evicted, not accessible.
    Draining = 3,
}

impl SegmentState {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Free,
            1 => Self::Filling,
            2 => Self::Active,
            3 => Self::Draining,
            _ => Self::Free,
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
/// 16       4    prev_seg      (AtomicU32, 0=None)
/// 20       4    next_seg      (AtomicU32, 0=None)
/// 24       4    create_at     (AtomicInstant)
/// 28       4    merge_at      (AtomicInstant)
/// 32       4    ttl           (AtomicU32, seconds)
/// 36       1    state         (AtomicU8, SegmentState)
/// 37       1    pool          (AtomicU8, SegmentPool)
/// 38       2    generation    (AtomicU16, bumped on recycle)
/// 40       4    ref_count     (AtomicU32, active readers)
/// 44      20    _pad
/// ```
#[repr(C, align(64))]
pub(crate) struct SegmentHeader {
    id: u32,
    write_offset: AtomicI32,
    live_bytes: AtomicI32,
    live_items: AtomicI32,
    prev_seg: AtomicU32,
    next_seg: AtomicU32,
    create_at: AtomicInstant,
    merge_at: AtomicInstant,
    ttl: AtomicU32,
    state: AtomicU8,
    pool: AtomicU8,
    generation: AtomicU16,
    ref_count: AtomicU32,
    _pad: [u8; 20],
}

// Loom atomics are larger than std atomics, so skip size check under loom.
#[cfg(not(feature = "loom"))]
const _: () = assert!(std::mem::size_of::<SegmentHeader>() == 64);
#[cfg(not(feature = "loom"))]
const _: () = assert!(std::mem::align_of::<SegmentHeader>() == 64);

impl SegmentHeader {
    /// Create a new header for the given segment id.
    pub fn new(id: NonZeroU32) -> Self {
        Self {
            id: id.get(),
            write_offset: AtomicI32::new(0),
            live_bytes: AtomicI32::new(0),
            live_items: AtomicI32::new(0),
            prev_seg: AtomicU32::new(0),
            next_seg: AtomicU32::new(0),
            create_at: AtomicInstant::new(Instant::default()),
            merge_at: AtomicInstant::new(Instant::default()),
            ttl: AtomicU32::new(0),
            state: AtomicU8::new(SegmentState::Free as u8),
            pool: AtomicU8::new(SegmentPool::Main as u8),
            generation: AtomicU16::new(0),
            ref_count: AtomicU32::new(0),
            _pad: [0; 20],
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
        self.state
            .store(SegmentState::Free as u8, Ordering::Relaxed);
    }

    /// Reset the header when returning to the free queue.
    /// When the `magic` feature is enabled, preserves the magic byte offset.
    ///
    /// Bumps the generation counter so that CAS tokens issued against the
    /// previous use of this segment can never match items written after it
    /// is recycled.
    pub fn reset(&self) {
        let initial_offset = if cfg!(feature = "integrity") {
            std::mem::size_of::<u64>() as i32
        } else {
            0
        };
        self.write_offset.store(initial_offset, Ordering::Relaxed);
        self.live_bytes.store(initial_offset, Ordering::Relaxed);
        self.live_items.store(0, Ordering::Relaxed);
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Get the generation counter. Incremented each time the segment is
    /// returned to the free queue; wraps at `u16::MAX`.
    #[inline]
    pub fn generation(&self) -> u16 {
        self.generation.load(Ordering::Relaxed)
    }

    // -- Reader pinning --

    /// Try to pin this segment for reading, using a two-phase protocol:
    /// check the state, increment the reader count, then re-check the
    /// state. If the segment became inaccessible between the first check
    /// and the increment, back out and fail.
    ///
    /// While the reader count is non-zero the segment must not be
    /// recycled, merged, or compacted. Every successful acquire must be
    /// paired with exactly one [`Self::release_reader`].
    ///
    /// Uses explicit `Acquire` loads on the state (rather than the
    /// `Relaxed` loads in [`Self::accessible`]) so the protocol is sound
    /// once concurrent writers exist.
    #[inline]
    pub fn try_acquire_reader(&self) -> bool {
        let readable = |s: u8| {
            matches!(
                SegmentState::from_u8(s),
                SegmentState::Filling | SegmentState::Active
            )
        };

        if !readable(self.state.load(Ordering::Acquire)) {
            return false;
        }

        self.ref_count.fetch_add(1, Ordering::Acquire);

        // Re-check after the increment: a writer that observed
        // ref_count == 0 may have transitioned the state concurrently.
        if !readable(self.state.load(Ordering::Acquire)) {
            self.ref_count.fetch_sub(1, Ordering::Release);
            return false;
        }

        true
    }

    /// Release a reader pin taken with [`Self::try_acquire_reader`].
    #[inline]
    pub fn release_reader(&self) {
        let prev = self.ref_count.fetch_sub(1, Ordering::Release);
        debug_assert!(prev > 0, "release_reader without matching acquire");
    }

    /// Number of active readers pinning this segment.
    #[inline]
    pub fn ref_count(&self) -> u32 {
        self.ref_count.load(Ordering::Acquire)
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

    /// Atomically add to the write offset, returning the previous value.
    /// The returned value is the offset where the caller can write.
    #[inline]
    pub fn fetch_add_write_offset(&self, size: i32) -> i32 {
        self.write_offset.fetch_add(size, Ordering::Relaxed)
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

    // -- Chain pointers --

    #[inline]
    pub fn prev_seg(&self) -> Option<NonZeroU32> {
        NonZeroU32::new(self.prev_seg.load(Ordering::Relaxed))
    }

    #[inline]
    pub fn set_prev_seg(&self, id: Option<NonZeroU32>) {
        self.prev_seg
            .store(id.map_or(0, |v| v.get()), Ordering::Relaxed);
    }

    #[inline]
    pub fn next_seg(&self) -> Option<NonZeroU32> {
        NonZeroU32::new(self.next_seg.load(Ordering::Relaxed))
    }

    #[inline]
    pub fn set_next_seg(&self, id: Option<NonZeroU32>) {
        self.next_seg
            .store(id.map_or(0, |v| v.get()), Ordering::Relaxed);
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
    pub fn state(&self) -> SegmentState {
        SegmentState::from_u8(self.state.load(Ordering::Relaxed))
    }

    #[inline]
    pub fn set_state(&self, state: SegmentState) {
        self.state.store(state as u8, Ordering::Relaxed);
    }

    /// Returns true if the segment is accessible (Filling or Active).
    #[inline]
    pub fn accessible(&self) -> bool {
        matches!(self.state(), SegmentState::Filling | SegmentState::Active)
    }

    /// Set the segment as accessible. Maps to `Filling` if not already
    /// `Active`, preserving the evictable distinction.
    #[inline]
    pub fn set_accessible(&self, accessible: bool) {
        if accessible {
            if self.state() == SegmentState::Free || self.state() == SegmentState::Draining {
                self.set_state(SegmentState::Filling);
            }
        } else if self.state() != SegmentState::Free {
            self.set_state(SegmentState::Draining);
        }
    }

    /// Returns true if the segment is evictable (Active state).
    #[inline]
    pub fn evictable(&self) -> bool {
        self.state() == SegmentState::Active
    }

    /// Set the segment as evictable. Transitions to Active when setting
    /// true, ensuring the segment is at least Filling first. This makes
    /// the call order-independent with `set_accessible`.
    #[inline]
    pub fn set_evictable(&self, evictable: bool) {
        if evictable {
            let state = self.state();
            if state == SegmentState::Free || state == SegmentState::Draining {
                self.set_state(SegmentState::Filling);
            }
            if self.state() == SegmentState::Filling {
                self.set_state(SegmentState::Active);
            }
        } else if self.state() == SegmentState::Active {
            self.set_state(SegmentState::Filling);
        }
    }

    /// Check if the segment can actually be evicted.
    /// Requires: Active state, has a next segment (not the current write
    /// target), and no readers pinning it.
    #[inline]
    pub fn can_evict(&self) -> bool {
        self.evictable() && self.next_seg().is_some() && self.ref_count() == 0
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
        f.debug_struct("SegmentHeader")
            .field("id", &self.id)
            .field("write_offset", &self.write_offset())
            .field("live_bytes", &self.live_bytes())
            .field("live_items", &self.live_items())
            .field("state", &self.state())
            .field("pool", &self.pool())
            .field("prev_seg", &self.prev_seg())
            .field("next_seg", &self.next_seg())
            .field("ttl", &self.ttl())
            .finish()
    }
}

#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use super::*;
    use core::num::NonZeroU32;
    use loom::sync::Arc;
    use loom::thread;

    // Two readers race a writer that mirrors the production eviction gate
    // (check ref_count == 0 and evictable, then store Draining). The store
    // is not a CAS because production writers run under `&mut Segments`
    // exclusivity today; there is a benign interleaving where a reader
    // passes its re-check just before the writer's store, which is only
    // safe because of that exclusivity. The AwaitingRelease state-machine
    // port must replace the plain store with a CAS transition
    // (Active -> Draining only if ref_count == 0) before real concurrent
    // writers exist, at which point this model gains strong assertions.
    #[test]
    fn loom_two_readers_one_writer_refcount() {
        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = Some(3);
        builder.check(|| {
            let header = Arc::new(SegmentHeader::new(NonZeroU32::new(1).unwrap()));
            header.set_state(SegmentState::Active);

            let readers: Vec<_> = (0..2)
                .map(|_| {
                    let h = Arc::clone(&header);
                    thread::spawn(move || {
                        if h.try_acquire_reader() {
                            // simulate a read while pinned
                            let _ = h.state();
                            h.release_reader();
                        }
                    })
                })
                .collect();

            let writer = {
                let h = Arc::clone(&header);
                thread::spawn(move || {
                    // mirrors can_evict() + drain in production
                    if h.ref_count() == 0 && h.evictable() {
                        h.set_state(SegmentState::Draining);
                    }
                })
            };

            for r in readers {
                r.join().unwrap();
            }
            writer.join().unwrap();

            // all pins released; state is one of the two valid outcomes
            assert_eq!(header.ref_count(), 0);
            assert!(matches!(
                header.state(),
                SegmentState::Active | SegmentState::Draining
            ));
        });
    }

    // Once a segment is Draining, acquisition must fail in every
    // interleaving, and a failed acquire must leave no pin behind.
    #[test]
    fn loom_acquire_fails_after_drain() {
        loom::model(|| {
            let header = Arc::new(SegmentHeader::new(NonZeroU32::new(1).unwrap()));
            header.set_state(SegmentState::Draining);

            let h = Arc::clone(&header);
            let reader = thread::spawn(move || h.try_acquire_reader());

            assert!(!reader.join().unwrap());
            assert_eq!(header.ref_count(), 0);
        });
    }
}
