//! Fuzz target: a structured stream of typed per-type ops -- `HSET`/`HGET`/
//! `HDEL`/`HINCRBY`, `SADD`/`SREM`/`SISMEMBER`, `ZADD`/`ZREM`/`ZSCORE`/
//! `ZINCRBY`/`ZCOUNT`/`ZRANGE`(by rank, incl. `rev`+negative indices)/
//! `ZRANGEBYSCORE`, `LPUSH`/`RPUSH`/`LPOP`/`RPOP`/`LINDEX`/`LTRIM`/`LRANGE`
//! -- applied through the typed `HashMut`/`SetMut`/`ZsetMut`/`ListMut`
//! wrappers.
//!
//! Unlike `ops.rs` (which drives the untyped `BlockMut` splice engine --
//! `insert_at`/`remove_at`/`replace_at` -- directly, independent of any
//! type's conventions), this target's job is the pairing/sort/delta-
//! arithmetic logic layered on top of that engine: `map.rs`'s `pair_seek`,
//! `zset.rs`'s linear `find_member` scan, `hincrby`/`zincrby`'s
//! over/underflow handling, and `list.rs`'s Redis-style index
//! normalization. That's exactly the surface the Task 9 model-based
//! proptests (`tests/model.rs`) already exercise *semantically* (full-state
//! equality against a `std` reference model after every op) -- but with a
//! narrow, fixed byte-string strategy (`[a-z]{0,8}` plus four canned
//! numeric strings). This target complements that with libFuzzer's
//! coverage-guided arbitrary bytes and buffer sizes instead, checking
//! self-consistency rather than model equality (the model-equality job
//! stays with proptest).
//!
//! One collection type is chosen for the whole fuzz case (from the input),
//! and that type's ops are applied in sequence to a single `*Mut` over a
//! buffer whose size also comes from the input (64..=4096 bytes -- small
//! enough that `NeedBytes` capacity refusals fire routinely, not just on a
//! lucky large-input case). After every op: the block must still re-parse
//! cleanly via `Block::parse` on the type's own `bytes()` (the block's used
//! length, per the crate's cursor-safety contract -- never a raw
//! oversized buffer), and, for the two pair types (hash, zset), the
//! resulting header's `nentry` must stay even.

#![no_main]
use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use ziplist::{Block, Bound, EntryVal, HashMut, ListMut, SetMut, ZsetMut};

/// Cap on applied ops per case, mirroring `ops.rs`'s reasoning: `Vec<Op>::
/// arbitrary` would otherwise keep consuming fuzzer bytes until the input
/// is exhausted, which for a large corpus entry can make a single case
/// slow without adding meaningfully more coverage per byte.
const MAX_OPS: usize = 256;

/// Buffer size range: small enough (lower bound) that capacity refusals
/// are routine, large enough (upper bound) that plenty of cases build up
/// nontrivial collections before hitting one.
const MIN_BUF: usize = 64;
const MAX_BUF: usize = 4096;

/// Field/member/value byte length cap, keeping individual ops cheap; tier
/// boundaries for the entry codec itself are already pinned by golden and
/// Kani/proptest coverage, so this target's job is the ops layer, not the
/// entry codec.
const MAX_BYTES_LEN: usize = 64;

/// A fuzzer-controlled byte string (field/member/value), wrapped so
/// `#[derive(Arbitrary)]` can use it as an enum variant field with a
/// bounded length.
#[derive(Debug)]
struct Bytes(Vec<u8>);

impl<'a> Arbitrary<'a> for Bytes {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        let len = u.int_in_range(0..=MAX_BYTES_LEN)?;
        Ok(Bytes(u.bytes(len)?.to_vec()))
    }
}

/// A fuzzer-controlled list element: either variant exercises a different
/// stored representation (`ListMut::push_front`/`push_back` take an
/// explicit `EntryVal`, unlike hash/set/zset members which are classified
/// from raw bytes).
#[derive(Debug)]
enum ElemArb {
    Uint(u64),
    Str(Bytes),
}

impl ElemArb {
    fn as_entry(&self) -> EntryVal<'_> {
        match self {
            ElemArb::Uint(v) => EntryVal::Uint(*v),
            ElemArb::Str(b) => EntryVal::Str(&b.0),
        }
    }
}

impl<'a> Arbitrary<'a> for ElemArb {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        if bool::arbitrary(u)? {
            Ok(ElemArb::Uint(u64::arbitrary(u)?))
        } else {
            Ok(ElemArb::Str(Bytes::arbitrary(u)?))
        }
    }
}

/// Mirrors `ziplist::Bound`'s 4 variants (can't derive `Arbitrary` on the
/// crate's own type directly: orphan rules, and it's not `Arbitrary`
/// upstream).
#[derive(Debug, Arbitrary)]
enum BoundArb {
    Inclusive(u64),
    Exclusive(u64),
    NegInf,
    PosInf,
}

impl BoundArb {
    fn as_bound(&self) -> Bound {
        match self {
            BoundArb::Inclusive(v) => Bound::Inclusive(*v),
            BoundArb::Exclusive(v) => Bound::Exclusive(*v),
            BoundArb::NegInf => Bound::NegInf,
            BoundArb::PosInf => Bound::PosInf,
        }
    }
}

#[derive(Debug, Arbitrary)]
enum HashOp {
    Set(Bytes, Bytes),
    Get(Bytes),
    Del(Bytes),
    IncrBy(Bytes, i64),
}

#[derive(Debug, Arbitrary)]
enum SetOp {
    Add(Bytes),
    Rem(Bytes),
    IsMember(Bytes),
}

