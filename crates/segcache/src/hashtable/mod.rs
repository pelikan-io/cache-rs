//! Lock-free N-choice hashtable with SIMD-accelerated bucket scanning.
//!
//! The hashtable maps keys to opaque [`Location`] values using 12-bit tags,
//! 8-bit frequency counters, and N-choice hashing. Ghost entries preserve
//! frequency counters after eviction for second-chance admission.
//!
//! The hashtable is fully decoupled from storage via the [`KeyVerifier`] trait.
//! Storage backends implement this trait to verify tag matches against actual keys.

pub(crate) mod bucket;
pub(crate) mod location;
pub(crate) mod table;
pub(crate) mod traits;

/// Shared loom fixture. Test-only, and only under the `loom` feature —
/// see the module docs for why the slot protocol needs a stateful verifier
/// rather than `AlwaysVerifier`.
#[cfg(all(test, feature = "loom"))]
pub(crate) mod loom_oracle;

pub use location::Location;
pub(crate) use table::{MultiChoiceHashtable, SlotRef};
pub(crate) use traits::{Hashtable, KeyVerifier};

use core::num::NonZeroU32;
use keyvalue::RawItem;

/// Pack a segment id, incarnation generation, and offset into a Location.
///
/// Layout (44 bits total):
/// - bits 43..26: segment id (18 bits)
/// - bits 25..20: incarnation tag (6 bits — `generation` masked here)
/// - bits 19..0: offset / 8 (20 bits, 8-byte aligned)
///
/// This is the ONLY way a `Location` is composed from parts.
///
/// # The generation is an explicit parameter, on purpose
///
/// It is deliberately NOT read from the segment header inside this function.
/// Two production sites do not publish a *new* location but *reconstruct* a
/// previously published one in order to compare-and-swap against it:
///
/// - [`crate::segments::Segment::copy_into`] — rebuilds `old_loc` from
///   `(src.id(), read_offset)` as the expected value of its relink CAS;
/// - `Segments::s3fifo_promote_from` — the same shape for promotion.
///
/// If reconstruction cannot reproduce the tag that was published, those CASes
/// fail *permanently* and merge/promotion silently degrades to a no-op —
/// nothing errors, throughput just quietly stops relocating. Making the
/// generation an argument forces each such site to state which incarnation it
/// means instead of picking up whatever the header happens to say later.
///
/// **Precondition at the reconstruction sites:** the generation passed must be
/// the one the location was published under. Both sites satisfy it by reading
/// the header of a segment they have claimed for drain (`Draining` /
/// `Relinking`): the claim owns the segment, and the generation only advances
/// on the transitions that end a used incarnation (`Draining -> Free`,
/// `AwaitingRelease -> Free`), neither of which can run while the claim is
/// held. So the header's generation there IS the publishing generation, and
/// cannot advance underneath the scan.
#[inline]
pub(crate) fn pack_location(seg_id: NonZeroU32, generation: u16, offset: u64) -> Location {
    debug_assert!(
        seg_id.get() <= Location::MAX_SEGMENTS,
        "segment id exceeds the largest issuable id"
    );
    debug_assert!(
        (offset >> 3) <= location::OFFSET_MASK,
        "offset exceeds the location's offset field"
    );
    let tag = location::tag_for_generation(generation) as u64;
    Location::new(
        ((seg_id.get() as u64) << location::SEG_ID_SHIFT)
            | (tag << location::TAG_SHIFT)
            | ((offset >> 3) & location::OFFSET_MASK),
    )
}

/// Unpack a Location into (segment_id, byte_offset).
///
/// Returns (0, _) for invalid locations — callers must check. The incarnation
/// tag is deliberately NOT returned here: it is not part of the address. Read
/// it with [`Location::tag`] when validating.
#[inline]
pub(crate) fn unpack_location(loc: Location) -> (u32, usize) {
    let raw = loc.as_raw();
    let seg_id = (raw >> location::SEG_ID_SHIFT) as u32;
    let offset = ((raw & location::OFFSET_MASK) << 3) as usize;
    (seg_id, offset)
}

