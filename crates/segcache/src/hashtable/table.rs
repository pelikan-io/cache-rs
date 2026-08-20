//! Lock-free N-choice hashtable implementation.
//!
//! Supports:
//! - Configurable N-choice hashing (1-8 choices) for tunable load factors
//! - ASFC (Adaptive Software Frequency Counter) for frequency tracking
//! - Ghost entries for preserving frequency after eviction
//! - Storage-agnostic location handling via KeyVerifier
//! - SIMD-accelerated bucket scanning on supported platforms

use crate::hashtable::bucket::Hashbucket;
use crate::hashtable::location::Location;
use crate::hashtable::traits::{Hashtable, KeyVerifier};
use crate::sync::{Mutex, Ordering};
use ahash::RandomState;
use core::hash::{BuildHasher, Hasher};
use crossbeam_utils::CachePadded;

/// Maximum number of bucket choices supported.
pub const MAX_CHOICES: u8 = 8;

/// A located hashtable slot: the bucket and slot index where `lookup_slot`
/// found a matching entry, plus the tag extracted from the key's hash so a
/// follow-up `cas_location_at` doesn't need to re-hash the key or re-probe
/// the candidate buckets.
///
/// A `SlotRef` is a *hint*, not a claim on the slot: `cas_location_at`
/// still validates that the slot currently encodes the expected
/// `old_location` before swapping it, exactly like `cas_location`'s probe
/// would. See `cas_location_at` for why a stale `SlotRef` can never cause
/// a CAS against the wrong entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SlotRef {
    bucket_index: usize,
    slot_index: usize,
    tag: u16,
}

/// Result of checking a key against the location published in a bucket slot.
///
/// See [`MultiChoiceHashtable::verify_slot`] for the STALE-LOCATION
/// INVARIANT that gives `DifferentKey` its meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotVerify {
    /// The location published in this slot holds the key.
    Match,
    /// The slot word was unchanged across the comparison, so the bytes read
    /// were this entry's throughout: the slot genuinely holds another key.
    DifferentKey,
    /// The slot word changed under the comparison, so the bytes read may
    /// have belonged to a recycled segment. Re-read THIS slot and retry;
    /// concluding "different key" here would be a false absent.
    Changed,
}

/// Lock-free hashtable for caches.
///
/// Each entry stores:
/// - 12-bit tag (hash suffix for fast filtering)
/// - 8-bit frequency counter (ASFC algorithm)
/// - 44-bit location (opaque, meaning defined by storage backend)
pub struct MultiChoiceHashtable {
    hash_builder: Box<RandomState>,
    buckets: Box<[Hashbucket]>,
    num_buckets: usize,
    mask: u64,
    num_choices: u8,
    /// Striped insert locks. Entry CREATION for a key (empty-slot claim,
    /// ghost takeover) is serialized per key-hash stripe with an
    /// under-lock absence re-check (see `insert`); entry MUTATION
    /// (replace, relocate, remove, ghost-convert) stays lock-free.
    ///
    /// LOCK: insert-stripe — leaf; the critical section is pure bucket-word CAS +
    /// verifier reads; it is never held across any other lock, pin
    /// acquisition, or wait.
    insert_locks: Box<[CachePadded<Mutex<()>>]>,
}

// SAFETY: All mutable state is behind AtomicU64 (bucket slots) or Mutex
// (insert stripes), both Sync; the raw-pointer-free remainder is immutable
// after construction.
unsafe impl Send for MultiChoiceHashtable {}
unsafe impl Sync for MultiChoiceHashtable {}

#[allow(dead_code)]
impl MultiChoiceHashtable {
    /// Insert-lock stripe count (power of two). Contention needs two
    /// concurrent FRESH inserts whose key hashes collide mod the stripe
    /// count — rare, and a collision costs a short wait, not correctness.
    /// Under loom the array shrinks (loom tracks every sync object) —
    /// but a stripe COLLISION between two keys in a loom model silently
    /// serializes them and shrinks the explored interleaving space, so
    /// multi-key loom models must assert their keys map to distinct
    /// stripes.
    const NUM_STRIPES: usize = if cfg!(feature = "loom") { 16 } else { 1024 };

    /// Create a new hashtable with two-choice hashing (default).
    ///
    /// # Parameters
    /// - `power`: Total item capacity is 2^power (8 slots per bucket, minimum power 7)
    pub fn new(power: u8) -> Self {
        Self::with_choices(power, 2)
    }

    /// Create a new hashtable with configurable N-choice hashing.
    ///
    /// # Parameters
    /// - `power`: Total item capacity is 2^power (8 slots per bucket, minimum power 7)
    /// - `num_choices`: Number of bucket choices (1-8)
    pub fn with_choices(power: u8, num_choices: u8) -> Self {
        assert!(power >= 7, "power must be at least 7 (128 slots)");
        assert!(
            (1..=MAX_CHOICES).contains(&num_choices),
            "num_choices must be 1-{}",
            MAX_CHOICES
        );

        // Use fixed seeds for deterministic behavior
        let hash_builder = RandomState::with_seeds(
            0xbb8c484891ec6c86,
            0x0522a25ae9c769f9,
            0xeed2797b9571bc75,
            0x4feb29c1fbbd59d0,
        );

        // 8 slots per bucket, so bucket count = 2^(power-3)
        let bucket_power = power - 3;
        let num_buckets = 1_usize << bucket_power;
        let mask = (num_buckets as u64) - 1;

        let buckets = (0..num_buckets)
            .map(|_| Hashbucket::new())
            .collect::<Vec<_>>()
            .into_boxed_slice();

        let insert_locks = (0..Self::NUM_STRIPES)
            .map(|_| CachePadded::new(Mutex::new(())))
            .collect::<Vec<_>>()
            .into_boxed_slice();

        Self {
            hash_builder: Box::new(hash_builder),
            buckets,
            num_buckets,
            mask,
            num_choices,
            insert_locks,
        }
    }

    /// Get a reference to the hash builder (used by S3-FIFO ghost queue).
    pub fn hash_builder(&self) -> &RandomState {
        &self.hash_builder
    }

    #[inline]
    fn bucket(&self, index: usize) -> &Hashbucket {
        debug_assert!(index < self.num_buckets);
        &self.buckets[index]
    }

    /// The insert stripe for a key hash (see `insert_locks`).
    #[inline]
    fn stripe(&self, hash: u64) -> &Mutex<()> {
        &self.insert_locks[(hash as usize) & (Self::NUM_STRIPES - 1)]
    }

