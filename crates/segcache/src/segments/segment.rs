//! Segment view combining a header and data slice.
//!
//! A `Segment` provides operations on a single segment's data, delegating
//! metadata access to the atomic fields in [`SegmentHeader`].

use super::{SegmentHeader, SegmentPool, SegmentsError};
use crate::*;
use core::num::NonZeroU32;

pub const SEG_MAGIC: u64 = 0xBADC0FFEEBADCAFE;

/// A view of a single segment, combining a shared header reference with
/// a mutable data slice. The header is accessed via shared reference
/// since all its fields are atomic.
pub struct Segment<'a> {
    header: &'a SegmentHeader,
    data: &'a mut [u8],
}

impl<'a> Segment<'a> {
    /// Construct a `Segment` from its raw parts.
    pub fn from_raw_parts(header: &'a SegmentHeader, data: &'a mut [u8]) -> Self {
        Segment { header, data }
    }

    /// Returns a raw pointer to the segment's data buffer.
    pub fn data_ptr(&self) -> *mut u8 {
        self.data.as_ptr() as *mut u8
    }

    /// Initialize the segment. Sets magic bytes (if enabled) and resets header.
    pub fn init(&mut self) {
        if cfg!(feature = "integrity") {
            for (i, byte) in SEG_MAGIC.to_be_bytes().iter().enumerate() {
                self.data[i] = *byte;
            }
        }
        self.header.init();
    }

    #[cfg(feature = "integrity")]
    #[inline]
    pub fn magic(&self) -> u64 {
        u64::from_be_bytes([
            self.data[0],
            self.data[1],
            self.data[2],
            self.data[3],
            self.data[4],
            self.data[5],
            self.data[6],
            self.data[7],
        ])
    }

    #[inline]
    pub fn check_magic(&self) {
        #[cfg(feature = "integrity")]
        assert_eq!(self.magic(), SEG_MAGIC)
    }

    /// Maximum valid item start offset within the data slice.
    pub(crate) fn max_item_offset(&self) -> usize {
        if self.write_offset() >= ITEM_HDR_SIZE as i32 {
            std::cmp::min(self.write_offset() as usize, self.data.len()) - ITEM_HDR_SIZE
        } else if cfg!(feature = "integrity") {
            std::mem::size_of_val(&SEG_MAGIC)
        } else {
            0
        }
    }

    #[cfg(feature = "debug")]
    pub(crate) fn check_integrity(&self, hashtable: &MultiChoiceHashtable) -> bool {
        self.check_magic();

        let mut integrity = true;
        let max_offset = self.max_item_offset();
        let mut offset = if cfg!(feature = "integrity") {
            std::mem::size_of_val(&SEG_MAGIC)
        } else {
            0
        };

        let mut count = 0;

        while offset < max_offset {
            let item = RawItem::from_ptr((self.data.as_ptr() as *mut u8).wrapping_add(offset));
            if item.klen() == 0 {
                break;
            }

            if !item.is_deleted() {
                let loc = pack_location(self.id(), offset as u64);
                let deleted = hashtable.get_item_frequency(item.key(), loc).is_none();
                if !deleted {
                    count += 1;
                }
            }
            offset += item.size();
        }

        if count != self.live_items() {
            error!(
                "seg: {} has mismatch between counted items: {} and header items: {}",
                self.id(),
                count,
                self.live_items()
            );
            integrity = false;
        }

        integrity
    }

    // -- Header delegation (all via shared reference) --

    #[inline]
    pub fn id(&self) -> NonZeroU32 {
        self.header.id()
    }

    #[inline]
    pub fn write_offset(&self) -> i32 {
        self.header.write_offset()
    }

    #[inline]
    pub fn set_write_offset(&self, bytes: i32) {
        self.header.set_write_offset(bytes);
    }

    #[inline]
    pub fn live_bytes(&self) -> i32 {
        self.header.live_bytes()
    }

    #[inline]
    pub fn live_items(&self) -> i32 {
        self.header.live_items()
    }

    #[inline]
    pub fn incr_live_items(&self) {
        self.header.incr_live_items();
    }

    #[inline]
    pub fn incr_live_bytes(&self, bytes: i32) {
        self.header.incr_live_bytes(bytes);
    }

    #[inline]
    pub fn state(&self) -> State {
        self.header.state()
    }

    #[inline]
    pub fn header_metadata(&self) -> crate::segments::state::Metadata {
        self.header.metadata(crate::sync::Ordering::Acquire)
    }

