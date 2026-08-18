//! `ZsetView`/`ZsetMut`: Redis-sorted-set semantics over a `Zset`-typed
//! block, with `u64` integer scores only (no float scores).
//!
//! A zset's body is `(member, score)` adjacent entry pairs — member entry
//! first, then score entry — sorted by `(score asc, then member via
//! [`compare`])`. This is the opposite pairing convention from `hash.rs`:
//! a hash sorts by its pair's *first* entry (the field), so `map.rs`'s
//! [`pair_seek`](crate::map::pair_seek) handles it directly; a zset's sort
//! key is the *second* entry (the score) with the first entry (the member)
//! only breaking ties, so it needs its own seek,
//! [`zset_seek`] — see that function's docs (and `map.rs`'s module docs)
//! for why.
//!
//! # Member lookups can't use either seek
//!
//! [`ZsetView::zscore`] and every `ZsetMut` op that starts from a member
//! name (`zadd`, `zrem`, `zincrby`) need to find a member regardless of its
//! score, but the body is score-sorted, not member-sorted — a member
//! lookup can't binary-search or early-exit the way a hash's `hget` does.
//! [`find_member`] is a plain linear scan across every pair.
//!
//! # `zadd` rescore: remove, then reinsert at the new sort position
//!
//! Changing an existing member's score can move its pair anywhere in the
//! sort order, so there's no in-place `replace_at` the way `hincrby` (whose
//! value entry never affects the hash's field-sort order) can do. Instead
//! [`ZsetMut::rescore`] removes the old pair and reinserts a new one at the
//! score-sorted position — all-or-nothing: the combined fit is checked
//! *before* the removal runs, per `used - old_pair_len + new_pair_len <=
//! capacity`, so a `NeedBytes` failure leaves the member at its old score,
//! byte-for-byte unmodified (pinned by
//! `zadd_rescore_needbytes_leaves_buffer_untouched` below).
//!
//! # `zscore`'s `Result` can't currently produce `Err`
//!
//! [`find_member`] treats any decode inconsistency as "member not found"
//! rather than surfacing it (see its docs — this can't happen on a block
//! built exclusively through this crate's ops), so [`ZsetView::zscore`]
//! always returns `Ok(..)`. Its signature is `Result<Option<u64>,
//! DecodeError>` anyway, matching [`HashView::hget`](crate::hash::HashView::hget)'s
//! shape for API consistency and so callers can use `?` uniformly.

use crate::block::{Block, BlockMut, InsertPos};
use crate::cursor::{locate, Cursor};
use crate::entry::{canonical_uint, compare, encoded_len, EntryVal};
use crate::error::{DecodeError, Fit, NeedBytes};
use crate::hash::{apply_delta, IncrError};
use crate::header::{BlockHeader, Type};
use crate::map::{zset_seek, SeekResult};
use core::cmp::Ordering;

/// Classifies raw member bytes the same way
/// [`compare_raw`](crate::entry::compare_raw) does: a canonical decimal
/// renders as `EntryVal::Uint`, anything else as `EntryVal::Str`. Mirrors
/// `hash.rs`'s and `set.rs`'s identically-named helper.
fn classify(bytes: &[u8]) -> EntryVal<'_> {
    canonical_uint(bytes).map_or(EntryVal::Str(bytes), EntryVal::Uint)
}

/// A lower or upper score bound for [`ZsetView::zcount`] and
/// [`ZsetView::zrange_by_score`] (Redis `ZCOUNT`/`ZRANGEBYSCORE` semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    /// Score must be `>=` this value (as a lower bound) or `<=` (as an
    /// upper bound).
    Inclusive(u64),
    /// Score must be `>` this value (as a lower bound) or `<` (as an upper
    /// bound).
    Exclusive(u64),
    /// No lower limit: every score satisfies this as a lower bound (and,
    /// degenerately, none satisfies it as an upper bound).
    NegInf,
    /// No upper limit: every score satisfies this as an upper bound (and,
    /// degenerately, none satisfies it as a lower bound).
    PosInf,
}