    /// Prefetch a bucket into cache.
    #[inline]
    fn prefetch_bucket(&self, index: usize) {
        debug_assert!(index < self.num_buckets);
        let bucket_ptr = &self.buckets[index] as *const Hashbucket as *const i8;

        #[cfg(all(target_arch = "x86_64", target_feature = "sse"))]
        unsafe {
            std::arch::x86_64::_mm_prefetch::<{ std::arch::x86_64::_MM_HINT_T0 }>(bucket_ptr);
        }

        #[cfg(target_arch = "aarch64")]
        unsafe {
            std::arch::asm!(
                "prfm pldl1keep, [{ptr}]",
                ptr = in(reg) bucket_ptr,
                options(nostack, preserves_flags)
            );
        }

        #[cfg(not(any(
            all(target_arch = "x86_64", target_feature = "sse"),
            target_arch = "aarch64"
        )))]
        let _ = bucket_ptr;
    }

    /// Compute hash for a key.
    #[inline]
    fn hash_key(&self, key: &[u8]) -> u64 {
        let mut hasher = self.hash_builder.build_hasher();
        hasher.write(key);
        hasher.finish()
    }

    /// Compute bucket indices for N-choice hashing.
    #[inline]
    fn bucket_indices(&self, hash: u64) -> [usize; MAX_CHOICES as usize] {
        let mask = self.mask;
        [
            (hash & mask) as usize,
            ((hash ^ (hash >> 32)) & mask) as usize,
            (((hash >> 16) ^ (hash << 16)) & mask) as usize,
            (((hash >> 48) ^ (hash >> 8) ^ hash) & mask) as usize,
            ((hash.rotate_left(17) ^ hash) & mask) as usize,
            ((hash.rotate_left(31) ^ (hash >> 16)) & mask) as usize,
            ((hash.wrapping_mul(0x9E3779B97F4A7C15) >> 32) & mask) as usize,
            ((hash.wrapping_mul(0x517CC1B727220A95) >> 32) & mask) as usize,
        ]
    }

    /// Extract tag from hash.
    #[inline]
    fn tag_from_hash(hash: u64) -> u16 {
        ((hash >> 32) & 0xFFF) as u16
    }

    /// Hash a key once and derive its raw hash, tag, and N-choice bucket
    /// indices. The raw hash also selects the insert stripe (see `insert`).
    #[inline]
    fn probe_with_hash(&self, key: &[u8]) -> (u64, u16, [usize; MAX_CHOICES as usize]) {
        let hash = self.hash_key(key);
        (hash, Self::tag_from_hash(hash), self.bucket_indices(hash))
    }

    /// Hash a key once and derive its tag and N-choice bucket indices.
    ///
    /// Every keyed operation starts here, so the single hash and its
    /// expansion into candidate buckets live in one place.
    #[inline]
    fn probe(&self, key: &[u8]) -> (u16, [usize; MAX_CHOICES as usize]) {
        let (_hash, tag, buckets) = self.probe_with_hash(key);
        (tag, buckets)
    }

    /// Count occupied (non-empty, non-ghost) slots in a bucket.
    #[inline]
    fn count_occupied(&self, bucket_index: usize) -> usize {
        let bucket = self.bucket(bucket_index);
        let mut count = 0;
        for slot in &bucket.items {
            let packed = slot.load(Ordering::Relaxed);
            if packed != 0 && !Hashbucket::is_ghost(packed) {
                count += 1;
            }
        }
        count
    }

    // =========================================================================
    // SIMD tag scanning
    // =========================================================================

    /// Find slots with matching tags using SIMD (AVX2).
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2", not(feature = "loom")))]
    #[inline]
    fn find_tag_matches_simd(bucket: &Hashbucket, tag_shifted: u64) -> u8 {
        use std::arch::x86_64::*;

        unsafe {
            let items_ptr = bucket.items.as_ptr() as *const u8;

            let slots_0_3 = _mm256_load_si256(items_ptr as *const __m256i);
            let slots_4_7 = _mm256_load_si256(items_ptr.add(32) as *const __m256i);

            let tag_mask_val = 0xFFF0_0000_0000_0000_u64 as i64;
            let tag_shifted_i64 = tag_shifted as i64;

            let tag_mask = _mm256_set1_epi64x(tag_mask_val);
            let tag_vec = _mm256_set1_epi64x(tag_shifted_i64);

            let ghost_mask_val = 0x0000_0FFF_FFFF_FFFF_u64 as i64;
            let ghost_vec = _mm256_set1_epi64x(ghost_mask_val);
            let zero = _mm256_setzero_si256();
            let all_ones = _mm256_set1_epi64x(-1);

            let tags_0_3 = _mm256_and_si256(slots_0_3, tag_mask);
            let tag_match_0_3 = _mm256_cmpeq_epi64(tags_0_3, tag_vec);
            let nonzero_0_3 = _mm256_xor_si256(_mm256_cmpeq_epi64(slots_0_3, zero), all_ones);
            let locs_0_3 = _mm256_and_si256(slots_0_3, _mm256_set1_epi64x(ghost_mask_val));
            let nonghost_0_3 = _mm256_xor_si256(_mm256_cmpeq_epi64(locs_0_3, ghost_vec), all_ones);
            let valid_0_3 =
                _mm256_and_si256(tag_match_0_3, _mm256_and_si256(nonzero_0_3, nonghost_0_3));

            let tags_4_7 = _mm256_and_si256(slots_4_7, tag_mask);
            let tag_match_4_7 = _mm256_cmpeq_epi64(tags_4_7, tag_vec);
            let nonzero_4_7 = _mm256_xor_si256(_mm256_cmpeq_epi64(slots_4_7, zero), all_ones);
            let locs_4_7 = _mm256_and_si256(slots_4_7, _mm256_set1_epi64x(ghost_mask_val));
            let nonghost_4_7 = _mm256_xor_si256(_mm256_cmpeq_epi64(locs_4_7, ghost_vec), all_ones);
            let valid_4_7 =
                _mm256_and_si256(tag_match_4_7, _mm256_and_si256(nonzero_4_7, nonghost_4_7));

            let mask_0_3 = _mm256_movemask_pd(_mm256_castsi256_pd(valid_0_3)) as u8;
            let mask_4_7 = _mm256_movemask_pd(_mm256_castsi256_pd(valid_4_7)) as u8;

            mask_0_3 | (mask_4_7 << 4)
        }
    }

    /// Find slots with matching tags using NEON (ARM64).
    #[cfg(all(target_arch = "aarch64", not(feature = "loom")))]
    #[inline]
    fn find_tag_matches_simd(bucket: &Hashbucket, tag_shifted: u64) -> u8 {
        use std::arch::aarch64::*;

        const TAG_MASK: u64 = 0xFFF0_0000_0000_0000;
        const GHOST_LOCATION: u64 = 0x0000_0FFF_FFFF_FFFF;

        unsafe {
            let items_ptr = bucket.items.as_ptr() as *const u64;

            let slots_0_1: uint64x2_t;
            let slots_2_3: uint64x2_t;
            let slots_4_5: uint64x2_t;
            let slots_6_7: uint64x2_t;

            std::arch::asm!(
                "ld1 {{{v0:v}.2d}}, [{p0}]",
                "ld1 {{{v1:v}.2d}}, [{p1}]",
                "ld1 {{{v2:v}.2d}}, [{p2}]",
                "ld1 {{{v3:v}.2d}}, [{p3}]",
                p0 = in(reg) items_ptr,
                p1 = in(reg) items_ptr.add(2),
                p2 = in(reg) items_ptr.add(4),
                p3 = in(reg) items_ptr.add(6),
                v0 = out(vreg) slots_0_1,
                v1 = out(vreg) slots_2_3,
                v2 = out(vreg) slots_4_5,
                v3 = out(vreg) slots_6_7,
                options(nostack, preserves_flags),
            );

            let tag_mask_vec = vdupq_n_u64(TAG_MASK);
            let tag_vec = vdupq_n_u64(tag_shifted);
            let ghost_vec = vdupq_n_u64(GHOST_LOCATION);
            let zero_vec = vdupq_n_u64(0);

            let tags_0_1 = vandq_u64(slots_0_1, tag_mask_vec);
            let tag_match_0_1 = vceqq_u64(tags_0_1, tag_vec);
            let nonzero_0_1 = vmvnq_u32(vreinterpretq_u32_u64(vceqq_u64(slots_0_1, zero_vec)));
            let locs_0_1 = vandq_u64(slots_0_1, vdupq_n_u64(GHOST_LOCATION));
            let nonghost_0_1 = vmvnq_u32(vreinterpretq_u32_u64(vceqq_u64(locs_0_1, ghost_vec)));
            let valid_0_1 = vandq_u32(
                vreinterpretq_u32_u64(tag_match_0_1),
                vandq_u32(nonzero_0_1, nonghost_0_1),
            );

            let tags_2_3 = vandq_u64(slots_2_3, tag_mask_vec);
            let tag_match_2_3 = vceqq_u64(tags_2_3, tag_vec);
            let nonzero_2_3 = vmvnq_u32(vreinterpretq_u32_u64(vceqq_u64(slots_2_3, zero_vec)));
            let locs_2_3 = vandq_u64(slots_2_3, vdupq_n_u64(GHOST_LOCATION));
            let nonghost_2_3 = vmvnq_u32(vreinterpretq_u32_u64(vceqq_u64(locs_2_3, ghost_vec)));
            let valid_2_3 = vandq_u32(
                vreinterpretq_u32_u64(tag_match_2_3),
                vandq_u32(nonzero_2_3, nonghost_2_3),
            );

            let tags_4_5 = vandq_u64(slots_4_5, tag_mask_vec);
            let tag_match_4_5 = vceqq_u64(tags_4_5, tag_vec);
            let nonzero_4_5 = vmvnq_u32(vreinterpretq_u32_u64(vceqq_u64(slots_4_5, zero_vec)));
            let locs_4_5 = vandq_u64(slots_4_5, vdupq_n_u64(GHOST_LOCATION));
            let nonghost_4_5 = vmvnq_u32(vreinterpretq_u32_u64(vceqq_u64(locs_4_5, ghost_vec)));
            let valid_4_5 = vandq_u32(
                vreinterpretq_u32_u64(tag_match_4_5),
                vandq_u32(nonzero_4_5, nonghost_4_5),
            );

            let tags_6_7 = vandq_u64(slots_6_7, tag_mask_vec);
            let tag_match_6_7 = vceqq_u64(tags_6_7, tag_vec);
            let nonzero_6_7 = vmvnq_u32(vreinterpretq_u32_u64(vceqq_u64(slots_6_7, zero_vec)));
            let locs_6_7 = vandq_u64(slots_6_7, vdupq_n_u64(GHOST_LOCATION));
            let nonghost_6_7 = vmvnq_u32(vreinterpretq_u32_u64(vceqq_u64(locs_6_7, ghost_vec)));
            let valid_6_7 = vandq_u32(
                vreinterpretq_u32_u64(tag_match_6_7),
                vandq_u32(nonzero_6_7, nonghost_6_7),
            );

            let v0_1 = vreinterpretq_u64_u32(valid_0_1);
            let v2_3 = vreinterpretq_u64_u32(valid_2_3);
            let v4_5 = vreinterpretq_u64_u32(valid_4_5);
            let v6_7 = vreinterpretq_u64_u32(valid_6_7);

            let r0 = (vgetq_lane_u64(v0_1, 0) >> 63) as u8;
            let r1 = ((vgetq_lane_u64(v0_1, 1) >> 63) << 1) as u8;
            let r2 = ((vgetq_lane_u64(v2_3, 0) >> 63) << 2) as u8;
            let r3 = ((vgetq_lane_u64(v2_3, 1) >> 63) << 3) as u8;
            let r4 = ((vgetq_lane_u64(v4_5, 0) >> 63) << 4) as u8;
            let r5 = ((vgetq_lane_u64(v4_5, 1) >> 63) << 5) as u8;
            let r6 = ((vgetq_lane_u64(v6_7, 0) >> 63) << 6) as u8;
            let r7 = ((vgetq_lane_u64(v6_7, 1) >> 63) << 7) as u8;

            r0 | r1 | r2 | r3 | r4 | r5 | r6 | r7
        }
    }

    /// Scalar fallback for finding tag matches.
    #[cfg(any(
        feature = "loom",
        not(any(
            all(target_arch = "x86_64", target_feature = "avx2"),
            target_arch = "aarch64"
        ))
    ))]
    #[inline]
    fn find_tag_matches_simd(bucket: &Hashbucket, tag_shifted: u64) -> u8 {
        const TAG_MASK: u64 = 0xFFF0_0000_0000_0000;
        const GHOST_LOCATION: u64 = 0x0000_0FFF_FFFF_FFFF;

        let mut result = 0u8;
        for slot_index in 0..8 {
            let packed = bucket.items[slot_index].load(Ordering::Relaxed);
            if packed != 0
                && (packed & GHOST_LOCATION) != GHOST_LOCATION
                && (packed & TAG_MASK) == tag_shifted
            {
                result |= 1 << slot_index;
            }
        }
        result
    }

    // =========================================================================
    // Bucket-level search helpers
    // =========================================================================

    /// Verify `key` against the location encoded in `packed`, guarding the
    /// comparison against a stale location.
    ///
    /// # STALE-LOCATION INVARIANT
    ///
    /// Every production `verify` call site goes through here. `verify`
    /// compares key bytes by reading raw storage at the location it is
    /// handed, holding no pin and no generation tag. Between the load of
    /// `packed` and that read, a merge drain can relocate the entry and the
    /// old segment can be finalized, recycled, and rewritten by another
    /// writer. So a `false` from `verify` does NOT by itself mean "different
    /// key" — it can equally mean "the bytes at that location stopped being
    /// this entry's while we were looking at them".
    ///
    /// Re-reading the slot separates the two cases. It is sound because:
    ///
    /// (a) A published entry's location cannot have been recycled while its
    ///     slot still points at it: a drain unlinks or ghosts EVERY entry it
    ///     drains (`try_unlink_in_bucket` / `try_to_ghost_in_bucket`) before
    ///     the source segment may be finalized and reused. #46's same-slot
    ///     CAS retry is what makes that reliable — a lost race there would
    ///     leave a dangling entry and break this clause.
    /// (b) Relocation mutates the slot IN PLACE (`cas_location` /
    ///     `cas_location_at`), so it always surfaces as a changed word.
    ///
    /// Together: slot word unchanged across the verify implies the location
    /// was continuously published, implies the bytes compared were this
    /// entry's throughout, implies `false` really is a different key. If (a)
    /// or (b) ever stops holding, EVERY caller of this function becomes
    /// unsound at once — which is the point of routing them all through one
    /// place.
    ///
    /// Termination: a [`SlotVerify::Changed`] retry is paid for by another
    /// thread's successful `Release` CAS on this exact slot, so retries are
    /// bounded by real system progress rather than spinning on a stable
    /// word. The one repeatable mutation, a frequency bump, saturates
    /// (probabilistic above 16, hard cap 127).
    ///
    /// Accepted ABA residual — the same class the `cas_location` retry loops
    /// accept: a byte-identical `packed` re-published into the SAME slot
    /// between the two loads reads as unchanged. That needs a full unlink ->
    /// recycle -> republish carrying the same tag, freq, AND location value
    /// between two adjacent loads. Generation-tagged locations are the
    /// broader fix, tracked separately.
    ///
    /// Cost: one extra `Acquire` load, on the verify-FAILURE path only —
    /// reached only via a 12-bit tag collision (~1/4096 per examined slot)
    /// or the race above, so it is off the read hot path.
    #[inline]
    fn verify_slot(
        bucket: &Hashbucket,
        slot_index: usize,
        packed: u64,
        key: &[u8],
        allow_deleted: bool,
        verifier: &impl KeyVerifier,
    ) -> SlotVerify {
        let location = Hashbucket::location(packed);

        if verifier.verify(key, location, allow_deleted) {
            return SlotVerify::Match;
        }

        let outcome = Self::classify_failed_verify(bucket, slot_index, packed);

        // Turn the invariant into a checked PRECONDITION rather than only
        // testing for its visible failure. `DifferentKey` claims the slot
        // held `location` continuously, so `location`'s key bytes were
        // stable — a published item's key is immutable in place (delete's
        // `set_deleted` runs under a remover pin and touches only the
        // header; numeric updates rewrite value/CRC, never the key). Then a
        // second verify MUST agree. Disagreement means clause (a) or (b)
        // above no longer holds — a dangling entry, or a relocation that
        // did not go through the slot — and the "unchanged slot" conclusion
        // is unsound. Debug builds only, and only on the already-cold
        // failure path.
        #[cfg(debug_assertions)]
        if outcome == SlotVerify::DifferentKey {
            debug_assert!(
                !verifier.verify(key, location, allow_deleted),
                "STALE-LOCATION INVARIANT violated: slot word unchanged across two \
                 verifies that disagree, so an unchanged slot no longer proves the \
                 compared bytes were this entry's"
            );
        }

        outcome
    }

    /// Failure half of [`Self::verify_slot`]: decide whether a `false` from
    /// `verify` was a real key mismatch or a stale-location read.
    ///
    /// Split out and marked `#[cold]` so the re-load is laid out off the
    /// read hot path — on a hit, `verify_slot` is just the comparison. A
    /// measured ~1-2% `get` regression on the merged form is what motivated
    /// the split; keep the attributes if you touch this.
    #[cold]
    #[inline(never)]
    fn classify_failed_verify(bucket: &Hashbucket, slot_index: usize, packed: u64) -> SlotVerify {
        if bucket.items[slot_index].load(Ordering::Acquire) == packed {
            SlotVerify::DifferentKey
        } else {
            SlotVerify::Changed
        }
    }

    /// Search a bucket for an item, updating frequency on hit.
    #[inline]
    fn search_bucket_for_get(
        &self,
        bucket_index: usize,
        tag: u16,
        key: &[u8],
        verifier: &impl KeyVerifier,
    ) -> Option<(Location, u8)> {
        let bucket = self.bucket(bucket_index);
        let tag_shifted = (tag as u64) << 52;

        let mut mask = Self::find_tag_matches_simd(bucket, tag_shifted);

        while mask != 0 {
            let slot_index = mask.trailing_zeros() as usize;
            mask &= mask - 1;

            // Re-read THIS slot on a `Changed` verify (STALE-LOCATION
            // INVARIANT, see `verify_slot`): giving up here instead would
            // end the scan and report a false absent for a live key.
            loop {
                let packed = bucket.items[slot_index].load(Ordering::Acquire);

                if packed == 0 || Hashbucket::is_ghost(packed) {
                    break;
                }
                if (packed & 0xFFF0_0000_0000_0000) != tag_shifted {
                    break;
                }

                let location = Hashbucket::location(packed);
                verifier.prefetch(location);

                match Self::verify_slot(bucket, slot_index, packed, key, false, verifier) {
                    SlotVerify::Changed => continue,
                    SlotVerify::DifferentKey => break,
                    SlotVerify::Match => {}
                }

                let freq = Hashbucket::freq(packed);
                if freq < 127 {
                    if let Some(new_packed) = Hashbucket::try_update_freq(packed, freq) {
                        let _ = bucket.items[slot_index].compare_exchange(
                            packed,
                            new_packed,
                            Ordering::Release,
                            Ordering::Relaxed,
                        );
                    }
                }

                return Some((location, freq));
            }
        }

        None
    }

    /// Search a bucket for an item WITHOUT updating frequency.
    #[inline]
    fn search_bucket_no_freq(
        &self,
        bucket_index: usize,
        tag: u16,
        key: &[u8],
        verifier: &impl KeyVerifier,
    ) -> Option<(Location, u8)> {
        let bucket = self.bucket(bucket_index);
        let tag_shifted = (tag as u64) << 52;

        let mut mask = Self::find_tag_matches_simd(bucket, tag_shifted);

        while mask != 0 {
            let slot_index = mask.trailing_zeros() as usize;
            mask &= mask - 1;

            // Same-slot retry on a `Changed` verify — STALE-LOCATION
            // INVARIANT, see `verify_slot`.
            loop {
                let packed = bucket.items[slot_index].load(Ordering::Acquire);

                if packed == 0 || Hashbucket::is_ghost(packed) {
                    break;
                }
                if (packed & 0xFFF0_0000_0000_0000) != tag_shifted {
                    break;
                }

                let location = Hashbucket::location(packed);
                verifier.prefetch(location);

                match Self::verify_slot(bucket, slot_index, packed, key, false, verifier) {
                    SlotVerify::Changed => continue,
                    SlotVerify::DifferentKey => break,
                    SlotVerify::Match => return Some((location, Hashbucket::freq(packed))),
                }
            }
        }

        None
    }

    /// Search a bucket for an item WITHOUT updating frequency, also
    /// returning the slot index of the match so the caller can go
    /// straight back to it later (feeds `lookup_slot` / `cas_location_at`).
    #[inline]
    fn search_bucket_no_freq_slot(
        &self,
        bucket_index: usize,
        tag: u16,
        key: &[u8],
        verifier: &impl KeyVerifier,
    ) -> Option<(Location, usize)> {
        let bucket = self.bucket(bucket_index);
        let tag_shifted = (tag as u64) << 52;

        let mut mask = Self::find_tag_matches_simd(bucket, tag_shifted);

        while mask != 0 {
            let slot_index = mask.trailing_zeros() as usize;
            mask &= mask - 1;

            // Same-slot retry on a `Changed` verify — STALE-LOCATION
            // INVARIANT, see `verify_slot`.
            loop {
                let packed = bucket.items[slot_index].load(Ordering::Acquire);

                if packed == 0 || Hashbucket::is_ghost(packed) {
                    break;
                }
                if (packed & 0xFFF0_0000_0000_0000) != tag_shifted {
                    break;
                }

                let location = Hashbucket::location(packed);
                verifier.prefetch(location);

                match Self::verify_slot(bucket, slot_index, packed, key, false, verifier) {
                    SlotVerify::Changed => continue,
                    SlotVerify::DifferentKey => break,
                    SlotVerify::Match => return Some((location, slot_index)),
                }
            }
        }

        None
    }

    /// Search a bucket for existence (no frequency update).
    fn search_bucket_exists(
        &self,
        bucket_index: usize,
        tag: u16,
        key: &[u8],
        verifier: &impl KeyVerifier,
    ) -> bool {
        let bucket = self.bucket(bucket_index);
        let tag_shifted = (tag as u64) << 52;

        let mut mask = Self::find_tag_matches_simd(bucket, tag_shifted);

        while mask != 0 {
            let slot_index = mask.trailing_zeros() as usize;
            mask &= mask - 1;

            // Same-slot retry on a `Changed` verify — STALE-LOCATION
            // INVARIANT, see `verify_slot`. `contains` is an exact-answer
            // query, not a cheap approximate probe, so it gets the same
            // guard as `lookup`: a false `false` here is the same bug.
            loop {
                let packed = bucket.items[slot_index].load(Ordering::Acquire);

                if packed == 0 || Hashbucket::is_ghost(packed) {
                    break;
                }
                if (packed & 0xFFF0_0000_0000_0000) != tag_shifted {
                    break;
                }

                let location = Hashbucket::location(packed);
                verifier.prefetch(location);

                match Self::verify_slot(bucket, slot_index, packed, key, false, verifier) {
                    SlotVerify::Changed => continue,
                    SlotVerify::DifferentKey => break,
                    SlotVerify::Match => return true,
                }
            }
        }

        false
    }

    /// Search for a ghost entry's frequency.
    fn search_bucket_for_ghost(&self, bucket_index: usize, tag: u16) -> Option<u8> {
        let bucket = self.bucket(bucket_index);

        for slot_index in 0..Hashbucket::NUM_ITEM_SLOTS {
            let speculative = bucket.items[slot_index].load(Ordering::Relaxed);

            if Hashbucket::is_ghost(speculative) && Hashbucket::tag(speculative) == tag {
                let packed = bucket.items[slot_index].load(Ordering::Acquire);
                if Hashbucket::is_ghost(packed) && Hashbucket::tag(packed) == tag {
                    return Some(Hashbucket::freq(packed));
                }
            }
        }

        None
    }

    /// Increment frequency of ghost entries with matching tag.
    fn increment_ghost_freq_in_bucket(&self, bucket_index: usize, tag: u16) {
        let bucket = self.bucket(bucket_index);

        for slot_index in 0..Hashbucket::NUM_ITEM_SLOTS {
            let packed = bucket.items[slot_index].load(Ordering::Acquire);

            if packed != 0 && Hashbucket::is_ghost(packed) && Hashbucket::tag(packed) == tag {
                let freq = Hashbucket::freq(packed);
                if freq < 127 {
                    if let Some(new_packed) = Hashbucket::try_update_freq(packed, freq) {
                        let _ = bucket.items[slot_index].compare_exchange(
                            packed,
                            new_packed,
                            Ordering::Release,
                            Ordering::Relaxed,
                        );
                    }
                }
            }
        }
    }

    /// Search for frequency of a specific item.
    fn search_bucket_for_freq(
        &self,
        bucket_index: usize,
        tag: u16,
        key: &[u8],
        verifier: &impl KeyVerifier,
    ) -> Option<u8> {
        let bucket = self.bucket(bucket_index);

        for slot_index in 0..Hashbucket::NUM_ITEM_SLOTS {
            let speculative = bucket.items[slot_index].load(Ordering::Relaxed);

            if speculative == 0 || Hashbucket::is_ghost(speculative) {
                continue;
            }

            if Hashbucket::tag(speculative) != tag {
                continue;
            }

            // Same-slot retry on a `Changed` verify — STALE-LOCATION
            // INVARIANT, see `verify_slot`. A false `None` here would feed
            // the eviction policy a wrong frequency, so it is guarded like
            // the lookup paths rather than left approximate.
            loop {
                let packed = bucket.items[slot_index].load(Ordering::Acquire);
                if packed == 0 || Hashbucket::is_ghost(packed) || Hashbucket::tag(packed) != tag {
                    break;
                }

                match Self::verify_slot(bucket, slot_index, packed, key, false, verifier) {
                    SlotVerify::Changed => continue,
                    SlotVerify::DifferentKey => break,
                    SlotVerify::Match => return Some(Hashbucket::freq(packed)),
                }
            }
        }

        None
    }

    /// Search for frequency by exact location.
    fn search_bucket_for_item_freq(
        &self,
        bucket_index: usize,
        tag: u16,
        location: Location,
    ) -> Option<u8> {
        let bucket = self.bucket(bucket_index);

        for slot_index in 0..Hashbucket::NUM_ITEM_SLOTS {
            let speculative = bucket.items[slot_index].load(Ordering::Relaxed);

            if speculative == 0 || Hashbucket::is_ghost(speculative) {
                continue;
            }

            if Hashbucket::tag(speculative) == tag {
                let packed = bucket.items[slot_index].load(Ordering::Acquire);
                if packed == 0 || Hashbucket::is_ghost(packed) {
                    continue;
                }

                if Hashbucket::tag(packed) == tag && Hashbucket::location(packed) == location {
                    return Some(Hashbucket::freq(packed));
                }
            }
        }

        None
    }

    // =========================================================================
    // Insert / remove helpers
    // =========================================================================

    /// Replace the key's existing LIVE entry in this bucket, if present,
    /// via a same-slot CAS retry loop (item 7f, F4: on a matching-slot CAS
    /// failure, re-read the SAME slot — a racing same-key writer's update
    /// must be seen, never skipped).
    ///
    /// Ghost slots are deliberately NOT taken here: taking over a ghost
    /// CREATES a live entry for the key, and all entry creation is
    /// serialized under the insert stripe lock (`try_claim_new_slot`).
    /// Without that split, two racing fresh inserters could each take over
    /// a different same-tag ghost (one per candidate bucket) and publish a
    /// duplicate on the lock-free path.
    ///
    /// Every successful `compare_exchange` publishes with `Release`: it is
    /// the linearization point exposing a location to readers, ordering
    /// the item bytes written by reserve/define ahead of it (concurrent-
    /// reserve spec §4).
    ///
    /// Returns `Some(old_location)` if this call replaced a live entry,
    /// `None` if this bucket holds no live entry for the key.
    fn try_replace_existing(
        &self,
        bucket_index: usize,
        tag: u16,
        key: &[u8],
        new_packed: u64,
        verifier: &impl KeyVerifier,
    ) -> Option<Location> {
        let bucket = self.bucket(bucket_index);

        for slot_index in 0..Hashbucket::NUM_ITEM_SLOTS {
            loop {
                let packed = bucket.items[slot_index].load(Ordering::Acquire);

                if packed == 0 || Hashbucket::tag(packed) != tag || Hashbucket::is_ghost(packed) {
                    break; // empty, or not a live entry with our tag — next slot
                }

                let location = Hashbucket::location(packed);

                // `packed` may be stale: a racing same-key relocation moved
                // this entry and `location`'s bytes were recycled, so verify
                // falsely reports "different key". STALE-LOCATION INVARIANT,
                // see `verify_slot` — this was the first site guarded (#46);
                // the read paths now share the same guard.
                match Self::verify_slot(bucket, slot_index, packed, key, true, verifier) {
                    SlotVerify::Changed => continue, // slot changed — re-read THIS slot
                    SlotVerify::DifferentKey => break, // genuinely a different key
                    SlotVerify::Match => {}
                }

                let freq = Hashbucket::freq(packed);
                let new_with_freq = Hashbucket::with_freq(new_packed, freq);

                match bucket.items[slot_index].compare_exchange(
                    packed,
                    new_with_freq,
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return Some(location),
                    // Re-read THIS slot — a racing same-key writer changed it.
                    Err(_) => continue,
                }
            }
        }

        None
    }

    /// Claim a NEW live entry for the key in this bucket: a matching-tag
    /// ghost first (freq-preserving takeover), then an empty slot, then
    /// any ghost. Returns true if a slot was claimed.
    ///
    /// Entry creation only — the caller (`insert`) has already established
    /// that no live entry for the key exists and holds the key's insert
    /// stripe lock while calling this.
    fn try_claim_new_slot(&self, bucket_index: usize, tag: u16, new_packed: u64) -> bool {
        let bucket = self.bucket(bucket_index);

        // Matching-tag ghost: take it over, preserving its frequency.
        for slot_index in 0..Hashbucket::NUM_ITEM_SLOTS {
            loop {
                let packed = bucket.items[slot_index].load(Ordering::Acquire);

                if Hashbucket::tag(packed) != tag || !Hashbucket::is_ghost(packed) {
                    break; // next slot
                }

                let freq = Hashbucket::freq(packed);
                let new_with_freq = Hashbucket::with_freq(new_packed, freq);

                match bucket.items[slot_index].compare_exchange(
                    packed,
                    new_with_freq,
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return true,
                    Err(_) => continue, // re-read THIS slot
                }
            }
        }

        // Empty slot.
        for slot_index in 0..Hashbucket::NUM_ITEM_SLOTS {
            let packed = bucket.items[slot_index].load(Ordering::Relaxed);

            if packed == 0 {
                match bucket.items[slot_index].compare_exchange(
                    0,
                    new_packed,
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return true,
                    Err(_) => continue,
                }
            }
        }

        // Any ghost (evict it).
        for slot_index in 0..Hashbucket::NUM_ITEM_SLOTS {
            let speculative = bucket.items[slot_index].load(Ordering::Relaxed);

            if Hashbucket::is_ghost(speculative) {
                let packed = bucket.items[slot_index].load(Ordering::Acquire);

                if Hashbucket::is_ghost(packed) {
                    match bucket.items[slot_index].compare_exchange(
                        packed,
                        new_packed,
                        Ordering::Release,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => return true,
                        Err(_) => continue,
                    }
                }
            }
        }

        false // bucket full of live entries
    }

    /// Try to unlink an item from a bucket.
    ///
    /// Same-slot CAS retry (item 7f, F4), and for the same reason as
    /// `try_replace_existing`: a warm reader bumps the frequency counter
    /// with a CAS on this very word (`search_bucket_for_get`, on every
    /// hit while freq <= 16), so a CAS failure here does NOT imply
    /// another mutator took the entry. Advancing to the next slot on such
    /// a failure would abandon a live entry while reporting `false` —
    /// which `Segment::clear` reads as "another unlinker owns it",
    /// letting a segment be recycled with a still-published entry.
    ///
    /// Termination: every retry is paid for by another thread's
    /// successful CAS on this word, and freq bumps saturate (probabilistic
    /// above 16, hard cap 127), so the spin is bounded.
    fn try_unlink_in_bucket(&self, bucket_index: usize, tag: u16, expected: Location) -> bool {
        let bucket = self.bucket(bucket_index);

        for slot_index in 0..Hashbucket::NUM_ITEM_SLOTS {
            loop {
                let packed = bucket.items[slot_index].load(Ordering::Acquire);

                if packed == 0
                    || Hashbucket::is_ghost(packed)
                    || Hashbucket::tag(packed) != tag
                    || Hashbucket::location(packed) != expected
                {
                    break; // not our entry (any more) — next slot
                }

                match bucket.items[slot_index].compare_exchange(
                    packed,
                    0,
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return true,
                    // Re-read THIS slot: most likely just a freq bump.
                    Err(_) => continue,
                }
            }
        }

        false
    }

    /// Try to convert an item to ghost in a bucket.
    ///
    /// Same-slot CAS retry, same rationale and termination argument as
    /// `try_unlink_in_bucket`: a racing freq bump must not cost us the
    /// entry. The ghost word is recomputed from the FRESH packed on every
    /// attempt so a bump that landed in between is preserved rather than
    /// rolled back.
    fn try_to_ghost_in_bucket(&self, bucket_index: usize, tag: u16, expected: Location) -> bool {
        let bucket = self.bucket(bucket_index);

        for slot_index in 0..Hashbucket::NUM_ITEM_SLOTS {
            loop {
                let packed = bucket.items[slot_index].load(Ordering::Acquire);

                if packed == 0
                    || Hashbucket::is_ghost(packed)
                    || Hashbucket::tag(packed) != tag
                    || Hashbucket::location(packed) != expected
                {
                    break; // not our entry (any more) — next slot
                }

                let ghost = Hashbucket::to_ghost(packed);

                match bucket.items[slot_index].compare_exchange(
                    packed,
                    ghost,
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return true,
                    // Re-read THIS slot: most likely just a freq bump.
                    Err(_) => continue,
                }
            }
        }

        false
    }

    /// Try to CAS update location in a bucket. Same publish reasoning as
    /// `try_replace_existing`: the success ordering below is Release, which
    /// orders the new item's reserve/define byte writes ahead of the
    /// location becoming visible to readers.
    ///
    /// Same-slot CAS retry, mirroring `cas_location_at` (the direct-slot
    /// sibling of this probe): the new packed value is recomputed from
    /// the fresh freq on every attempt, so a racing reader's freq bump
    /// costs a retry rather than the relocation — abandoning it here
    /// would abort a merge mid-candidate.
    ///
    /// Termination: as in `try_unlink_in_bucket` — each retry is paid for
    /// by another thread's successful CAS, and freq bumps saturate.
    fn try_cas_in_bucket(
        &self,
        bucket_index: usize,
        tag: u16,
        old_location: Location,
        new_location: Location,
        preserve_freq: bool,
    ) -> bool {
        let bucket = self.bucket(bucket_index);

        for slot_index in 0..Hashbucket::NUM_ITEM_SLOTS {
            loop {
                let packed = bucket.items[slot_index].load(Ordering::Acquire);

                if packed == 0
                    || Hashbucket::is_ghost(packed)
                    || Hashbucket::tag(packed) != tag
                    || Hashbucket::location(packed) != old_location
                {
                    break; // not our entry (any more) — next slot
                }

                let freq = if preserve_freq {
                    Hashbucket::freq(packed)
                } else {
                    1
                };
                let new_packed = Hashbucket::pack(tag, freq, new_location);

                match bucket.items[slot_index].compare_exchange(
                    packed,
                    new_packed,
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return true,
                    // Re-read THIS slot: most likely just a freq bump.
                    Err(_) => continue,
                }
            }
        }

        false
    }

    /// Look up a key without updating frequency, also returning a
    /// `SlotRef` pinpointing where the match was found. A follow-up
    /// `cas_location_at(slot, ...)` can then swap that exact slot without
    /// re-hashing the key or re-probing its candidate buckets — the
    /// second-probe cost `cas_location` pays when called right after a
    /// `lookup_no_freq_update` for the same key.
    ///
    /// Same miss/hit semantics as `lookup_no_freq_update`: only live
    /// (non-ghost) entries are returned.
    pub(crate) fn lookup_slot(
        &self,
        key: &[u8],
        verifier: &impl KeyVerifier,
    ) -> Option<(Location, SlotRef)> {
        let (tag, buckets) = self.probe(key);
        let num_choices = self.num_choices as usize;

        for &bucket_index in &buckets[..num_choices] {
            self.prefetch_bucket(bucket_index);
        }

        for &bucket_index in &buckets[..num_choices] {
            if let Some((location, slot_index)) =
                self.search_bucket_no_freq_slot(bucket_index, tag, key, verifier)
            {
                return Some((
                    location,
                    SlotRef {
                        bucket_index,
                        slot_index,
                        tag,
                    },
                ));
            }
        }

        None
    }

    /// CAS an item's location directly at a slot located by `lookup_slot`,
    /// skipping the bucket re-probe `cas_location` performs internally.
    ///
    /// Same return contract as `cas_location`: `true` if the swap
    /// happened, `false` if `old_location` is no longer present at this
    /// slot — the entry moved, was overwritten, or was removed since the
    /// lookup that produced `slot`. Callers handle `false` exactly as a
    /// `cas_location` miss today: re-`lookup_slot` and retry.
    ///
    /// Retries the CAS in place across a spurious failure caused by a
    /// concurrent frequency-counter bump changing the packed value's freq
    /// bits underneath us (the same race `cas_location`'s callers already
    /// retry through today, e.g. `replace_at`'s `get_item_frequency`
    /// re-check) — it only gives up once the slot's packed value no
    /// longer encodes `old_location`. This does not weaken the contract:
    /// it can only turn a `false` that today's outer retry loop would
    /// have converted into a re-attempt into an immediate re-attempt.
    ///
    /// # Correctness: why a stale `SlotRef` can't CAS the wrong entry
    ///
    /// The compare operand of the CAS is the *exact* packed value
    /// (`tag`+`freq`+`old_location`) read from the slot just before it,
    /// not merely "some entry at this slot index". If the entry `slot`
    /// pointed at has since moved, been overwritten, or been removed, the
    /// slot's current packed value fails the check below for one of these
    /// reasons:
    /// - the slot is now empty (`0`) or a ghost — rejected outright;
    /// - the slot holds a different key's entry that happens to have
    ///   landed there (bucket/slot indices are reused once vacated) — its
    ///   `location` is necessarily different from `old_location`, because
    ///   `old_location` names a segment slot that stays claimed (a
    ///   `WriterPin`/remover pin brackets the unlink and the segment
    ///   decrement) until this exact CAS or its `cas_location` sibling
    ///   resolves, so no other live entry can carry that same location
    ///   value in the meantime;
    /// - the same key was updated in place by a racing writer to a new
    ///   location — tag matches but `location` does not.
    ///
    /// In every case the `compare_exchange` below fails closed and we
    /// return `false` without touching the slot; we never overwrite an
    /// entry other than the one `old_location` uniquely identifies.
    pub(crate) fn cas_location_at(
        &self,
        slot: SlotRef,
        old_location: Location,
        new_location: Location,
        preserve_freq: bool,
    ) -> bool {
        let bucket = self.bucket(slot.bucket_index);
        let slot_index = slot.slot_index;

        // NOTE: relocation calls this while holding an item's numeric
        // version lock; the retry-through-freq-bumps loop below stays
        // bounded because the 8-bit frequency counter saturates, keeping
        // the version lock's critical section finite.
        loop {
            let packed = bucket.items[slot_index].load(Ordering::Acquire);

            if packed == 0 || Hashbucket::is_ghost(packed) {
                return false;
            }
            if Hashbucket::tag(packed) != slot.tag || Hashbucket::location(packed) != old_location {
                return false;
            }

            let freq = if preserve_freq {
                Hashbucket::freq(packed)
            } else {
                1
            };
            let new_packed = Hashbucket::pack(slot.tag, freq, new_location);

            match bucket.items[slot_index].compare_exchange(
                packed,
                new_packed,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                // Lost the CAS — re-read and retry while old_location is
                // still there (mirrors the freq-bump retry `cas_location`
                // relies on its callers for; see doc comment above).
                Err(_) => continue,
            }
        }
    }
}

