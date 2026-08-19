//! Opaque location type for cache storage.
//!
//! `Location` is a 44-bit packed value that identifies where an item is stored.
//! The hashtable treats this as an opaque identifier — storage backends define
//! their own interpretation of the bits.

use std::fmt;

/// Opaque 44-bit location value.
///
/// The hashtable stores this alongside a 12-bit tag and 8-bit frequency,
/// fitting in a single 64-bit atomic. The meaning of the 44 bits is defined
/// by the storage backend:
///
/// ```text
/// Hashtable entry layout:
/// +--------+--------+---------------------------+
/// | 63..52 | 51..44 |          43..0            |
/// |  tag   |  freq  |         location          |
/// | 12 bits| 8 bits |         44 bits           |
/// +--------+--------+---------------------------+
/// ```
///
/// For segcache, the location encodes:
/// - bits 43..26: segment id (18 bits)
/// - bits 25..20: incarnation tag (6 bits — the low bits of the segment
///   header's `generation`)
/// - bits 19..0: offset / 8 (20 bits, 8-byte aligned)
///
/// The incarnation tag is what makes a location identify an *incarnation* of
/// a segment rather than just an address: a segment that is drained and
/// recycled advances its generation, so every location published into the
/// previous incarnation now carries a stale tag. Because the tag rides inside
/// the 44 bits that sit in the packed hashtable slot word, every existing
/// compare-exchange on a published entry validates it for free.
///
/// Locations are built ONLY by `crate::pack_location`, which takes the
/// generation explicitly. There is deliberately no way to assemble one from
/// `(id, tag, offset)` parts elsewhere; [`Location::new`]/[`Location::from_raw`]
/// reinterpret a whole 44-bit word (as the hashtable does when it unpacks a
/// slot), they do not compose fields.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Location(u64);

/// Width of the segment-id field.
pub(crate) const SEG_ID_BITS: u32 = 18;
/// Width of the incarnation-tag field.
///
/// Six bits, so a location survives 64 lifecycles of its segment before its
/// tag can repeat. The width is a pure trade against [`Location::MAX_SEGMENTS`]
/// — every bit here halves the addressable segment count — and the reasoning
/// for landing on 64 is in
/// `docs/superpowers/specs/2026-08-19-generation-tagged-locations-design.md`.
pub(crate) const TAG_BITS: u32 = 6;
/// Width of the offset field, which stores `byte_offset >> 3`.
pub(crate) const OFFSET_BITS: u32 = 20;

/// Bit position of the incarnation tag.
pub(crate) const TAG_SHIFT: u32 = OFFSET_BITS;
/// Bit position of the segment id.
pub(crate) const SEG_ID_SHIFT: u32 = OFFSET_BITS + TAG_BITS;
/// Mask for the incarnation tag, once shifted down.
pub(crate) const TAG_MASK: u64 = (1 << TAG_BITS) - 1;
/// Mask for the offset field.
pub(crate) const OFFSET_MASK: u64 = (1 << OFFSET_BITS) - 1;

/// The three fields tile the 44 bits exactly: no gap (which would waste tag
/// width) and no overlap (which would alias ids onto tags). Retuning the split
/// is meant to be a matter of editing the widths above, so the invariant that
/// makes such an edit safe is checked here rather than left to the tests.
const _: () = assert!(SEG_ID_BITS + TAG_BITS + OFFSET_BITS == 44);

impl Location {
    /// Maximum raw value (44 bits set).
    pub const MAX_RAW: u64 = 0xFFF_FFFF_FFFF;

    /// Largest segment id the 18-bit id field can encode.
    ///
    /// This is the field's capacity, NOT the largest id a heap may issue —
    /// see [`Self::MAX_SEGMENTS`], which is one lower on purpose.
    pub(crate) const MAX_SEGMENT_ID: u32 = (1 << SEG_ID_BITS) - 1;

    /// Largest number of segments a heap may contain, and (ids being 1-based)
    /// the largest segment id that is ever issued.
    ///
    /// One below the field's capacity, so that [`Self::MAX_SEGMENT_ID`] is
    /// **never** a live segment id. That reservation is what makes [`Self::GHOST`]
    /// — all 44 bits set — unreachable by construction: the only packing that
    /// equals it needs `MAX_SEGMENT_ID` in the id field, which no heap can
    /// issue, so no `pack_location` of a real item can ever alias the sentinel.
    /// At the current widths that is 262,142 segments — 256 GiB of heap at
    /// 1 MiB segments, 2 TiB at the 8 MiB maximum.
    /// `Segments` construction refuses a larger heap rather than relying on
    /// "an 8 MiB segment whose last 8 bytes hold an item is implausible".
    pub(crate) const MAX_SEGMENTS: u32 = Self::MAX_SEGMENT_ID - 1;

