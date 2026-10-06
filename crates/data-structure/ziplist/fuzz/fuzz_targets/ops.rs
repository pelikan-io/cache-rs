//! Fuzz target: a structured stream of block-level splice ops
//! (`insert_at`/`remove_at`/`replace_at`), applied directly through
//! `BlockMut` rather than through the typed `HashMut`/`ListMut`/`SetMut`/
//! `ZsetMut` wrappers. This targets the splice engine shared by every
//! type (`BlockMut::splice`'s `copy_within` memmove and its
//! `nentry`/`tail_off` bookkeeping) with type-agnostic entries at
//! arbitrary positions, independent of any single type's pairing/sort
//! convention -- those are covered by the Task 9 model-based proptests
//! (`tests/model.rs`), which check semantic equivalence against a
//! reference model. This target instead checks self-consistency: after
//! every op, the block must still re-parse cleanly via `Block::parse`.
//!
//! The op stream is decoded from the raw fuzzer bytes via
//! `arbitrary::Arbitrary`: a leading type selector picks which `Type` the
//! block is initialized as (the splice engine itself doesn't interpret
//! the type tag, but exercising every value keeps `Block::parse`'s type
//! check live), followed by a bounded sequence of insert/remove/replace
//! ops with fuzzer-controlled positions and values.

#![no_main]
use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use ziplist::{locate, Block, BlockHeader, BlockMut, EntryVal, InsertPos, Type};

/// Buffer capacity for every case: large enough to host several dozen
/// entries (including the occasional larger string) so inserts/removes
/// actually shift a nontrivial tail, small enough to keep each case fast.
const BUF_LEN: usize = 4096;

/// Cap on applied ops per case. `Vec<Op>::arbitrary` would otherwise keep
/// consuming fuzzer bytes until the input is exhausted; for a large
/// corpus entry that can make a single case slow without adding
/// meaningfully more splice-engine coverage per byte.
const MAX_OPS: usize = 256;

/// A fuzzer-controlled entry value. String length is bounded to keep
/// individual splices cheap; the varint-length tier boundaries (127/128
/// bytes) and every `Uint` tag tier are already pinned by golden and
/// property tests, so this target's job is throughput over the splice
/// engine, not tier-boundary coverage.
#[derive(Debug)]
enum ElemArb {
    Uint(u64),
    Str(Vec<u8>),
}

impl ElemArb {
    fn as_entry(&self) -> EntryVal<'_> {
        match self {
            ElemArb::Uint(v) => EntryVal::Uint(*v),
            ElemArb::Str(b) => EntryVal::Str(b),
        }
    }
}

impl<'a> Arbitrary<'a> for ElemArb {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        if bool::arbitrary(u)? {
            Ok(ElemArb::Uint(u64::arbitrary(u)?))
        } else {
            let len = u.int_in_range(0..=96usize)?;
            Ok(ElemArb::Str(u.bytes(len)?.to_vec()))
        }
    }
}

/// A single block-level splice op. Indices are taken modulo the block's
/// current `nentry` at apply time (see `fuzz_target!` below), so every
/// generated index lands on a real entry once the block is non-empty.
#[derive(Debug, Arbitrary)]
enum Op {
    /// Insert `val` before the entry at `idx % nentry` (tail if empty).
    InsertBefore { idx: u32, val: ElemArb },
    /// Insert `val` at the tail.
    InsertTail { val: ElemArb },
    /// Remove the entry at `idx % nentry` (no-op if empty).
    Remove { idx: u32 },
    /// Replace the entry at `idx % nentry` with `val` (no-op if empty).
    Replace { idx: u32, val: ElemArb },
}

fn block_type(selector: u8) -> Type {
    match selector % 4 {
        0 => Type::List,
        1 => Type::Hash,
        2 => Type::Set,
        _ => Type::Zset,
    }
}

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let type_selector: u8 = match Arbitrary::arbitrary(&mut u) {
        Ok(v) => v,
        Err(_) => return,
    };
    let ops: Vec<Op> = match Arbitrary::arbitrary(&mut u) {
        Ok(v) => v,
        Err(_) => return,
    };

    let mut buf = [0u8; BUF_LEN];
    if BlockHeader::init_empty(block_type(type_selector), &mut buf).is_err() {
        return;
    }
    let mut blk = match BlockMut::parse(&mut buf) {
        Ok(b) => b,
        Err(_) => return,
    };

    for op in ops.into_iter().take(MAX_OPS) {
        let nentry = blk.header().nentry;
        match op {
            Op::InsertBefore { idx, val } => {
                let entry = val.as_entry();
                let pos = if nentry == 0 {
                    InsertPos::Tail
                } else {
                    match locate(blk.bytes(), blk.header(), idx % nentry) {
                        Ok(cur) => InsertPos::Before(cur),
                        Err(_) => InsertPos::Tail,
                    }
                };
                let _ = blk.insert_at(pos, &entry);
            }
            Op::InsertTail { val } => {
                let _ = blk.insert_at(InsertPos::Tail, &val.as_entry());
            }
            Op::Remove { idx } => {
                if nentry > 0 {
                    if let Ok(cur) = locate(blk.bytes(), blk.header(), idx % nentry) {
                        blk.remove_at(cur);
                    }
                }
            }
            Op::Replace { idx, val } => {
                if nentry > 0 {
                    if let Ok(cur) = locate(blk.bytes(), blk.header(), idx % nentry) {
                        let _ = blk.replace_at(cur, &val.as_entry());
                    }
                }
            }
        }
        assert!(
            Block::parse(blk.bytes()).is_ok(),
            "block failed to re-parse after an op"
        );
    }
});