const _: () = assert!(MultiChoiceHashtable::NUM_STRIPES.is_power_of_two());

// ============================================================================
// Hashtable trait implementation
// ============================================================================

impl Hashtable for MultiChoiceHashtable {
    fn lookup(&self, key: &[u8], verifier: &impl KeyVerifier) -> Option<(Location, u8)> {
        let (tag, buckets) = self.probe(key);
        let num_choices = self.num_choices as usize;

        for &bucket_index in &buckets[..num_choices] {
            self.prefetch_bucket(bucket_index);
        }

        for &bucket_index in &buckets[..num_choices] {
            if let Some(result) = self.search_bucket_for_get(bucket_index, tag, key, verifier) {
                return Some(result);
            }
        }

        // Miss: increment frequency of any matching ghosts
        for &bucket_index in &buckets[..num_choices] {
            self.increment_ghost_freq_in_bucket(bucket_index, tag);
        }

        None
    }

    fn lookup_no_freq_update(
        &self,
        key: &[u8],
        verifier: &impl KeyVerifier,
    ) -> Option<(Location, u8)> {
        let (tag, buckets) = self.probe(key);
        let num_choices = self.num_choices as usize;

        for &bucket_index in &buckets[..num_choices] {
            self.prefetch_bucket(bucket_index);
        }

        for &bucket_index in &buckets[..num_choices] {
            if let Some(result) = self.search_bucket_no_freq(bucket_index, tag, key, verifier) {
                return Some(result);
            }
        }

        None
    }

