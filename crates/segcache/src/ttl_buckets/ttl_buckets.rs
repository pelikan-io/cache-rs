//! Collection of TTL buckets covering the full TTL range.
//!
//! 1024 buckets organized in 4 logarithmic tiers:
//!
//! | Tier | TTL range          | Bucket width | Buckets |
//! |------|--------------------|--------------|---------|
//! | 1    | 1s – 2048s         | 8s           | 256     |
//! | 2    | 2048s – 32,768s    | 128s         | 256     |
//! | 3    | 32,768s – 524,288s | 2,048s       | 256     |
//! | 4    | 524,288s – 8.4Ms   | 32,768s      | 256     |
//!
//! TTL of 0 (no expiry) and TTLs beyond ~97 days map to the last bucket.

use crate::sync::Ordering;
use crate::*;
use clocksource::coarse::AtomicInstant;
// Deliberately aliased: `Instant` in this crate is the *coarse* (1-second)
// clock, which is correct for expiry deadlines but cannot measure the
// sub-millisecond duration of a sweep. Duration measurement uses this.
use std::time::Instant as StdInstant;

const BUCKETS_PER_TIER: usize = 256;
const TIER_COUNT: usize = 4;
const TOTAL_BUCKETS: usize = BUCKETS_PER_TIER * TIER_COUNT;

// Tier widths as bit shifts (each tier is 4x wider than the previous).
const TIER_1_SHIFT: usize = 3; //   8s
const TIER_2_SHIFT: usize = 7; // 128s
const TIER_3_SHIFT: usize = 11; // 2048s
const TIER_4_SHIFT: usize = 15; // 32768s

// Tier boundaries: the max TTL (exclusive) that fits in each tier.
const TIER_1_MAX: i32 = 1 << (TIER_1_SHIFT + 8); //   2,048
const TIER_2_MAX: i32 = 1 << (TIER_2_SHIFT + 8); //  32,768
const TIER_3_MAX: i32 = 1 << (TIER_3_SHIFT + 8); // 524,288

/// The full collection of TTL buckets.
pub struct TtlBuckets {
    pub(crate) buckets: Box<[TtlBucket]>,
    pub(crate) last_expired: AtomicInstant,
}

impl TtlBuckets {
    /// Create a new set of 1024 TTL buckets covering the full TTL range.
    pub fn new() -> Self {
        let widths = [
            1 << TIER_1_SHIFT,
            1 << TIER_2_SHIFT,
            1 << TIER_3_SHIFT,
            1 << TIER_4_SHIFT,
        ];

        let mut buckets = Vec::with_capacity(TOTAL_BUCKETS);
        for width in &widths {
            for j in 0..BUCKETS_PER_TIER {
                let ttl = width * j + 1;
                buckets.push(TtlBucket::new(ttl as i32));
            }
        }

        Self {
            buckets: buckets.into_boxed_slice(),
            last_expired: AtomicInstant::now(),
        }
    }

    /// Map a TTL duration to its bucket index (0–1023).
    pub(crate) fn get_bucket_index(&self, ttl: Duration) -> usize {
        bucket_index(ttl.as_secs() as i32)
    }

    /// Get the bucket for the given TTL.
    pub(crate) fn get_bucket(&self, ttl: Duration) -> &TtlBucket {
        let index = self.get_bucket_index(ttl);
        // SAFETY: get_bucket_index always returns a valid index.
        unsafe { self.buckets.get_unchecked(index) }
    }

    /// Run eager expiration across all buckets. Returns total segments expired.
    ///
    /// The once-per-tick debounce (`last_expired`) is an atomic swap rather
    /// than a plain compare-and-set: under `&self` more than one caller can
    /// race this method, and the swap still admits exactly one winner per
    /// coarse tick (the loser observes its own freshly-stored value and
    /// skips the redundant pass) without needing any lock.
    pub(crate) fn expire(&self, hashtable: &MultiChoiceHashtable, segments: &Segments) -> usize {
        let now = Instant::now();
        if self.last_expired.swap(now, Ordering::Relaxed) == now {
            return 0;
        }

        let start = StdInstant::now();
        let mut expired = 0;
        for bucket in self.buckets.iter() {
            expired += bucket.expire(hashtable, segments);
        }
        let duration = start.elapsed();
        debug!("expired: {expired} segments in {duration:?}");

        #[cfg(feature = "metrics")]
        EXPIRE_TIME.add(duration.as_nanos() as _);

        expired
    }

    /// Clear all segments across all buckets. Returns total segments cleared.
    pub(crate) fn clear(&self, hashtable: &MultiChoiceHashtable, segments: &Segments) -> usize {
        let start = StdInstant::now();
        let mut cleared = 0;
        for bucket in self.buckets.iter() {
            cleared += bucket.clear(hashtable, segments);
        }
        let duration = start.elapsed();
        debug!("cleared: {cleared} segments in {duration:?}");

        #[cfg(feature = "metrics")]
        CLEAR_TIME.add(duration.as_nanos() as _);

        cleared
    }
}

/// The pure tier arithmetic behind [`TtlBuckets::get_bucket_index`],
/// separated from the collection so the `< TOTAL_BUCKETS` bound —
/// which `get_bucket`'s `get_unchecked` rests on — is a checkable fact
/// about a function of one integer rather than a claim about `self`.
/// Proven total by the Kani harness below.
fn bucket_index(secs: i32) -> usize {
    if secs <= 0 {
        TOTAL_BUCKETS - 1
    } else if secs & !(TIER_1_MAX - 1) == 0 {
        (secs >> TIER_1_SHIFT) as usize
    } else if secs & !(TIER_2_MAX - 1) == 0 {
        (secs >> TIER_2_SHIFT) as usize + BUCKETS_PER_TIER
    } else if secs & !(TIER_3_MAX - 1) == 0 {
        (secs >> TIER_3_SHIFT) as usize + BUCKETS_PER_TIER * 2
    } else {
        let idx = (secs >> TIER_4_SHIFT) as usize + BUCKETS_PER_TIER * 3;
        idx.min(TOTAL_BUCKETS - 1)
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    /// Every possible seconds value — negative, zero, the full i32 range
    /// — maps inside the bucket array. This is the bound
    /// `TtlBuckets::get_bucket`'s `get_unchecked` relies on, previously
    /// carried by a SAFETY comment.
    #[kani::proof]
    fn bucket_index_always_in_range() {
        let secs: i32 = kani::any();
        assert!(bucket_index(secs) < TOTAL_BUCKETS);
    }

    /// The mapping is globally monotone over positive TTLs — across tier
    /// boundaries and the tier-4 clamp included: a longer TTL never maps
    /// to an earlier bucket (so eager expiration's per-bucket cutoffs
    /// stay ordered).
    #[kani::proof]
    fn bucket_index_monotone_for_positive() {
        let a: i32 = kani::any();
        let b: i32 = kani::any();
        kani::assume(a > 0 && b > 0 && a <= b);
        assert!(bucket_index(a) <= bucket_index(b));
    }
}

impl Default for TtlBuckets {
    fn default() -> Self {
        Self::new()
    }
}