impl Bound {
    /// True iff `score` satisfies this bound used as a *lower* bound.
    fn satisfied_by(self, score: u64) -> bool {
        match self {
            Bound::NegInf => true,
            Bound::PosInf => false,
            Bound::Inclusive(v) => score >= v,
            Bound::Exclusive(v) => score > v,
        }
    }

    /// True iff `score` exceeds this bound used as an *upper* bound. Since
    /// zset bodies are score-ascending, a walk can stop as soon as this is
    /// true (see [`ZsetView::zrange_by_score`]).
    fn exceeded_by(self, score: u64) -> bool {
        match self {
            Bound::PosInf => false,
            Bound::NegInf => true,
            Bound::Inclusive(v) => score > v,
            Bound::Exclusive(v) => score >= v,
        }
    }
}

/// Result of [`ZsetMut::zadd`] (Redis `ZADD` semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZAdd {
    /// The member did not exist; a new `(member, score)` pair was
    /// inserted.
    New,
    /// The member already existed with a different score; its pair was
    /// removed and reinserted at the new score-sorted position.
    ScoreChanged,
    /// The member already existed with this exact score; nothing changed.
    Unchanged,
}

/// Linear-scans the body for `member`'s pair. Unlike a hash's fields, a
/// zset's body is sorted by *score*, not member, so a member lookup can't
/// binary-search or early-exit the way [`pair_seek`](crate::map::pair_seek)'s
/// hash/set callers do; every pair must be checked in the worst case.
/// Returns the member/score cursors and the current score, or `None` if
/// `member` isn't present.
///
/// Never panics: like [`pair_seek`](crate::map::pair_seek), a decode
/// failure, a score entry that isn't `EntryVal::Uint` (every zset score is
/// written as `Uint`; this is defensive, not expected), or a truncated
/// trailing unit is treated as "member not found" rather than panicking —
/// impossible on a block built exclusively through this module's
/// stride-preserving ops, but not re-validated here.
fn find_member(buf: &[u8], hdr: &BlockHeader, key: &EntryVal) -> Option<(Cursor, Cursor, u64)> {
    let mut cur = Cursor::first(buf, hdr)?;
    loop {
        let member_cur = cur;
        let m = cur.value(buf).ok()?;
        let score_cur = cur.next(buf)?;
        let score = match score_cur.value(buf).ok()? {
            EntryVal::Uint(v) => v,
            EntryVal::Str(_) => return None,
        };
        if compare(&m, key) == Ordering::Equal {
            return Some((member_cur, score_cur, score));
        }
        cur = score_cur.next(buf)?;
    }
}

/// A read-only view over a `Zset`-typed block, providing Redis sorted-set
/// semantics (`ZSCORE`/`ZCARD`/`ZCOUNT`/`ZRANGE`/`ZRANGEBYSCORE`) on top of
/// the block's score-sorted `(member, score)` pairs.
#[derive(Debug, Clone, Copy)]
pub struct ZsetView<'a> {
    buf: &'a [u8],
    hdr: BlockHeader,
}

impl<'a> ZsetView<'a> {
    /// Parses `buf` as a zset block: full structural validation
    /// ([`Block::parse`]) plus a `Type::Zset` check and an even-`nentry`
    /// check, mirroring [`HashView::parse`](crate::hash::HashView::parse)
    /// (a zset's `(member, score)` pairing has the same "no orphan half of
    /// a pair" requirement a hash's `(field, value)` pairing does).
    pub fn parse(buf: &'a [u8]) -> Result<Self, DecodeError> {
        let blk = Block::parse(buf)?;
        Self::from_block(blk)
    }