    /// Sentinel value indicating a ghost entry (recently evicted).
    /// All 44 location bits set to 1. Unreachable as a real location: see
    /// [`Self::MAX_SEGMENTS`].
    pub const GHOST: Self = Self(Self::MAX_RAW);

    /// Create a location from a raw 44-bit value.
    ///
    /// # Panics
    ///
    /// Panics in debug mode if `raw > MAX_RAW`.
    #[inline]
    pub fn new(raw: u64) -> Self {
        debug_assert!(raw <= Self::MAX_RAW, "location exceeds 44 bits");
        Self(raw)
    }

    /// Get the raw 44-bit value.
    #[inline(always)]
    pub fn as_raw(&self) -> u64 {
        self.0
    }

    /// Construct from raw value, masking to 44 bits.
    #[inline(always)]
    pub fn from_raw(raw: u64) -> Self {
        Self(raw & Self::MAX_RAW)
    }

    /// Check if this is the ghost sentinel.
    #[inline(always)]
    pub fn is_ghost(&self) -> bool {
        *self == Self::GHOST
    }

    /// The 6-bit incarnation tag: the low bits of the segment header's
    /// `generation` at the time the location was published.
    ///
    /// Validation compares this against the segment's *live* generation; a
    /// mismatch means the location names an incarnation that has since been
    /// drained and recycled, i.e. "this is no longer yours". It is not an
    /// address component — use `crate::unpack_location` for addressing.
    #[inline(always)]
    pub fn tag(&self) -> u8 {
        ((self.0 >> TAG_SHIFT) & TAG_MASK) as u8
    }
}

/// The incarnation tag carried by a location published under `generation`.
///
/// The ONE definition of the generation -> tag projection: `crate::pack_location`
/// stamps it and `Segments::resolve` compares against it, so the two cannot
/// drift apart into a mask that matches nothing.
#[inline(always)]
pub(crate) fn tag_for_generation(generation: u16) -> u8 {
    ((generation as u64) & TAG_MASK) as u8
}

impl fmt::Debug for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_ghost() {
            write!(f, "Location::GHOST")
        } else {
            write!(f, "Location(0x{:011X})", self.0)
        }
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_ghost() {
            write!(f, "GHOST")
        } else {
            write!(f, "0x{:011X}", self.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_and_as_raw() {
        let loc = Location::new(0x123_4567_89AB);
        assert_eq!(loc.as_raw(), 0x123_4567_89AB);
        assert!(!loc.is_ghost());
    }

    #[test]
    fn test_ghost_sentinel() {
        assert!(Location::GHOST.is_ghost());
        assert_eq!(Location::GHOST.as_raw(), Location::MAX_RAW);
    }

    #[test]
    fn test_from_raw_masks() {
        let loc = Location::from_raw(0xFFFF_FFFF_FFFF_FFFF);
        assert_eq!(loc.as_raw(), Location::MAX_RAW);
        assert!(loc.is_ghost());
    }

    /// Compose a raw 44-bit value field by field, so the test states the
    /// layout independently of `pack_location`.
    fn raw(seg_id: u64, tag: u64, offset_field: u64) -> u64 {
        (seg_id << SEG_ID_SHIFT) | (tag << TAG_SHIFT) | offset_field
    }

    #[test]
    fn test_tag_reads_the_middle_field() {
        // Every tag value is readable, and none of them disturbs — or is
        // disturbed by — the neighbouring id and offset fields. The id and
        // offset used here are asymmetric bit patterns at the full width of
        // their fields, so a one-bit spill in either direction shows up.
        const ID: u64 = 0x3_ABCD; // 18 bits
        const OFFSET_FIELD: u64 = 0x1_2345; // 20 bits
        for tag in 0..=TAG_MASK {
            let loc = Location::new(raw(ID, tag, OFFSET_FIELD));
            assert_eq!(loc.tag() as u64, tag);
            assert_eq!(loc.as_raw() >> SEG_ID_SHIFT, ID);
            assert_eq!(loc.as_raw() & OFFSET_MASK, OFFSET_FIELD);
        }
        // The widths are what this test is pinning down; state them, so a
        // silent re-split cannot leave the sweep above passing vacuously.
        assert_eq!(TAG_MASK, 0x3F, "the tag field must be 6 bits wide");
        assert_eq!(SEG_ID_SHIFT, 26);
        assert_eq!(TAG_SHIFT, 20);
        assert_eq!(Location::GHOST.tag(), 0x3F);
    }

    #[test]
    fn test_equality() {
        let a = Location::new(12345);
        let b = Location::new(12345);
        let c = Location::new(12346);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
