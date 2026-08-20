//! Shared loom fixture: a stateful location -> key oracle.
//!
//! # Why this exists
//!
//! [`KeyVerifier`] is the seam between the hashtable's slot protocol and raw
//! storage: the hashtable NEVER touches segment bytes itself, it asks the
//! verifier "does `location` hold `key`?". That makes the verifier the only
//! thing a loom model needs in order to reproduce the hazard the slot
//! protocol actually defends against — **a location whose bytes stopped
//! being this entry's while a thread was looking at them**.
//!
//! The loom models in `table.rs` historically stubbed the verifier with
//! `AlwaysVerifier`, which answers `true` for everything. That is fine for
//! CAS-uniqueness and election-shaped invariants, but it makes every model
//! using it structurally blind to key identity: a location can be relocated,
//! recycled, refilled with somebody else's key, or freed outright and the
//! stub keeps saying "yes, your key is there". Every read path's
//! `verify`-failure branch — the entire STALE-LOCATION guard in
//! `MultiChoiceHashtable::verify_slot` — is dead code under `AlwaysVerifier`.
//!
//! [`KeyOracle`] replaces that stub with model atomics representing
//! "which key currently lives at this location". Raw mmap'd bytes stay
//! entirely outside the model; there is no production hook and nothing to
//! compile out of release builds.
//!
//! # What it can model
//!
//! - **relocation** — the key moves to a new location, the slot is relinked
//!   in place ([`KeyOracle::drain_relocate`]);
//! - **recycle + refill** — the source segment is finalized, recycled, and
//!   rewritten by another writer, so the old location now holds an unrelated
//!   key ([`OTHER`]);
//! - **removal** — the item is freed and the location holds nothing at all
//!   ([`KeyOracle::vacate`]).
//! - **recycle + refill with the SAME key at the SAME address** — the one
//!   shape the verifier cannot see through, because the bytes really are
//!   this key's again ([`KeyOracle::recycle_and_refill`]). Only the
//!   incarnation tag distinguishes the two locations, which is why cells are
//!   addressed with the production `pack_location` (below).
//!
//! # Cells are segments, and locations carry a real incarnation tag
//!
//! A cell's [`Location`] is built by [`crate::hashtable::pack_location`] with
//! the cell as the segment id and offset zero — not by a hand-rolled encoding.
//! That is deliberate: a model about the incarnation tag must be built out of
//! the production packing, or neutering `tag_for_generation` leaves it green.
//! Offset zero is faithful rather than lazy — segments are append-only from a
//! fixed start, so under uniform item sizes the n-th item of every incarnation
//! lands at exactly the same offset (design §"Why 6 bits").
//!
//! # Faithfulness rules
//!
//! Models must sequence oracle mutations in the order the real system
//! performs them, or they manufacture states production cannot reach and
//! the resulting "bug" is a fiction. [`KeyOracle::drain_relocate`] and
//! [`KeyOracle::recycle_and_refill`] each encode the one ordering that
//! matters so callers cannot get it wrong.

use crate::hashtable::location::Location;
use crate::hashtable::pack_location;
use crate::hashtable::table::MultiChoiceHashtable;
use crate::hashtable::traits::{Hashtable, KeyVerifier};
use crate::sync::{AtomicU64, Ordering};
use core::num::NonZeroU32;

/// The key every oracle-backed model tracks. Deliberately the same literal
/// the pre-existing `AlwaysVerifier` models use, so models can be converted
/// without re-tuning bucket/stripe assignments.
pub(crate) const KEY: &[u8] = b"key";

/// A key that only ever appears as the OCCUPANT of a recycled location: it
/// is never inserted into the hashtable. Placing it at a cell is how a model
/// says "this segment was finalized, recycled, and rewritten by an unrelated
/// writer".
pub(crate) const OTHER: &[u8] = b"other";

/// Non-zero so a vacant cell (`0`) is distinguishable from an occupied one.
const KEY_ID: u64 = 1;
const OTHER_ID: u64 = 2;

/// Where the subject key starts out.
pub(crate) const SRC: usize = 0;
/// An intermediate location, for models that need two successive drains.
pub(crate) const MID: usize = 1;
/// Where a relocation moves the key to.
pub(crate) const DST: usize = 2;
/// Where a racing writer publishes a replacement copy of the key.
pub(crate) const NEW: usize = 3;

