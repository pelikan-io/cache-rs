//! Segment state machine and packed metadata.
//!
//! Ported from crucible's `cache/core/src/state.rs` with one deviation:
//! chain pointers use cache-rs's 1-indexed `Option<NonZeroU32>` convention
//! (0 = none) rather than crucible's `INVALID_SEGMENT_ID = 0xFF_FFFF`
//! sentinel. Segment ids are capped at `Location::MAX_SEGMENT_ID` (< 2^20)
//! at construction — a `Location` addresses a segment in 20 bits — so they
//! always fit the 24-bit packed fields here with room to spare.

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
/// - **Relinking**: The copy-DESTINATION state during a concurrent
///   merge/s3fifo fill (item 7c). A freshly reserved spare (merge) or target
///   (s3fifo) is linked at the bucket head as `Relinking`, filled with
///   survivors relinked in via `cas_location`, then transitioned to `Sealed`
///   once the fill completes. `Relinking` is readable (so the relinked
///   survivors stay reachable to readers) but NOT evictable (only `Sealed`
///   is), so a concurrent evictor can neither select the destination
///   (`can_evict` is false) nor win its `Sealed->Draining` claim while its
///   owning evictor is still writing into it — closing the destination
///   writable-while-drainable hole.
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
///        | (copy dest:    Sealed  (readable, evictable)           |
///        |  Linking ->        | begin drain (SeqCst)              |
///        |  Relinking ->      v                                   |
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
    /// AwaitingRelease is deliberately NOT readable. Draining the
    /// hashtable before condemning stops new *lookups* from routing to a
    /// segment, but a reader whose lookup preceded the drain still calls
    /// `try_acquire_reader` afterwards, so a new *pin* can still arrive
    /// after the condemn. Permitting it lets the reader count return to
    /// non-zero after reaching zero, which no reference-count handoff can
    /// survive: the last-reader drop frees the segment while that later
    /// pin is live.
    #[inline]
    pub fn is_readable(self) -> bool {
        matches!(self, State::Live | State::Sealed | State::Relinking)
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
/// bits 63..56  tag         (8)  lifetime tag, see `cas_condemn`
/// bits 55..48  state       (8)
/// bits 47..24  prev        (24)  0 = none
/// bits 23..0   next        (24)  0 = none
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Metadata {
    pub next: Option<NonZeroU32>,
    pub prev: Option<NonZeroU32>,
    pub state: State,
    /// Lifetime tag, meaningful only while AwaitingRelease (see `cas_condemn`).
    pub tag: u8,
}

impl Metadata {
    const LINK_MASK: u64 = 0xFF_FFFF;

    /// Bit position of the tag byte, which `SegmentHeader::cas_condemn`
    /// uses to make the AwaitingRelease -> Free CAS token unique to one
    /// use of a segment.
    const TAG_SHIFT: u32 = 56;

    /// Create metadata for a fresh, unlinked segment.
    pub fn new_free() -> Self {
        Self {
            next: None,
            prev: None,
            state: State::Free,
            tag: 0,
        }
    }

    /// Pack into the u64 representation.
    #[inline]
    pub fn pack(self) -> u64 {
        let next = self.next.map_or(0, NonZeroU32::get) as u64;
        let prev = self.prev.map_or(0, NonZeroU32::get) as u64;
        debug_assert!(next <= Self::LINK_MASK, "segment id exceeds 24 bits");
        debug_assert!(prev <= Self::LINK_MASK, "segment id exceeds 24 bits");
        ((self.tag as u64) << Self::TAG_SHIFT) | ((self.state as u64) << 48) | (prev << 24) | next
    }

    /// Unpack from the u64 representation.
    #[inline]
    pub fn unpack(packed: u64) -> Self {
        Self {
            next: NonZeroU32::new((packed & Self::LINK_MASK) as u32),
            prev: NonZeroU32::new(((packed >> 24) & Self::LINK_MASK) as u32),
            state: State::from_u8(((packed >> 48) & 0xFF) as u8),
            tag: ((packed >> Self::TAG_SHIFT) & 0xFF) as u8,
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
        for s in [Free, Reserved, Linking, Draining, Locked, AwaitingRelease] {
            assert!(!s.is_readable(), "{s:?} must not be readable");
        }
        for s in [Live, Sealed, Relinking] {
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
                tag: 0,
            },
            Metadata {
                next: NonZeroU32::new(0xFF_FFFF),
                prev: NonZeroU32::new(0xFF_FFFE),
                state: State::AwaitingRelease,
                tag: 0,
            },
            Metadata {
                next: None,
                prev: NonZeroU32::new(42),
                state: State::Sealed,
                tag: 0,
            },
            /* a non-zero tag must survive alongside links */
            Metadata {
                next: NonZeroU32::new(7),
                prev: NonZeroU32::new(9),
                state: State::AwaitingRelease,
                tag: 0xFF,
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