    fn contains(&self, key: &[u8], verifier: &impl KeyVerifier) -> bool {
        let (tag, buckets) = self.probe(key);
        let num_choices = self.num_choices as usize;

        for &bucket_index in &buckets[..num_choices] {
            self.prefetch_bucket(bucket_index);
        }

        for &bucket_index in &buckets[..num_choices] {
            if self.search_bucket_exists(bucket_index, tag, key, verifier) {
                return true;
            }
        }

        false
    }

    fn insert(
        &self,
        key: &[u8],
        location: Location,
        verifier: &impl KeyVerifier,
    ) -> Result<Option<Location>, ()> {
        let (hash, tag, buckets) = self.probe_with_hash(key);
        let choices = &buckets[..self.num_choices as usize];

        let new_packed = Hashbucket::pack(tag, 1, location);

        // Replace the key's existing LIVE entry, wherever it lives among
        // the candidate buckets. Scanning ALL choices before any claim is
        // load-bearing: claiming a new slot in an earlier bucket while the
        // key's live entry sits in a later one would publish a duplicate.
        // NB: kept identical to the under-lock re-check below — change both together.
        for &bucket_index in choices {
            if let Some(old) =
                self.try_replace_existing(bucket_index, tag, key, new_packed, verifier)
            {
                return Ok(Some(old));
            }
        }

        // Fresh key: entry CREATION is serialized per key-hash stripe.
        // Two racing fresh inserters of one key both reach here; the
        // loser of the lock sees the winner's entry in the re-check below
        // and resolves to a replace. Mutation paths never make an
        // existing key's entry vanish-and-reappear (replace/relocate are
        // in-place slot CASes; a concurrent delete linearizes as
        // delete-then-insert), so a re-check miss really means absent.
        // The stripe lock is a LEAF: the critical section is pure
        // bucket-word CAS + verifier reads — it never takes another lock,
        // pin, or wait.
        // LOCK: insert-stripe
        // Poison recovery: the stripe guards `()` — every mutation under
        // it is a single slot CAS, so a panicking inserter leaves the
        // table consistent and poisoning must not permanently kill
        // 1/NUM_STRIPES of the keyspace.
        let _guard = self.stripe(hash).lock().unwrap_or_else(|e| e.into_inner());

        // Re-check under the lock: a racing fresh insert may have
        // published while we waited.
        // NB: kept identical to the phase-A scan above — change both together.
        for &bucket_index in choices {
            if let Some(old) =
                self.try_replace_existing(bucket_index, tag, key, new_packed, verifier)
            {
                return Ok(Some(old));
            }
        }

        // Fresh key: claim a new slot (matching ghost, then empty, then
        // any ghost — per bucket, in choice order).
        for &bucket_index in choices {
            if self.try_claim_new_slot(bucket_index, tag, new_packed) {
                return Ok(None);
            }
        }

        // All candidate buckets full of live entries: retry least-full
        // first (a racing remove may have freed a slot since the scan).
        if self.num_choices > 1 {
            let mut sorted = [0usize; MAX_CHOICES as usize];
            sorted[..choices.len()].copy_from_slice(choices);
            let sorted = &mut sorted[..choices.len()];
            sorted.sort_unstable_by_key(|&b| self.count_occupied(b));
            for &bucket_index in sorted.iter() {
                if self.try_claim_new_slot(bucket_index, tag, new_packed) {
                    return Ok(None);
                }
            }
        }

        Err(())
    }

    fn remove(&self, key: &[u8], expected: Location) -> bool {
        let (tag, buckets) = self.probe(key);

        for &bucket_index in &buckets[..self.num_choices as usize] {
            if self.try_unlink_in_bucket(bucket_index, tag, expected) {
                return true;
            }
        }

        false
    }

    fn convert_to_ghost(&self, key: &[u8], expected: Location) -> bool {
        let (tag, buckets) = self.probe(key);

        for &bucket_index in &buckets[..self.num_choices as usize] {
            if self.try_to_ghost_in_bucket(bucket_index, tag, expected) {
                return true;
            }
        }

        false
    }

    fn cas_location(
        &self,
        key: &[u8],
        old_location: Location,
        new_location: Location,
        preserve_freq: bool,
    ) -> bool {
        let (tag, buckets) = self.probe(key);

        for &bucket_index in &buckets[..self.num_choices as usize] {
            if self.try_cas_in_bucket(bucket_index, tag, old_location, new_location, preserve_freq)
            {
                return true;
            }
        }

        false
    }

    fn get_frequency(&self, key: &[u8], verifier: &impl KeyVerifier) -> Option<u8> {
        let (tag, buckets) = self.probe(key);

        for &bucket_index in &buckets[..self.num_choices as usize] {
            if let Some(freq) = self.search_bucket_for_freq(bucket_index, tag, key, verifier) {
                return Some(freq);
            }
        }

        None
    }

    fn get_item_frequency(&self, key: &[u8], location: Location) -> Option<u8> {
        let (tag, buckets) = self.probe(key);

        for &bucket_index in &buckets[..self.num_choices as usize] {
            if let Some(freq) = self.search_bucket_for_item_freq(bucket_index, tag, location) {
                return Some(freq);
            }
        }

        None
    }

    fn get_ghost_frequency(&self, key: &[u8]) -> Option<u8> {
        let (tag, buckets) = self.probe(key);

        for &bucket_index in &buckets[..self.num_choices as usize] {
            if let Some(freq) = self.search_bucket_for_ghost(bucket_index, tag) {
                return Some(freq);
            }
        }

        None
    }

    fn clear(&self) {
        for bucket in self.buckets.iter() {
            for slot in bucket.items.iter() {
                slot.store(0, Ordering::Release);
            }
        }
    }
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use super::*;

    pub(super) struct MockVerifier {
        entries: Vec<(Vec<u8>, Location, bool)>,
    }

    impl MockVerifier {
        pub(super) fn new() -> Self {
            Self {
                entries: Vec::new(),
            }
        }

        pub(super) fn add(&mut self, key: &[u8], location: Location, deleted: bool) {
            self.entries.push((key.to_vec(), location, deleted));
        }
    }

    impl KeyVerifier for MockVerifier {
        fn verify(&self, key: &[u8], location: Location, allow_deleted: bool) -> bool {
            self.entries.iter().any(|(k, loc, deleted)| {
                k == key && *loc == location && (allow_deleted || !deleted)
            })
        }
    }

    /// Count live (non-empty, non-ghost) entries across `key`'s candidate
    /// buckets whose tag matches and whose location verifies for `key`.
    fn count_live_entries(
        ht: &MultiChoiceHashtable,
        key: &[u8],
        verifier: &impl KeyVerifier,
    ) -> usize {
        let hash = ht.hash_key(key);
        let tag = MultiChoiceHashtable::tag_from_hash(hash);
        let buckets = ht.bucket_indices(hash);
        let num_choices = ht.num_choices as usize;

        let mut live_count = 0;
        let mut scanned: Vec<usize> = Vec::with_capacity(num_choices);
        for &bucket_index in &buckets[..num_choices] {
            // A key's choices can alias (small tables); scanning the same
            // bucket twice would count one entry as two.
            if scanned.contains(&bucket_index) {
                continue;
            }
            scanned.push(bucket_index);

            let bucket = ht.bucket(bucket_index);
            for slot_index in 0..Hashbucket::NUM_ITEM_SLOTS {
                let packed = bucket.items[slot_index].load(Ordering::Acquire);
                if packed == 0 || Hashbucket::is_ghost(packed) {
                    continue;
                }
                if Hashbucket::tag(packed) != tag {
                    continue;
                }
                if verifier.verify(key, Hashbucket::location(packed), true) {
                    live_count += 1;
                }
            }
        }
        live_count
    }

    #[test]
    fn test_hashtable_creation() {
        // power=10 → 2^10 = 1024 slots → 128 buckets (8 slots each)
        let ht = MultiChoiceHashtable::new(10);
        assert_eq!(ht.num_buckets, 128);
        assert_eq!(ht.num_choices, 2);
    }

    #[test]
    fn test_insert_and_lookup() {
        let ht = MultiChoiceHashtable::new(10);
        let mut verifier = MockVerifier::new();

        let location = Location::new(12345);
        verifier.add(b"test", location, false);

        let result = ht.insert(b"test", location, &verifier);
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());

