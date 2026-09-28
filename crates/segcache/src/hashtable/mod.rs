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
#[cfg(all(test, model_checking))]
pub(crate) mod loom_oracle;

pub use location::Location;
pub(crate) use table::{MultiChoiceHashtable, SlotRef};
pub(crate) use traits::{Hashtable, Hit, Insert, KeyVerifier, Lookup, RelinkFreq, Verified};

use crate::segments::SegmentGuard;
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

/// The [`KeyVerifier`] the cache runs on: it compares key bytes **under a
/// reader pin whose incarnation tag it checked**.
///
/// # Why the pin is the whole design (#91)
///
/// The previous verifier held a `&[u8]` over the segment heap and compared
/// bytes at whatever offset a hashtable slot named, with no pin and no
/// generation tag. A slot's location can be stale — the segment behind it
/// recycled and rewritten between the slot read and the compare — so that read
/// raced a writer's plain writes. Three consequences, all closed here:
///
/// - **it was formally UB.** A plain read racing a plain write is undefined
///   behaviour whatever the protocol does with the answer, and it was the one
///   remaining ThreadSanitizer report class, suppressed by function name so the
///   CI gate could land.
/// - **it could read out of bounds.** A garbage `klen`/`olen` decoded at a
///   stale offset near a segment's end let `RawItem::key` build a slice past
///   the end of the heap.
/// - **it forced two probes on every hit.** Because an unpinned compare is not
///   authoritative, `get` pinned and then performed a *second full hashtable
///   lookup* to revalidate.
///
/// [`Segments::acquire_item_at`] is already exactly the read this needs: it
/// takes the reader guard first and checks the location's generation tag
/// *under* the pin, so the generation is frozen while it is read.
///
/// **Under a held pin with a matching tag the compared bytes are
/// published-immutable.** A pin blocks both `-> Free` transitions, and within
/// one incarnation a segment is append-only: an offset is never rewritten
/// until the segment is recycled, and recycling bumps the generation, which
/// fails the tag check. No writer can be mutating those bytes, so the compare
/// is a plain `memcmp` — it vectorizes, which is the entire cost difference
/// from the parked word-granular-atomics attempt.
pub(crate) struct SegmentsVerifier<'a> {
    segments: &'a crate::segments::Segments,
    /// Fire the `after_lookup` fault hook before each pin attempt.
    ///
    /// Set only on the verifier `get` uses for its **from-scratch** probe, so
    /// the hook keeps meaning "once per from-scratch lookup" and does not also
    /// fire for the cold path's revalidation lookups. It sits before the pin
    /// rather than after the lookup returns because that is now the whole
    /// hazard window: a slot word read a moment before a drain recycles the
    /// segment behind it is exactly what produces a stale location, and a hook
    /// that fired after the lookup would be holding this reader's pin — which
    /// would stop the very recycle the test is standing in for.
    #[cfg(feature = "fault-injection")]
    fault_probe: bool,
}

impl<'a> SegmentsVerifier<'a> {
    /// Create a new verifier over the segment heap.
    #[inline]
    pub(crate) fn new(segments: &'a crate::segments::Segments) -> Self {
        Self {
            segments,
            #[cfg(feature = "fault-injection")]
            fault_probe: false,
        }
    }

    /// The same verifier, wired to fire the `after_lookup` fault hook before
    /// each pin attempt. See [`Self::fault_probe`].
    #[cfg(feature = "fault-injection")]
    #[inline]
    pub(crate) fn probing(mut self) -> Self {
        self.fault_probe = true;
        self
    }
}