    #[inline]
    pub fn cas_metadata(
        &self,
        expected_state: State,
        new_state: State,
        new_next: Option<Option<NonZeroU32>>,
        new_prev: Option<Option<NonZeroU32>>,
        success: crate::sync::Ordering,
    ) -> bool {
        self.header
            .cas_metadata(expected_state, new_state, new_next, new_prev, success)
    }

    #[inline]
    pub fn can_evict(&self) -> bool {
        self.header.can_evict()
    }

    #[inline]
    pub fn header_ref_count_seqcst(&self) -> u32 {
        self.header.ref_count_seqcst()
    }

    #[inline]
    pub fn ttl(&self) -> Duration {
        self.header.ttl()
    }

    #[inline]
    pub fn create_at(&self) -> Instant {
        self.header.create_at()
    }

    #[inline]
    pub fn pool(&self) -> SegmentPool {
        self.header.pool()
    }

    #[inline]
    pub fn set_pool(&self, pool: SegmentPool) {
        self.header.set_pool(pool);
    }

    // -- Item operations --

    /// Remove an item at the given offset, decrementing live counters.
    pub(crate) fn remove_item_at(&self, offset: usize) {
        let item = self.get_item_at(offset).unwrap();
        let item_size = item.size() as i32;

        #[cfg(feature = "metrics")]
        {
            ITEM_CURRENT.decrement();
            ITEM_CURRENT_BYTES.sub(item_size as _);
            ITEM_DEAD.increment();
            ITEM_DEAD_BYTES.add(item_size as _);
        }

        self.check_magic();
        self.header.decr_item(item_size);
        assert!(self.live_bytes() >= 0);
        assert!(self.live_items() >= 0);

        self.check_magic();
    }

    /// Get a `RawItem` at the given offset within the segment data.
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn get_item_at(&self, offset: usize) -> Option<RawItem> {
        assert!(offset <= self.max_item_offset());
        Some(RawItem::from_ptr(
            (self.data.as_ptr() as *mut u8).wrapping_add(offset),
        ))
    }

    /// Copy live items from this segment into the target segment,
    /// relinking them in the hashtable.
    pub(crate) fn copy_into(
        &mut self,
        target: &mut Segment,
        hashtable: &MultiChoiceHashtable,
    ) -> Result<(), SegmentsError> {
        let max_offset = self.max_item_offset();
        let mut read_offset = if cfg!(feature = "integrity") {
            std::mem::size_of_val(&SEG_MAGIC)
        } else {
            0
        };

        #[cfg(feature = "metrics")]
        let mut items_copied = 0;
        #[cfg(feature = "metrics")]
        let mut bytes_copied = 0;

        while read_offset <= max_offset {
            let item = self.get_item_at(read_offset).unwrap();
            if item.klen() == 0 && self.live_items() == 0 {
                break;
            }

            item.check_magic();
            let item_size = item.size();
            let write_offset = target.write_offset() as usize;

            let old_loc = pack_location(self.id(), read_offset as u64);
            let deleted =
                item.is_deleted() || hashtable.get_item_frequency(item.key(), old_loc).is_none();
            if deleted || write_offset + item_size >= target.data.len() {
                read_offset += item_size;
                continue;
            }

            let src = unsafe { self.data.as_ptr().add(read_offset) };
            let dst = unsafe { target.data.as_mut_ptr().add(write_offset) };

            let new_loc = pack_location(target.id(), write_offset as u64);
            // NUMERIC RELOCATION GATE: in-place numeric writers
            // (`numeric_update`) mutate item bytes holding only a READER
            // pin plus the item's seqlock writer lock — and the drain
            // claim behind this copy waits on writers/removers, NOT
            // readers. Without this gate the raw copy below races the
            // value/CRC stores (torn copy, formally a data race), can
            // capture the version word in its transient ODD state and
            // publish a permanently write-in-progress destination (every
            // subsequent seqlock read/incr of the key spins forever), and
            // can publish a pre-increment value AFTER the increment acked
            // (a lost acked increment). Taking the item's version lock
            // across the byte copy AND the relink CAS closes all three:
            // an increment that locked first completes before we read
            // (the copy carries its final value/CRC); one that arrives
            // while we hold the lock spins, and its in-lock linkage
            // re-validation then observes the published new location and
            // retries against the destination — no lost ack. The copied
            // version word is odd (our own lock), so the destination is
            // stamped back to the guard's frozen even version before the
            // publish. Deadlock-free: the version lock is a leaf — its
            // holders (`numeric_update`, `replace_at`) do bounded
            // lock-free work and never wait on chain locks, claims, or
            // pins (and `replace_at` cannot even hold it on an item in
            // this Draining source: it requires a remover pin the claim
            // already waited out). Non-numeric items stay on the raw
            // copy: they are immutable in place once published (the only
            // header mutation, delete's `set_deleted`, runs under a
            // remover pin, which the drain claim waited out).
            let vguard = item.lock_numeric_version().ok();
            // Copy-then-publish: write the bytes into the destination BEFORE the
            // Release-CAS publishes the new location. The Release success ordering
            // on cas_location orders these writes ahead of the publish, so a
            // reader that observes new_loc (Acquire) always sees the copied bytes.
            // On CAS failure the bytes are orphaned at dst (write_offset is not
            // advanced, nothing points here), and we abort the copy.
            unsafe {
                std::ptr::copy_nonoverlapping(src, dst, item_size);
            }
            if let Some(guard) = &vguard {
                guard.stamp_relocated_copy(&RawItem::from_ptr(dst));
            }
            if hashtable.cas_location(item.key(), old_loc, new_loc, true) {
                // Unlock only AFTER the publish resolved: a numeric writer
                // spinning on this lock re-validates its linkage inside the
                // lock, and the acquire it wins synchronizes-with this drop's
                // Release store, making the new location visible to it.
                drop(vguard);
                self.remove_item_at(read_offset);
                target.header.incr_live_items();
                target.header.incr_live_bytes(item_size as i32);
                target.set_write_offset(write_offset as i32 + item_size as i32);

                #[cfg(feature = "metrics")]
                {
                    ITEM_RELINK.increment();
                    items_copied += 1;
                    bytes_copied += item_size;
                }
            } else {
                drop(vguard);
                return Err(SegmentsError::RelinkFailure);
            }

            read_offset += item_size;
        }

        #[cfg(feature = "metrics")]
        {
            ITEM_CURRENT.add(items_copied);
            ITEM_CURRENT_BYTES.add(bytes_copied as _);
        }

        Ok(())
    }

