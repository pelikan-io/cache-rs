//! Lock-free N-choice hashtable implementation.
//!
//! Supports:
//! - Configurable N-choice hashing (1-8 choices) for tunable load factors
//! - ASFC (Adaptive Software Frequency Counter) for frequency tracking
//! - Ghost entries for preserving frequency after eviction
//! - Storage-agnostic location handling via KeyVerifier
//! - SIMD-accelerated bucket scanning on supported platforms

use super::RelinkFreq;
use crate::hashtable::bucket::Hashbucket;
use crate::hashtable::location::Location;
use crate::hashtable::traits::{Hashtable, Hit, Insert, KeyVerifier, Lookup, Verified};
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
///
/// Packed into 8 bytes rather than three naturally-sized fields. Since #91 a
/// `SlotRef` rides inside every [`Hit`] a lookup returns, and a lookup returns
/// by value through several frames; three `usize`-shaped fields made that
/// return 24 bytes wider than it needs to be, which shows up on the miss path
/// where there is nothing else to pay for it. `bucket_index` as a `u32` caps
/// the table at 2^32 buckets — 2^35 slots, a quarter-terabyte of hashtable —
/// and `with_choices` asserts the bound rather than leaving it implied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SlotRef {
    bucket_index: u32,
    slot_index: u8,
    tag: u16,
}

impl SlotRef {
    #[inline]
    fn bucket(&self) -> usize {
        self.bucket_index as usize
    }

    #[inline]
    fn slot(&self) -> usize {
        self.slot_index as usize
    }
}