    fn from_block(blk: Block<'a>) -> Result<Self, DecodeError> {
        let hdr = *blk.header();
        if hdr.type_ != Type::Zset {
            return Err(DecodeError::Corrupt);
        }
        if !hdr.nentry.is_multiple_of(2) {
            return Err(DecodeError::Corrupt);
        }
        Ok(ZsetView {
            buf: blk.bytes(),
            hdr,
        })
    }

    /// Number of members in the zset (Redis `ZCARD`): `nentry / 2`, since
    /// each member is paired with a score entry.
    pub fn zcard(&self) -> u32 {
        self.hdr.nentry / 2
    }

    /// True iff the zset has no members.
    pub fn is_empty(&self) -> bool {
        self.hdr.nentry == 0
    }

    /// Returns `member`'s score (Redis `ZSCORE`), or `None` if `member`
    /// doesn't exist. See the [module docs](self) for why this always
    /// returns `Ok(..)`.
    pub fn zscore(&self, member: &[u8]) -> Result<Option<u64>, DecodeError> {
        let key = classify(member);
        Ok(find_member(self.buf, &self.hdr, &key).map(|(_, _, score)| score))
    }

    /// Counts members whose score falls within `[min, max]` (Redis
    /// `ZCOUNT`), honoring each [`Bound`] variant's inclusive/exclusive
    /// sense. Implemented atop [`zrange_by_score`](Self::zrange_by_score)
    /// with a counting callback.
    pub fn zcount(&self, min: Bound, max: Bound) -> u32 {
        let mut n = 0u32;
        self.zrange_by_score(min, max, |_, _| n += 1);
        n
    }