    /// Prune low-frequency items from the segment based on a cutoff.
    /// Returns the adjusted cutoff frequency.
    pub(crate) fn prune(
        &mut self,
        hashtable: &MultiChoiceHashtable,
        cutoff_freq: f64,
        target_ratio: f64,
    ) -> f64 {
        let max_offset = self.max_item_offset();
        let mut offset = if cfg!(feature = "integrity") {
            std::mem::size_of_val(&SEG_MAGIC)
        } else {
            0
        };

        let to_keep = (self.data.len() as f64 * target_ratio).floor() as i32;
        let to_drop = self.live_bytes() - to_keep;

        let mut n_scanned = 0;
        let mut n_dropped = 0;
        let mut n_retained = 0;

        let mean_size = self.live_bytes() as f64 / self.live_items() as f64;
        let mut cutoff = (1.0 + cutoff_freq) / 2.0;
        let mut n_th_update = 1;
        let update_interval = self.data.len() / 10;

        while offset <= max_offset {
            let item = self.get_item_at(offset).unwrap();
            if item.klen() == 0 && self.live_items() == 0 {
                break;
            }

            item.check_magic();
            let item_size = item.size();

            // Fast path: item was explicitly deleted — no hashtable lookup needed.
            if item.is_deleted() {
                offset += item_size;
                continue;
            }

            let loc = pack_location(self.id(), offset as u64);
            // Fallback for items deleted before is_deleted was introduced.
            let deleted = hashtable.get_item_frequency(item.key(), loc).is_none();
            if deleted {
                offset += item_size;
                continue;
            }

            n_scanned += item_size;

            if n_scanned >= (n_th_update * update_interval) {
                n_th_update += 1;
                let t = ((n_retained as f64) / (n_scanned as f64) - target_ratio) / target_ratio;
                if !(-0.5..=0.5).contains(&t) {
                    // Floor the multiplier: a degenerate early reading — e.g.
                    // `n_retained == 0` at the first checkpoint (all cold items
                    // dropped so far) gives `t == -1`, and a bare `1.0 + t == 0`
                    // would zero `cutoff` PERMANENTLY (0 stays 0), disabling the
                    // `cutoff >= 0.0001` drop-gate for the rest of the segment so
                    // prune retains the WHOLE candidate. That over-retention can
                    // starve the free queue and livelock `reserve_and_define`.
                    // Bounding the shrink to 0.25x/step keeps the adaptive
                    // direction while never collapsing cutoff to zero.
                    cutoff *= (1.0 + t).max(0.25);
                }
                trace!("cutoff adj to: {cutoff}");
            }

            let item_frequency = hashtable.get_item_frequency(item.key(), loc).unwrap_or(0) as f64;
            let weighted_frequency = item_frequency / (item_size as f64 / mean_size);

            if cutoff >= 0.0001
                && to_drop > 0
                && n_dropped < to_drop as usize
                && weighted_frequency <= cutoff
            {
                trace!(
                    "evicting item size: {item_size} freq: {item_frequency} w_freq: {weighted_frequency} cutoff: {cutoff}"
                );
                if hashtable.remove(item.key(), loc) {
                    self.remove_item_at(offset);

                    #[cfg(feature = "metrics")]
                    ITEM_EVICT.increment();
                }
                n_dropped += item_size;
                offset += item_size;
                continue;
            } else {
                trace!(
                    "keeping item size: {item_size} freq: {item_frequency} w_freq: {weighted_frequency} cutoff: {cutoff}"
                );
            }

            offset += item_size;
            n_retained += item_size;
        }

        cutoff
    }