/// Adapter that implements [`KeyVerifier`] for the existing Segments data buffer.
///
/// This is temporary — it will be removed when Segments is replaced in Phase 2.
/// It only needs read access to the segment data for key comparison.
pub(crate) struct SegmentsVerifier<'a> {
    data: &'a [u8],
    segment_size: usize,
    num_segments: usize,
}

impl<'a> SegmentsVerifier<'a> {
    /// Create a new verifier from the segments data buffer.
    #[inline]
    pub(crate) fn new(data: &'a [u8], segment_size: usize, num_segments: usize) -> Self {
        Self {
            data,
            segment_size,
            num_segments,
        }
    }
}

impl KeyVerifier for SegmentsVerifier<'_> {
    fn verify(&self, key: &[u8], location: Location, _allow_deleted: bool) -> bool {
        let (seg_id, offset) = unpack_location(location);

        if seg_id == 0 || seg_id as usize > self.num_segments {
            return false;
        }

        let byte_offset = self.segment_size * (seg_id as usize - 1) + offset;

        if byte_offset + keyvalue::ITEM_HDR_SIZE > self.data.len() {
            return false;
        }

        // SAFETY: We verified the offset is within the data buffer.
        // The data buffer is the segment heap and items are written with valid headers.
        let item = RawItem::from_ptr(unsafe { (self.data.as_ptr() as *mut u8).add(byte_offset) });
        item.key() == key
    }

    #[inline]
    fn prefetch(&self, location: Location) {
        let (seg_id, offset) = unpack_location(location);
        if seg_id == 0 || seg_id as usize > self.num_segments {
            return;
        }
        let byte_offset = self.segment_size * (seg_id as usize - 1) + offset;
        if byte_offset >= self.data.len() {
            return;
        }
        let ptr = unsafe { self.data.as_ptr().add(byte_offset) as *const i8 };

        #[cfg(all(target_arch = "x86_64", target_feature = "sse"))]
        unsafe {
            std::arch::x86_64::_mm_prefetch::<{ std::arch::x86_64::_MM_HINT_T0 }>(ptr);
        }

        #[cfg(target_arch = "aarch64")]
        unsafe {
            std::arch::asm!(
                "prfm pldl1keep, [{ptr}]",
                ptr = in(reg) ptr,
                options(nostack, preserves_flags)
            );
        }

        #[cfg(not(any(
            all(target_arch = "x86_64", target_feature = "sse"),
            target_arch = "aarch64"
        )))]
        let _ = ptr;
    }
}