    /// Calls `f` with each `(member, score)` pair whose score falls within
    /// `[min, max]` (Redis `ZRANGEBYSCORE`), in score-ascending order.
    /// Since the body is already score-sorted, the walk stops as soon as a
    /// score exceeds `max` — everything after is guaranteed to exceed it
    /// too.
    pub fn zrange_by_score(&self, min: Bound, max: Bound, mut f: impl FnMut(&EntryVal<'a>, u64)) {
        let mut cur = match Cursor::first(self.buf, &self.hdr) {
            Some(c) => c,
            None => return,
        };
        loop {
            let member = match cur.value(self.buf) {
                Ok(v) => v,
                Err(_) => return,
            };
            let score_cur = match cur.next(self.buf) {
                Some(c) => c,
                None => return,
            };
            let score = match score_cur.value(self.buf) {
                Ok(EntryVal::Uint(v)) => v,
                _ => return,
            };
            if max.exceeded_by(score) {
                return;
            }
            if min.satisfied_by(score) {
                f(&member, score);
            }
            cur = match score_cur.next(self.buf) {
                Some(c) => c,
                None => return,
            };
        }
    }

    /// Calls `f` with each `(member, score)` pair in the inclusive rank
    /// window `[start, stop]` (Redis `ZRANGE`/`ZREVRANGE` semantics):
    /// negative indices normalize from the tail, out-of-range bounds clamp
    /// into `[0, zcard - 1]`, and an empty or fully out-of-range window
    /// (including `start > stop` after normalizing) yields no calls at
    /// all.
    ///
    /// `rev` does more than reverse the walk direction: per Redis
    /// `ZREVRANGE`, `start`/`stop` address the *reversed* (descending-score)
    /// list, not the ascending one — reversed index `i` is ascending index
    /// `npairs - 1 - i`. So after normalizing/clamping `start`/`stop` in
    /// reversed-index space, the window is re-expressed in ascending-index
    /// space as `[npairs - 1 - stop, npairs - 1 - start]` (the endpoints
    /// swap because the mapping is order-reversing) before walking
    /// high-to-low. Only the symmetric full window (`0, -1`) leaves the
    /// window unchanged by this transform, which is why testing solely that
    /// case would miss the transform being skipped entirely.
    ///
    /// Mirrors [`ListView::range`](crate::list::ListView::range)'s
    /// normalize/clamp rules, adapted to walk two-entry pairs instead of
    /// single entries.
    pub fn zrange_by_rank(
        &self,
        start: i64,
        stop: i64,
        rev: bool,
        mut f: impl FnMut(&EntryVal<'a>, u64),
    ) {
        let npairs = (self.hdr.nentry / 2) as i64;
        if npairs == 0 {
            return;
        }
        let normalize = |i: i64| if i < 0 { i + npairs } else { i };
        let start = normalize(start).max(0);
        let stop = normalize(stop).min(npairs - 1);
        if start > stop {
            return;
        }
        // For `rev`, re-express the [start, stop] window (given in
        // reversed/descending-index space) in ascending-index space: the
        // mapping `i -> npairs - 1 - i` is order-reversing, so the window's
        // low/high endpoints swap. For the non-`rev` case, the window is
        // already ascending-index space.
        let (lo, hi) = if rev {
            ((npairs - 1 - stop) as u32, (npairs - 1 - start) as u32)
        } else {
            (start as u32, stop as u32)
        };
        let first = if rev { hi } else { lo };

        // 0 <= first <= hi < npairs, so locate cannot fail here; treat an
        // error as "nothing to walk" rather than panicking (mirrors
        // ListView::range's handling of an impossible-by-invariant decode
        // error).
        let mut member_cur = match locate(self.buf, &self.hdr, first * 2) {
            Ok(c) => c,
            Err(_) => return,
        };
        let mut score_cur = match member_cur.next(self.buf) {
            Some(c) => c,
            None => return,
        };
        let mut idx = first;
        loop {
            if let (Ok(m), Ok(EntryVal::Uint(s))) =
                (member_cur.value(self.buf), score_cur.value(self.buf))
            {
                f(&m, s);
            }
            if idx == if rev { lo } else { hi } {
                break;
            }
            if rev {
                idx -= 1;
                let prev_score = match member_cur.prev(self.buf, &self.hdr) {
                    Some(c) => c,
                    None => break,
                };
                let prev_member = match prev_score.prev(self.buf, &self.hdr) {
                    Some(c) => c,
                    None => break,
                };
                member_cur = prev_member;
                score_cur = prev_score;
            } else {
                idx += 1;
                let next_member = match score_cur.next(self.buf) {
                    Some(c) => c,
                    None => break,
                };
                let next_score = match next_member.next(self.buf) {
                    Some(c) => c,
                    None => break,
                };
                member_cur = next_member;
                score_cur = next_score;
            }
        }
    }
}

/// A mutable view over a `Zset`-typed block, providing `ZADD`/`ZREM`/
/// `ZINCRBY` ops with Redis sorted-set semantics.
#[derive(Debug)]
pub struct ZsetMut<'a> {
    blk: BlockMut<'a>,
}

impl<'a> ZsetMut<'a> {
    /// Initializes `buf` as an empty `Zset` block and wraps it. The type is
    /// `Zset` and `nentry` starts at `0` (even) by construction, so no
    /// runtime check is needed here or in any other `ZsetMut`/`ZsetView`
    /// op derived from it.
    pub fn init(buf: &'a mut [u8]) -> Result<Self, NeedBytes> {
        BlockHeader::init_empty(Type::Zset, buf)?;
        let blk =
            BlockMut::parse(buf).expect("a block just initialized as empty must parse cleanly");
        Ok(ZsetMut { blk })
    }

    /// A read-only view over the zset's current contents.
    pub fn view(&self) -> ZsetView<'_> {
        ZsetView {
            buf: self.blk.bytes(),
            hdr: *self.blk.header(),
        }
    }

    /// The zset's used bytes: header plus entries, through the end of the
    /// last entry. Same contract as
    /// [`BlockMut::bytes`](crate::block::BlockMut::bytes) (never the full
    /// backing capacity) -- exposed here so external callers (fuzzing,
    /// differential testing) can independently re-validate the block via
    /// [`Block::parse`](crate::block::Block::parse) without needing access
    /// to the private `BlockMut` this type wraps.
    pub fn bytes(&self) -> &[u8] {
        self.blk.bytes()
    }

