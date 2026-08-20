//! A single TTL bucket containing a segment chain.
//!
//! Items with similar TTLs are stored in segments linked together in a
//! doubly-linked chain. The head segment is always the oldest, enabling
//! O(1) expiration by checking only the head.
//!
//! ```text
//! ┌──────────────┬──────────────┬─────────────┬──────────────┐
//! │   HEAD SEG   │   TAIL SEG   │     TTL     │     NSEG     │
//! │              │              │             │              │
//! │    32 bit    │    32 bit    │    32 bit   │    32 bit    │
//! ├──────────────┼──────────────┴─────────────┴──────────────┤
//! │  NEXT MERGE  │                  PADDING                  │
//! │              │                                           │
//! │    32 bit    │                  96 bit                   │
//! ├──────────────┴───────────────────────────────────────────┤
//! │                         PADDING                          │
//! │                                                          │
//! │                         128 bit                          │
//! ├──────────────────────────────────────────────────────────┤
//! │                         PADDING                          │
//! │                                                          │
//! │                         128 bit                          │
//! └──────────────────────────────────────────────────────────┘
//! ```

use crate::segments::AllocOutcome;
use crate::sync::{AtomicU32, Ordering};
use crate::*;
use core::num::NonZeroU32;
use crossbeam_utils::Backoff;

/// A TTL bucket holding a doubly-linked segment chain.
///
/// Padded to exactly 64 bytes (one cache line). Chain pointers use the
/// 0-is-none convention (segment ids are `NonZeroU32`), matching the
/// packed metadata links in `segments::state`.
///
/// Acquire/Release on head/tail: concurrent reservers read the tail
/// word and must see the winner's published chain state.
pub struct TtlBucket {
    head: AtomicU32,
    tail: AtomicU32,
    ttl: i32,
    /// Total segments ever linked (never decremented; read only by
    /// tests today).
    nseg: AtomicU32,
    next_to_merge: AtomicU32,
    /// Serializes all chain-STRUCTURE mutations of THIS bucket (head/tail
    /// pointer updates and the prev/next neighbour patches done as chain
    /// surgery): `reserve`'s `try_expand` (link/seal/set_tail), eviction's
    /// dest head-insert (`link_dest_at_head` + `set_head`), each drained
    /// candidate's `finalize_drained` unlink/splice, `drain_chain`/`expire`/
    /// `clear`, and `remove_at`'s empty-free + head fixup all take it.
    ///
    /// The reserve hot path never touches it: `try_alloc_item` (the CAS on
    /// `write_offset`) makes no chain change, so only the infrequent
    /// `try_expand` (tail full -> new segment) acquires the lock. Held only
    /// around the brief per-bucket pointer surgery.
    ///
    /// Boxed so the `TtlBucket` stays exactly 64 bytes (one cache line, a
    /// `std::sync::Mutex` is not a fixed size across platforms) with the hot
    /// head/tail atomics cache-line-local; the mutex itself lives off-line.
    ///
    /// Lock order: `chain_lock` is OUTER to `Segments::evict` (the eviction
    /// policy Mutex) — code may take `evict` while holding this, never the
    /// reverse.
    // LOCK: bucket-chain
    chain_lock: Box<std::sync::Mutex<()>>,
    _pad: [u8; 36],
}

// Loom atomics are larger than std atomics, so skip size check under loom.
#[cfg(not(feature = "loom"))]
const _: () = assert!(std::mem::size_of::<TtlBucket>() == 64);

impl TtlBucket {
    /// Create an empty bucket for the given TTL.
    pub(super) fn new(ttl: i32) -> Self {
        Self {
            head: AtomicU32::new(0),
            tail: AtomicU32::new(0),
            ttl,
            nseg: AtomicU32::new(0),
            next_to_merge: AtomicU32::new(0),
            chain_lock: Box::new(std::sync::Mutex::new(())),
            _pad: [0; 36],
        }
    }