// SAFETY: SegmentsVerifier only holds a shared reference to a byte slice.
unsafe impl Send for SegmentsVerifier<'_> {}
unsafe impl Sync for SegmentsVerifier<'_> {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The largest id a heap can actually issue. Deliberately NOT
    /// `MAX_SEGMENT_ID` (the field's capacity): that value is reserved so no
    /// real location can alias `Location::GHOST` — see
    /// `test_ghost_is_unreachable_by_construction`.
    const MAX_ID: u32 = Location::MAX_SEGMENTS;
    const MAX_OFFSET: u64 = location::OFFSET_MASK << 3;
    /// The largest tag the field holds — i.e. the generation just before the
    /// projection wraps. Derived, so the boundary tests below follow the width
    /// instead of pinning a stale literal.
    const MAX_TAG: u16 = location::TAG_MASK as u16;
    /// How many distinct incarnations a tag distinguishes.
    const TAG_PERIOD: u16 = MAX_TAG + 1;

    #[test]
    fn test_pack_unpack_roundtrip() {
        let seg_id = NonZeroU32::new(42).unwrap();
        let offset = 1024u64; // must be 8-byte aligned

        let loc = pack_location(seg_id, 7, offset);
        let (unpacked_seg, unpacked_offset) = unpack_location(loc);

        assert_eq!(unpacked_seg, 42);
        assert_eq!(unpacked_offset, 1024);
        assert_eq!(loc.tag(), 7);
    }

    #[test]
    fn test_pack_max_seg_id() {
        // The largest id the 18-bit field can actually issue.
        let seg_id = NonZeroU32::new(MAX_ID).unwrap();
        let offset = 0u64;

        let loc = pack_location(seg_id, 0, offset);
        let (unpacked_seg, unpacked_offset) = unpack_location(loc);
        assert_eq!(unpacked_seg, MAX_ID);
        assert_eq!(unpacked_offset, 0);
        assert_eq!(loc.tag(), 0);
    }

    #[test]
    fn test_pack_max_offset() {
        let seg_id = NonZeroU32::new(1).unwrap();
        // 20-bit offset field × 8 = max ~8MB offset
        let offset = MAX_OFFSET;

        let loc = pack_location(seg_id, 0, offset);
        let (unpacked_seg, unpacked_offset) = unpack_location(loc);
        assert_eq!(unpacked_seg, 1);
        assert_eq!(unpacked_offset, offset as usize);
        assert_eq!(loc.tag(), 0);
    }

    /// Every field at its maximum simultaneously, so a field that bled into
    /// its neighbour could not hide behind a zero.
    #[test]
    fn test_pack_all_fields_maxed() {
        let seg_id = NonZeroU32::new(MAX_ID).unwrap();
        let loc = pack_location(seg_id, MAX_TAG, MAX_OFFSET);
        let (unpacked_seg, unpacked_offset) = unpack_location(loc);

        assert_eq!(unpacked_seg, MAX_ID);
        assert_eq!(unpacked_offset, MAX_OFFSET as usize);
        assert_eq!(loc.tag(), MAX_TAG as u8);
        // Even with every issuable field maxed, the raw word falls short of
        // all-ones: the reserved id keeps the ghost sentinel out of reach.
        assert_ne!(loc.as_raw(), Location::MAX_RAW);
        assert!(!loc.is_ghost());
    }

    /// The three fields are independent: walking each boundary while the
    /// others sit at their extremes must not disturb them.
    #[test]
    fn test_fields_are_independent_across_boundaries() {
        let ids = [1u32, 2, MAX_ID / 2, MAX_ID - 1, MAX_ID];
        let offsets = [0u64, 8, MAX_OFFSET - 8, MAX_OFFSET];

        for id in ids {
            for offset in offsets {
                for generation in 0..=MAX_TAG {
                    let loc = pack_location(NonZeroU32::new(id).unwrap(), generation, offset);
                    let (unpacked_id, unpacked_offset) = unpack_location(loc);
                    assert_eq!(unpacked_id, id, "id {id} gen {generation} offset {offset}");
                    assert_eq!(
                        unpacked_offset, offset as usize,
                        "id {id} gen {generation} offset {offset}"
                    );
                    assert_eq!(
                        loc.tag(),
                        generation as u8,
                        "id {id} gen {generation} offset {offset}"
                    );
                }
            }
        }
    }

    /// The generation is masked to the tag width inside `pack_location`, so a
    /// wrapped counter aliases every 64 lifecycles (by design) and never
    /// corrupts the segment id above it.
    ///
    /// All 65,536 generations are swept, so the projection is checked to be
    /// exactly `generation % 64` — 1024 generations onto each of the 64 tags —
    /// rather than merely "some function that looks periodic".
    #[test]
    fn test_generation_is_masked_to_the_tag_width() {
        let seg_id = NonZeroU32::new(12345).unwrap();
        let mut per_tag = [0u32; 64];
        for generation in 0u16..=u16::MAX {
            let loc = pack_location(seg_id, generation, 4096);
            assert_eq!(loc.tag() as u16, generation % TAG_PERIOD);
            assert_eq!(unpack_location(loc), (12345, 4096));
            per_tag[loc.tag() as usize] += 1;
        }
        assert_eq!(TAG_PERIOD, 64, "the tag must distinguish 64 incarnations");
        assert!(
            per_tag.iter().all(|&n| n == 65536 / 64),
            "every tag must be hit equally often: {per_tag:?}"
        );
    }

    /// The tag width, asserted as behaviour rather than as a constant: a
    /// generation aliases the fresh one after exactly 64 lifecycles, and NOT
    /// after 16 — which is what it did while the field was 4 bits wide. This
    /// is the test that fails if a future edit narrows the field back.
    #[test]
    fn test_tag_aliases_after_sixty_four_lifecycles_not_sixteen() {
        let seg_id = NonZeroU32::new(7).unwrap();
        let offset = 4096;
        let base = pack_location(seg_id, 0, offset);

        // 64 lifecycles later the location is indistinguishable — the honest
        // limit of the scheme, documented in the design doc.
        assert_eq!(
            base,
            pack_location(seg_id, 64, offset),
            "generation 64 must alias generation 0 at a 6-bit tag"
        );

        // Everything short of that stays distinct, 16 (the old wrap point)
        // included.
        for generation in 1u16..64 {
            assert_ne!(
                base,
                pack_location(seg_id, generation, offset),
                "generation {generation} must NOT alias generation 0"
            );
        }
    }

    /// A recycled segment publishes a DIFFERENT location for the same address,
    /// which is the entire point of the tag: a CAS holding the old one fails.
    #[test]
    fn test_tag_distinguishes_incarnations() {
        let seg_id = NonZeroU32::new(9).unwrap();
        let before = pack_location(seg_id, 3, 512);
        let after = pack_location(seg_id, 4, 512);

        assert_ne!(before, after);
        assert_eq!(unpack_location(before), unpack_location(after));
    }

    /// `Location::GHOST` is all 44 bits set, and no real location can equal it
    /// — not "implausibly", but by construction.
    ///
    /// The only packing that would alias it needs ALL of: the id field at
    /// `MAX_SEGMENT_ID`, tag 63, and an item at the very last encodable offset.
    /// The id field's maximum is deliberately NOT issuable (`MAX_SEGMENTS` is
    /// one lower, and `Segments::from_builder` refuses a heap that would need
    /// it — see `segments::capacity_tests`), so the first conjunct is
    /// unsatisfiable and the alias cannot arise however extreme the other two
    /// get.
    #[test]
    fn test_ghost_is_unreachable_by_construction() {
        assert!(Location::GHOST.is_ghost());

        // Every extreme an issuable id can reach, including all three fields
        // simultaneously maximal.
        let corners = [
            (1u32, 0u16, 0u64),
            (1, MAX_TAG, MAX_OFFSET),
            (MAX_ID, MAX_TAG, 0),
            (MAX_ID, 0, MAX_OFFSET),
            (MAX_ID - 1, MAX_TAG, MAX_OFFSET),
            (MAX_ID, MAX_TAG - 1, MAX_OFFSET),
            (MAX_ID, MAX_TAG, MAX_OFFSET - 8),
            (MAX_ID, MAX_TAG, MAX_OFFSET),
        ];
        for (id, generation, offset) in corners {
            let loc = pack_location(NonZeroU32::new(id).unwrap(), generation, offset);
            assert!(
                !loc.is_ghost(),
                "id {id} gen {generation} offset {offset} aliased the ghost sentinel"
            );
        }

        // Why the sweep above is exhaustive rather than lucky: the sentinel's
        // id field is one value, and that value is not issuable.
        assert_eq!(
            Location::GHOST.as_raw() >> location::SEG_ID_SHIFT,
            Location::MAX_SEGMENT_ID as u64,
            "GHOST must sit at the id field's maximum"
        );
        const {
            assert!(
                Location::MAX_SEGMENTS < Location::MAX_SEGMENT_ID,
                "the aliasing id must be reserved, not issuable"
            )
        };
    }
}