    /// Clear all items from the segment, unlinking them from the hashtable.
    pub(crate) fn clear(&mut self, hashtable: &MultiChoiceHashtable, expire: bool) {
        debug_assert_eq!(
            self.state(),
            State::Draining,
            "callers own the Draining transition before clearing"
        );

        let max_offset = self.max_item_offset();
        let mut offset = if cfg!(feature = "integrity") {
            std::mem::size_of_val(&SEG_MAGIC)
        } else {
            0
        };

        while offset <= max_offset {
            let item = self.get_item_at(offset).unwrap();
            if item.klen() == 0 && self.live_items() == 0 {
                break;
            }

            item.check_magic();

            debug_assert!(item.klen() > 0, "invalid klen: ({})", item.klen());

            // F1 (single decrement): only the unlinker decrements. A
            // `remove` returning false therefore has to mean another
            // unlinker/replacer owns this entry — which holds because
            // `try_unlink_in_bucket` retries the same slot across a
            // racing reader's freq-bump CAS instead of abandoning the
            // entry (table.rs). Without that retry a spurious false here
            // would recycle the segment with the entry still published.
            let loc = pack_location(self.id(), offset as u64);
            let deleted = hashtable.get_item_frequency(item.key(), loc).is_none();
            if !deleted && hashtable.remove(item.key(), loc) {
                trace!("evicting from hashtable");
                self.remove_item_at(offset);

                #[cfg(feature = "metrics")]
                if expire {
                    ITEM_EXPIRE.increment();
                } else {
                    ITEM_EVICT.increment();
                }
            }

            debug_assert!(
                self.live_items() >= 0,
                "cleared segment has invalid number of live items: ({})",
                self.live_items()
            );
            debug_assert!(
                self.live_bytes() >= 0,
                "cleared segment has invalid number of live bytes: ({})",
                self.live_bytes()
            );
            offset += item.size();
        }

        // Item 7f: `clear` does NOT assert the segment is empty here. The
        // `active_removers == 0` wait (claim_for_drain, drain_chain) +
        // try_pin_remover's recheck-bail cover every replace/delete remove that
        // PINS BEFORE UNLINKING — those decrement before this sweep runs and are
        // then skipped (get_item_frequency is None). But the raced-old handling
        // in `Segcache::insert`'s fresh-key arm (a racing writer published
        // between the lookup miss and the hashtable upsert, resolved to a
        // replace under the stripe re-check) and the hashtable-full rollback
        // path unlink an entry WITHOUT holding a remover pin — a narrow,
        // ACCEPTED gap: if that item's segment is being cleared, it can be
        // unlinked-but-not-yet-decremented while we sweep, so it is
        // counted-yet-skipped and `live_items`/`live_bytes` may be transiently
        // over-counted. Not corruption (it self-heals when the segment is
        // recycled: `init()` resets the counters) — the drain owns the
        // segment's accounting wholesale. A synchronous "empty after clear"
        // assertion is therefore not a valid concurrent invariant; the
        // crash-direction (`live_bytes() >= 0`) is still asserted
        // per-decrement in `remove_item_at`. Reclaim uses the live counters, so
        // set write_offset to whatever remains.
        self.set_write_offset(self.live_bytes());
    }
}

#[cfg(feature = "integrity")]
impl std::fmt::Debug for Segment<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        f.debug_struct("Segment")
            .field("header", &self.header)
            .field("magic", &format!("0x{:X}", self.magic()))
            .field("data", &format!("{:02X?}", self.data))
            .finish()
    }
}

#[cfg(not(feature = "integrity"))]
impl std::fmt::Debug for Segment<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        f.debug_struct("Segment")
            .field("header", &self.header)
            .field("data", &format!("{:X?}", self.data))
            .finish()
    }
}