    /// Adds `member` with `score`, or updates its score if it already
    /// exists (Redis `ZADD`). A new member is inserted at its score-sorted
    /// position ([`ZAdd::New`]); an existing member with a different score
    /// is removed and reinserted there ([`ZAdd::ScoreChanged`], see
    /// [`rescore`](Self::rescore)); an existing member with the same score
    /// is a no-op ([`ZAdd::Unchanged`]).
    pub fn zadd(&mut self, member: &[u8], score: u64) -> Result<ZAdd, NeedBytes> {
        let key = classify(member);
        match find_member(self.blk.bytes(), self.blk.header(), &key) {
            Some((member_cur, score_cur, old_score)) => {
                if old_score == score {
                    return Ok(ZAdd::Unchanged);
                }
                self.rescore(member_cur, score_cur, &key, score)?;
                Ok(ZAdd::ScoreChanged)
            }
            None => {
                let pos = self.seek_pos(score, &key);
                self.insert_pair(pos, &key, score)?;
                Ok(ZAdd::New)
            }
        }
    }

    /// Removes `member` and its score (Redis `ZREM`). Returns `None` if
    /// the member didn't exist (nothing removed).
    pub fn zrem(&mut self, member: &[u8]) -> Option<Fit> {
        let key = classify(member);
        let (member_cur, score_cur, _) = find_member(self.blk.bytes(), self.blk.header(), &key)?;
        // score_cur always sits at a higher offset than member_cur
        // (adjacent pair, score follows member): removing it first doesn't
        // shift member_cur's offset. Removing member_cur first would
        // shift-invalidate score_cur before its own removal (mirrors
        // HashMut::hdel's ordering).
        self.blk.remove_at(score_cur);
        self.blk.remove_at(member_cur);
        Some(Fit)
    }

    /// Increments `member`'s score by `delta` (Redis `ZINCRBY`), moving its
    /// pair to the new score-sorted position if the score actually
    /// changes. A missing member auto-vivifies starting at `0` (same
    /// precedent as [`HashMut::hincrby`](crate::hash::HashMut::hincrby)).
    /// The outer `Result` is capacity (`NeedBytes`); the inner is the
    /// numeric outcome — same nested-`Result` shape as `hincrby`, minus
    /// `IncrError::NotAnInteger` (a zset score is always stored as
    /// `EntryVal::Uint`, so there's nothing non-numeric to read).
    pub fn zincrby(
        &mut self,
        member: &[u8],
        delta: i64,
    ) -> Result<Result<u64, IncrError>, NeedBytes> {
        let key = classify(member);
        match find_member(self.blk.bytes(), self.blk.header(), &key) {
            Some((member_cur, score_cur, current)) => {
                let new_val = match apply_delta(current, delta) {
                    Ok(v) => v,
                    Err(e) => return Ok(Err(e)),
                };
                if new_val != current {
                    self.rescore(member_cur, score_cur, &key, new_val)?;
                }
                Ok(Ok(new_val))
            }
            None => {
                let new_val = match apply_delta(0, delta) {
                    Ok(v) => v,
                    Err(e) => return Ok(Err(e)),
                };
                let pos = self.seek_pos(new_val, &key);
                self.insert_pair(pos, &key, new_val)?;
                Ok(Ok(new_val))
            }
        }
    }