/// Number of distinct storage locations the oracle models. Kept small on
/// purpose: every cell is a loom-tracked atomic.
pub(crate) const NUM_CELLS: usize = 4;

/// How many incarnations of a cell a model may name. Two is enough for the
/// hazard the tag exists for — one outgoing, one refilled — and the sweep in
/// [`KeyOracle::drain_live_entries`] is linear in it.
pub(crate) const NUM_INCARNATIONS: u16 = 2;

fn key_id(key: &[u8]) -> u64 {
    if key == KEY {
        KEY_ID
    } else if key == OTHER {
        OTHER_ID
    } else {
        // Unknown keys never match an occupied cell (ids start at 1).
        0
    }
}

/// A location -> key map backed by loom-tracked atomics.
///
/// One cell per modeled storage location. `0` means the location holds
/// nothing this model knows about (freed, or recycled and not yet rewritten);
/// otherwise the cell holds the id of the key whose bytes currently live
/// there.
pub(crate) struct KeyOracle {
    cells: [AtomicU64; NUM_CELLS],
}

impl KeyOracle {
    /// All locations start vacant. Seed with [`KeyOracle::place`].
    pub(crate) fn new() -> Self {
        Self {
            cells: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// The [`Location`] naming `cell` in its FIRST incarnation.
    ///
    /// Shorthand for `location_in(cell, 0)`, which is what every model that
    /// does not care about incarnations wants.
    pub(crate) fn location(cell: usize) -> Location {
        Self::location_in(cell, 0)
    }

    /// The [`Location`] naming `cell` in incarnation `generation`.
    ///
    /// Built with the production [`pack_location`] — cell as the segment id
    /// (offset by one so no cell maps to segment 0), offset zero — so the
    /// incarnation tag in the returned word is the real one and a model
    /// asserting on it reddens when `tag_for_generation` is neutered. The
    /// issuable-id range keeps every result clear of `Location::GHOST`.
    pub(crate) fn location_in(cell: usize, generation: u16) -> Location {
        debug_assert!(cell < NUM_CELLS);
        let seg_id = NonZeroU32::new(cell as u32 + 1).expect("cell + 1 is non-zero");
        pack_location(seg_id, generation, 0)
    }

    /// Write `key`'s bytes at `cell` — a segment write (reserve + define, or
    /// a merge's `copy_into` destination write).
    ///
    /// `Release`, because in production the bytes must be visible to any
    /// thread that later observes a slot published with this location.
    pub(crate) fn place(&self, cell: usize, key: &[u8]) {
        let id = key_id(key);
        debug_assert!(id != 0, "place() called with a key the oracle cannot name");
        self.cells[cell].store(id, Ordering::Release);
    }

    /// The item at `cell` was freed and the space released: the location now
    /// holds nothing. Models removal (`remove` + segment decrement).
    pub(crate) fn vacate(&self, cell: usize) {
        self.cells[cell].store(0, Ordering::Release);
    }

    /// One merge-drain relocation of [`KEY`], in the order production
    /// performs it (`Segment::copy_into`):
    ///
    /// 1. copy the item into `dst` — its bytes are valid there BEFORE
    ///    anything points at them;
    /// 2. relink the slot with the `Release` CAS, publishing `dst`;
    /// 3. the source segment is finalized, recycled, and rewritten by
    ///    another writer, so `src` now holds an unrelated key.
    ///
    /// Step 3 runs whether or not the relink landed: a lost relink means the
    /// item at `src` was superseded by a racing writer, and the source
    /// segment is recycled all the same.
    ///
    /// Returns whether the relink CAS landed. Models that race the drain
    /// against a mutator must tolerate `false`; models where nothing else
    /// touches the entry should assert `true`.
    pub(crate) fn drain_relocate(&self, ht: &MultiChoiceHashtable, src: usize, dst: usize) -> bool {
        self.place(dst, KEY);
        let relinked = ht.cas_location(KEY, Self::location(src), Self::location(dst), true);
        self.place(src, OTHER);
        relinked
    }

    /// One drain -> recycle -> re-reserve -> refill of `cell`, republishing
    /// [`KEY`] at the SAME address in the NEXT incarnation, in the order
    /// production performs it:
    ///
    /// 1. the drain's sweep unlinks the entry published by the outgoing
    ///    incarnation (`Segment::clear`, under its `Sealed -> Draining`
    ///    claim). It may already be gone — a `false` here is normal;
    /// 2. the segment is recycled: its bytes stop being anybody's item, and
    ///    the `Draining -> Free` transition spends a generation
    ///    (`Segments::recycle`);
    /// 3. it is re-reserved and a writer defines [`KEY`] at the same offset —
    ///    the bytes are this key's again BEFORE anything points at them;
    /// 4. the writer publishes the new location, tagged with the new
    ///    incarnation (`Segcache::insert`'s fresh-key arm).
    ///
    /// Steps 1 and 2 are what make the caller's captured
    /// `location_in(cell, generation)` stale; steps 3 and 4 are what make the
    /// verifier blind to it — the bytes at that address really do spell
    /// [`KEY`] again, so `verify` says yes and only the tag can say no.
    ///
    /// Returns whether step 1's sweep is what unlinked the outgoing entry —
    /// `false` means somebody else got there first, which is a legal race
    /// outcome, not a failure.
    pub(crate) fn recycle_and_refill(
        &self,
        ht: &MultiChoiceHashtable,
        cell: usize,
        generation: u16,
    ) -> bool {
        let swept = ht.remove(KEY, Self::location_in(cell, generation));
        self.vacate(cell);
        self.place(cell, KEY);
        ht.insert(KEY, Self::location_in(cell, generation + 1), self)
            .expect("the republish must find a slot");
        swept
    }

    /// Count the live hashtable entries for [`KEY`] across every modeled
    /// location — the duplicate detector for insert-path models.
    ///
    /// DESTRUCTIVE, and deliberately so: it counts by unlinking. `remove`
    /// matches on tag AND location, so each call removes at most one slot,
    /// and the inner loop catches the pathological case of two slots
    /// published with the same location. Call it once, after every thread
    /// has joined.
    ///
    /// Every cell is swept in every modeled incarnation: an entry published
    /// by incarnation 1 carries a different location word than the same
    /// address in incarnation 0, so sweeping only generation 0 would count a
    /// refilled entry as absent and turn a leak into a pass.
    ///
    /// Counting this way keeps the fixture out of `table.rs`'s private
    /// internals — the alternative is a hand-rolled bucket scan, which is
    /// what the `AlwaysVerifier` models copy-paste today.
    pub(crate) fn drain_live_entries(ht: &MultiChoiceHashtable) -> usize {
        let mut live = 0;
        for cell in 0..NUM_CELLS {
            for generation in 0..NUM_INCARNATIONS {
                while ht.remove(KEY, Self::location_in(cell, generation)) {
                    live += 1;
                }
            }
        }
        live
    }
}

impl KeyVerifier for KeyOracle {
    /// Answer from the CURRENT occupant of `location`, exactly as
    /// `SegmentsVerifier` answers from the bytes currently at that offset.
    ///
    /// A `false` here therefore carries the same ambiguity as production's:
    /// it may mean "different key", or it may mean "this location stopped
    /// being your entry's while you were asking". Resolving that ambiguity
    /// is what the slot protocol's STALE-LOCATION guard is for, and what
    /// these models exercise.
    ///
    /// TAG-BLIND, exactly like `SegmentsVerifier`: it addresses with
    /// `unpack_location`, which deliberately drops the incarnation tag
    /// because the tag is not part of the address. A location from a dead
    /// incarnation therefore verifies `true` whenever the current occupant
    /// happens to be the same key — which is the whole reason the tag has to
    /// be checked by somebody else.
    fn verify(&self, key: &[u8], location: Location, _allow_deleted: bool) -> bool {
        let (seg_id, offset) = crate::hashtable::unpack_location(location);
        if seg_id == 0 || seg_id as usize > NUM_CELLS || offset != 0 {
            return false;
        }
        let cell = seg_id as usize - 1;
        let occupant = self.cells[cell].load(Ordering::Acquire);
        occupant != 0 && occupant == key_id(key)
    }
}