#[derive(Debug, Arbitrary)]
enum ZsetOp {
    Add(Bytes, u64),
    Rem(Bytes),
    Score(Bytes),
    IncrBy(Bytes, i64),
    Count(BoundArb, BoundArb),
    RangeByRank(i64, i64, bool),
    RangeByScore(BoundArb, BoundArb),
}

#[derive(Debug, Arbitrary)]
enum ListOp {
    PushFront(ElemArb),
    PushBack(ElemArb),
    PopFront,
    PopBack,
    Index(i64),
    Trim(i64, i64),
    Range(i64, i64),
}

/// Asserts the pair-typed (hash/zset) block re-parses and its `nentry`
/// stayed even -- an odd `nentry` would mean a mutator inserted/removed
/// one half of a `(field, value)`/`(member, score)` pair without its
/// partner, exactly the bug class `HashView::parse`/`ZsetView::parse`
/// guard against on external re-entry (see their module docs).
fn assert_pair_block_consistent(bytes: &[u8]) {
    let blk = Block::parse(bytes).expect("block failed to re-parse");
    assert_eq!(
        blk.header().nentry % 2,
        0,
        "pair-typed nentry must stay even"
    );
}

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let type_sel: u8 = match Arbitrary::arbitrary(&mut u) {
        Ok(v) => v,
        Err(_) => return,
    };
    let buf_len: usize = match u.int_in_range(MIN_BUF..=MAX_BUF) {
        Ok(v) => v,
        Err(_) => return,
    };
    let mut buf = vec![0u8; buf_len];

    match type_sel % 4 {
        0 => {
            let ops: Vec<ListOp> = match Arbitrary::arbitrary(&mut u) {
                Ok(v) => v,
                Err(_) => return,
            };
            let mut l = match ListMut::init(&mut buf) {
                Ok(v) => v,
                Err(_) => return,
            };
            for op in ops.into_iter().take(MAX_OPS) {
                match op {
                    ListOp::PushFront(e) => {
                        let _ = l.push_front(&e.as_entry());
                    }
                    ListOp::PushBack(e) => {
                        let _ = l.push_back(&e.as_entry());
                    }
                    ListOp::PopFront => {
                        l.pop_front(|_| ());
                    }
                    ListOp::PopBack => {
                        l.pop_back(|_| ());
                    }
                    ListOp::Index(i) => {
                        let _ = l.view().index(i);
                    }
                    ListOp::Trim(start, stop) => {
                        l.trim(start, stop);
                    }
                    ListOp::Range(start, stop) => {
                        l.view().range(start, stop, |_| ());
                    }
                }
                assert!(
                    Block::parse(l.bytes()).is_ok(),
                    "list block failed to re-parse"
                );
            }
        }
        1 => {
            let ops: Vec<HashOp> = match Arbitrary::arbitrary(&mut u) {
                Ok(v) => v,
                Err(_) => return,
            };
            let mut h = match HashMut::init(&mut buf) {
                Ok(v) => v,
                Err(_) => return,
            };
            for op in ops.into_iter().take(MAX_OPS) {
                match op {
                    HashOp::Set(field, value) => {
                        let _ = h.hset(&field.0, &value.0);
                    }
                    HashOp::Get(field) => {
                        let _ = h.view().hget(&field.0);
                    }
                    HashOp::Del(field) => {
                        let _ = h.hdel(&field.0);
                    }
                    HashOp::IncrBy(field, delta) => {
                        let _ = h.hincrby(&field.0, delta);
                    }
                }
                assert_pair_block_consistent(h.bytes());
            }
        }
        2 => {
            let ops: Vec<SetOp> = match Arbitrary::arbitrary(&mut u) {
                Ok(v) => v,
                Err(_) => return,
            };
            let mut s = match SetMut::init(&mut buf) {
                Ok(v) => v,
                Err(_) => return,
            };
            for op in ops.into_iter().take(MAX_OPS) {
                match op {
                    SetOp::Add(member) => {
                        let _ = s.sadd(&member.0);
                    }
                    SetOp::Rem(member) => {
                        let _ = s.srem(&member.0);
                    }
                    SetOp::IsMember(member) => {
                        let _ = s.view().sismember(&member.0);
                    }
                }
                assert!(
                    Block::parse(s.bytes()).is_ok(),
                    "set block failed to re-parse"
                );
            }
        }
        _ => {
            let ops: Vec<ZsetOp> = match Arbitrary::arbitrary(&mut u) {
                Ok(v) => v,
                Err(_) => return,
            };
            let mut z = match ZsetMut::init(&mut buf) {
                Ok(v) => v,
                Err(_) => return,
            };
            for op in ops.into_iter().take(MAX_OPS) {
                match op {
                    ZsetOp::Add(member, score) => {
                        let _ = z.zadd(&member.0, score);
                    }
                    ZsetOp::Rem(member) => {
                        let _ = z.zrem(&member.0);
                    }
                    ZsetOp::Score(member) => {
                        let _ = z.view().zscore(&member.0);
                    }
                    ZsetOp::IncrBy(member, delta) => {
                        let _ = z.zincrby(&member.0, delta);
                    }
                    ZsetOp::Count(min, max) => {
                        let _ = z.view().zcount(min.as_bound(), max.as_bound());
                    }
                    ZsetOp::RangeByRank(start, stop, rev) => {
                        z.view().zrange_by_rank(start, stop, rev, |_, _| ());
                    }
                    ZsetOp::RangeByScore(min, max) => {
                        z.view()
                            .zrange_by_score(min.as_bound(), max.as_bound(), |_, _| ());
                    }
                }
                assert_pair_block_consistent(z.bytes());
            }
        }
    }
});
