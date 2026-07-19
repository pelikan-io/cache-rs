//! Segment state machine and packed metadata.
//!
//! Ported from crucible's `cache/core/src/state.rs` with one deviation:
//! chain pointers use cache-rs's 1-indexed `Option<NonZeroU32>` convention
//! (0 = none) rather than crucible's `INVALID_SEGMENT_ID = 0xFF_FFFF`
//! sentinel. Segment ids are asserted `< 2^24` at construction, so they
//! always fit the 24-bit packed fields.

use core::num::NonZeroU32;

/// State of a segment in its lifecycle.
///
/// # State Semantics
///
/// - **Free**: In the free queue, available for allocation
/// - **Reserved**: Allocated for use, being prepared for chain insertion
/// - **Linking**: Being added to a chain (next/prev being set)
/// - **Live**: Active tail segment accepting writes and reads
/// - **Sealed**: No more writes accepted, but data readable and chain
///   stable; the only evictable state
/// - **Relinking**: DECLARED, UNUSED — the copy-to-spare merge rework
///   (item 5b) and the s3fifo evict/admission paths reach a reader-safe
///   chain-relink by copying survivors into a fresh Sealed segment instead
///   of updating pointers on a live one in place, making an in-place
///   relinking state unnecessary under today's serialized eviction. Kept
///   reserved for a future concurrent-eviction design.
/// - **Draining**: Being processed (eviction/expiration/clear). Exclusive:
///   exactly one thread holds a segment in Draining. New reads rejected.
/// - **Locked**: DECLARED, UNUSED — for the same reason as `Relinking`,
///   drain-safe merge never needs to lock out all access to a segment
///   being cleared (candidates are drained via the existing
///   Sealed→Draining→AwaitingRelease path, not an exclusive in-place
///   clear). Kept reserved for a future concurrent-eviction design.
/// - **AwaitingRelease**: Condemned — removed from its chain and from the
///   hashtable; data remains valid for in-flight pinned readers. The last
///   reader's guard drop transitions it to Free and returns it to the
///   free queue.
///
/// # State Transition Diagram (as used by this crate)
///
/// ```text
///        +--------------->  Free  <-------------------------------+
///        |                   | try_reserve (generation bump)      |
///        |                   v                                    |
///  try_release           Reserved                                 |
///        |                   | link into chain (prev set)         |
///        +---------------  Linking                                |
///                            | publish                            |
///                            v                                    |
///        +---------------> Live  (bucket tail: writable)          |
///        |                   | sealed by the appender, in the     |
///        |                   | same CAS that sets `next`          |
///        |                   v                                    |
///        | (s3fifo        Sealed  (readable, evictable)           |
///        |  head-insert:      | begin drain (SeqCst)              |
///        |  Linking ->        v                                   |
///        |  Sealed)       Draining ---- ref_count == 0 -----------+
///        |                 |     ^  \                        (-> Free)
///        +--- revert ------+     |   \ ref_count > 0
///            (merge source        \   v
///             found pinned)        AwaitingRelease
///                                    | last reader guard drop
///                                    +------------------> Free
/// ```
///
/// Live -> Draining also exists for draining the bucket tail during
/// `clear()`/`expire()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum State {
    Free = 0,
    Reserved = 1,
    Linking = 2,
    Live = 3,
    Sealed = 4,
    Relinking = 5,
    Draining = 6,
    Locked = 7,
    AwaitingRelease = 8,
}

impl State {
    /// Convert from raw u8 value.
    ///
    /// # Panics
    /// Panics if the value is not a valid state (0-8). This replaces the
    /// old silent map-to-Free fallback, which masked corruption.
    #[inline]
    pub fn from_u8(value: u8) -> Self {
        match value {
            0 => State::Free,
            1 => State::Reserved,
            2 => State::Linking,
            3 => State::Live,
            4 => State::Sealed,
            5 => State::Relinking,
            6 => State::Draining,
            7 => State::Locked,
            8 => State::AwaitingRelease,
            _ => panic!("invalid segment state value: {value}"),
        }
    }

    /// Check if the segment is readable (allows get operations).
    ///
    /// Note: AwaitingRelease is readable so in-flight pinned readers can
    /// complete; new reads cannot arrive because the hashtable is fully
    /// drained before a segment is condemned.
    #[inline]
    pub fn is_readable(self) -> bool {
        matches!(
            self,
            State::Live | State::Sealed | State::Relinking | State::AwaitingRelease
        )
    }

    /// Check if the segment is writable (allows append operations).
    #[inline]
    pub fn is_writable(self) -> bool {
        matches!(self, State::Live)
    }