        let lookup = ht.lookup(b"test", &verifier);
        assert!(lookup.is_some());
        let (loc, _freq) = lookup.unwrap();
        assert_eq!(loc, location);
    }

    #[test]
    fn test_remove() {
        let ht = MultiChoiceHashtable::new(10);
        let mut verifier = MockVerifier::new();

        let location = Location::new(12345);
        verifier.add(b"test", location, false);

        ht.insert(b"test", location, &verifier).unwrap();

        assert!(ht.contains(b"test", &verifier));
        assert!(ht.remove(b"test", location));
        assert!(!ht.contains(b"test", &verifier));
    }

    #[test]
    fn test_ghost() {
        let ht = MultiChoiceHashtable::new(10);
        let mut verifier = MockVerifier::new();

        let location = Location::new(12345);
        verifier.add(b"test", location, false);

        ht.insert(b"test", location, &verifier).unwrap();
        assert!(ht.convert_to_ghost(b"test", location));

        // Ghost should not appear in lookup
        assert!(ht.lookup(b"test", &verifier).is_none());

        // Ghost frequency should be retrievable
        let freq = ht.get_ghost_frequency(b"test");
        assert!(freq.is_some());
    }

    #[test]
    fn test_cas_location() {
        let ht = MultiChoiceHashtable::new(10);
        let mut verifier = MockVerifier::new();

        let loc1 = Location::new(100);
        let loc2 = Location::new(200);
        verifier.add(b"test", loc1, false);
        verifier.add(b"test", loc2, false);

        ht.insert(b"test", loc1, &verifier).unwrap();

        // CAS with wrong old location should fail
        assert!(!ht.cas_location(b"test", Location::new(999), loc2, true));

        // CAS with correct old location should succeed
        assert!(ht.cas_location(b"test", loc1, loc2, true));

        let (loc, _) = ht.lookup(b"test", &verifier).unwrap();
        assert_eq!(loc, loc2);
    }

    #[test]
    fn test_clear() {
        let ht = MultiChoiceHashtable::new(10);
        let mut verifier = MockVerifier::new();

        let location = Location::new(12345);
        verifier.add(b"test", location, false);

        ht.insert(b"test", location, &verifier).unwrap();
        assert!(ht.contains(b"test", &verifier));

        ht.clear();
        assert!(!ht.contains(b"test", &verifier));
    }

    #[test]
    fn test_replace_existing() {
        let ht = MultiChoiceHashtable::new(10);
        let mut verifier = MockVerifier::new();

        let loc1 = Location::new(100);
        let loc2 = Location::new(200);
        verifier.add(b"test", loc1, false);
        verifier.add(b"test", loc2, false);

        ht.insert(b"test", loc1, &verifier).unwrap();

        let result = ht.insert(b"test", loc2, &verifier);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Some(loc1));
    }

    // F4: concurrent same-key inserts must never leave two live entries for
    // the same key. The replace pass (now `try_replace_existing`) previously
    // advanced to the NEXT slot on a matching-slot CAS failure instead of
    // re-reading the SAME slot; a losing writer could then fall through to
    // the empty-slot pass and publish a second, distinct live entry for the
    // same key.
    //
    // The key is seeded with an initial entry BEFORE the threads start, so
    // every concurrent insert below is a genuine *overwrite* race (the F4
    // scenario: "two threads overwriting the same key") rather than a race
    // over which thread claims the very first (empty-slot) entry — that is
    // a distinct, pre-existing race (see NOTE below) outside this fix's
    // scope.
    //
    // The verifier here is a standalone `MockVerifier` built directly at the
    // hashtable layer (no `Segments`/real storage involved) — every location
    // this test will ever insert is pre-registered for the key before the
    // threads start, so `verify()` is a pure read over an immutable `Vec`
    // and safe to share (read-only) across threads via `Arc`.
    //
    // NOTE: a separate, pre-existing race was observed while developing this
    // test: if the key has NO seed entry and multiple threads race the very
    // first insert, each can pass the replace scan (no match found yet) and
    // then independently claim two *different* empty slots via
    // `compare_exchange(0, ..)`, producing a duplicate. That is a TOCTOU
    // race across the replace/claim boundary in `insert`, not the
    // matching-slot CAS-retry bug this test targets. It is now CLOSED: entry
    // creation is serialized per key-hash stripe with an under-lock
    // absence re-check (see the stripe lock in `insert` below), and
    // coverage lives in `test_concurrent_fresh_key_insert_no_duplicates`
    // below.
    #[test]
    fn test_concurrent_same_key_insert_no_duplicates() {
        use std::sync::Arc;

        const NUM_THREADS: usize = 4;
        const ITERS: usize = 500;
        const KEY: &[u8] = b"same-key";

        // power=7 -> 16 buckets total; with num_choices=2 the key's two
        // candidate buckets are small and heavily contended by all threads,
        // maximizing the chance of hitting the matching-slot CAS race.
        let ht = Arc::new(MultiChoiceHashtable::new(7));

        let mut verifier = MockVerifier::new();
        let seed_loc = Location::new(1);
        verifier.add(KEY, seed_loc, false);
        let mut all_locations = Vec::with_capacity(NUM_THREADS * ITERS);
        for t in 0..NUM_THREADS {
            for i in 0..ITERS {
                // Offset locations past `seed_loc` so they're all distinct.
                let loc = Location::new((t * ITERS + i + 2) as u64);
                verifier.add(KEY, loc, false);
                all_locations.push(loc);
            }
        }
        let verifier = Arc::new(verifier);

        // Seed the key single-threaded so the race under test is always an
        // overwrite of an existing entry (the F4 scenario), not a race to
        // create the first entry.
        ht.insert(KEY, seed_loc, &*verifier).unwrap();

        std::thread::scope(|scope| {
            for t in 0..NUM_THREADS {
                let ht = ht.clone();
                let verifier = verifier.clone();
                let locs: Vec<Location> = all_locations[t * ITERS..(t + 1) * ITERS].to_vec();
                scope.spawn(move || {
                    for loc in locs {
                        // Errors (bucket full) are fine for this test — the
                        // property under test is "never more than one live
                        // entry", not "every insert succeeds".
                        let _ = ht.insert(KEY, loc, &*verifier);
                    }
                });
            }
        });

        // Count live (non-empty, non-ghost) slots across the key's candidate
        // buckets whose tag matches AND whose location verifies for KEY.
        let live_count = count_live_entries(&ht, KEY, &*verifier);

        assert_eq!(
            live_count, 1,
            "expected exactly one live entry for the key after concurrent \
             same-key inserts, found {live_count}"
        );
    }

    // The fresh-key duplicate-publish race (item 7f's tracked follow-up):
    // with NO seed entry, racing first inserts of one key could each pass
    // the live-entry scan and then claim two DIFFERENT slots (same or
    // different candidate bucket). The insert stripe lock serializes entry
    // creation with an under-lock re-check, so exactly one live entry
    // must survive every trial.
    // NOTE: a fully serialized scheduling yields a vacuous green — this
    // test's strength rests on the recorded red/green bite-check, not the
    // assertion alone.
    #[test]
    fn test_concurrent_fresh_key_insert_no_duplicates() {
        use std::sync::{Arc, Barrier};

        const NUM_THREADS: usize = 4;
        const TRIALS: usize = 2000;

        for trial in 0..TRIALS {
            let ht = Arc::new(MultiChoiceHashtable::new(7));
            let key = format!("fresh-{trial}").into_bytes();

            let mut verifier = MockVerifier::new();
            for t in 0..NUM_THREADS {
                verifier.add(&key, Location::new((t + 1) as u64), false);
            }
            let verifier = Arc::new(verifier);
            let barrier = Arc::new(Barrier::new(NUM_THREADS));

            std::thread::scope(|scope| {
                for t in 0..NUM_THREADS {
                    let ht = ht.clone();
                    let verifier = verifier.clone();
                    let barrier = barrier.clone();
                    let key = key.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        let _ = ht.insert(&key, Location::new((t + 1) as u64), &*verifier);
                    });
                }
            });

            assert_eq!(
                count_live_entries(&ht, &key, &*verifier),
                1,
                "trial {trial}: fresh-key race published a duplicate"
            );
        }
    }

    // A live entry must be REPLACED wherever it lives among the candidate
    // buckets — never shadowed by a fresh claim in an earlier bucket.
    // Setup: fill the key's first-choice bucket so its first insert lands
    // in the second-choice bucket, then free a first-bucket slot and
    // insert the key again. The old per-bucket pass order (match/empty/
    // ghost fully in bucket 0 before looking at bucket 1) claimed the
    // freed first-bucket slot and left TWO live entries — single-threaded,
    // no race required.
    #[test]
    fn test_replace_across_buckets_no_duplicate() {
        let ht = MultiChoiceHashtable::new(7); // 16 buckets
        let mut verifier = MockVerifier::new();

        // Find a key whose two candidate buckets differ.
        let mut key: Vec<u8> = Vec::new();
        for i in 0u64..100_000 {
            let cand = format!("xbucket-{i}").into_bytes();
            let ch = ht.bucket_indices(ht.hash_key(&cand));
            if ch[0] != ch[1] {
                key = cand;
                break;
            }
        }
        assert!(!key.is_empty(), "no candidate key found");
        let buckets = ht.bucket_indices(ht.hash_key(&key));
        let b0 = buckets[0];

        // Brute-force 8 filler keys whose FIRST choice is b0; inserting
        // them fills b0 with live entries of OTHER keys.
        let mut fillers: Vec<Vec<u8>> = Vec::new();
        for i in 0u64..100_000 {
            if fillers.len() == Hashbucket::NUM_ITEM_SLOTS {
                break;
            }
            let cand = format!("filler-{i}").into_bytes();
            if ht.bucket_indices(ht.hash_key(&cand))[0] == b0 {
                fillers.push(cand);
            }
        }
        assert_eq!(
            fillers.len(),
            Hashbucket::NUM_ITEM_SLOTS,
            "not enough filler keys found"
        );
        for (n, f) in fillers.iter().enumerate() {
            let loc = Location::new(100 + n as u64);
            verifier.add(f, loc, false);
            assert_eq!(ht.insert(f, loc, &verifier), Ok(None));
        }

        // b0 is full -> the key's first insert lands in its second choice.
        let loc_a = Location::new(1);
        verifier.add(&key, loc_a, false);
        assert_eq!(ht.insert(&key, loc_a, &verifier), Ok(None));

        // Free one b0 slot, then insert the key again: it MUST replace
        // the second-choice entry (returning loc_a), not claim the freed
        // b0 slot alongside it.
        assert!(ht.remove(&fillers[0], Location::new(100)));
        let loc_b = Location::new(2);
        verifier.add(&key, loc_b, false);
        assert_eq!(ht.insert(&key, loc_b, &verifier), Ok(Some(loc_a)));

        assert_eq!(
            count_live_entries(&ht, &key, &verifier),
            1,
            "cross-bucket replace must not leave a duplicate"
        );
    }

    // Native-code tripwire for the freq-bump-vs-unlink race: a warm
    // reader CASes the slot word on every hit, so a bump landing between
    // `try_unlink_in_bucket`'s load and its CAS must not cost the unlink
    // its entry. `remove` returning false there would tell
    // `Segment::clear` that another unlinker owns a still-published
    // entry, letting the segment be recycled under it.
    //
    // The deterministic guarantee is the loom model
    // (`loom_remove_vs_freq_bump_unlinks`), which enumerates the
    // interleaving; this test only reproduces it probabilistically on
    // real hardware across many trials.
    #[test]
    fn test_remove_survives_concurrent_freq_bumps() {
        use std::sync::{Arc, Barrier};

        const TRIALS: usize = 2000;
        const BURST: usize = 64;

        for trial in 0..TRIALS {
            let ht = Arc::new(MultiChoiceHashtable::new(7));
            let key = format!("bumped-{trial}").into_bytes();
            let loc = Location::new(1);

            let mut verifier = MockVerifier::new();
            verifier.add(&key, loc, false);
            let verifier = Arc::new(verifier);

            ht.insert(&key, loc, &*verifier).unwrap();

            let barrier = Arc::new(Barrier::new(2));

            std::thread::scope(|scope| {
                {
                    let ht = ht.clone();
                    let verifier = verifier.clone();
                    let barrier = barrier.clone();
                    let key = key.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        for _ in 0..BURST {
                            let _ = ht.lookup(&key, &*verifier);
                        }
                    });
                }

                barrier.wait();
                assert!(
                    ht.remove(&key, loc),
                    "trial {trial}: a racing freq bump defeated the unlink"
                );
            });
        }
    }
}

/// Deterministic reproduction of the stale-location ABA on the read path.
///
/// The hazard window is between loading `packed` from a slot and `verify`
/// reading the storage bytes at the location it encodes. `verify` is a
/// caller-supplied callback, so it IS the seam: a verifier that performs
/// the relocation-and-recycle from inside its own `verify` puts the race
/// exactly where it happens in production, with no scheduler involvement
/// and no test-only hook in the production path.
///
/// Each test drives one read entry point and fails in milliseconds instead
/// of racing for a ~1-in-2,400 interleaving.
#[cfg(all(test, not(feature = "loom")))]
mod stale_location_tests {
    use super::*;
    use crate::sync::AtomicU64;

    const KEY: &[u8] = b"hotkey";
    const OLD: u64 = 0x1000;
    const NEW: u64 = 0x2000;

    /// Verifier that models a merge drain landing mid-`verify`.
    ///
    /// The FIRST comparison against `KEY` is the racing read: before
    /// answering, it relocates the entry to a new location via
    /// `cas_location` — exactly what `Segment::copy_into` does — and then
    /// reports `false`, because in production the old segment has by now
    /// been finalized, recycled, and rewritten, so the bytes at the old
    /// location belong to somebody else's key.
    ///
    /// Every later comparison answers from the post-relocation state, so
    /// the verifier is CONSISTENT: the old location never verifies for
    /// `KEY` again. A guard that re-reads the slot therefore resolves the
    /// key at its new location; a guard that does not concludes "different
    /// key" and reports a false absent.
    struct RelocatingVerifier<'a> {
        ht: &'a MultiChoiceHashtable,
        /// Where the entry currently lives (raw `Location`).
        live: AtomicU64,
        /// 0 until the relocation has fired, 1 afterwards.
        fired: AtomicU64,
    }

    impl<'a> RelocatingVerifier<'a> {
        fn new(ht: &'a MultiChoiceHashtable) -> Self {
            Self {
                ht,
                live: AtomicU64::new(OLD),
                fired: AtomicU64::new(0),
            }
        }

        fn fired(&self) -> bool {
            self.fired.load(Ordering::Acquire) == 1
        }
    }

    impl KeyVerifier for RelocatingVerifier<'_> {
        fn verify(&self, key: &[u8], location: Location, _allow_deleted: bool) -> bool {
            if key != KEY {
                return false;
            }

            if self
                .fired
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                // Mid-verify: the drain relocates the entry and recycles
                // the source segment under us.
                assert!(
                    self.ht
                        .cas_location(KEY, Location::new(OLD), Location::new(NEW), true),
                    "test setup: the relocation CAS must land"
                );
                self.live.store(NEW, Ordering::Release);
                // The old location's bytes are now another key's.
                return false;
            }

            location.as_raw() == self.live.load(Ordering::Acquire)
        }
    }

    /// Seed `KEY` at `OLD`, then run `f` with a verifier that races a
    /// relocation into the first comparison. Asserts the race actually
    /// fired, so a test can never pass by failing to enter the window.
    fn with_relocation_race<R>(
        f: impl FnOnce(&MultiChoiceHashtable, &RelocatingVerifier<'_>) -> R,
    ) -> R {
        let ht = MultiChoiceHashtable::new(10);

        let mut seed = super::tests::MockVerifier::new();
        seed.add(KEY, Location::new(OLD), false);
        ht.insert(KEY, Location::new(OLD), &seed)
            .expect("test setup: seed insert");

        let verifier = RelocatingVerifier::new(&ht);
        let result = f(&ht, &verifier);

        assert!(
            verifier.fired(),
            "test setup: the read never reached the verify window"
        );
        result
    }

    #[test]
    fn lookup_survives_relocation_during_verify() {
        with_relocation_race(|ht, verifier| {
            let got = ht.lookup(KEY, verifier);
            assert_eq!(
                got.map(|(loc, _)| loc),
                Some(Location::new(NEW)),
                "a relocation landing inside verify must not turn a live key \
                 into a miss: the slot changed, so the scan owes it a re-read"
            );
        });
    }

    #[test]
    fn lookup_no_freq_update_survives_relocation_during_verify() {
        with_relocation_race(|ht, verifier| {
            assert_eq!(
                ht.lookup_no_freq_update(KEY, verifier).map(|(loc, _)| loc),
                Some(Location::new(NEW)),
            );
        });
    }

    #[test]
    fn lookup_slot_survives_relocation_during_verify() {
        with_relocation_race(|ht, verifier| {
            assert_eq!(
                ht.lookup_slot(KEY, verifier).map(|(loc, _)| loc),
                Some(Location::new(NEW)),
            );
        });
    }

    #[test]
    fn contains_survives_relocation_during_verify() {
        with_relocation_race(|ht, verifier| {
            assert!(
                ht.contains(KEY, verifier),
                "contains must not report a live key absent"
            );
        });
    }

    #[test]
    fn get_frequency_survives_relocation_during_verify() {
        with_relocation_race(|ht, verifier| {
            assert!(
                ht.get_frequency(KEY, verifier).is_some(),
                "get_frequency must not report a live key absent"
            );
        });
    }

    /// The write path shares `verify_slot`, so pin its behaviour too: an
    /// `insert` whose verify races a relocation must resolve to a REPLACE
    /// of the existing entry, never publish a duplicate.
    ///
    /// NB this one does NOT go red when the guard is removed — `insert`
    /// re-checks for an existing entry under the stripe lock, and by then
    /// the relocation has settled, so the second pass finds it. It is a
    /// regression test for the outcome, not a reproducer.
    #[test]
    fn insert_replaces_through_relocation_during_verify() {
        with_relocation_race(|ht, verifier| {
            let replaced = ht
                .insert(KEY, Location::new(0x3000), verifier)
                .expect("insert must not fail");
            assert_eq!(
                replaced,
                Some(Location::new(NEW)),
                "insert must replace the relocated entry in place"
            );
        });
    }

    /// Find two DISTINCT keys that share both a 12-bit tag and their first
    /// candidate bucket, so a lookup of one genuinely reaches the other's
    /// slot and calls `verify` on it.
    ///
    /// Sharing `buckets[0]` specifically (rather than any candidate) is what
    /// makes the collision reliable: `try_claim_new_slot` scans candidates in
    /// order, so into an empty table the resident key lands in its first
    /// choice — which is the first bucket the probing key examines.
    ///
    /// `RandomState` is seeded per hashtable, so this searches the live
    /// instance rather than hardcoding keys. Expected cost is a few hundred
    /// candidates (birthday over 4096 tags x 128 buckets).
    fn find_tag_colliding_pair(ht: &MultiChoiceHashtable) -> (Vec<u8>, Vec<u8>) {
        let mut seen: std::collections::HashMap<(u16, usize), Vec<u8>> =
            std::collections::HashMap::new();

        for i in 0u64..5_000_000 {
            let cand = format!("tagcollide-{i}").into_bytes();
            let (tag, buckets) = ht.probe(&cand);
            let key = (tag, buckets[0]);
            if let Some(prev) = seen.get(&key) {
                return (prev.clone(), cand);
            }
            seen.insert(key, cand);
        }
        panic!("no tag-colliding key pair found");
    }

    /// A GENUINE 12-bit tag collision — a different key whose probe really
    /// does land on the resident key's slot — must still resolve to
    /// "different key" and report absent.
    ///
    /// This is the control for the whole fix: it is what fails if the guard
    /// degrades into "retry on every mismatch". That regression is not a
    /// wrong answer but an infinite loop — the slot never changes, so an
    /// unconditional `Changed` re-reads the same word forever — so this test
    /// catches it by hanging rather than by asserting.
    ///
    /// The keys MUST be tag-colliding for any of that to be true. An earlier
    /// version of this test used unrelated keys (`b"present"` / `b"absent"`);
    /// with a 12-bit tag the SIMD mask screened the probe out before `verify`
    /// was ever called, so it made ZERO calls into the guard and could not
    /// have failed if the regression occurred. If you change the keys here,
    /// re-check that the assertion below still holds.
    #[test]
    fn genuine_tag_collision_still_reports_absent() {
        let ht = MultiChoiceHashtable::new(10);
        let (present, absent) = find_tag_colliding_pair(&ht);

        // The precondition that makes this test non-vacuous. Without it the
        // tag filter rejects `absent` before `verify` runs and nothing below
        // exercises the guard.
        assert_ne!(present, absent, "the pair must be two distinct keys");
        let (present_tag, present_buckets) = ht.probe(&present);
        let (absent_tag, absent_buckets) = ht.probe(&absent);
        assert_eq!(
            present_tag, absent_tag,
            "keys must share a 12-bit tag or the SIMD filter screens the probe out"
        );
        assert_eq!(
            present_buckets[0], absent_buckets[0],
            "keys must share their first candidate bucket or the probe never \
             examines the resident slot"
        );

        let mut verifier = super::tests::MockVerifier::new();
        let loc = Location::new(OLD);
        verifier.add(&present, loc, false);
        ht.insert(&present, loc, &verifier).unwrap();

        // The resident key still resolves: the guard has not broken hits.
        assert_eq!(
            ht.lookup(&present, &verifier).map(|(l, _)| l),
            Some(loc),
            "the resident key must still resolve"
        );

        // The colliding key reaches that slot, fails `verify`, and must be
        // reported absent by every read entry point.
        assert!(ht.lookup(&absent, &verifier).is_none());
        assert!(ht.lookup_no_freq_update(&absent, &verifier).is_none());
        assert!(ht.lookup_slot(&absent, &verifier).is_none());
        assert!(!ht.contains(&absent, &verifier));
        assert!(ht.get_frequency(&absent, &verifier).is_none());
    }
}