/// Fold the per-bucket scans of one lookup into a single outcome.
///
/// The policy in one place, because every candidate-scanning entry point owes
/// the same answer: a `Found` wins outright — an authoritative match makes
/// every unverifiable sibling irrelevant — while [`Lookup::Unknown`] is
/// **sticky but last-resort**. It is remembered and the scan continues, and it
/// is reported only if no candidate matched, i.e. only when it could genuinely
/// have hidden the answer.
///
/// Neither failure mode is available to a simpler rule. Treating `Unknown` as
/// `Absent` reports a false miss for a key whose segment merely happens to be
/// draining; spinning on it inside the scan is the wait #54 forbids in the
/// hashtable, where the caller's pins are not visible. If several candidates
/// are unknown the first is reported — triaging any one of them makes
/// progress.
#[inline]
fn fold_choices<T>(choices: &[usize], mut scan: impl FnMut(usize) -> Lookup<T>) -> Lookup<T> {
    let mut unknown = None;
    for &bucket_index in choices {
        match scan(bucket_index) {
            Lookup::Found(found) => return Lookup::Found(found),
            Lookup::Absent => {}
            Lookup::Unknown(location) => {
                unknown.get_or_insert(location);
            }
        }
    }
    match unknown {
        Some(location) => Lookup::Unknown(location),
        None => Lookup::Absent,
    }
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
    /// Generator for ASFC's probabilistic increment, seeded so a
    /// measurement is reproducible. Separate from eviction's so the two
    /// cannot perturb each other's streams.
    freq_rng: crate::Random,
    /// Striped insert locks. Entry CREATION for a key (empty-slot claim,
    /// ghost takeover) is serialized per key-hash stripe with an
    /// under-lock absence re-check (see `insert`); entry MUTATION
    /// (replace, relocate, remove, ghost-convert) stays lock-free.
    ///
    /// LOCK: insert-stripe — leaf; the critical section is bucket-word CASes
    /// and verifier calls, and it is never held across another lock or a WAIT.
    /// Since #91 a verifier call takes a reader pin — a `fetch_add` plus a
    /// state check, released before `try_replace_existing` returns — and that
    /// keeps the section wait-free rather than breaking it: NOTHING EVER WAITS
    /// ON A READER COUNT (a drain waits on `active_writers`/`active_removers`,
    /// and condemns to `AwaitingRelease` when it finds readers), so a pin taken
    /// here cannot be an edge in any wait-for graph and no cycle can form
    /// through it.
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
    const NUM_STRIPES: usize = if cfg!(model_checking) { 16 } else { 1024 };

    /// Create a new hashtable with two-choice hashing (default).
    ///
    /// # Parameters
    /// - `power`: Total item capacity is 2^power (8 slots per bucket, minimum power 7)
    pub fn new(power: u8) -> Self {
        Self::with_choices(power, 2)
    }

    /// Seed the frequency generator.
    ///
    /// Defaults to `rand::DEFAULT_SEED`. Several seeds sample the
    /// distribution of outcomes; one seed only pins an arbitrary point of
    /// it, which is worth remembering before quoting a seeded number as
    /// though it were the value.
    pub fn set_freq_seed(&mut self, seed: u64) {
        self.freq_rng = crate::Random::new(seed);
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
        assert!(
            bucket_power < 32,
            "power must be under 35: `SlotRef` addresses buckets with a u32"
        );
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
            freq_rng: crate::Random::new(crate::rand::DEFAULT_SEED),
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
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2", not(model_checking)))]
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
    #[cfg(all(target_arch = "aarch64", not(model_checking)))]
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
        model_checking,
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

    /// Scan one bucket's tag matches for `key`.
    ///
    /// `UPDATE_FREQ` selects whether a match bumps the entry's frequency
    /// counter. It is a const parameter rather than an argument so the two
    /// callers monomorphize into two straight-line scans, the way the
    /// hand-duplicated `search_bucket_for_get`/`search_bucket_no_freq` pair
    /// did before them.
    ///
    /// # Why there is no same-slot re-read here any more
    ///
    /// Until #91 a `false` from `verify` was ambiguous — the compared bytes
    /// might have stopped being this entry's mid-comparison — so each slot sat
    /// inside a retry loop that re-read the slot word to tell a real key
    /// mismatch from a stale-location read (the retired `verify_slot`'s
    /// STALE-LOCATION INVARIANT). The verifier now compares under a pin whose
    /// generation tag
    /// it checked, so it can no longer produce that ambiguity: it answers
    /// [`Verified::DifferentKey`], which is authoritative, or
    /// [`Verified::Unknown`], which says it could not look at all. Each slot is
    /// therefore examined exactly once.
    #[inline]
    fn search_bucket<const UPDATE_FREQ: bool, V: KeyVerifier>(
        &self,
        bucket_index: usize,
        tag: u16,
        key: &[u8],
        verifier: &V,
    ) -> Lookup<Hit<V::Pin>> {
        let bucket = self.bucket(bucket_index);
        let tag_shifted = (tag as u64) << 52;

        let mut mask = Self::find_tag_matches_simd(bucket, tag_shifted);
        let mut unknown = None;

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

            let pin = match verifier.verify(key, location, false) {
                Verified::Match(pin) => pin,
                Verified::DifferentKey => continue,
                Verified::Unknown(location) => {
                    unknown.get_or_insert(location);
                    continue;
                }
            };

            if UPDATE_FREQ {
                let freq = Hashbucket::freq(packed);
                if freq < 127 {
                    if let Some(new_packed) =
                        Hashbucket::try_update_freq(packed, freq, self.freq_rng.next_u64())
                    {
                        let _ = bucket.items[slot_index].compare_exchange(
                            packed,
                            new_packed,
                            Ordering::Release,
                            Ordering::Relaxed,
                        );
                    }
                }
            }

            return Lookup::Found(Hit {
                location,
                slot: SlotRef {
                    bucket_index: bucket_index as u32,
                    slot_index: slot_index as u8,
                    tag,
                },
                pin,
            });
        }

        match unknown {
            Some(location) => Lookup::Unknown(location),
            None => Lookup::Absent,
        }
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
                    if let Some(new_packed) =
                        Hashbucket::try_update_freq(packed, freq, self.freq_rng.next_u64())
                    {
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
    fn search_bucket_for_freq<V: KeyVerifier>(
        &self,
        bucket_index: usize,
        tag: u16,
        key: &[u8],
        verifier: &V,
    ) -> Lookup<u8> {
        let bucket = self.bucket(bucket_index);
        let mut unknown = None;

        for slot_index in 0..Hashbucket::NUM_ITEM_SLOTS {
            let speculative = bucket.items[slot_index].load(Ordering::Relaxed);

            if speculative == 0 || Hashbucket::is_ghost(speculative) {
                continue;
            }

            if Hashbucket::tag(speculative) != tag {
                continue;
            }

            let packed = bucket.items[slot_index].load(Ordering::Acquire);
            if packed == 0 || Hashbucket::is_ghost(packed) || Hashbucket::tag(packed) != tag {
                continue;
            }

            // A false `None` here would feed the eviction policy a wrong
            // frequency, so an unverifiable candidate is reported rather than
            // silently read as absent — the same rule the lookup paths follow.
            match verifier.verify(key, Hashbucket::location(packed), false) {
                Verified::Match(_pin) => return Lookup::Found(Hashbucket::freq(packed)),
                Verified::DifferentKey => continue,
                Verified::Unknown(location) => {
                    unknown.get_or_insert(location);
                }
            }
        }

        match unknown {
            Some(location) => Lookup::Unknown(location),
            None => Lookup::Absent,
        }
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
    fn try_replace_existing<V: KeyVerifier>(
        &self,
        bucket_index: usize,
        tag: u16,
        key: &[u8],
        new_packed: u64,
        verifier: &V,
    ) -> Lookup<Location> {
        let bucket = self.bucket(bucket_index);
        let mut unknown = None;

        for slot_index in 0..Hashbucket::NUM_ITEM_SLOTS {
            loop {
                let packed = bucket.items[slot_index].load(Ordering::Acquire);

                if packed == 0 || Hashbucket::tag(packed) != tag || Hashbucket::is_ghost(packed) {
                    break; // empty, or not a live entry with our tag — next slot
                }

                let location = Hashbucket::location(packed);

                // The verify pin is dropped immediately: this is a write path,
                // and the invariant is that a verify pin is never held across a
                // lock acquisition or a wait (`insert` goes on to take a
                // remover pin and, through `remove_at`, a bucket `chain_lock`).
                // All this call needs from the pin is that the compare it
                // guarded was exact.
                match verifier.verify(key, location, true) {
                    Verified::Match(_pin) => {}
                    Verified::DifferentKey => break, // authoritative: another key
                    Verified::Unknown(location) => {
                        // Cannot conclude this slot is somebody else's, and
                        // guessing "absent" is how a duplicate entry gets
                        // published for a key that already has one (#46).
                        unknown.get_or_insert(location);
                        break;
                    }
                }

                let freq = Hashbucket::freq(packed);
                let new_with_freq = Hashbucket::with_freq(new_packed, freq);

                match bucket.items[slot_index].compare_exchange(
                    packed,
                    new_with_freq,
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return Lookup::Found(location),
                    // Re-read THIS slot — a racing same-key writer changed it.
                    Err(_) => continue,
                }
            }
        }

        match unknown {
            Some(location) => Lookup::Unknown(location),
            None => Lookup::Absent,
        }
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
    /// with a CAS on this very word (the frequency bump in `search_bucket`, on every
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
        relink: RelinkFreq,
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

                let freq = match relink {
                    RelinkFreq::Preserve => Hashbucket::freq(packed),
                    RelinkFreq::Decrement => Hashbucket::freq(packed).saturating_sub(1),
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
    ///
    /// # The pin is dropped inside
    ///
    /// `lookup_slot` is used only by write paths, and none of them has any use
    /// for the pinned item: they go on to `try_pin_remover` ->
    /// `cas_location_at` -> `remove_at`, and `remove_at` re-validates the
    /// incarnation under the REMOVER pin, which is the pin that actually
    /// matters there (a drain waits out removers before sweeping; it does not
    /// wait out readers). Releasing the verify pin here keeps its lifetime as
    /// short as the compare it guarded.
    ///
    /// # What the real safety property is
    ///
    /// It is tempting to state this as "a verify pin is never held across a
    /// lock acquisition or a wait" — and #91's design did. That is FALSE as a
    /// blanket rule: `Segcache::numeric_update` deliberately retains its verify
    /// pin across `RawItem::lock_numeric_version`, because the pin is what
    /// keeps `raw` valid while the seqlock is held.
    ///
    /// The property that actually holds, and the one the pinning verifier's
    /// safety rests on: **nothing ever waits on a reader count.** A drain waits
    /// on `active_writers` and `active_removers` (`claim_for_drain`); when it
    /// finds readers it CONDEMNS the segment to `AwaitingRelease` and walks
    /// away (`finalize_drained`). So a reader pin can never be an edge in a
    /// wait-for graph, and holding one — across the insert stripe lock, across
    /// the item seqlock, into an `Item` — cannot close a cycle. The rules that
    /// ARE about lock order (`WriterPin` vs a bucket `chain_lock`) concern the
    /// pins that are waited on, and are unchanged.
    pub(crate) fn lookup_slot<V: KeyVerifier>(
        &self,
        key: &[u8],
        verifier: &V,
    ) -> Lookup<(Location, SlotRef)> {
        match self.lookup_no_freq_update(key, verifier) {
            // `hit` — and with it the verify pin — is dropped here.
            Lookup::Found(hit) => Lookup::Found((hit.location, hit.slot)),
            Lookup::Absent => Lookup::Absent,
            Lookup::Unknown(location) => Lookup::Unknown(location),
        }
    }

    /// The key's 12-bit tag and candidate bucket indices.
    ///
    /// Test-only. The deterministic pin/collision tests need to build a
    /// GENUINE tag collision — two distinct keys whose probes land on the same
    /// slot — and searching for one through the public API would be slow and
    /// would silently stop finding collisions if the hash or the tag width
    /// changed.
    #[cfg(all(test, not(model_checking)))]
    pub(crate) fn probe_for_test(&self, key: &[u8]) -> (u16, [usize; MAX_CHOICES as usize]) {
        self.probe(key)
    }

    /// Does `slot` still publish `location`?
    ///
    /// The freshness half of a `get`. A pinned verify settles *which item the
    /// bytes at a location are*; it says nothing about whether the entry is
    /// still **published**, and a reader that loaded the slot word, was
    /// descheduled, and resumed after a `delete` would otherwise hand back a
    /// deleted item (nothing on the read path consults the tombstone — that is
    /// #97, and it covers deletes only).
    ///
    /// Re-reading the same slot word answers it exactly, by the CAS-in-place
    /// argument: unlinks CAS the slot to `0` in place, and relocations and
    /// replaces go through `cas_location`/`cas_location_at` on the slot holding
    /// the entry. No path moves a live entry between slots without CASing the
    /// slot it left, so a delete, relocation or replace landing in the pin
    /// window is detected here.
    ///
    /// Only the **location field** is compared: a concurrent frequency bump
    /// rewrites the packed word, and comparing the whole word would turn that
    /// into a spurious mismatch on every hot key.
    ///
    /// Cost: one `Acquire` load of a cache line the scan just touched.
    #[inline]
    pub(crate) fn slot_publishes(&self, slot: SlotRef, location: Location) -> bool {
        let bucket = self.bucket(slot.bucket());
        let packed = bucket.items[slot.slot()].load(Ordering::Acquire);
        // An empty slot decodes to location 0 and a ghost to `Location::GHOST`,
        // neither of which any published item can carry, so the location
        // compare alone rejects both.
        Hashbucket::location(packed) == location
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
        let bucket = self.bucket(slot.bucket());
        let slot_index = slot.slot();

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
    fn lookup<V: KeyVerifier>(&self, key: &[u8], verifier: &V) -> Lookup<Hit<V::Pin>> {
        let (tag, buckets) = self.probe(key);
        let choices = &buckets[..self.num_choices as usize];

        for &bucket_index in choices {
            self.prefetch_bucket(bucket_index);
        }

        match fold_choices(choices, |bucket_index| {
            self.search_bucket::<true, V>(bucket_index, tag, key, verifier)
        }) {
            Lookup::Found(hit) => Lookup::Found(hit),
            // Only a CONFIRMED miss bumps the ghosts. An unverifiable
            // candidate is not evidence the key was evicted, and crediting a
            // ghost for it would feed the admission policy a phantom hit.
            Lookup::Unknown(location) => Lookup::Unknown(location),
            Lookup::Absent => {
                for &bucket_index in choices {
                    self.increment_ghost_freq_in_bucket(bucket_index, tag);
                }
                Lookup::Absent
            }
        }
    }

    fn lookup_no_freq_update<V: KeyVerifier>(
        &self,
        key: &[u8],
        verifier: &V,
    ) -> Lookup<Hit<V::Pin>> {
        let (tag, buckets) = self.probe(key);
        let choices = &buckets[..self.num_choices as usize];

        for &bucket_index in choices {
            self.prefetch_bucket(bucket_index);
        }

        fold_choices(choices, |bucket_index| {
            self.search_bucket::<false, V>(bucket_index, tag, key, verifier)
        })
    }

    fn contains<V: KeyVerifier>(&self, key: &[u8], verifier: &V) -> Lookup<()> {
        match self.lookup_no_freq_update(key, verifier) {
            // The pin drops here: `contains` answers a question, it does not
            // hand out storage.
            Lookup::Found(_hit) => Lookup::Found(()),
            Lookup::Absent => Lookup::Absent,
            Lookup::Unknown(location) => Lookup::Unknown(location),
        }
    }

    fn insert<V: KeyVerifier>(
        &self,
        key: &[u8],
        location: Location,
        verifier: &V,
    ) -> Result<Insert, ()> {
        let (hash, tag, buckets) = self.probe_with_hash(key);
        let choices = &buckets[..self.num_choices as usize];

        let new_packed = Hashbucket::pack(tag, 1, location);

        // Replace the key's existing LIVE entry, wherever it lives among
        // the candidate buckets. Scanning ALL choices before any claim is
        // load-bearing: claiming a new slot in an earlier bucket while the
        // key's live entry sits in a later one would publish a duplicate.
        // NB: kept identical to the under-lock re-check below — change both together.
        //
        // An unverifiable candidate ends the insert here rather than under the
        // stripe lock: the answer is the same (the caller must roll back and
        // restart), and taking a lock to reach it is pure waste.
        match fold_choices(choices, |bucket_index| {
            self.try_replace_existing(bucket_index, tag, key, new_packed, verifier)
        }) {
            Lookup::Found(old) => return Ok(Insert::Replaced(old)),
            Lookup::Unknown(location) => return Ok(Insert::Unknown(location)),
            Lookup::Absent => {}
        }

        // Fresh key: entry CREATION is serialized per key-hash stripe.
        // Two racing fresh inserters of one key both reach here; the
        // loser of the lock sees the winner's entry in the re-check below
        // and resolves to a replace. Mutation paths never make an
        // existing key's entry vanish-and-reappear (replace/relocate are
        // in-place slot CASes; a concurrent delete linearizes as
        // delete-then-insert), so a re-check miss really means absent.
        // The stripe lock is a LEAF: the critical section is bucket-word
        // CASes and verifier calls — it never takes another lock and never
        // waits. The verifier's reader pin (#91) is taken and released inside
        // one call and nothing ever blocks on a reader count, so it does not
        // change that (see the field's own note on `insert_locks`).
        // LOCK: insert-stripe
        // Poison recovery: the stripe guards `()` — every mutation under
        // it is a single slot CAS, so a panicking inserter leaves the
        // table consistent and poisoning must not permanently kill
        // 1/NUM_STRIPES of the keyspace.
        let _guard = self.stripe(hash).lock().unwrap_or_else(|e| e.into_inner());

        // Re-check under the lock: a racing fresh insert may have
        // published while we waited.
        // NB: kept identical to the phase-A scan above — change both together.
        match fold_choices(choices, |bucket_index| {
            self.try_replace_existing(bucket_index, tag, key, new_packed, verifier)
        }) {
            Lookup::Found(old) => return Ok(Insert::Replaced(old)),
            Lookup::Unknown(location) => return Ok(Insert::Unknown(location)),
            Lookup::Absent => {}
        }

        // Fresh key: claim a new slot (matching ghost, then empty, then
        // any ghost — per bucket, in choice order).
        for &bucket_index in choices {
            if self.try_claim_new_slot(bucket_index, tag, new_packed) {
                return Ok(Insert::Created);
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
                    return Ok(Insert::Created);
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
        freq: RelinkFreq,
    ) -> bool {
        let (tag, buckets) = self.probe(key);

        for &bucket_index in &buckets[..self.num_choices as usize] {
            if self.try_cas_in_bucket(bucket_index, tag, old_location, new_location, freq) {
                return true;
            }
        }

        false
    }

    fn get_frequency<V: KeyVerifier>(&self, key: &[u8], verifier: &V) -> Lookup<u8> {
        let (tag, buckets) = self.probe(key);
        let choices = &buckets[..self.num_choices as usize];

        fold_choices(choices, |bucket_index| {
            self.search_bucket_for_freq(bucket_index, tag, key, verifier)
        })
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

#[cfg(all(test, not(model_checking)))]
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
        /// No storage to pin: the map IS the storage, and it is immutable for
        /// the life of a test.
        type Pin = ();

        fn verify(&self, key: &[u8], location: Location, allow_deleted: bool) -> Verified<()> {
            if self.entries.iter().any(|(k, loc, deleted)| {
                k == key && *loc == location && (allow_deleted || !deleted)
            }) {
                Verified::Match(())
            } else {
                Verified::DifferentKey
            }
        }
    }

    /// The frequency generator must be seeded, and reach ASFC's branch.
    ///
    /// Above frequency 16 ASFC increments with probability 1/freq, and that
    /// draw used to come from `rand::rng()` -- a thread-local ChaCha12
    /// reseeded from the OS. So the cache's miss ratio moved between runs
    /// of one build on one workload with no way to pin it: 0.0021 of spread
    /// remained even once the eviction draw was seeded.
    ///
    /// Driven past 16 on purpose. At 16 and below every access counts and
    /// the draw is never consulted, so a fixture stopping short passes
    /// whether or not the generator is connected to anything.
    #[test]
    fn a_seeded_frequency_generator_makes_asfc_reproducible() {
        let climb = |seed: u64| -> u8 {
            let mut ht = MultiChoiceHashtable::new(10);
            ht.set_freq_seed(seed);
            let mut verifier = MockVerifier::new();
            let location = Location::new(4242);
            verifier.add(b"climber", location, false);
            assert!(
                matches!(
                    ht.insert(b"climber", location, &verifier),
                    Ok(Insert::Created) | Ok(Insert::Replaced(_))
                ),
                "the climber must be published"
            );
            for _ in 0..4000 {
                let _ = ht.lookup(b"climber", &verifier);
            }
            match ht.get_frequency(b"climber", &verifier) {
                Lookup::Found(freq) => freq,
                _ => panic!("the climber must still be present"),
            }
        };

        let a = climb(1);
        assert!(
            a > 16,
            "the fixture must drive frequency past 16 or the probabilistic \
             branch is never reached: got {a}"
        );
        assert!(
            a < 127,
            "and must not saturate, or every seed agrees: got {a}"
        );
        assert_eq!(a, climb(1), "seed 1 gave two different frequencies");
        assert_ne!(
            a,
            climb(0xDEAD_BEEF),
            "two seeds gave the same frequency, so ASFC is not consulting \
             the seeded generator"
        );
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
                if matches!(
                    verifier.verify(key, Hashbucket::location(packed), true),
                    Verified::Match(_)
                ) {
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
        assert_eq!(result, Ok(Insert::Created));

        let hit = ht
            .lookup(b"test", &verifier)
            .found()
            .expect("the key must resolve");
        assert_eq!(hit.location, location);
    }

    #[test]
    fn test_remove() {
        let ht = MultiChoiceHashtable::new(10);
        let mut verifier = MockVerifier::new();

        let location = Location::new(12345);
        verifier.add(b"test", location, false);

        ht.insert(b"test", location, &verifier).unwrap();

        assert!(ht.contains(b"test", &verifier).is_found());
        assert!(ht.remove(b"test", location));
        assert!(!ht.contains(b"test", &verifier).is_found());
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
        assert!(!ht.lookup(b"test", &verifier).is_found());

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
        assert!(!ht.cas_location(b"test", Location::new(999), loc2, RelinkFreq::Preserve));

        // CAS with correct old location should succeed
        assert!(ht.cas_location(b"test", loc1, loc2, RelinkFreq::Preserve));

        let hit = ht.lookup(b"test", &verifier).found().unwrap();
        assert_eq!(hit.location, loc2);
    }

    #[test]
    fn test_clear() {
        let ht = MultiChoiceHashtable::new(10);
        let mut verifier = MockVerifier::new();

        let location = Location::new(12345);
        verifier.add(b"test", location, false);

        ht.insert(b"test", location, &verifier).unwrap();
        assert!(ht.contains(b"test", &verifier).is_found());

        ht.clear();
        assert!(!ht.contains(b"test", &verifier).is_found());
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
        assert_eq!(result, Ok(Insert::Replaced(loc1)));
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
            assert_eq!(ht.insert(f, loc, &verifier), Ok(Insert::Created));
        }

        // b0 is full -> the key's first insert lands in its second choice.
        let loc_a = Location::new(1);
        verifier.add(&key, loc_a, false);
        assert_eq!(ht.insert(&key, loc_a, &verifier), Ok(Insert::Created));

        // Free one b0 slot, then insert the key again: it MUST replace
        // the second-choice entry (returning loc_a), not claim the freed
        // b0 slot alongside it.
        assert!(ht.remove(&fillers[0], Location::new(100)));
        let loc_b = Location::new(2);
        verifier.add(&key, loc_b, false);
        assert_eq!(
            ht.insert(&key, loc_b, &verifier),
            Ok(Insert::Replaced(loc_a))
        );

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

/// Deterministic coverage of the read paths against a location that goes
/// STALE mid-lookup.
///
/// The hazard: a bucket scan reads a slot word, and before it can look at the
/// bytes that location names, a merge drain relocates the entry and recycles
/// the segment behind it. The pinned verifier cannot compare anything in that
/// state — `acquire_item_at` refuses the pin because the location's incarnation
/// tag no longer matches the segment's generation — so it answers
/// [`Verified::Unknown`].
///
/// What every entry point owes that answer is **not to call it absent**. The
/// key is live; it just moved. Reporting `Absent` is a false miss, and
/// downstream that is `add` clobbering a live key and `replace` answering
/// NOT_STORED. Reporting [`Lookup::Unknown`] hands the caller the location to
/// triage and lets it retry — which is exactly what `Segcache::get_pinned`,
/// `cas`, `delete` and the numeric paths do.
///
/// (Before #91 the verifier was unpinned and answered a bare `false` here,
/// indistinguishable from a real key mismatch. The scan recovered by re-reading
/// the slot word — the retired `verify_slot`'s STALE-LOCATION INVARIANT — and resolving the
/// key at its new location. That machinery is gone: a verifier that pins cannot
/// produce the ambiguity, so `DifferentKey` is authoritative and the recovery it
/// needed is unreachable. These tests now pin the replacement contract.)
///
/// `verify` is a caller-supplied callback, so it IS the seam: a verifier that
/// performs the relocation-and-recycle from inside its own `verify` puts the
/// race exactly where it happens in production, with no scheduler involvement
/// and no test-only hook in the production path. Each test drives one read
/// entry point and fails in milliseconds instead of racing for a ~1-in-2,400
/// interleaving.
#[cfg(all(test, not(model_checking)))]
mod stale_location_tests {
    use super::*;
    use crate::sync::AtomicU64;

    const KEY: &[u8] = b"hotkey";
    const OLD: u64 = 0x1000;
    const NEW: u64 = 0x2000;

    /// Verifier that models a merge drain landing mid-`verify`.
    ///
    /// The FIRST comparison against `KEY` is the racing read: before answering,
    /// it relocates the entry to a new location via `cas_location` — exactly
    /// what `Segment::copy_into` does — and then reports `Unknown`, because in
    /// production the old segment has by now been finalized and recycled, so its
    /// generation has moved and the pin for the old location is refused.
    ///
    /// Every later comparison answers from the post-relocation state, so the
    /// verifier is CONSISTENT: the old location never verifies for `KEY` again.
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
        type Pin = ();

        fn verify(&self, key: &[u8], location: Location, _allow_deleted: bool) -> Verified<()> {
            if key != KEY {
                return Verified::DifferentKey;
            }

            if self
                .fired
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                // Mid-verify: the drain relocates the entry and recycles
                // the source segment under us.
                assert!(
                    self.ht.cas_location(
                        KEY,
                        Location::new(OLD),
                        Location::new(NEW),
                        RelinkFreq::Preserve
                    ),
                    "test setup: the relocation CAS must land"
                );
                self.live.store(NEW, Ordering::Release);
                // The old incarnation is gone, so the pin is refused.
                return Verified::Unknown(location);
            }

            if location.as_raw() == self.live.load(Ordering::Acquire) {
                Verified::Match(())
            } else {
                Verified::DifferentKey
            }
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

    /// The shared assertion: the stale candidate must come back as `Unknown`,
    /// naming the location the caller has to triage — never as `Absent`.
    #[track_caller]
    fn assert_unknown_at_old<T>(got: Lookup<T>, what: &str) {
        match got {
            Lookup::Unknown(location) => assert_eq!(
                location,
                Location::new(OLD),
                "{what} must report WHICH location it could not verify, or the \
                 caller cannot tell a drain window from a dead incarnation"
            ),
            Lookup::Absent => panic!(
                "{what} reported a live key ABSENT: a relocation landing inside \
                 verify makes the location unverifiable, not the key missing"
            ),
            Lookup::Found(_) => panic!(
                "{what} claimed a match on a location it could not pin — the \
                 verifier never compared any bytes"
            ),
        }
    }

    #[test]
    fn lookup_reports_unknown_through_relocation_during_verify() {
        with_relocation_race(|ht, verifier| {
            assert_unknown_at_old(ht.lookup(KEY, verifier), "lookup");
        });
    }

    #[test]
    fn lookup_no_freq_update_reports_unknown_through_relocation_during_verify() {
        with_relocation_race(|ht, verifier| {
            assert_unknown_at_old(
                ht.lookup_no_freq_update(KEY, verifier),
                "lookup_no_freq_update",
            );
        });
    }

    #[test]
    fn lookup_slot_reports_unknown_through_relocation_during_verify() {
        with_relocation_race(|ht, verifier| {
            assert_unknown_at_old(ht.lookup_slot(KEY, verifier), "lookup_slot");
        });
    }

    #[test]
    fn contains_reports_unknown_through_relocation_during_verify() {
        with_relocation_race(|ht, verifier| {
            assert_unknown_at_old(ht.contains(KEY, verifier), "contains");
        });
    }

    #[test]
    fn get_frequency_reports_unknown_through_relocation_during_verify() {
        with_relocation_race(|ht, verifier| {
            assert_unknown_at_old(ht.get_frequency(KEY, verifier), "get_frequency");
        });
    }

    /// The write path shares the verifier, so pin its behaviour too. An
    /// `insert` that cannot verify a candidate slot must NOT fall through to
    /// the fresh-key arm and claim a second slot — that is #46, a duplicate
    /// entry for a key that already has one. It reports `Unknown` and the
    /// caller (`Segcache::insert`) rolls its reservation back and restarts,
    /// which is also what releases the WriterPin a blocked drain may be
    /// waiting on.
    #[test]
    fn insert_reports_unknown_rather_than_publishing_a_duplicate() {
        with_relocation_race(|ht, verifier| {
            let outcome = ht
                .insert(KEY, Location::new(0x3000), verifier)
                .expect("insert must not fail");
            assert_eq!(
                outcome,
                Insert::Unknown(Location::new(OLD)),
                "insert must report the unverifiable candidate, not guess"
            );
            assert_eq!(
                count_live_entries_raw(ht, KEY, Location::new(NEW)),
                1,
                "the aborted insert must not have published anything"
            );
        });
    }

    /// Count live slots across `key`'s candidate buckets that publish
    /// `location`. Deliberately verifier-free: this is asserting on the table's
    /// raw state after a verifier that answers `Unknown`, so routing the count
    /// through that verifier would be circular.
    fn count_live_entries_raw(ht: &MultiChoiceHashtable, key: &[u8], location: Location) -> usize {
        let (tag, buckets) = ht.probe(key);
        let mut scanned: Vec<usize> = Vec::new();
        let mut count = 0;
        for &bucket_index in &buckets[..ht.num_choices as usize] {
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
                if Hashbucket::tag(packed) == tag && Hashbucket::location(packed) == location {
                    count += 1;
                }
            }
        }
        count
    }

    /// A scan that sees `Unknown` on one candidate slot and `Match` on another
    /// must return the MATCH.
    ///
    /// `Unknown` is sticky but LAST RESORT: an authoritative match makes every
    /// unverifiable sibling irrelevant, because the key demonstrably resolves.
    /// The failure this guards is a scan that reports the first `Unknown` it
    /// meets and sends the caller off to triage-and-retry a location that had
    /// nothing to do with the answer — a live key turned into a spin.
    ///
    /// Built out of a genuine tag collision so both slots are really examined:
    /// `stale` occupies a slot the probe for `key` reaches, and its location is
    /// unpinnable.
    #[test]
    fn a_match_beats_an_unknown_sibling() {
        struct OneStale {
            stale: Location,
            live: Location,
        }

        impl KeyVerifier for OneStale {
            type Pin = ();

            fn verify(&self, _key: &[u8], location: Location, _allow: bool) -> Verified<()> {
                if location == self.stale {
                    Verified::Unknown(location)
                } else if location == self.live {
                    Verified::Match(())
                } else {
                    Verified::DifferentKey
                }
            }
        }

        let ht = MultiChoiceHashtable::new(10);
        let (first, second) = find_tag_colliding_pair(&ht);

        let stale = Location::new(OLD);
        let live = Location::new(NEW);

        // Both land in the shared first-choice bucket, in probe order, so a
        // scan for either key examines the stale slot BEFORE the live one.
        let verifier = OneStale { stale, live };
        let mut seed = super::tests::MockVerifier::new();
        seed.add(&first, stale, false);
        seed.add(&second, live, false);
        ht.insert(&first, stale, &seed).expect("seed stale");
        ht.insert(&second, live, &seed).expect("seed live");

        let hit = match ht.lookup(&second, &verifier) {
            Lookup::Found(hit) => hit,
            Lookup::Unknown(location) => panic!(
                "the scan gave up at the unverifiable candidate {location:?} \
                 instead of going on to the slot that matches"
            ),
            Lookup::Absent => panic!("the live entry must resolve"),
        };
        assert_eq!(hit.location, live);
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
    /// does land on the resident key's slot — must resolve to "different key"
    /// and report ABSENT, not `Unknown`.
    ///
    /// This is the control for the whole design: `DifferentKey` is what a
    /// pinned verifier buys, and a scan that could not distinguish it from an
    /// unverifiable candidate would send every tag collision — one examined
    /// slot in 4096 — into the caller's triage-and-retry loop.
    ///
    /// The keys MUST be tag-colliding for any of that to be true. An earlier
    /// version of this test used unrelated keys (`b"present"` / `b"absent"`);
    /// with a 12-bit tag the SIMD mask screened the probe out before `verify`
    /// was ever called, so it made ZERO calls into the verifier and could not
    /// have failed if the regression occurred. If you change the keys here,
    /// re-check that the assertion below still holds.
    #[test]
    fn genuine_tag_collision_still_reports_absent() {
        let ht = MultiChoiceHashtable::new(10);
        let (present, absent) = find_tag_colliding_pair(&ht);

        // The precondition that makes this test non-vacuous. Without it the
        // tag filter rejects `absent` before `verify` runs and nothing below
        // exercises the compare.
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

        // The resident key still resolves: the compare has not broken hits.
        assert_eq!(
            ht.lookup(&present, &verifier)
                .found()
                .map(|hit| hit.location),
            Some(loc),
            "the resident key must still resolve"
        );

        // The colliding key reaches that slot, fails the compare, and must be
        // reported ABSENT — not `Unknown` — by every read entry point.
        assert!(matches!(ht.lookup(&absent, &verifier), Lookup::Absent));
        assert!(matches!(
            ht.lookup_no_freq_update(&absent, &verifier),
            Lookup::Absent
        ));
        assert!(matches!(ht.lookup_slot(&absent, &verifier), Lookup::Absent));
        assert!(matches!(ht.contains(&absent, &verifier), Lookup::Absent));
        assert!(matches!(
            ht.get_frequency(&absent, &verifier),
            Lookup::Absent
        ));
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
    /// slot protocol is unreachable: the `DifferentKey`/`Unknown` arms,
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
        /// No storage behind it, so nothing to pin.
        type Pin = ();

        fn verify(&self, _key: &[u8], _location: Location, _allow_deleted: bool) -> Verified<()> {
            Verified::Match(())
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
            let found1 = ht.lookup(b"key1", &*verifier).is_found();
            let found2 = ht.lookup(b"key2", &*verifier).is_found();

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
            let final_loc = lookup.found().expect("the key must resolve").location;
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
            assert!(r1.is_found());
            assert!(r2.is_found());

            // Both should return the same location
            assert_eq!(r1.found().unwrap().location, loc);
            assert_eq!(r2.found().unwrap().location, loc);
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
            assert!(!lookup.is_found());
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
                ht1.cas_location(b"key", loc1, loc2, RelinkFreq::Preserve)
            });

            let ht2 = ht.clone();
            let t2 = thread::spawn(move || {
                let loc3 = Location::new(3);
                ht2.cas_location(b"key", loc1, loc3, RelinkFreq::Preserve)
            });

            let r1 = t1.join().unwrap();
            let r2 = t2.join().unwrap();

            // Exactly one CAS should succeed
            let successes = [r1, r2].iter().filter(|&&x| x).count();
            assert_eq!(successes, 1, "Exactly one CAS should succeed");

            // The key should now point to either loc2 or loc3
            let lookup = ht.lookup(b"key", &*verifier);
            let final_loc = lookup.found().expect("the key must resolve").location;
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
                ht1.cas_location(b"key", loc_initial, loc_new, RelinkFreq::Preserve)
            });

            let t2 = thread::spawn(move || {
                let loc_new = Location::new(20);
                ht2.cas_location(b"key", loc_initial, loc_new, RelinkFreq::Preserve)
            });

            let t3 = thread::spawn(move || {
                let loc_new = Location::new(30);
                ht3.cas_location(b"key", loc_initial, loc_new, RelinkFreq::Preserve)
            });

            let r1 = t1.join().unwrap();
            let r2 = t2.join().unwrap();
            let r3 = t3.join().unwrap();

            // Exactly one CAS should succeed
            let successes = [r1, r2, r3].iter().filter(|&&x| x).count();
            assert_eq!(successes, 1, "Exactly one CAS should succeed");

            // Final location should be one of the new values
            let lookup = ht.lookup(b"key", &*verifier);
            let final_loc = lookup.found().expect("the key must resolve").location;
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
                assert!(ht.lookup(b"key1", &*verifier).is_found());
            }
            if r2.is_ok() {
                assert!(ht.lookup(b"key2", &*verifier).is_found());
            }
            if r3.is_ok() {
                assert!(ht.lookup(b"key3", &*verifier).is_found());
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
                    ht.cas_location(b"key", old_loc, new_loc, RelinkFreq::Preserve);
                })
            };

            let reader = {
                let ht = ht.clone();
                let verifier = verifier.clone();
                let payload = payload.clone();
                thread::spawn(move || {
                    // Observe the published location (Acquire load inside lookup).
                    if let Lookup::Found(hit) = ht.lookup_no_freq_update(b"key", &*verifier) {
                        if hit.location == new_loc {
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
                [&r1, &r2]
                    .iter()
                    .filter(|r| matches!(r, Ok(Insert::Created)))
                    .count(),
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
                [&r1, &r2]
                    .iter()
                    .filter(|r| matches!(r, Ok(Insert::Created)))
                    .count(),
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
    // entry. The scan's frequency bump CASes the slot word on every hit
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
                !ht.lookup(b"key", &*verifier).is_found(),
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
    // verify-failure half of the slot protocol — the `DifferentKey` and
    // `Unknown` arms, the sticky-`Unknown` fold, `allow_deleted`, every "is
    // this really MY entry" decision — is unreachable code. The models above are blind to it BY
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
    // NOT MODELED HERE, deliberately: the converse property, "a genuine tag
    // collision must still report ABSENT rather than `Unknown`". The
    // regression that would break it is a scan that routes every failed
    // compare into the caller's triage-and-retry loop, and that is not a
    // wrong answer but a livelock — the slot never changes, so the caller
    // re-resolves forever. loom detects deadlock, not livelock, so such a
    // model would hang rather than fail. That direction is pinned by
    // `stale_location_tests::genuine_tag_collision_still_reports_absent`.
    // =====================================================================

    use crate::hashtable::loom_oracle::{KeyOracle, DST, KEY, MID, NEW, SRC};

    /// Discard a lookup's payload, keeping only WHICH of the three answers it
    /// gave — the only thing these models assert on. Written out so that
    /// dropping a `Hit` (and with it its pin, in production) is explicit
    /// rather than an accident of type inference.
    fn erase<T>(lookup: Lookup<T>) -> Lookup<()> {
        match lookup {
            Lookup::Found(_) => Lookup::Found(()),
            Lookup::Absent => Lookup::Absent,
            Lookup::Unknown(location) => Lookup::Unknown(location),
        }
    }

    /// Drive one read entry point through a merge drain that relocates the
    /// key out from under it and recycles the location it was holding.
    ///
    /// The key is LIVE at every instant — at `SRC`, then at `DST`, never
    /// nowhere — so no interleaving may report it MISSING. Asserts:
    ///
    /// 1. **no false absent.** The dangerous interleaving is: reader loads
    ///    the slot (`SRC`), drain relinks to `DST` and recycles `SRC`, and the
    ///    reader then tries to verify a location whose incarnation is gone.
    ///    `Lookup::Absent` there is a live key reported missing.
    ///
    ///    `Lookup::Unknown` is NOT a miss and is the expected answer in that
    ///    interleaving: the pin is refused, so the scan compared nothing and
    ///    says so, and the caller retries (`Segcache::triage_unknown_location`).
    ///    Before #91 the scan recovered inside the hashtable by re-reading the
    ///    slot word; that recovery existed only because an unpinned compare
    ///    could not tell "moved" from "different key", and it retired with the
    ///    ambiguity.
    /// 2. **the relink lands.** Nothing else mutates this entry except the
    ///    reader's frequency bump, which CASes the same slot word. So
    ///    `try_cas_in_bucket` must absorb a lost CAS by re-reading the slot
    ///    rather than giving up — abandoning it there would abort a merge
    ///    mid-candidate. (Only the `lookup` variant bumps; the others reach
    ///    this assertion trivially.)
    /// 3. **the retry converges.** Once the drain has settled, the very same
    ///    read resolves the key — at `DST`. This is what stops (1) from being
    ///    satisfiable by a path that answers `Unknown` forever.
    ///
    /// `read` is a fn pointer rather than a closure so each entry point is
    /// its own `#[test]`: the read helpers each route through the scan, and a
    /// copy that collapsed `Unknown` into `Absent` must fail on its own model,
    /// not hide behind a sibling's.
    fn assert_read_survives_relocation_and_recycle(
        read: fn(&MultiChoiceHashtable, &KeyOracle) -> Lookup<()>,
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
                !matches!(found, Lookup::Absent),
                "FALSE ABSENT: a relocation + recycle racing the key comparison \
                 must not turn a live key into a miss — an unverifiable \
                 candidate is `Unknown` (retry), never `Absent` (gone)"
            );
            assert!(
                relinked,
                "the relink CAS must land: only a reader's frequency bump can \
                 lose it the slot word, and that must cost a retry, not the \
                 relocation"
            );
            assert!(
                matches!(read(&ht, &oracle), Lookup::Found(())),
                "the entry must resolve once the drain has settled: an \
                 `Unknown` the caller can never convert into a hit is a false \
                 absent with extra steps"
            );
            assert_eq!(
                ht.lookup_no_freq_update(KEY, &*oracle)
                    .found()
                    .map(|hit| hit.location),
                Some(KeyOracle::location(DST)),
                "the settled entry must be published at the relocation target"
            );
        });
    }

    /// `lookup` — `search_bucket::<true, _>`, the only read path that also
    /// CASes the slot to bump frequency.
    #[test]
    fn loom_lookup_survives_relocation_and_recycle() {
        assert_read_survives_relocation_and_recycle(|ht, oracle| erase(ht.lookup(KEY, oracle)));
    }

    /// `contains`. No in-tree caller today (the
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
            erase(ht.lookup_no_freq_update(KEY, oracle))
        });
    }

    /// `lookup_slot` — the entry point behind
    /// `segcache`'s replace and numeric-update paths (`lookup_slot` +
    /// `cas_location_at`). A false absent here reports a live key missing to
    /// a caller that is about to relink it.
    #[test]
    fn loom_lookup_slot_survives_relocation_and_recycle() {
        assert_read_survives_relocation_and_recycle(|ht, oracle| {
            erase(ht.lookup_slot(KEY, oracle))
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
            erase(ht.get_frequency(KEY, oracle))
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
                    // `Insert::Unknown` is the caller's rollback-restart arm
                    // (`Segcache::insert`): a candidate slot named a location
                    // whose incarnation is gone, so whether the key already has
                    // an entry is unknown, and publishing on that guess is the
                    // #46 duplicate. Production drops the reservation and
                    // retries; the model retries in place, because the
                    // replacement bytes at `NEW` are already written and a
                    // fresh reservation would only rename them.
                    //
                    // Unbounded, and it terminates for the same reason
                    // production's does: the drain is finite work, and every
                    // `Unknown` is paid for by a recycle that has already
                    // happened.
                    loop {
                        match ht.insert(KEY, KeyOracle::location(NEW), &*oracle) {
                            Ok(Insert::Unknown(_)) => continue,
                            other => return other,
                        }
                    }
                })
            };

            drain.join().unwrap();
            let inserted = writer
                .join()
                .unwrap()
                .expect("insert must not report the table full");

            assert!(
                matches!(inserted, Insert::Replaced(_)),
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
                ht.lookup_no_freq_update(KEY, &*oracle)
                    .found()
                    .map(|hit| hit.location),
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
                ht.lookup_no_freq_update(KEY, &*oracle)
                    .found()
                    .map(|hit| hit.location),
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
                ht.lookup_no_freq_update(KEY, &*oracle)
                    .found()
                    .map(|hit| hit.location),
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
                !ht.cas_location(KEY, stale, KeyOracle::location(DST), RelinkFreq::Preserve),
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
        type Pin = ();

        fn verify(&self, key: &[u8], location: Location, _allow_deleted: bool) -> Verified<()> {
            if key != b"key" {
                return Verified::DifferentKey;
            }
            let live = CHAIN
                .iter()
                .position(|&l| l == location.as_raw())
                .is_some_and(|i| self.valid[i].load(Ordering::Acquire) == 1);
            if live {
                Verified::Match(())
            } else {
                // The location's incarnation is gone: in production the pin is
                // refused, not the compare answered.
                Verified::Unknown(location)
            }
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
                // `get_pinned`: resolve the key from scratch, then re-validate,
                // FOLLOWING the location the revalidation returns rather than
                // looking the key up again.
                //
                // Two counters, because there are now two reasons to go round:
                // `lookups` is every hashtable lookup the get performs, and
                // `resolves` is how many of those were from scratch. The
                // convergence property is that a MISMATCH never costs a
                // from-scratch resolve — only an unverifiable candidate does,
                // and that one is paid for by a real recycle.
                let mut lookups = 0;
                let mut resolves = 0;
                let mut attempts = 0;
                'resolve: loop {
                    lookups += 1;
                    resolves += 1;
                    let mut location = match ht_reader.lookup_no_freq_update(b"key", &*o_reader) {
                        Lookup::Found(hit) => hit.location,
                        Lookup::Absent => return (None, lookups, resolves),
                        // The pin was refused, so nothing was compared:
                        // `triage_unknown_location` waits and the outer loop
                        // re-resolves.
                        Lookup::Unknown(_) => continue 'resolve,
                    };
                    loop {
                        // (pin `location` — see the scope note above)
                        lookups += 1;
                        match ht_reader.lookup_no_freq_update(b"key", &*o_reader) {
                            Lookup::Found(hit) if hit.location == location => {
                                return (Some(location), lookups, resolves)
                            }
                            Lookup::Found(hit) => {
                                attempts += 1;
                                if attempts >= crate::segcache::REVALIDATE_RETRIES {
                                    return (None, lookups, resolves);
                                }
                                location = hit.location;
                            }
                            Lookup::Absent => return (None, lookups, resolves),
                            Lookup::Unknown(_) => continue 'resolve,
                        }
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
                            RelinkFreq::Preserve
                        ),
                        "relink CAS must land: nothing else touches this entry"
                    );
                    // 3. The superseded item's segment is recycled.
                    o_writer.valid[i].store(0, Ordering::Release);
                }
            });

            let (resolved, lookups, resolves) = reader.join().unwrap();
            writer.join().unwrap();

            // One from-scratch lookup per RESOLVE, then one revalidation per
            // attempt. Re-resolving the key after a mismatch — what the pre-#65
            // loop did — doubles this and is what the budget was being spent
            // on.
            //
            // `resolves` appears in the bound rather than being pinned at 1
            // because an unverifiable candidate legitimately costs a fresh
            // resolve: the location the reader was holding names an incarnation
            // that is gone, so there is nothing left to follow. What must never
            // cost one is a MISMATCH, and that is exactly what this still
            // catches — the pre-#65 loop re-resolved on every mismatch while a
            // faithful `resolves` stayed at 1.
            assert!(
                lookups <= CHAIN.len() + resolves,
                "each retry must follow the location the revalidation already \
                 returned: {lookups} lookups ({resolves} of them from scratch) \
                 for {} republications means an attempt re-raced from scratch",
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
                ht.lookup(b"key", &*oracle).is_found(),
                "the entry must still resolve once the writer has settled"
            );
        });
    }
}