    /// Check if the segment can be evicted.
    #[inline]
    pub fn is_evictable(self) -> bool {
        matches!(self, State::Sealed)
    }
}

/// Unpacked view of a segment's metadata word.
///
/// The packed layout in the `AtomicU64`:
///
/// ```text
/// bits 63..56  unused      (8)
/// bits 55..48  state       (8)
/// bits 47..24  prev        (24)  0 = none
/// bits 23..0   next        (24)  0 = none
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Metadata {
    pub next: Option<NonZeroU32>,
    pub prev: Option<NonZeroU32>,
    pub state: State,
}

impl Metadata {
    const LINK_MASK: u64 = 0xFF_FFFF;

    /// Create metadata for a fresh, unlinked segment.
    pub fn new_free() -> Self {
        Self {
            next: None,
            prev: None,
            state: State::Free,
        }
    }

    /// Pack into the u64 representation.
    #[inline]
    pub fn pack(self) -> u64 {
        let next = self.next.map_or(0, NonZeroU32::get) as u64;
        let prev = self.prev.map_or(0, NonZeroU32::get) as u64;
        debug_assert!(next <= Self::LINK_MASK, "segment id exceeds 24 bits");
        debug_assert!(prev <= Self::LINK_MASK, "segment id exceeds 24 bits");
        ((self.state as u64) << 48) | (prev << 24) | next
    }

    /// Unpack from the u64 representation.
    #[inline]
    pub fn unpack(packed: u64) -> Self {
        Self {
            next: NonZeroU32::new((packed & Self::LINK_MASK) as u32),
            prev: NonZeroU32::new(((packed >> 24) & Self::LINK_MASK) as u32),
            state: State::from_u8(((packed >> 48) & 0xFF) as u8),
        }
    }
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use super::*;

    #[test]
    fn state_values_match_crucible() {
        assert_eq!(State::Free as u8, 0);
        assert_eq!(State::Reserved as u8, 1);
        assert_eq!(State::Linking as u8, 2);
        assert_eq!(State::Live as u8, 3);
        assert_eq!(State::Sealed as u8, 4);
        assert_eq!(State::Relinking as u8, 5);
        assert_eq!(State::Draining as u8, 6);
        assert_eq!(State::Locked as u8, 7);
        assert_eq!(State::AwaitingRelease as u8, 8);
    }

    #[test]
    fn state_roundtrip() {
        for v in 0..=8u8 {
            assert_eq!(State::from_u8(v) as u8, v);
        }
    }

    #[test]
    #[should_panic(expected = "invalid segment state value")]
    fn state_from_u8_invalid_panics() {
        let _ = State::from_u8(9);
    }

    #[test]
    fn predicates() {
        use State::*;
        for s in [Free, Reserved, Linking, Draining, Locked] {
            assert!(!s.is_readable(), "{s:?} must not be readable");
        }
        for s in [Live, Sealed, Relinking, AwaitingRelease] {
            assert!(s.is_readable(), "{s:?} must be readable");
        }
        for s in [
            Free,
            Reserved,
            Linking,
            Sealed,
            Relinking,
            Draining,
            Locked,
            AwaitingRelease,
        ] {
            assert!(!s.is_writable(), "{s:?} must not be writable");
        }
        assert!(Live.is_writable());
        for s in [
            Free,
            Reserved,
            Linking,
            Live,
            Relinking,
            Draining,
            Locked,
            AwaitingRelease,
        ] {
            assert!(!s.is_evictable(), "{s:?} must not be evictable");
        }
        assert!(Sealed.is_evictable());
    }

    #[test]
    fn metadata_roundtrip() {
        let cases = [
            Metadata::new_free(),
            Metadata {
                next: NonZeroU32::new(1),
                prev: None,
                state: State::Live,
            },
            Metadata {
                next: NonZeroU32::new(0xFF_FFFF),
                prev: NonZeroU32::new(0xFF_FFFE),
                state: State::AwaitingRelease,
            },
            Metadata {
                next: None,
                prev: NonZeroU32::new(42),
                state: State::Sealed,
            },
        ];
        for m in cases {
            assert_eq!(Metadata::unpack(m.pack()), m);
        }
    }

    #[test]
    fn metadata_new_free_is_zero_state_no_links() {
        let m = Metadata::new_free();
        assert_eq!(m.state, State::Free);
        assert!(m.next.is_none());
        assert!(m.prev.is_none());
        // Free with no links packs the state bits only
        assert_eq!(m.pack() & 0xFFFF_FFFF_FFFF, 0);
    }
}