    /// Finds where `(score, key)` belongs among the current pairs, via
    /// [`zset_seek`].
    fn seek_pos(&self, score: u64, key: &EntryVal) -> InsertPos {
        match zset_seek(self.blk.bytes(), self.blk.header(), score, key) {
            SeekResult::InsertBefore(cur) => InsertPos::Before(cur),
            SeekResult::Tail => InsertPos::Tail,
            // A `Found` match means some pair already sits at exactly
            // `(score, key)` — which, since `key` is the member, would
            // have to be `key` itself. Every caller has already ensured
            // `key` is absent from the block at this point (confirmed by
            // `find_member` before a plain insert; just removed by
            // `rescore` before reinserting), so this arm shouldn't be
            // reachable. Kept for exhaustiveness, not because it's
            // expected: insert immediately before the match to keep the
            // body sorted.
            SeekResult::Found { key_cur, .. } => InsertPos::Before(key_cur),
        }
    }

    /// Inserts `(key, score)` as an adjacent pair at `pos` (member first,
    /// then score), all-or-nothing: the combined size is checked against
    /// capacity *before* either splice runs, so a `NeedBytes` failure
    /// leaves the buffer byte-for-byte unmodified. Mirrors
    /// [`HashMut::insert_pair`](crate::hash::HashMut::insert_pair).
    fn insert_pair(
        &mut self,
        pos: InsertPos,
        key: &EntryVal,
        score: u64,
    ) -> Result<Fit, NeedBytes> {
        let val = EntryVal::Uint(score);
        let klen = encoded_len(key);
        let vlen = encoded_len(&val);
        let capacity = self.blk.bytes_full().len();
        let need = klen
            .checked_add(vlen)
            .and_then(|n| self.blk.used_len().checked_add(n));
        match need {
            Some(total) if total <= capacity => {}
            Some(total) => return Err(NeedBytes(total)),
            None => return Err(NeedBytes(usize::MAX)),
        }

        match pos {
            InsertPos::Before(cur) => {
                self.blk
                    .insert_at(InsertPos::Before(cur), key)
                    .expect("capacity already confirmed to fit both entries");
                let shifted = Cursor {
                    off: cur.off + klen,
                    len: cur.len,
                };
                self.blk
                    .insert_at(InsertPos::Before(shifted), &val)
                    .expect("capacity already confirmed to fit both entries");
            }
            InsertPos::Tail => {
                self.blk
                    .insert_at(InsertPos::Tail, key)
                    .expect("capacity already confirmed to fit both entries");
                self.blk
                    .insert_at(InsertPos::Tail, &val)
                    .expect("capacity already confirmed to fit both entries");
            }
        }
        Ok(Fit)
    }