// Shuttle twins of two slot-protocol models: randomized schedules under
// sequential consistency, complementary to the exhaustive loom suite above
// (see the note on `segments/header.rs`'s shuttle module for the full
// division of labor). The slot protocol's invariants are SC-independent —
// loom already verifies them exhaustively within its preemption bound —
// so these twins buy randomized depth BEYOND that bound and prove the
// shuttle wiring against the production hashtable, cheap insurance both.
// A failing model prints a schedule string for `shuttle::replay`.
#[cfg(all(test, feature = "shuttle", not(feature = "loom")))]
mod shuttle_tests {
    use super::*;
    use crate::hashtable::loom_oracle::{KeyOracle, DST, KEY, SRC};
    use crate::hashtable::traits::Hashtable;
    use crate::sync::shuttle_iters;
    use shuttle::thread;
    use std::sync::Arc;

    /// See `loom_tests::AlwaysVerifier` for what this stub can and cannot
    /// model; the fresh-key model below is about mutex-serialized entry
    /// creation, where key identity plays no part.
    struct AlwaysVerifier;

    impl KeyVerifier for AlwaysVerifier {
        /// No storage behind it, so nothing to pin.
        type Pin = ();

        fn verify(&self, _key: &[u8], _location: Location, _allow_deleted: bool) -> Verified<()> {
            Verified::Match(())
        }
    }