    /// Acquire this bucket's chain-structure lock. See the field docs and the
    /// lock inventory in the design spec (`docs/superpowers/specs/...`). Held
    /// only around brief per-bucket chain pointer surgery.
    pub(crate) fn chain_lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.chain_lock.lock().unwrap()
    }

    /// Head of the segment chain (oldest segment).
    pub fn head(&self) -> Option<NonZeroU32> {
        NonZeroU32::new(self.head.load(Ordering::Acquire))
    }

    /// Set the head segment.
    pub fn set_head(&self, id: Option<NonZeroU32>) {
        self.head
            .store(id.map_or(0, NonZeroU32::get), Ordering::Release);
    }

    /// Tail of the segment chain (the writable segment, when Live).
    pub(crate) fn tail(&self) -> Option<NonZeroU32> {
        NonZeroU32::new(self.tail.load(Ordering::Acquire))
    }

    /// Set the tail segment.
    fn set_tail(&self, id: Option<NonZeroU32>) {
        self.tail
            .store(id.map_or(0, NonZeroU32::get), Ordering::Release);
    }

    /// Elect the first segment of an empty bucket: CAS the tail word
    /// from empty to `id`. Exactly one concurrent expander wins.
    fn cas_tail_none_to(&self, id: NonZeroU32) -> bool {
        self.tail
            .compare_exchange(0, id.get(), Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Total segments ever linked into this bucket.
    #[cfg(all(test, not(feature = "loom")))]
    pub(crate) fn nseg(&self) -> u32 {
        self.nseg.load(Ordering::Relaxed)
    }

    /// Next segment to merge (for merge eviction policy).
    /// Relaxed: a soft merge-resume hint; concurrent `&self` evictors may race
    /// it harmlessly — the `Sealed->Draining` claim CAS, not this field, guards
    /// candidate mutation (spec §1: redundant selection is harmless).
    pub fn next_to_merge(&self) -> Option<NonZeroU32> {
        NonZeroU32::new(self.next_to_merge.load(Ordering::Relaxed))
    }

    /// Set the next merge target.
    pub fn set_next_to_merge(&self, next: Option<NonZeroU32>) {
        self.next_to_merge
            .store(next.map_or(0, NonZeroU32::get), Ordering::Relaxed);
    }

    /// Expire segments whose TTL has elapsed.
    ///
    /// Walks the chain from head, draining segments whose
    /// `create_at + ttl <= now`. Unpinned segments are freed; a segment
    /// pinned by readers is condemned (AwaitingRelease) and unlinked
    /// immediately — the last reader's guard drop frees it. Returns the
    /// number of segments actually freed by this pass.
    pub(super) fn expire(&self, hashtable: &MultiChoiceHashtable, segments: &Segments) -> usize {
        let now = Instant::now();
        self.drain_chain(hashtable, segments, Some(now))
    }

    /// Clear all segments in this bucket, draining every one from the
    /// hashtable. Unpinned segments are freed; pinned ones are condemned
    /// and freed by the last reader's guard drop. Returns the number of
    /// segments actually freed by this pass.
    pub(super) fn clear(&self, hashtable: &MultiChoiceHashtable, segments: &Segments) -> usize {
        self.drain_chain(hashtable, segments, None)
    }

    /// Shared drain walk for expire (with an age cutoff) and clear.
    ///
    /// Each drained segment's walk waits for `active_writers == 0` (the
    /// claimer half of the writer-vs-drain Dekker pair, item 7d) after
    /// winning its state CAS and before parsing the item stream, so a
    /// concurrent reserver's in-flight define+publish is never torn or
    /// recycled out from under it.
    fn drain_chain(
        &self,
        hashtable: &MultiChoiceHashtable,
        segments: &Segments,
        expire_cutoff: Option<Instant>,
    ) -> usize {
        // LOCK: bucket-chain — the drain walk mutates this bucket's chain
        // structure (set_head/set_tail + each recycle/condemn's unlink/splice).
        // Held across the whole walk (coarse: also spans the per-segment
        // hashtable clear) to serialize against concurrent evictors, reservers
        // (try_expand), and other drains of this bucket without re-entrant
        // re-locking in recycle/condemn. Lock order: chain_lock is outer to the
        // eviction policy lock, which the primitives below never take.
        let _chain = self.chain_lock();

        let mut freed = 0;
        let mut cursor = self.head();

        while let Some(seg_id) = cursor {
            let mut segment = segments.segment(seg_id).unwrap();

            if let Some(now) = expire_cutoff {
                // the chain is oldest-first: stop at the first live segment
                if segment.create_at() + segment.ttl() > now {
                    break;
                }
            }

            let meta = segment.header_metadata();
            let next = meta.next;
            let prev = meta.prev;

            // Take exclusivity: interior segments are Sealed, the tail is
            // Live (SeqCst: Dekker pair with try_acquire_reader).
            let drained =
                segment.cas_metadata(State::Sealed, State::Draining, None, None, Ordering::SeqCst)
                    || segment.cas_metadata(
                        State::Live,
                        State::Draining,
                        None,
                        None,
                        Ordering::SeqCst,
                    );
            debug_assert!(drained, "chain segment was neither Sealed nor Live");
            if !drained {
                cursor = next;
                continue;
            }

            // Wait for in-flight reservers to finish define+publish before we
            // parse this segment's item stream (item 7d, H1/H2). Claimer half of
            // the Dekker pair: our SeqCst state CAS above precedes this SeqCst
            // load, so every writer that passed its recheck-Live is counted, and
            // any later writer sees Draining and bails. The snooze yields after
            // a short spin so a descheduled pin holder gets CPU on an
            // oversubscribed host.
            let backoff = Backoff::new();
            while segments.header(seg_id).active_writers() != 0 {
                backoff.snooze();
            }
            // Item 7f: wait for in-flight replace/delete removes of this
            // segment's items before parsing (see claim_for_drain).
            while segments.header(seg_id).active_removers() != 0 {
                backoff.snooze();
            }

            segment.clear(hashtable, true);

            // The segment leaves the chain either way.
            if self.head() == Some(seg_id) {
                self.set_head(next);
            }
            if self.tail() == Some(seg_id) {
                self.set_tail(prev);
            }

            if segment.header_ref_count_seqcst() == 0 {
                // recycle unlinks the segment, splicing its neighbors
                segments.recycle(seg_id);

                #[cfg(feature = "metrics")]
                if expire_cutoff.is_some() {
                    SEGMENT_EXPIRE.increment();
                } else {
                    SEGMENT_CLEAR.increment();
                }

                freed += 1;
            } else {
                // Condemn: unlinked immediately, freed by whichever of the
                // three claimants wins the AwaitingRelease -> Free CAS (the
                // last reader's guard drop, the race-fix recheck below, or
                // the backout of an acquire that failed after its
                // increment).
                match segments.condemn(seg_id, next, prev) {
                    ClearOutcome::Freed => freed += 1,
                    ClearOutcome::Deferred => {
                        #[cfg(feature = "metrics")]
                        SEGMENT_PINNED_SKIP.increment();
                    }
                }
            }

            cursor = next;
        }

        freed
    }

    /// Extend the segment chain past `observed_tail`, following
    /// crucible's append protocol as a lock-free election: the one-CAS
    /// seal (Live→Sealed + next pointer set together) admits exactly
    /// one winner per tail, so concurrent expanders coordinate without
    /// a chain mutex.
    ///
    /// `observed_tail` is the tail the caller found full (or None for
    /// an empty bucket). The seal targets exactly that segment — never
    /// whatever the tail happens to be at CAS time — so an election
    /// loser can never seal the winner's freshly linked, near-empty
    /// segment.
    ///
    /// The *election* is lock-free; losers briefly wait for the
    /// winner's tail publish, which is bounded straight-line work. The
    /// spins have no yield fallback yet — acceptable while writers are
    /// internal-test-only; revisit at item 7.
    ///
    /// `observed` pairs the observed tail with its generation at the time
    /// the caller read it (`None` for an empty bucket). The tail and its
    /// generation are carried as one value, not two independent `Option`s,
    /// so a caller cannot supply a tail without the generation that guards
    /// it. It closes the tail-recycle ABA (item 7d, H3): the generation is
    /// re-checked under `chain_lock` before the seal fires, so a tail that
    /// was drained, recycled, and reused since the caller observed it is
    /// never sealed.
    fn try_expand(
        &self,
        observed: Option<(NonZeroU32, u16)>,
        segments: &Segments,
    ) -> Result<(), TtlBucketsError> {
        // The election and the loser-spin only care about the tail id; the
        // generation is used solely for the H3 re-check below.
        let observed_tail = observed.map(|(tail_id, _)| tail_id);

        let id = segments
            .reserve_free()
            .ok_or(TtlBucketsError::NoFreeSegments)?;

        segments
            .header(id)
            .set_ttl(Duration::from_secs(self.ttl as u32));

        // LOCK: bucket-chain — the tail-extension surgery (seal the old tail +
        // link the new segment + set_tail/set_head) mutates this bucket's chain
        // structure and must serialize against concurrent eviction/drain
        // surgery on the same bucket. Held across the election so a merge's
        // head-insert or a drain's unlink cannot interleave with the seal. The
        // reserve hot path (`try_alloc_item`) never reaches here. Under this
        // lock at most one expander runs at a time, so the loser's spin-wait
        // for the winner's tail publish is always immediately satisfied (the
        // winner publishes before releasing) — no self-deadlock.
        let _chain = self.chain_lock();

        // H3 (item 7d): the tail we observed may have been advanced, drained, or
        // drained→recycled→reused since we observed it (before taking this lock).
        // Under `chain_lock` this bucket's tail is frozen, so re-validate that
        // `tail_id` is STILL this bucket's tail AND carries the generation we
        // observed; bail otherwise, returning our reserved segment so the caller
        // re-reads the (now-advanced) tail and retries. Two checks, two hazards:
        //   * `self.tail() != observed_tail` — the tail left this bucket: another
        //     expander advanced past it, a drain removed it, or it was recycled
        //     and reused as ANOTHER bucket's tail. A segment lives in exactly one
        //     chain, so if it were now some other bucket's Live tail it could not
        //     also be ours — this closes the cross-bucket seal ABA (the non-atomic
        //     observe-tail-then-observe-gen window in `reserve`).
        //   * generation mismatch — the same-bucket ABA: `tail_id` was recycled
        //     and reused as THIS bucket's tail again (so `self.tail()` matches),
        //     but it is a different incarnation.
        // Bail DIRECTLY — do not fall through to the election-lost spin-wait
        // below: under our held lock the tail cannot advance, so that spin would
        // never terminate if `tail_id` is our tail again.
        if let Some((tail_id, gen)) = observed {
            if self.tail() != observed_tail || segments.header(tail_id).generation() != gen {
                segments.release_unused(id);
                return Ok(());
            }
        }

        let won = match observed_tail {
            Some(tail_id) => {
                let tail = segments.header(tail_id);
                let backoff = Backoff::new();
                loop {
                    // THE SEAL: the old tail stops accepting writes and
                    // becomes evictable at the exact moment its
                    // successor exists — one CAS carries both the state
                    // transition and the next pointer. This is also the
                    // election: exactly one expander can seal a given
                    // tail.
                    if tail.cas_metadata(
                        State::Live,
                        State::Sealed,
                        Some(Some(id)),
                        None,
                        Ordering::AcqRel,
                    ) {
                        let linked = segments.header(id).cas_metadata(
                            State::Reserved,
                            State::Linking,
                            Some(None),
                            Some(Some(tail_id)),
                            Ordering::AcqRel,
                        );
                        debug_assert!(linked, "freshly reserved segment must be Reserved");
                        self.set_tail(Some(id));
                        break true;
                    }
                    // The CAS can fail without the election being
                    // decided: a draining neighbor patching `prev`
                    // changes the packed metadata word while the state
                    // stays Live. Only a state change decides the
                    // election.
                    if tail.state() == State::Live {
                        backoff.snooze();
                        continue;
                    }
                    break false;
                }
            }
            None => {
                if self.cas_tail_none_to(id) {
                    debug_assert!(self.head().is_none());
                    let linked = segments.header(id).cas_metadata(
                        State::Reserved,
                        State::Linking,
                        Some(None),
                        Some(None),
                        Ordering::AcqRel,
                    );
                    debug_assert!(linked, "freshly reserved segment must be Reserved");
                    self.set_head(Some(id));
                    true
                } else {
                    false
                }
            }
        };

        if won {
            // Publish the new tail as the writable segment.
            let live = segments.header(id).cas_metadata(
                State::Linking,
                State::Live,
                None,
                None,
                Ordering::AcqRel,
            );
            debug_assert!(live, "linking segment must publish as Live");
            self.nseg.fetch_add(1, Ordering::Relaxed);
        } else {
            // Election lost: another writer expanded past the tail we
            // observed (or eviction drained it). Wait for the tail word
            // to advance — the winner's store is imminent — so the
            // caller's retry sees the fresh segment, then put our
            // reserved segment back.
            let backoff = Backoff::new();
            while self.tail() == observed_tail {
                backoff.snooze();
            }
            segments.release_unused(id);
        }
        Ok(())
    }

    /// Reserve space for an item in this bucket's tail segment.
    ///
    /// Expands the bucket with a new segment if the current tail is
    /// full. Concurrent-safe among writers: space grants are a bounded
    /// CAS on the tail's write offset, and expansion is a lock-free
    /// seal election (see `try_expand`). Returns a `ReservedItem`
    /// pointing to the allocated space, or an error if the item is
    /// oversized or no segments are free.
    pub(crate) fn reserve(
        &self,
        size: usize,
        segments: &Segments,
    ) -> Result<ReservedItem, TtlBucketsError> {
        let seg_size = segments.segment_size() as usize;

        if size > seg_size {
            return Err(TtlBucketsError::ItemOversized { size });
        }

        let backoff = Backoff::new();
        loop {
            let tail = self.tail();
            match tail {
                Some(id) => {
                    // Capture the tail generation for the seal ABA guard (H3):
                    // if it is recycled/reused before we seal, try_expand bails.
                    let observed_gen = segments.header(id).generation();
                    match segments.try_alloc_item(id, size as i32) {
                        AllocOutcome::Reserved(reserved) => return Ok(reserved),
                        AllocOutcome::NotWritable => {
                            // Mid-election (Reserved/Linking) or being drained: the
                            // chain is about to advance. Re-read the tail rather
                            // than expanding behind a transient state. Unreachable
                            // single-threaded (seal+publish happen inside try_expand).
                            backoff.snooze();
                            continue;
                        }
                        AllocOutcome::Full => {
                            // Live but full: expand, sealing exactly this tail
                            // (paired with the generation we observed above).
                            self.try_expand(Some((id, observed_gen)), segments)?;
                        }
                    }
                }
                None => {
                    self.try_expand(None, segments)?;
                }
            }
        }
    }

    /// Test-only shim exposing `try_expand`'s `observed` (tail + generation)
    /// argument directly, so the H3 generation-ABA guard can be exercised
    /// without forcing the real drain/recycle race. `#[cfg(test)]` (not gated
    /// to `not(feature = "loom")`) because the caller test module below IS
    /// gated to `not(feature = "loom")` — under `--all-features` (loom on)
    /// that caller disappears, and an unconditional `#[cfg(test)]` shim
    /// would then be dead code that trips `clippy --all-features -D
    /// warnings`. `#[allow(dead_code)]` covers that combination (same
    /// reasoning as `header.rs`'s `store_metadata_for_test`, mirrored
    /// direction).
    #[cfg(test)]
    #[allow(dead_code)] // caller test module is cfg'd out under loom
    fn try_expand_for_test(
        &self,
        observed: Option<(NonZeroU32, u16)>,
        segments: &Segments,
    ) -> Result<(), TtlBucketsError> {
        self.try_expand(observed, segments)
    }
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use super::*;
    use crate::segments::{AllocOutcome, SegmentsBuilder};

    /// H3 (item 7d): try_expand must refuse to seal a tail whose generation
    /// no longer matches what the caller observed before taking the chain
    /// lock — that mismatch is the signature of a drain->recycle->reuse
    /// that happened between the caller's observation and the seal. A
    /// matching generation must still seal normally.
    #[test]
    fn try_expand_bails_on_stale_generation() {
        let segments = SegmentsBuilder::default()
            .segment_size(4096)
            .heap_size(4096 * 4)
            .build()
            .expect("build segments");
        let bucket = TtlBucket::new(60);

        // Establish a Live, full tail (reserve once, then fill it).
        let seg_size = segments.segment_size();
        let first = bucket.reserve(64, &segments).unwrap();
        let tail = first.seg();
        drop(first);
        while let AllocOutcome::Reserved(reserved) = segments.try_alloc_item(tail, seg_size / 4) {
            drop(reserved);
        }
        let good_gen = segments.header(tail).generation();

        // Stale generation must be refused: try_expand returns Ok WITHOUT
        // sealing the tail.
        assert!(bucket
            .try_expand_for_test(Some((tail, good_gen.wrapping_add(1))), &segments)
            .is_ok());
        assert_eq!(
            segments.header(tail).state(),
            State::Live,
            "stale-gen expand must not seal the tail"
        );

        // The correct generation seals it.
        assert!(bucket
            .try_expand_for_test(Some((tail, good_gen)), &segments)
            .is_ok());
        assert_eq!(segments.header(tail).state(), State::Sealed);
    }
}

#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use super::*;
    use loom::sync::Arc;
    use loom::thread;

    // See the loom discipline NOTE in segments/header.rs loom_tests.

    // Two writers race to install the first segment of an empty
    // bucket. The tail-word CAS admits exactly one winner — the mutual
    // exclusion the empty-bucket arm of try_expand relies on. Pure CAS
    // uniqueness: SC-independent, within loom's power.
    #[test]
    fn loom_empty_bucket_election_single_winner() {
        loom::model(|| {
            let bucket = Arc::new(TtlBucket::new(60));

            let handles: Vec<_> = [1u32, 2u32]
                .into_iter()
                .map(|id| {
                    let b = Arc::clone(&bucket);
                    thread::spawn(move || {
                        let won = b.cas_tail_none_to(NonZeroU32::new(id).unwrap());
                        if !won {
                            // Loser coherence: the winner's tail install
                            // is visible immediately after the failed
                            // CAS — the fact the production loser's
                            // spin-wait termination depends on. Pure
                            // coherence, not the SC total order.
                            assert!(b.tail().is_some());
                        }
                        won
                    })
                })
                .collect();
            let wins: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();

            assert_eq!(
                wins.iter().filter(|w| **w).count(),
                1,
                "exactly one install"
            );
            let tail = bucket.tail().unwrap().get();
            let expected = if wins[0] { 1 } else { 2 };
            assert_eq!(tail, expected);
        });
    }
}