impl KeyVerifier for SegmentsVerifier<'_> {
    /// The pinned item and the guard keeping its segment alive. `get` keeps
    /// both — the guard is what an [`crate::Item`] is built around; every other
    /// caller drops them the moment the compare is answered.
    type Pin = (RawItem, SegmentGuard);

    fn verify(&self, key: &[u8], location: Location, _allow_deleted: bool) -> Verified<Self::Pin> {
        // Range-check ahead of the pin. `acquire_item_at` opens with
        // `assert!(seg_id <= cap)`, and `Location::GHOST` is all-ones, so it
        // would trip that assert. Bucket scans filter ghosts before verifying,
        // but the verifier must not RELY on its caller.
        let (seg_id, _offset) = unpack_location(location);
        if seg_id == 0 || seg_id as usize > self.segments.num_segments() {
            debug_assert!(
                false,
                "verify reached an unrepresentable location: the bucket scans                  filter empty slots and ghosts before calling it"
            );
            return Verified::DifferentKey;
        }

        #[cfg(feature = "fault-injection")]
        if self.fault_probe {
            crate::segcache::revalidation_fault::after_lookup();
        }

        match self.segments.acquire_item_at(location) {
            Some((raw, guard)) => {
                // The out-of-bounds read is closed by construction, not by
                // clamping: a pinned, tag-valid location names a real published
                // item of that incarnation, so its `klen`/`olen` are the values
                // a writer wrote rather than garbage decoded at a recycled
                // offset. Recorded as a checked precondition rather than a
                // comment.
                debug_assert!(
                    _offset + raw.size() <= self.segments.segment_size() as usize,
                    "a pinned, tag-valid item must lie inside its segment:                      offset {_offset} + size {} > segment size {}",
                    raw.size(),
                    self.segments.segment_size()
                );
                if raw.key() == key {
                    Verified::Match((raw, guard))
                } else {
                    // Authoritative. Nothing was mutating these bytes, so
                    // "different key" cannot mean "the bytes stopped being this
                    // entry's" — the ambiguity `classify_failed_verify` and the
                    // STALE-LOCATION invariant block existed to resolve.
                    Verified::DifferentKey // guard drops here
                }
            }
            // Either the segment is not readable (a drain owns it) or the
            // location's incarnation is gone. Both are the caller's to triage;
            // see `Segcache::triage_unknown_location`.
            None => Verified::Unknown(location),
        }
    }

    #[inline]
    fn prefetch(&self, location: Location) {
        self.segments.prefetch_item_at(location);
    }
}

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

#[cfg(kani)]
mod verification {
    use super::*;
    use crate::hashtable::location::{tag_for_generation, TAG_MASK};

    /// A valid packing input: an issuable segment id, any generation, and
    /// an 8-aligned offset the builder-enforced segment ceiling admits.
    fn any_valid_packing() -> (NonZeroU32, u16, u64) {
        let id: u32 = kani::any();
        kani::assume(id >= 1 && id <= Location::MAX_SEGMENTS);
        let generation: u16 = kani::any();
        let offset: u64 = kani::any();
        kani::assume(offset < Location::MAX_SEGMENT_BYTES as u64);
        kani::assume(offset % 8 == 0);
        (NonZeroU32::new(id).unwrap(), generation, offset)
    }

    /// Every valid (id, generation, offset) survives the pack/unpack/tag
    /// roundtrip exactly. This is the #79 bug class made unreachable: a
    /// silent offset-field wrap would fail the offset equality here.
    #[kani::proof]
    fn pack_location_roundtrip() {
        let (id, generation, offset) = any_valid_packing();
        let loc = pack_location(id, generation, offset);
        let (id2, offset2) = unpack_location(loc);
        assert_eq!(id2, id.get());
        assert_eq!(offset2 as u64, offset);
        assert_eq!(loc.tag(), tag_for_generation(generation));
    }

    /// No valid packing can alias the GHOST sentinel — the property
    /// `Location::MAX_SEGMENTS`' doc comment argues in English (the only
    /// packing equal to GHOST needs the never-issued top id).
    #[kani::proof]
    fn pack_location_never_ghost() {
        let (id, generation, offset) = any_valid_packing();
        assert!(!pack_location(id, generation, offset).is_ghost());
    }

    /// Packing is injective up to the tag projection: equal packed words
    /// imply equal id, equal offset, and generations whose low `TAG_MASK`
    /// bits agree. Combined with the layout invariant that two
    /// simultaneously-live items occupy distinct (id, offset), no two live
    /// items can share a location word. The generations-mod-64 residue is
    /// NOT a two-live-items case but the documented stale-entry window: a
    /// location published exactly 64 incarnations ago validates falsely
    /// (the accepted 6-bit-tag trade, `location.rs` "Why 6 bits" /
    /// docs/superpowers/specs/2026-08-19-generation-tagged-locations).
    #[kani::proof]
    fn pack_location_injective() {
        let (id_a, gen_a, off_a) = any_valid_packing();
        let (id_b, gen_b, off_b) = any_valid_packing();
        kani::assume(pack_location(id_a, gen_a, off_a) == pack_location(id_b, gen_b, off_b));
        assert_eq!(id_a, id_b);
        assert_eq!(off_a, off_b);
        assert_eq!(gen_a as u64 & TAG_MASK, gen_b as u64 & TAG_MASK);
    }
}