    /// Randomized twin of `loom_lookup_survives_relocation_and_recycle`:
    /// a merge drain relocates the key and recycles its old location while
    /// a reader races the lookup. The key is live at every instant, so the
    /// read must find it in every schedule — this is the verify-ABA
    /// false-absent shape that reproduced at ~1 in 2,400 stress runs and
    /// that both model checkers catch in well under a second.
    #[test]
    fn shuttle_lookup_never_false_absent_under_relocation() {
        shuttle::check_random(
            || {
                let ht = Arc::new(MultiChoiceHashtable::new(7));
                let oracle = Arc::new(KeyOracle::new());

                oracle.place(SRC, KEY);
                ht.insert(KEY, KeyOracle::location(SRC), &*oracle)
                    .expect("seed insert");

                let reader = {
                    let ht = Arc::clone(&ht);
                    let oracle = Arc::clone(&oracle);
                    thread::spawn(move || matches!(ht.lookup(KEY, &*oracle), Lookup::Absent))
                };

                let drain = {
                    let ht = Arc::clone(&ht);
                    let oracle = Arc::clone(&oracle);
                    thread::spawn(move || oracle.drain_relocate(&ht, SRC, DST))
                };

                let absent = reader.join().unwrap();
                let relinked = drain.join().unwrap();

                assert!(
                    !absent,
                    "FALSE ABSENT: a relocation + recycle racing the key comparison \
                     must not turn a live key into a miss — an unverifiable \
                     candidate is `Unknown` (retry), never `Absent` (gone)"
                );
                assert!(
                    relinked,
                    "the relink CAS must land: only a reader's frequency bump can \
                     lose it the slot word, and that must cost a retry, not the \
                     relocation"
                );
                assert_eq!(
                    ht.lookup_no_freq_update(KEY, &*oracle)
                        .found()
                        .map(|hit| hit.location),
                    Some(KeyOracle::location(DST)),
                    "the settled entry must be published at the relocation target"
                );
            },
            shuttle_iters(20_000),
        );
    }