    /// Moves an existing member to `new_score`: removes its current pair
    /// and reinserts at the new score-sorted position. All-or-nothing per
    /// the [module docs](self): the *combined* fit (`used - old_pair_len +
    /// new_pair_len <= capacity`) is checked up front, before either the
    /// removal or the reinsertion touches the buffer, so a `NeedBytes`
    /// failure leaves the member at its old score, byte-for-byte
    /// unmodified.
    fn rescore(
        &mut self,
        member_cur: Cursor,
        score_cur: Cursor,
        key: &EntryVal,
        new_score: u64,
    ) -> Result<(), NeedBytes> {
        let old_pair_len = member_cur.len + score_cur.len;
        let new_pair_len = encoded_len(key) + encoded_len(&EntryVal::Uint(new_score));
        let capacity = self.blk.bytes_full().len();
        let need = self
            .blk
            .used_len()
            .checked_sub(old_pair_len)
            .and_then(|v| v.checked_add(new_pair_len));
        match need {
            Some(total) if total <= capacity => {}
            Some(total) => return Err(NeedBytes(total)),
            None => return Err(NeedBytes(usize::MAX)),
        }

        // score_cur always sits at a higher offset than member_cur: remove
        // it first so member_cur's offset isn't shifted before its own
        // removal (mirrors HashMut::hdel's ordering).
        self.blk.remove_at(score_cur);
        self.blk.remove_at(member_cur);
        let pos = self.seek_pos(new_score, key);
        self.insert_pair(pos, key, new_score)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use crate::entry::{render_uint, EntryVal};
    use crate::error::NeedBytes;
    use crate::hash::IncrError;
    use crate::zset::{Bound, ZAdd, ZsetMut, ZsetView};
    use std::vec;
    use std::vec::Vec;

    fn ev_bytes(v: &EntryVal) -> Vec<u8> {
        match v {
            EntryVal::Str(b) => b.to_vec(),
            EntryVal::Uint(n) => {
                let mut out = [0u8; 20];
                render_uint(*n, &mut out).to_vec()
            }
        }
    }

    fn members_in_order<'a>(view: &ZsetView<'a>) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        view.zrange_by_rank(0, -1, false, |m, _s| out.push(ev_bytes(m)));
        out
    }

    fn range_members<'a>(view: &ZsetView<'a>, min: Bound, max: Bound) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        view.zrange_by_score(min, max, |m, _s| out.push(ev_bytes(m)));
        out
    }

    #[test]
    fn zadd_orders_by_score_then_member_and_rescores() {
        let mut buf = [0u8; 512];
        let mut z = ZsetMut::init(&mut buf).unwrap();
        z.zadd(b"b", 20).unwrap();
        z.zadd(b"a", 20).unwrap();
        z.zadd(b"c", 10).unwrap();
        assert_eq!(
            members_in_order(&z.view()),
            vec![b"c".to_vec(), b"a".to_vec(), b"b".to_vec()]
        );
        assert!(matches!(z.zadd(b"c", 30).unwrap(), ZAdd::ScoreChanged)); // moves to tail
        assert_eq!(
            members_in_order(&z.view()),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
        );
        assert_eq!(z.view().zscore(b"c").unwrap(), Some(30));
    }

    #[test]
    fn zrange_by_score_bounds() {
        let mut buf = [0u8; 512];
        let mut z = ZsetMut::init(&mut buf).unwrap();
        for (m, sc) in [(&b"a"[..], 5u64), (b"b", 10), (b"c", 10), (b"d", 15)] {
            z.zadd(m, sc).unwrap();
        }
        assert_eq!(
            range_members(&z.view(), Bound::Inclusive(10), Bound::Inclusive(10)),
            vec![b"b".to_vec(), b"c".to_vec()]
        );
        assert_eq!(
            range_members(&z.view(), Bound::Exclusive(10), Bound::PosInf),
            vec![b"d".to_vec()]
        );
        assert_eq!(z.view().zcount(Bound::NegInf, Bound::Exclusive(10)), 1);
    }

    #[test]
    fn zadd_rescore_needbytes_leaves_buffer_untouched() {
        let mut buf = [0u8; 20]; // 12-byte header + 8 spare
        let mut z = ZsetMut::init(&mut buf).unwrap();
        // member "5" (canonical uint, 2 bytes) + score 1 (2 bytes) = 4
        // bytes; used = 16, 4 bytes spare.
        assert!(matches!(z.zadd(b"5", 1).unwrap(), ZAdd::New));
        let before: Vec<u8> = z.blk.bytes_full().to_vec(); // snapshot whole buffer

        // Rescoring to a value needing a much larger tier doesn't fit:
        // used(16) - old_pair_len(4) + new_pair_len(2 + 9) = 23 > 20.
        let big_score = 1u64 << 24;
        let err = z.zadd(b"5", big_score).unwrap_err();
        assert!(matches!(err, NeedBytes(_)));
        assert_eq!(
            z.blk.bytes_full(),
            before.as_slice(),
            "failed rescore must not mutate"
        );
        assert_eq!(z.view().zscore(b"5").unwrap(), Some(1), "score unchanged");
        assert_eq!(z.view().zcard(), 1);
    }

    #[test]
    fn zincrby_auto_vivifies_and_reports_underflow() {
        let mut buf = [0u8; 256];
        let mut z = ZsetMut::init(&mut buf).unwrap();
        assert_eq!(z.zincrby(b"m", 5).unwrap(), Ok(5)); // auto-vivify at 0+5
        assert_eq!(z.view().zscore(b"m").unwrap(), Some(5));
        assert_eq!(z.zincrby(b"m", 10).unwrap(), Ok(15));
        assert_eq!(
            z.zincrby(b"m", -100).unwrap(),
            Err(IncrError::Underflow) // u64 floor
        );
        assert_eq!(
            z.view().zscore(b"m").unwrap(),
            Some(15),
            "unchanged after error"
        );
    }

    #[test]
    fn zrem_removes_both_entries_and_updates_zcard() {
        let mut buf = [0u8; 256];
        let mut z = ZsetMut::init(&mut buf).unwrap();
        z.zadd(b"a", 1).unwrap();
        z.zadd(b"b", 2).unwrap();
        assert_eq!(z.view().zcard(), 2);
        assert!(z.zrem(b"a").is_some());
        assert!(z.zrem(b"a").is_none());
        assert_eq!(z.view().zcard(), 1);
        assert_eq!(z.view().zscore(b"a").unwrap(), None);
    }

    #[test]
    fn zrange_by_rank_rev_and_negative_indices() {
        let mut buf = [0u8; 512];
        let mut z = ZsetMut::init(&mut buf).unwrap();
        for (m, sc) in [(&b"a"[..], 1u64), (b"b", 2), (b"c", 3), (b"d", 4)] {
            z.zadd(m, sc).unwrap();
        }
        let mut out = Vec::new();
        z.view()
            .zrange_by_rank(-2, -1, false, |m, _s| out.push(ev_bytes(m)));
        assert_eq!(out, vec![b"c".to_vec(), b"d".to_vec()]);

        let mut rev = Vec::new();
        z.view()
            .zrange_by_rank(0, -1, true, |m, _s| rev.push(ev_bytes(m)));
        assert_eq!(
            rev,
            vec![b"d".to_vec(), b"c".to_vec(), b"b".to_vec(), b"a".to_vec()]
        );

        // start > stop after normalizing yields nothing.
        let mut empty = Vec::new();
        z.view()
            .zrange_by_rank(2, 0, false, |m, _s| empty.push(ev_bytes(m)));
        assert!(empty.is_empty());
    }

    #[test]
    fn zrange_by_rank_rev_partial_window_matches_redis_zrevrange_docs_example() {
        // The Redis ZREVRANGE docs example: ZADD key 1 "one" 2 "two" 3
        // "three"; ZREVRANGE key 0 -1 => ["three","two","one"];
        // ZREVRANGE key 2 3 => ["one"]; ZREVRANGE key -2 -1 =>
        // ["two","one"]. `start`/`stop` address the *reversed* list, not
        // the ascending one, so a partial/asymmetric window must map
        // through that transform -- only the full (0, -1) window happens
        // to look the same either way.
        let mut buf = [0u8; 512];
        let mut z = ZsetMut::init(&mut buf).unwrap();
        z.zadd(b"one", 1).unwrap();
        z.zadd(b"two", 2).unwrap();
        z.zadd(b"three", 3).unwrap();

        let mut full = Vec::new();
        z.view()
            .zrange_by_rank(0, -1, true, |m, _s| full.push(ev_bytes(m)));
        assert_eq!(
            full,
            vec![b"three".to_vec(), b"two".to_vec(), b"one".to_vec()]
        );

        let mut partial = Vec::new();
        z.view()
            .zrange_by_rank(2, 3, true, |m, _s| partial.push(ev_bytes(m)));
        assert_eq!(partial, vec![b"one".to_vec()]);

        let mut neg = Vec::new();
        z.view()
            .zrange_by_rank(-2, -1, true, |m, _s| neg.push(ev_bytes(m)));
        assert_eq!(neg, vec![b"two".to_vec(), b"one".to_vec()]);
    }
}
