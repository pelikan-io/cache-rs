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
    /// LOCK: leaf — the critical section is pure bucket-word CAS +
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

            let packed = bucket.items[slot_index].load(Ordering::Acquire);

            if packed == 0 || Hashbucket::is_ghost(packed) {
                continue;
            }
            if (packed & 0xFFF0_0000_0000_0000) != tag_shifted {
                continue;
            }

            let location = Hashbucket::location(packed);
            verifier.prefetch(location);

            if verifier.verify(key, location, false) {
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

            let packed = bucket.items[slot_index].load(Ordering::Acquire);

            if packed == 0 || Hashbucket::is_ghost(packed) {
                continue;
            }
            if (packed & 0xFFF0_0000_0000_0000) != tag_shifted {
                continue;
            }

            let location = Hashbucket::location(packed);
            verifier.prefetch(location);

            if verifier.verify(key, location, false) {
                return Some((location, Hashbucket::freq(packed)));
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

            let packed = bucket.items[slot_index].load(Ordering::Acquire);

            if packed == 0 || Hashbucket::is_ghost(packed) {
                continue;
            }
            if (packed & 0xFFF0_0000_0000_0000) != tag_shifted {
                continue;
            }

            let location = Hashbucket::location(packed);
            verifier.prefetch(location);

            if verifier.verify(key, location, false) {
                return Some((location, slot_index));
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

            let packed = bucket.items[slot_index].load(Ordering::Acquire);

            if packed == 0 || Hashbucket::is_ghost(packed) {
                continue;
            }
            if (packed & 0xFFF0_0000_0000_0000) != tag_shifted {
                continue;
            }

            let location = Hashbucket::location(packed);
            verifier.prefetch(location);

            if verifier.verify(key, location, false) {
                return true;
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

            if Hashbucket::tag(speculative) == tag {
                let packed = bucket.items[slot_index].load(Ordering::Acquire);
                if packed == 0 || Hashbucket::is_ghost(packed) || Hashbucket::tag(packed) != tag {
                    continue;
                }

                let location = Hashbucket::location(packed);
                if verifier.verify(key, location, false) {
                    return Some(Hashbucket::freq(packed));
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

                if !verifier.verify(key, location, true) {
                    // `packed` may be stale: a racing same-key relocation
                    // moved this entry and `location`'s bytes were recycled,
                    // so verify falsely reports "different key". Re-validate
                    // before concluding.
                    if bucket.items[slot_index].load(Ordering::Acquire) == packed {
                        break; // slot unchanged — genuinely a different key
                    }
                    continue; // slot changed under us — re-read THIS slot
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
    fn try_unlink_in_bucket(&self, bucket_index: usize, tag: u16, expected: Location) -> bool {
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

                if Hashbucket::tag(packed) == tag && Hashbucket::location(packed) == expected {
                    match bucket.items[slot_index].compare_exchange(
                        packed,
                        0,
                        Ordering::Release,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => return true,
                        Err(_) => continue,
                    }
                }
            }
        }

        false
    }

    /// Try to convert an item to ghost in a bucket.
    fn try_to_ghost_in_bucket(&self, bucket_index: usize, tag: u16, expected: Location) -> bool {
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

                if Hashbucket::tag(packed) == tag && Hashbucket::location(packed) == expected {
                    let ghost = Hashbucket::to_ghost(packed);
                    match bucket.items[slot_index].compare_exchange(
                        packed,
                        ghost,
                        Ordering::Release,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => return true,
                        Err(_) => continue,
                    }
                }
            }
        }

        false
    }

    /// Try to CAS update location in a bucket. Same publish reasoning as
    /// `try_replace_existing`: the success ordering below is Release, which
    /// orders the new item's reserve/define byte writes ahead of the
    /// location becoming visible to readers.
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
            let speculative = bucket.items[slot_index].load(Ordering::Relaxed);

            if speculative == 0 || Hashbucket::is_ghost(speculative) {
                continue;
            }

            if Hashbucket::tag(speculative) == tag {
                let packed = bucket.items[slot_index].load(Ordering::Acquire);
                if packed == 0 || Hashbucket::is_ghost(packed) {
                    continue;
                }

                if Hashbucket::tag(packed) == tag && Hashbucket::location(packed) == old_location {
                    let freq = if preserve_freq {
                        Hashbucket::freq(packed)
                    } else {
                        1
                    };
                    let new_packed = Hashbucket::pack(tag, freq, new_location);

                    if bucket.items[slot_index]
                        .compare_exchange(packed, new_packed, Ordering::Release, Ordering::Relaxed)
                        .is_ok()
                    {
                        return true;
                    }
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
        let _guard = self.stripe(hash).lock().unwrap();

        // Re-check under the lock: a racing fresh insert may have
        // published while we waited.
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

    struct MockVerifier {
        entries: Vec<(Vec<u8>, Location, bool)>,
    }

    impl MockVerifier {
        fn new() -> Self {
            Self {
                entries: Vec::new(),
            }
        }

        fn add(&mut self, key: &[u8], location: Location, deleted: bool) {
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
    // matching-slot CAS-retry bug this test targets; it is covered by
    // `test_concurrent_fresh_key_insert_no_duplicates` below.
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
}

#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use super::*;
    use crate::hashtable::traits::Hashtable;
    use crate::sync::{AtomicU64, AtomicU8};
    use loom::sync::Arc;
    use loom::thread;

    /// Simple verifier that always returns true for testing hashtable mechanics.
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
}