    /// Randomized twin of `loom_fresh_key_insert_single_entry`: two racing
    /// first inserts of one key must resolve to exactly one creator and one
    /// live entry (the stripe lock serializes entry creation).
    #[test]
    fn shuttle_fresh_key_insert_single_entry() {
        shuttle::check_random(
            || {
                let ht = Arc::new(MultiChoiceHashtable::new(7));
                let verifier = Arc::new(AlwaysVerifier);

                let ht1 = Arc::clone(&ht);
                let v1 = Arc::clone(&verifier);
                let t1 = thread::spawn(move || ht1.insert(b"key", Location::new(1), &*v1));

                let ht2 = Arc::clone(&ht);
                let v2 = Arc::clone(&verifier);
                let t2 = thread::spawn(move || ht2.insert(b"key", Location::new(2), &*v2));

                let r1 = t1.join().unwrap();
                let r2 = t2.join().unwrap();

                assert!(r1.is_ok() && r2.is_ok());
                assert_eq!(
                    [&r1, &r2]
                        .iter()
                        .filter(|r| matches!(r, Ok(Insert::Created)))
                        .count(),
                    1,
                    "exactly one racer creates; the other must replace"
                );

                // Count live same-tag entries across the key's candidate
                // buckets, deduping coincident bucket indices — same scan as
                // the loom twin.
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
            },
            shuttle_iters(20_000),
        );
    }
}