#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use super::*;
    use crate::hashtable::traits::Hashtable;
    use crate::sync::{AtomicU64, AtomicU8};
    use loom::sync::Arc;
    use loom::thread;

    /// Verifier that always returns true, for models about hashtable
    /// MECHANICS: CAS uniqueness, election winners, mutex-serialized entry
    /// creation, message-passing publication order.
    ///
    /// KNOW WHAT IT CANNOT MODEL. It answers "yes, your key is there" for
    /// every location, so under it the entire verify-FAILURE half of the
    /// slot protocol is unreachable: `verify_slot`, its `Changed` retries,
    /// `allow_deleted`, and every "is this still MY entry" decision. A model
    /// built on `AlwaysVerifier` is blind to key identity BY CONSTRUCTION —
    /// it cannot represent a location whose bytes were rewritten under a
    /// reader, which is the hazard the read paths actually defend against.
    ///
    /// Reach for `KeyOracle` (`crate::hashtable::loom_oracle`, used by the
    /// models at the bottom of this file) whenever the invariant depends on
    /// WHICH key a location holds. Verified: neutering any of the five
    /// read-path guards leaves every `AlwaysVerifier` model above green.
    struct AlwaysVerifier;

    impl KeyVerifier for AlwaysVerifier {
        fn verify(&self, _key: &[u8], _location: Location, _allow_deleted: bool) -> bool {
            true
        }
    }

    #[test]
    fn test_concurrent_insert_different_keys() {
        loom::model(|| {
            let ht = Arc::new(MultiChoiceHashtable::new(7));
            let verifier = Arc::new(AlwaysVerifier);

            // Distinct stripes: a collision would serialize these keys and
            // silently shrink the interleaving space loom explores (see
            // NUM_STRIPES).
            let s1 = ht.stripe(ht.hash_key(b"key1"));
            let s2 = ht.stripe(ht.hash_key(b"key2"));
            assert!(
                !std::ptr::eq(s1, s2),
                "key1/key2 share an insert stripe: choose different keys or raise NUM_STRIPES under loom"
            );

            let ht1 = ht.clone();
            let v1 = verifier.clone();
            let t1 = thread::spawn(move || {
                let loc = Location::new(1);
                ht1.insert(b"key1", loc, &*v1)
            });

            let ht2 = ht.clone();
            let v2 = verifier.clone();
            let t2 = thread::spawn(move || {
                let loc = Location::new(2);
                ht2.insert(b"key2", loc, &*v2)
            });

            let _ = t1.join().unwrap();
            let _ = t2.join().unwrap();

            // Both keys should be present (or one may fail due to full bucket)
            let found1 = ht.lookup(b"key1", &*verifier).is_some();
            let found2 = ht.lookup(b"key2", &*verifier).is_some();

            // At least one should succeed
            assert!(found1 || found2);
        });
    }

    #[test]
    fn test_concurrent_insert_same_key() {
        loom::model(|| {
            let ht = Arc::new(MultiChoiceHashtable::new(7));
            let verifier = Arc::new(AlwaysVerifier);

            let ht1 = ht.clone();
            let v1 = verifier.clone();
            let t1 = thread::spawn(move || {
                let loc = Location::new(1);
                ht1.insert(b"key", loc, &*v1)
            });

            let ht2 = ht.clone();
            let v2 = verifier.clone();
            let t2 = thread::spawn(move || {
                let loc = Location::new(2);
                ht2.insert(b"key", loc, &*v2)
            });

            let r1 = t1.join().unwrap();
            let r2 = t2.join().unwrap();

            // Both should succeed (insert does upsert, not add-only)
            assert!(r1.is_ok());
            assert!(r2.is_ok());

            // Key should be present with one of the locations
            let lookup = ht.lookup(b"key", &*verifier);
            assert!(lookup.is_some());
            let final_loc = lookup.unwrap().0;
            assert!(final_loc == Location::new(1) || final_loc == Location::new(2));
        });
    }

    #[test]
    fn test_concurrent_lookup_frequency_update() {
        loom::model(|| {
            let ht = Arc::new(MultiChoiceHashtable::new(7));
            let verifier = Arc::new(AlwaysVerifier);

            // Insert a key first
            let loc = Location::new(42);
            ht.insert(b"key", loc, &*verifier).unwrap();

            let ht1 = ht.clone();
            let v1 = verifier.clone();
            let t1 = thread::spawn(move || ht1.lookup(b"key", &*v1));

            let ht2 = ht.clone();
            let v2 = verifier.clone();
            let t2 = thread::spawn(move || ht2.lookup(b"key", &*v2));

            let r1 = t1.join().unwrap();
            let r2 = t2.join().unwrap();

            // Both lookups should find the key
            assert!(r1.is_some());
            assert!(r2.is_some());

            // Both should return the same location
            assert_eq!(r1.unwrap().0, loc);
            assert_eq!(r2.unwrap().0, loc);
        });
    }

    #[test]
    fn test_concurrent_insert_and_remove() {
        loom::model(|| {
            let ht = Arc::new(MultiChoiceHashtable::new(7));
            let verifier = Arc::new(AlwaysVerifier);

            // Insert a key first
            let loc = Location::new(42);
            ht.insert(b"key", loc, &*verifier).unwrap();

            let ht1 = ht.clone();
            let t1 = thread::spawn(move || ht1.remove(b"key", loc));

            let ht2 = ht.clone();
            let v2 = verifier.clone();
            let t2 = thread::spawn(move || {
                let new_loc = Location::new(99);
                ht2.insert(b"key2", new_loc, &*v2)
            });

            let removed = t1.join().unwrap();
            let _ = t2.join().unwrap();

            // Remove should have succeeded
            assert!(removed);

            // Original key should be gone
            let lookup = ht.lookup(b"key", &*verifier);
            assert!(lookup.is_none());
        });
    }

    #[test]
    fn test_concurrent_cas_operations() {
        loom::model(|| {
            let ht = Arc::new(MultiChoiceHashtable::new(7));
            let verifier = Arc::new(AlwaysVerifier);

            // Insert a key first
            let loc1 = Location::new(1);
            ht.insert(b"key", loc1, &*verifier).unwrap();

            let ht1 = ht.clone();
            let t1 = thread::spawn(move || {
                let loc2 = Location::new(2);
                ht1.cas_location(b"key", loc1, loc2, true)
            });

            let ht2 = ht.clone();
            let t2 = thread::spawn(move || {
                let loc3 = Location::new(3);
                ht2.cas_location(b"key", loc1, loc3, true)
            });

            let r1 = t1.join().unwrap();
            let r2 = t2.join().unwrap();

            // Exactly one CAS should succeed
            let successes = [r1, r2].iter().filter(|&&x| x).count();
            assert_eq!(successes, 1, "Exactly one CAS should succeed");

            // The key should now point to either loc2 or loc3
            let lookup = ht.lookup(b"key", &*verifier);
            assert!(lookup.is_some());
            let final_loc = lookup.unwrap().0;
            assert!(final_loc == Location::new(2) || final_loc == Location::new(3));
        });
    }

    #[test]
    fn test_bucket_slot_cas_contention() {
        loom::model(|| {
            let bucket = Hashbucket::new();
            let slot = &bucket.items[0];

            let slot_ptr = slot as *const AtomicU64 as usize;

            let t1 = thread::spawn(move || {
                let slot = unsafe { &*(slot_ptr as *const AtomicU64) };
                let packed = Hashbucket::pack(0x123, 1, Location::new(1));
                slot.compare_exchange(0, packed, Ordering::Release, Ordering::Acquire)
            });

            let t2 = thread::spawn(move || {
                let slot = unsafe { &*(slot_ptr as *const AtomicU64) };
                let packed = Hashbucket::pack(0x456, 1, Location::new(2));
                slot.compare_exchange(0, packed, Ordering::Release, Ordering::Acquire)
            });

            let r1 = t1.join().unwrap();
            let r2 = t2.join().unwrap();

            // Exactly one should succeed (starting from 0)
            let successes = [r1.is_ok(), r2.is_ok()].iter().filter(|&&x| x).count();
            assert_eq!(successes, 1, "Exactly one CAS from 0 should succeed");
        });
    }

    /// Three threads doing CAS on the same key. Bounded preemption.
    #[test]
    fn test_three_way_cas_same_key() {
        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = Some(2);
        builder.check(|| {
            let ht = Arc::new(MultiChoiceHashtable::new(7));
            let verifier = Arc::new(AlwaysVerifier);

            let loc_initial = Location::new(1);
            ht.insert(b"key", loc_initial, &*verifier).unwrap();

            let ht1 = ht.clone();
            let ht2 = ht.clone();
            let ht3 = ht.clone();

            let t1 = thread::spawn(move || {
                let loc_new = Location::new(10);
                ht1.cas_location(b"key", loc_initial, loc_new, true)
            });

            let t2 = thread::spawn(move || {
                let loc_new = Location::new(20);
                ht2.cas_location(b"key", loc_initial, loc_new, true)
            });

            let t3 = thread::spawn(move || {
                let loc_new = Location::new(30);
                ht3.cas_location(b"key", loc_initial, loc_new, true)
            });

            let r1 = t1.join().unwrap();
            let r2 = t2.join().unwrap();
            let r3 = t3.join().unwrap();

            // Exactly one CAS should succeed
            let successes = [r1, r2, r3].iter().filter(|&&x| x).count();
            assert_eq!(successes, 1, "Exactly one CAS should succeed");

            // Final location should be one of the new values
            let lookup = ht.lookup(b"key", &*verifier);
            assert!(lookup.is_some());
            let final_loc = lookup.unwrap().0;
            assert!(
                final_loc == Location::new(10)
                    || final_loc == Location::new(20)
                    || final_loc == Location::new(30)
            );
        });
    }

    /// Three threads inserting different keys. Bounded preemption.
    #[test]
    fn test_three_way_insert_different_keys() {
        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = Some(2);
        builder.check(|| {
            let ht = Arc::new(MultiChoiceHashtable::new(10));
            let verifier = Arc::new(AlwaysVerifier);

            // Distinct stripes: a collision would serialize the colliding
            // keys and silently shrink the interleaving space loom explores
            // (see NUM_STRIPES).
            let s1 = ht.stripe(ht.hash_key(b"key1"));
            let s2 = ht.stripe(ht.hash_key(b"key2"));
            let s3 = ht.stripe(ht.hash_key(b"key3"));
            assert!(
                !std::ptr::eq(s1, s2),
                "key1/key2 share an insert stripe: choose different keys or raise NUM_STRIPES under loom"
            );
            assert!(
                !std::ptr::eq(s1, s3),
                "key1/key3 share an insert stripe: choose different keys or raise NUM_STRIPES under loom"
            );
            assert!(
                !std::ptr::eq(s2, s3),
                "key2/key3 share an insert stripe: choose different keys or raise NUM_STRIPES under loom"
            );

            let ht1 = ht.clone();
            let v1 = verifier.clone();
            let ht2 = ht.clone();
            let v2 = verifier.clone();
            let ht3 = ht.clone();
            let v3 = verifier.clone();

            let t1 = thread::spawn(move || {
                let loc = Location::new(1);
                ht1.insert(b"key1", loc, &*v1)
            });

            let t2 = thread::spawn(move || {
                let loc = Location::new(2);
                ht2.insert(b"key2", loc, &*v2)
            });

            let t3 = thread::spawn(move || {
                let loc = Location::new(3);
                ht3.insert(b"key3", loc, &*v3)
            });

            let r1 = t1.join().unwrap();
            let r2 = t2.join().unwrap();
            let r3 = t3.join().unwrap();

            let successes = [r1.is_ok(), r2.is_ok(), r3.is_ok()]
                .iter()
                .filter(|&&x| x)
                .count();

            // At least 2 should succeed with 256 buckets
            assert!(successes >= 2, "Most inserts should succeed");

            if r1.is_ok() {
                assert!(ht.lookup(b"key1", &*verifier).is_some());
            }
            if r2.is_ok() {
                assert!(ht.lookup(b"key2", &*verifier).is_some());
            }
            if r3.is_ok() {
                assert!(ht.lookup(b"key3", &*verifier).is_some());
            }
        });
    }

    // Copy-then-publish message-passing: the writer (mirroring copy_into /
    // s3fifo_promote_from) writes the destination bytes, then publishes the new
    // location via the Release-CAS cas_location. A reader that observes the new
    // location (Acquire, via lookup) must see the written bytes. SC-independent
    // message-passing (Release/Acquire), so loom can verify it -- no Dekker shape.
    #[test]
    fn loom_copy_then_publish_no_torn_read() {
        loom::model(|| {
            let ht = Arc::new(MultiChoiceHashtable::new(7));
            let verifier = Arc::new(AlwaysVerifier);

            let old_loc = Location::new(1);
            let new_loc = Location::new(2);

            // Seed the key at the OLD location.
            ht.insert(b"key", old_loc, &*verifier).unwrap();

            // Stand-in for the destination bytes at new_loc; 0 = "not yet written".
            let payload = Arc::new(AtomicU8::new(0));
            const SENTINEL: u8 = 0xAB;

            let writer = {
                let ht = ht.clone();
                let payload = payload.clone();
                thread::spawn(move || {
                    // copy_into order: write bytes FIRST, then publish.
                    payload.store(SENTINEL, Ordering::Relaxed);
                    ht.cas_location(b"key", old_loc, new_loc, true);
                })
            };

            let reader = {
                let ht = ht.clone();
                let verifier = verifier.clone();
                let payload = payload.clone();
                thread::spawn(move || {
                    // Observe the published location (Acquire load inside lookup).
                    if let Some((loc, _freq)) = ht.lookup_no_freq_update(b"key", &*verifier) {
                        if loc == new_loc {
                            // Published new_loc => bytes must already be written.
                            assert_eq!(
                                payload.load(Ordering::Acquire),
                                SENTINEL,
                                "reader observed the published location with unwritten payload"
                            );
                        }
                    }
                })
            };

            writer.join().unwrap();
            reader.join().unwrap();
        });
    }

    // Fresh-key insert de-dup: two threads race the very first insert of
    // one key; the stripe lock (loom::sync::Mutex under this cfg)
    // serializes entry creation, so exactly one live entry may exist
    // post-join. A mutex-serialized invariant is SC-independent, so --
    // unlike the SeqCst Dekker pairs -- loom genuinely verifies this one.
    #[test]
    fn loom_fresh_key_insert_single_entry() {
        loom::model(|| {
            let ht = Arc::new(MultiChoiceHashtable::new(7));
            let verifier = Arc::new(AlwaysVerifier);

            let ht1 = ht.clone();
            let v1 = verifier.clone();
            let t1 = thread::spawn(move || ht1.insert(b"key", Location::new(1), &*v1));

            let ht2 = ht.clone();
            let v2 = verifier.clone();
            let t2 = thread::spawn(move || ht2.insert(b"key", Location::new(2), &*v2));

            let r1 = t1.join().unwrap();
            let r2 = t2.join().unwrap();

            // Both succeed, and exactly one thread CREATES (Ok(None)) --
            // the other must observe the winner under the stripe re-check
            // and resolve to a replace (Ok(Some(_))).
            assert!(r1.is_ok() && r2.is_ok());
            assert_eq!(
                [&r1, &r2].iter().filter(|r| matches!(r, Ok(None))).count(),
                1,
                "exactly one racer creates; the other must replace"
            );

            // Count live same-tag entries across the key's candidate
            // buckets (AlwaysVerifier verifies anything, so tag-match
            // suffices -- only this one key was ever inserted). Dedupe
            // coincident bucket indices, like `count_live_entries`.
            let hash = ht.hash_key(b"key");
            let tag = MultiChoiceHashtable::tag_from_hash(hash);
            let buckets = ht.bucket_indices(hash);
            let mut scanned: Vec<usize> = Vec::new();
            let mut live = 0;
            for &bucket_index in &buckets[..ht.num_choices as usize] {
                if scanned.contains(&bucket_index) {
                    continue;
                }
                scanned.push(bucket_index);
                let bucket = ht.bucket(bucket_index);
                for slot_index in 0..Hashbucket::NUM_ITEM_SLOTS {
                    let packed = bucket.items[slot_index].load(Ordering::Acquire);
                    if packed != 0
                        && !Hashbucket::is_ghost(packed)
                        && Hashbucket::tag(packed) == tag
                    {
                        live += 1;
                    }
                }
            }
            assert_eq!(
                live, 1,
                "fresh-key race must resolve to exactly one live entry"
            );
        });
    }

    // Ghost-takeover variant of the fresh-key race: the key's prior entry
    // was converted to a ghost (S3-FIFO), so racing fresh inserts resolve
    // through try_claim_new_slot's matching-tag ghost takeover -- the
    // creation path where two racers could otherwise take over two
    // DIFFERENT slots. Same single-entry invariant, same result shape:
    // exactly one creator.
    #[test]
    fn loom_fresh_key_insert_after_ghost_single_entry() {
        loom::model(|| {
            let ht = Arc::new(MultiChoiceHashtable::new(7));
            let verifier = Arc::new(AlwaysVerifier);

            // Seed and ghost the key single-threaded, pre-race.
            ht.insert(b"key", Location::new(1), &*verifier).unwrap();
            assert!(ht.convert_to_ghost(b"key", Location::new(1)));

            let ht1 = ht.clone();
            let v1 = verifier.clone();
            let t1 = thread::spawn(move || ht1.insert(b"key", Location::new(2), &*v1));

            let ht2 = ht.clone();
            let v2 = verifier.clone();
            let t2 = thread::spawn(move || ht2.insert(b"key", Location::new(3), &*v2));

            let r1 = t1.join().unwrap();
            let r2 = t2.join().unwrap();

            assert!(r1.is_ok() && r2.is_ok());
            assert_eq!(
                [&r1, &r2].iter().filter(|r| matches!(r, Ok(None))).count(),
                1,
                "exactly one racer creates; the other must replace"
            );

            // Count live same-tag entries across the key's candidate
            // buckets (AlwaysVerifier verifies anything, so tag-match
            // suffices -- only this one key was ever inserted). Dedupe
            // coincident bucket indices, like `count_live_entries`.
            let hash = ht.hash_key(b"key");
            let tag = MultiChoiceHashtable::tag_from_hash(hash);
            let buckets = ht.bucket_indices(hash);
            let mut scanned: Vec<usize> = Vec::new();
            let mut live = 0;
            for &bucket_index in &buckets[..ht.num_choices as usize] {
                if scanned.contains(&bucket_index) {
                    continue;
                }
                scanned.push(bucket_index);
                let bucket = ht.bucket(bucket_index);
                for slot_index in 0..Hashbucket::NUM_ITEM_SLOTS {
                    let packed = bucket.items[slot_index].load(Ordering::Acquire);
                    if packed != 0
                        && !Hashbucket::is_ghost(packed)
                        && Hashbucket::tag(packed) == tag
                    {
                        live += 1;
                    }
                }
            }
            assert_eq!(
                live, 1,
                "fresh-key race must resolve to exactly one live entry"
            );
        });
    }

    // A warm reader's frequency bump must never make an unlink lose its
    // entry. `search_bucket_for_get` CASes the slot word on every hit
    // (freq <= 16 bumps unconditionally), so a bump landing between
    // `try_unlink_in_bucket`'s load and its CAS fails that CAS for a
    // reason that has nothing to do with ownership. Abandoning the slot
    // there would leave a live published entry behind while `remove`
    // reports false -- which `Segment::clear` reads as "another unlinker
    // owns it", recycling a segment whose entry is still reachable.
    // The same-slot retry makes `remove` return true regardless of where
    // the bump interleaves.
    #[test]
    fn loom_remove_vs_freq_bump_unlinks() {
        loom::model(|| {
            let ht = Arc::new(MultiChoiceHashtable::new(7));
            let verifier = Arc::new(AlwaysVerifier);

            let loc = Location::new(42);
            ht.insert(b"key", loc, &*verifier).unwrap();

            let ht1 = ht.clone();
            let v1 = verifier.clone();
            let reader = thread::spawn(move || ht1.lookup(b"key", &*v1));

            let ht2 = ht.clone();
            let remover = thread::spawn(move || ht2.remove(b"key", loc));

            let _ = reader.join().unwrap();
            let removed = remover.join().unwrap();

            assert!(
                removed,
                "unlink must not be defeated by a racing freq bump on the same slot"
            );
            assert!(
                ht.lookup(b"key", &*verifier).is_none(),
                "entry must be gone once remove reported success"
            );
        });
    }

    // =====================================================================
    // Oracle-backed slot-protocol models
    //
    // Everything below swaps `AlwaysVerifier` for `KeyOracle` (see
    // `crate::hashtable::loom_oracle`), a stateful location -> key map.
    // `AlwaysVerifier` verifies anything, so under it the whole
    // verify-failure half of the slot protocol — `verify_slot`, its
    // `Changed` retries, `allow_deleted`, every "is this really MY entry"
    // decision — is unreachable code. The models above are blind to it BY
    // CONSTRUCTION; these are the ones that exercise it.
    //
    // Every model below asserts an SC-INDEPENDENT property: a Release-CAS
    // winner count, a retry outcome, a live-entry count. None depends on a
    // sequentially-consistent total order, because loom admits
    // store-buffering outcomes even for pure-SeqCst litmus tests (see the
    // note in segments/header.rs's loom_tests) and would report false
    // violations for any Dekker/SB-shaped assertion. Invariants that DO need
    // SC — "a pinned reader never observes a committed drain" and friends —
    // are shuttle's territory, not loom's.
    //
    // NOT MODELED HERE, deliberately: the converse of the STALE-LOCATION
    // guard, "a genuine tag collision must still report absent". The
    // regression that would break it is `verify_slot` degrading into
    // "retry on every mismatch", and that is not a wrong answer but an
    // infinite loop — the slot never changes, so the re-read spins on a
    // stable word forever. loom detects deadlock, not livelock, so such a
    // model would hang rather than fail. That direction is pinned by
    // `stale_location_tests::genuine_tag_collision_still_reports_absent`,
    // which catches it the only way it can be caught: by hanging.
    // =====================================================================

    use crate::hashtable::loom_oracle::{KeyOracle, DST, KEY, MID, NEW, SRC};

    /// Drive one read entry point through a merge drain that relocates the
    /// key out from under it and recycles the location it was holding.
    ///
    /// The key is LIVE at every instant — at `SRC`, then at `DST`, never
    /// nowhere — so the read must find it in EVERY interleaving. Asserts:
    ///
    /// 1. **no false absent.** The dangerous interleaving is: reader loads
    ///    the slot (`SRC`), drain relinks to `DST` and recycles `SRC`,
    ///    reader's `verify` then compares the key against a recycled
    ///    location and gets `false`. Reading that as "different key" ends
    ///    the scan and reports a live key absent.
    /// 2. **the relink lands.** Nothing else mutates this entry except the
    ///    reader's frequency bump, which CASes the same slot word. So
    ///    `try_cas_in_bucket` must absorb a lost CAS by re-reading the slot
    ///    rather than giving up — abandoning it there would abort a merge
    ///    mid-candidate. (Only the `lookup` variant bumps; the others reach
    ///    this assertion trivially.)
    /// 3. **the key resolves at `DST` once the drain settles** — the read
    ///    did not merely fail to notice, it tracked the entry to its new
    ///    home.
    ///
    /// `read` is a fn pointer rather than a closure so each entry point is
    /// its own `#[test]`: the five read helpers each carry their own copy
    /// of the scan loop and its guard, and a copy that loses the guard must
    /// fail on its own model, not hide behind a sibling's.
    fn assert_read_survives_relocation_and_recycle(
        read: fn(&MultiChoiceHashtable, &KeyOracle) -> bool,
    ) {
        loom::model(move || {
            let ht = Arc::new(MultiChoiceHashtable::new(7));
            let oracle = Arc::new(KeyOracle::new());

            oracle.place(SRC, KEY);
            ht.insert(KEY, KeyOracle::location(SRC), &*oracle)
                .expect("seed insert");

            let reader = {
                let ht = ht.clone();
                let oracle = oracle.clone();
                thread::spawn(move || read(&ht, &oracle))
            };

            let drain = {
                let ht = ht.clone();
                let oracle = oracle.clone();
                thread::spawn(move || oracle.drain_relocate(&ht, SRC, DST))
            };

            let found = reader.join().unwrap();
            let relinked = drain.join().unwrap();

            assert!(
                found,
                "FALSE ABSENT: a relocation + recycle racing the key comparison \
                 must not turn a live key into a miss (STALE-LOCATION INVARIANT)"
            );
            assert!(
                relinked,
                "the relink CAS must land: only a reader's frequency bump can \
                 lose it the slot word, and that must cost a retry, not the \
                 relocation"
            );
            assert!(
                read(&ht, &oracle),
                "the entry must still resolve once the drain has settled"
            );
            assert_eq!(
                ht.lookup_no_freq_update(KEY, &*oracle).map(|(loc, _)| loc),
                Some(KeyOracle::location(DST)),
                "the settled entry must be published at the relocation target"
            );
        });
    }

    /// `lookup` — `search_bucket_for_get`, the only read path that also
    /// CASes the slot to bump frequency.
    #[test]
    fn loom_lookup_survives_relocation_and_recycle() {
        assert_read_survives_relocation_and_recycle(|ht, oracle| ht.lookup(KEY, oracle).is_some());
    }

    /// `contains` — `search_bucket_exists`. No in-tree caller today (the
    /// `Hashtable` trait is `#[allow(dead_code)]`), but it carries its own
    /// copy of the scan loop and answers exactly, not approximately, so a
    /// false `false` here is the same bug as a false absent from `lookup`.
    #[test]
    fn loom_contains_survives_relocation_and_recycle() {
        assert_read_survives_relocation_and_recycle(|ht, oracle| ht.contains(KEY, oracle));
    }

    /// `lookup_no_freq_update` — `search_bucket_no_freq`.
    #[test]
    fn loom_lookup_no_freq_update_survives_relocation_and_recycle() {
        assert_read_survives_relocation_and_recycle(|ht, oracle| {
            ht.lookup_no_freq_update(KEY, oracle).is_some()
        });
    }

    /// `lookup_slot` — `search_bucket_no_freq_slot`, the entry point behind
    /// `segcache`'s replace and numeric-update paths (`lookup_slot` +
    /// `cas_location_at`). A false absent here reports a live key missing to
    /// a caller that is about to relink it.
    #[test]
    fn loom_lookup_slot_survives_relocation_and_recycle() {
        assert_read_survives_relocation_and_recycle(|ht, oracle| {
            ht.lookup_slot(KEY, oracle).is_some()
        });
    }

    /// `get_frequency` — `search_bucket_for_freq`. Like `contains`, a trait
    /// method with no in-tree caller today; its location-keyed sibling
    /// `get_item_frequency` IS on the merge path, where a missing frequency
    /// is read as "this item is dead, drop it". Modeled because it is the
    /// fifth hand-written copy of the scan loop and the one most likely to
    /// be reached for next.
    #[test]
    fn loom_get_frequency_survives_relocation_and_recycle() {
        assert_read_survives_relocation_and_recycle(|ht, oracle| {
            ht.get_frequency(KEY, oracle).is_some()
        });
    }

    /// Insert's replace scan (`try_replace_existing`) against a drain that
    /// relocates the key TWICE.
    ///
    /// INVARIANT: the key ends with exactly ONE live entry, and the insert
    /// resolves as a REPLACE (`Ok(Some(_))`), never as a creation.
    ///
    /// Why two relocations. `insert` scans for an existing entry twice —
    /// once lock-free, then again under the key's stripe lock — and only
    /// creates a new entry if BOTH scans miss. A single relocation cannot
    /// fool both: by the time the second scan runs, the drain has settled
    /// and the entry verifies at its new location. Two successive drains
    /// (`SRC -> MID -> DST`) is the smallest trace that can strand a stale
    /// location in each scan — and it is an ordinary production trace, since
    /// a hot key is relocated by every merge that touches its segment.
    ///
    /// Without the guard, both scans conclude "different key", `insert`
    /// takes the creation path, and the table ends with the drain's entry
    /// AND the writer's entry both live for one key — the #46 duplicate.
    ///
    /// Both assertions below were checked non-vacuous SEPARATELY against the
    /// neutered guard: the replace assertion fires first (`inserted` is
    /// `Ok(None)`), and with that assertion removed the count assertion
    /// fires on its own with `left: 2`. Keep them independent if you edit
    /// this — the second is the one that names the actual damage.
    #[test]
    fn loom_insert_replace_scan_survives_repeated_relocation() {
        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = Some(3);
        builder.check(|| {
            let ht = Arc::new(MultiChoiceHashtable::new(7));
            let oracle = Arc::new(KeyOracle::new());

            oracle.place(SRC, KEY);
            ht.insert(KEY, KeyOracle::location(SRC), &*oracle)
                .expect("seed insert");

            let drain = {
                let ht = ht.clone();
                let oracle = oracle.clone();
                thread::spawn(move || {
                    // Two successive merges relocate the same hot key.
                    // Either relink may lose to the writer's replace — that
                    // is a normal outcome, not a model failure.
                    oracle.drain_relocate(&ht, SRC, MID);
                    oracle.drain_relocate(&ht, MID, DST);
                })
            };

            let writer = {
                let ht = ht.clone();
                let oracle = oracle.clone();
                thread::spawn(move || {
                    // Write the replacement item, then publish it.
                    oracle.place(NEW, KEY);
                    ht.insert(KEY, KeyOracle::location(NEW), &*oracle)
                })
            };

            drain.join().unwrap();
            let inserted = writer
                .join()
                .unwrap()
                .expect("insert must not report the table full");

            assert!(
                inserted.is_some(),
                "insert must resolve to a REPLACE: the key's entry is published \
                 at some location at every instant, so a scan that reports it \
                 absent has mistaken a stale location for a different key"
            );
            assert_eq!(
                KeyOracle::drain_live_entries(&ht),
                1,
                "one key must leave exactly one live entry: a replace scan that \
                 misses through a relocation publishes a DUPLICATE"
            );
        });
    }

    /// `remove`'s expected-location check is an ABA guard: it must unlink
    /// the entry the caller named, and refuse an entry that has since moved
    /// on.
    ///
    /// A deleter unlinks `KEY` at `SRC` while a merge drain relocates it to
    /// `DST`. Both name the same slot; exactly one may claim it.
    ///
    /// INVARIANT: exactly one of {relink, unlink} succeeds, and the table
    /// agrees with the winner — if the relink won, the entry is still
    /// reachable at `DST`; if the unlink won, the key is gone.
    ///
    /// Dropping the location check would let the unlink take the RELOCATED
    /// entry: `remove` reports success to a caller that asked about `SRC`
    /// (which `Segment::clear` reads as "that segment's entry is mine to
    /// recycle") while the item the drain just published at `DST` becomes
    /// unreachable — a live item leaked out of the index.
    #[test]
    fn loom_remove_does_not_unlink_a_relocated_entry() {
        loom::model(|| {
            let ht = Arc::new(MultiChoiceHashtable::new(7));
            let oracle = Arc::new(KeyOracle::new());

            oracle.place(SRC, KEY);
            ht.insert(KEY, KeyOracle::location(SRC), &*oracle)
                .expect("seed insert");

            let drain = {
                let ht = ht.clone();
                let oracle = oracle.clone();
                thread::spawn(move || oracle.drain_relocate(&ht, SRC, DST))
            };

            let remover = {
                let ht = ht.clone();
                let oracle = oracle.clone();
                thread::spawn(move || {
                    let removed = ht.remove(KEY, KeyOracle::location(SRC));
                    if removed {
                        // The item is freed and its space released.
                        oracle.vacate(SRC);
                    }
                    removed
                })
            };

            let relinked = drain.join().unwrap();
            let removed = remover.join().unwrap();

            assert_ne!(
                relinked, removed,
                "exactly one of the relink and the unlink may claim the entry \
                 (both succeeding means the unlink took an entry that had \
                 already moved to another location)"
            );
            assert_eq!(
                ht.lookup_no_freq_update(KEY, &*oracle).map(|(loc, _)| loc),
                if relinked {
                    Some(KeyOracle::location(DST))
                } else {
                    None
                },
                "a relocated entry must stay reachable at its new location; a \
                 removed one must be gone"
            );
        });
    }

    /// The ghost-conversion sibling of the model above:
    /// `try_to_ghost_in_bucket` carries its own copy of the expected-location
    /// check, and it fails differently — a wrongly-ghosted entry does not
    /// merely vanish, it leaves a ghost that keeps answering frequency
    /// queries for a key whose live item is still published elsewhere.
    ///
    /// INVARIANT: exactly one of {relink, ghost} succeeds; if the relink won
    /// the key resolves live at `DST` and has NO ghost; if the ghosting won
    /// the key is not live and has one.
    #[test]
    fn loom_ghost_conversion_does_not_capture_a_relocated_entry() {
        loom::model(|| {
            let ht = Arc::new(MultiChoiceHashtable::new(7));
            let oracle = Arc::new(KeyOracle::new());

            oracle.place(SRC, KEY);
            ht.insert(KEY, KeyOracle::location(SRC), &*oracle)
                .expect("seed insert");

            let drain = {
                let ht = ht.clone();
                let oracle = oracle.clone();
                thread::spawn(move || oracle.drain_relocate(&ht, SRC, DST))
            };

            let evictor = {
                let ht = ht.clone();
                let oracle = oracle.clone();
                thread::spawn(move || {
                    let ghosted = ht.convert_to_ghost(KEY, KeyOracle::location(SRC));
                    if ghosted {
                        // S3-FIFO evicted the item; its space is released.
                        oracle.vacate(SRC);
                    }
                    ghosted
                })
            };

            let relinked = drain.join().unwrap();
            let ghosted = evictor.join().unwrap();

            assert_ne!(
                relinked, ghosted,
                "exactly one of the relink and the ghost conversion may claim \
                 the entry (both succeeding means the eviction ghosted an entry \
                 that had already moved to another location)"
            );
            assert_eq!(
                ht.lookup_no_freq_update(KEY, &*oracle).map(|(loc, _)| loc),
                if relinked {
                    Some(KeyOracle::location(DST))
                } else {
                    None
                },
                "a relocated entry must stay live at its new location"
            );
            assert_eq!(
                ht.get_ghost_frequency(KEY).is_some(),
                ghosted,
                "a ghost may exist only if the ghost conversion actually won"
            );
        });
    }

    // =====================================================================
    // Incarnation tag: recycle-and-refill at the same address (#50)
    // =====================================================================

    /// The tag's own model: a location whose incarnation ended must not
    /// address the incarnation that took its place, even when every other
    /// defence has been stripped away by the trace itself.
    ///
    /// **Why the other oracle models above cannot cover this.** They relocate
    /// the key to a DIFFERENT cell, so the stale location ends up holding
    /// another key and the verifier alone rejects it. Here the segment is
    /// recycled and refilled with the SAME key at the SAME address — a
    /// commonplace trace, not a contrivance: segments are append-only from a
    /// fixed start, so under uniform item sizes the n-th item of every
    /// incarnation lands at exactly that offset (design §"Why 6 bits"). The
    /// bytes really are this key's again, `verify` says yes, and the 12-bit
    /// hash tag matches because it is the same key. The incarnation tag is
    /// the only thing left that can tell the two locations apart.
    ///
    /// **The racer is real, and it is unpinned.** `Segcache::delete`'s
    /// pin-fail arm unlinks the entry it looked up WITHOUT a remover pin —
    /// nothing stops the segment being drained, recycled and refilled between
    /// its lookup and its `remove`. Its generation snapshot narrows that
    /// window but does not close it (the window from the generation load to
    /// the remove CAS is exactly what the comment there calls residual), so
    /// this model deliberately omits the snapshot: the assertion is that the
    /// TAG ALONE suffices, which is the claim design §"Why 6 bits" makes for
    /// every unpinned unlink.
    ///
    /// Two independent invariants:
    ///
    /// 1. **exactly one claimant of the outgoing entry.** One entry exists at
    ///    incarnation 0 and both threads target it; if both report success,
    ///    one of them unlinked something that was not the entry it named.
    /// 2. **the refilled entry survives, in every interleaving**, and every
    ///    location-keyed consumer refuses the stale location afterwards
    ///    (`get_item_frequency` is the drain's own liveness check,
    ///    `cas_location` the relink, `convert_to_ghost` the S3-FIFO eviction,
    ///    `remove` the unlink). An acked delete may destroy its own
    ///    incarnation's entry; it may never destroy the next one's.
    ///
    /// **Proven to fail against neutered code**, per #67's discipline. The
    /// neutering is `location::tag_for_generation` returning a constant —
    /// the one projection every incarnation check funnels through, so
    /// collapsing it is exactly "the tag distinguishes nothing". Each layer
    /// was then peeled to show the next is non-vacuous too:
    ///
    /// 1. the premise guard fires first, on `left: Location(0x00004000000),
    ///    right: Location(0x00004000000)` — the two incarnations became one
    ///    word, which is the neutering announcing itself;
    /// 2. with the premise guard removed, the consumer sweep fires: *"the
    ///    drain's liveness check must report a stale location ABSENT"*;
    /// 3. with those removed, invariant 1 fires on `left: true, right: true`
    ///    — loom finds the interleaving where the delete's `remove` lands
    ///    AFTER the republish and takes the fresh entry;
    /// 4. with that removed too, invariant 2 fires with the refilled entry
    ///    gone (`left: None`).
    #[test]
    fn loom_stale_incarnation_unlink_cannot_take_the_refilled_entry() {
        loom::model(|| {
            let ht = Arc::new(MultiChoiceHashtable::new(7));
            let oracle = Arc::new(KeyOracle::new());

            // The same address in two successive incarnations of one segment.
            let stale = KeyOracle::location_in(SRC, 0);
            let refilled = KeyOracle::location_in(SRC, 1);
            assert_ne!(
                stale, refilled,
                "the two incarnations must be distinguishable, or this model \
                 asserts nothing"
            );

            oracle.place(SRC, KEY);
            ht.insert(KEY, stale, &*oracle).expect("seed insert");

            // `Segcache::delete`: looked the key up, failed to pin its
            // segment, and fell through to the unpinned unlink holding the
            // location it read before any of the below happened.
            let deleter = {
                let ht = ht.clone();
                thread::spawn(move || ht.remove(KEY, stale))
            };

            // Drain -> recycle -> re-reserve -> refill, in production order.
            let recycler = {
                let ht = ht.clone();
                let oracle = oracle.clone();
                thread::spawn(move || oracle.recycle_and_refill(&ht, SRC, 0))
            };

            let unlinked = deleter.join().unwrap();
            let swept = recycler.join().unwrap();

            assert_ne!(
                unlinked, swept,
                "exactly one of the delete's unpinned unlink and the drain's \
                 sweep may claim the outgoing incarnation's entry (both \
                 succeeding means one of them matched a location it does not \
                 name — the ABA the incarnation tag exists to close)"
            );
            assert_eq!(
                ht.lookup_no_freq_update(KEY, &*oracle).map(|(loc, _)| loc),
                Some(refilled),
                "the refilled entry must survive: an unlink holding a location \
                 from the PREVIOUS incarnation must not take it, however \
                 exactly its address and its key bytes match"
            );

            // Every location-keyed consumer refuses the stale location once
            // the race has settled. The verifier cannot help any of them —
            // the bytes at that address really are this key's.
            assert!(
                ht.get_item_frequency(KEY, stale).is_none(),
                "the drain's liveness check must report a stale location \
                 ABSENT, or a merge relocates the next incarnation's item"
            );
            assert!(
                !ht.cas_location(KEY, stale, KeyOracle::location(DST), true),
                "a relink CAS against a stale location must lose"
            );
            assert!(
                !ht.convert_to_ghost(KEY, stale),
                "an eviction must not ghost an entry it names by a dead \
                 incarnation"
            );
            assert!(
                !ht.remove(KEY, stale),
                "a second unpinned unlink must still be refused"
            );
            assert_eq!(
                KeyOracle::drain_live_entries(&ht),
                1,
                "one key must leave exactly one live entry"
            );
        });
    }

    // =====================================================================
    // get_pinned's post-pin revalidation retry (#65)
    // =====================================================================

    /// The chain of locations one key is republished through: a full `set`
    /// writes the item somewhere new and relinks the slot, so a key rewritten
    /// N times walks N+1 locations.
    const CHAIN: [u64; 3] = [0x1000, 0x2000, 0x3000];

    /// A location -> key oracle for a key that is REPUBLISHED under a reader,
    /// as opposed to `RecyclingOracle`'s single relocation.
    ///
    /// Each republication is sequenced in production order — the new bytes
    /// exist before anything points at them, the old segment is recycled only
    /// after the relink — so the model cannot manufacture a state the real
    /// system could not reach.
    struct ChurnOracle {
        valid: [AtomicU64; CHAIN.len()],
    }

    impl ChurnOracle {
        fn new() -> Self {
            Self {
                valid: [AtomicU64::new(1), AtomicU64::new(0), AtomicU64::new(0)],
            }
        }
    }

    impl KeyVerifier for ChurnOracle {
        fn verify(&self, key: &[u8], location: Location, _allow_deleted: bool) -> bool {
            if key != b"key" {
                return false;
            }
            CHAIN
                .iter()
                .position(|&l| l == location.as_raw())
                .is_some_and(|i| self.valid[i].load(Ordering::Acquire) == 1)
        }
    }

    /// Exhaustive model of `get_pinned`'s revalidation retry against a key
    /// being republished under it (#65).
    ///
    /// **Scope, stated plainly.** The reader below is a transcription of
    /// `Segcache::get_pinned`'s retry loop, not a call into it: the pin
    /// (`acquire_item_at`) reads `Segments`' mmap'd headers, which are not
    /// loom types, so it cannot appear here. What IS real is the part this
    /// issue is about — the hashtable operations, their interleaving with the
    /// republications, and the budget constant itself, which is imported from
    /// the production module so that changing it changes this model. The pin's
    /// own failure and success paths are covered deterministically by
    /// `pin_failure_tests` and `revalidation_tests`.
    ///
    /// Two assertions, for the two halves of the fix:
    ///
    /// - **no false absent.** Each mismatch costs the writer one republication,
    ///   so a reader survives exactly as long as its budget exceeds the
    ///   republications racing it. That is what the budget is *for*, and it is
    ///   why the budget may not be three.
    /// - **`lookups <= CHAIN.len() + 1`.** One from-scratch lookup, then one
    ///   revalidation per attempt. This is the convergence property itself
    ///   stated as a cost: re-resolving the key after a mismatch doubles the
    ///   count, and that doubling is what the old budget was spent on.
    ///
    /// **Proven to fail, twice** (it is not a control test):
    ///
    /// - setting the production `REVALIDATE_RETRIES` to 2 makes loom find the
    ///   interleaving where both republications land in a revalidation window:
    ///   *"false absent: the key was republished 2 times and never removed..."*
    /// - restoring the pre-#65 reader (re-resolve from scratch each attempt,
    ///   budget 3) trips the lookup count instead: *"each retry must follow the
    ///   location the revalidation already returned: 6 lookups for 2
    ///   republications..."*
    #[test]
    fn loom_revalidation_retry_survives_republication() {
        loom::model(|| {
            let ht = Arc::new(MultiChoiceHashtable::new(7));
            let oracle = Arc::new(ChurnOracle::new());

            ht.insert(b"key", Location::new(CHAIN[0]), &*oracle)
                .expect("seed insert");

            let ht_reader = ht.clone();
            let o_reader = oracle.clone();
            let reader = thread::spawn(move || {
                // `get_pinned`: resolve the key once from scratch, then pin
                // and re-validate, following the location the revalidation
                // returns rather than looking the key up again. `lookups`
                // counts every hashtable lookup the get performs — the
                // convergence property is a statement about that count.
                let mut lookups = 1;
                let Some((mut location, _freq)) =
                    ht_reader.lookup_no_freq_update(b"key", &*o_reader)
                else {
                    return (None, lookups);
                };
                let mut attempts = 0;
                loop {
                    // (pin `location` — see the scope note above)
                    lookups += 1;
                    match ht_reader
                        .lookup_no_freq_update(b"key", &*o_reader)
                        .map(|(l, _)| l)
                    {
                        Some(current) if current == location => return (Some(location), lookups),
                        Some(current) => {
                            attempts += 1;
                            if attempts >= crate::segcache::REVALIDATE_RETRIES {
                                return (None, lookups);
                            }
                            location = current;
                        }
                        None => return (None, lookups),
                    }
                }
            });

            let ht_writer = ht.clone();
            let o_writer = oracle.clone();
            let writer = thread::spawn(move || {
                for i in 0..CHAIN.len() - 1 {
                    // 1. The replacement item's bytes exist before anything
                    //    points at them.
                    o_writer.valid[i + 1].store(1, Ordering::Release);
                    // 2. Publish it (insert's replace relink).
                    assert!(
                        ht_writer.cas_location(
                            b"key",
                            Location::new(CHAIN[i]),
                            Location::new(CHAIN[i + 1]),
                            true
                        ),
                        "relink CAS must land: nothing else touches this entry"
                    );
                    // 3. The superseded item's segment is recycled.
                    o_writer.valid[i].store(0, Ordering::Release);
                }
            });

            let (resolved, lookups) = reader.join().unwrap();
            writer.join().unwrap();

            // One from-scratch lookup, then one revalidation per attempt.
            // Re-resolving the key after a mismatch — what the pre-#65 loop
            // did — doubles this and is what the budget was being spent on.
            assert!(
                lookups <= CHAIN.len() + 1,
                "each retry must follow the location the revalidation already \
                 returned: {lookups} lookups for {} republications means an \
                 attempt re-raced from scratch",
                CHAIN.len() - 1
            );
            assert!(
                resolved.is_some(),
                "false absent: the key was republished {} times and never removed, \
                 so every lookup could resolve it — a retry budget that is spent \
                 re-racing from scratch turns that into a miss (#65)",
                CHAIN.len() - 1
            );
            assert!(
                ht.lookup(b"key", &*oracle).is_some(),
                "the entry must still resolve once the writer has settled"
            );
        });
    }
}
