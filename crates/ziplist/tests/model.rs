//! Model-based property tests: for each of the four collection types, drive
//! a sequence of randomly generated ops against both a `ziplist` mutator
//! and a plain-`std` model (`VecDeque`/`BTreeMap`/`BTreeSet`/`HashMap`),
//! asserting full-state equality (iteration order, lengths, and targeted
//! lookups) after *every* op. A capacity refusal (`NeedBytes`) is a legal
//! outcome: the model is left unchanged and the loop continues.
//!
//! Each type's op-application logic lives in a `run_*_case` function so it
//! can be driven two ways: the main `*_matches_*_model` property (large
//! 8192-byte buffer, 512 cases, exercising ordinary op sequences) and a
//! `*_needbytes_refusals_fire_and_leave_model_unchanged` test (a small
//! buffer sized so capacity refusals actually happen). The large buffer is
//! big enough relative to average entry size (~5-13 bytes) and op count
//! (<=199) that `NeedBytes` essentially never fires there -- the
//! model-unchanged-on-refusal property would otherwise go completely
//! unexercised. The small-buffer tests use `TestRunner` directly (rather
//! than the `proptest!` macro) so a `Cell<u32>` refusal counter can be
//! threaded through every case and asserted `> 0` once the whole batch
//! completes, giving positive evidence the refusal path actually ran (not
//! just that it's legal if it happens to).
//!
//! Any divergence here is a genuine bug in the Task 5-8 op implementations
//! (`hash.rs`/`list.rs`/`set.rs`/`zset.rs`), not in the model.

use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use ziplist::{canonical_uint, render_uint, EntryVal, HSet, IncrError, SAdd, ZAdd};

/// A field/member's classification key: `(0, v, [])` for a canonical
/// `Uint(v)`, `(1, 0, bytes)` for a `Str`. Its derived `Ord` mirrors
/// `ziplist::compare`'s total order (see [`cmp_key`]).
type Key = (u8, u64, Vec<u8>);

/// Total buffer size (including the 12-byte header) for the
/// `*_needbytes_refusals_fire_and_leave_model_unchanged` tests: 128 usable
/// bytes holds only a handful of entries (a single non-canonical 20-digit
/// numeric string alone is ~24 bytes; a hash/zset pair is two entries), so
/// with up to 199 ops per case, capacity refusals are the norm rather than
/// an edge case.
const SMALL_BUF: usize = 140;

/// Number of cases for the small-buffer refusal tests. Smaller than the
/// main properties' 512: refusals need to fire at least once across the
/// whole batch, not on every single case, and 256 short-buffer cases is
/// already overwhelming evidence (see the fix-report evidence in
/// task-9-report.md for actual observed counts).
const SMALL_BUF_CASES: u32 = 256;

// ---------------------------------------------------------------------
// Shared strategies and helpers
// ---------------------------------------------------------------------

/// Short lowercase strings: `[a-z]{0,8}`.
fn short_string() -> impl Strategy<Value = Vec<u8>> {
    "[a-z]{0,8}".prop_map(|s| s.into_bytes())
}

/// Canonical and non-canonical numeric strings, to exercise
/// `canonical_uint` classification (a leading-zero string like `"01"` is
/// deliberately NOT canonical; `"18446744073709551615"` is `u64::MAX`).
fn numeric_string() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        Just(b"0".to_vec()),
        Just(b"7".to_vec()),
        Just(b"01".to_vec()),
        Just(b"18446744073709551615".to_vec()),
    ]
}

/// Field/member/value byte strategy: a mix of short strings and numeric
/// strings (canonical and non-canonical).
fn field_bytes() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![3 => short_string(), 2 => numeric_string()]
}

/// Classifies raw bytes the same way `hash.rs`/`set.rs`/`zset.rs`'s
/// `classify` helpers do (see task-9 brief): a canonical decimal renders as
/// `(0, v, [])`, anything else as `(1, 0, bytes)`. This tuple's `Ord` mirrors
/// `ziplist::compare`'s total order (`Uint` before `Str`, `Uint` by value,
/// `Str` byte-lex) exactly, so a `BTreeMap`/`BTreeSet` keyed by it iterates
/// in the same order the block does.
fn cmp_key(b: &[u8]) -> Key {
    match canonical_uint(b) {
        Some(v) => (0, v, Vec::new()),
        None => (1, 0, b.to_vec()),
    }
}

/// Same classification, applied to an already-decoded `EntryVal` (as
/// returned by `iter_pairs`/`iter_members`/`zrange_by_rank`) rather than raw
/// bytes. Since fields/members are classified before storage, this always
/// agrees with `cmp_key` applied to the original raw bytes.
fn key_from_entry(e: &EntryVal) -> Key {
    match e {
        EntryVal::Uint(v) => (0, *v, Vec::new()),
        EntryVal::Str(b) => (1, 0, b.to_vec()),
    }
}

/// Renders `v` as its canonical decimal byte string (what a *value* entry
/// looks like after `hincrby`/`zincrby` re-encodes it as `EntryVal::Uint`).
fn render_uint_vec(v: u64) -> Vec<u8> {
    let mut out = [0u8; 20];
    render_uint(v, &mut out).to_vec()
}

fn entry_to_bytes(v: EntryVal) -> Vec<u8> {
    match v {
        EntryVal::Str(b) => b.to_vec(),
        EntryVal::Uint(n) => render_uint_vec(n),
    }
}

/// Mirrors `hash::apply_delta`/`zset` `zincrby`'s shared `u64`-domain
/// arithmetic: `None` means the real op would report `Overflow` (delta >=
/// 0) or `Underflow` (delta < 0).
fn model_apply_delta(current: u64, delta: i64) -> Option<u64> {
    if delta >= 0 {
        current.checked_add(delta as u64)
    } else {
        current.checked_sub(delta.unsigned_abs())
    }
}

/// Runs `runner` over `strategy` with `body`, panicking with the failing
/// (already-shrunk) case on any property failure. Shared by every
/// small-buffer refusal test below.
fn run_small_buffer_property<S: Strategy>(
    strategy: &S,
    cases: u32,
    body: impl Fn(S::Value) -> Result<(), TestCaseError>,
) {
    let mut runner = TestRunner::new(ProptestConfig::with_cases(cases));
    if let Err(e) = runner.run(strategy, body) {
        panic!("{e}");
    }
}

// ---------------------------------------------------------------------
// Hash model
// ---------------------------------------------------------------------

#[derive(Debug, Clone)]
enum HashOp {
    Set(Vec<u8>, Vec<u8>),
    Del(Vec<u8>),
    Get(Vec<u8>),
    IncrBy(Vec<u8>, i64),
}

fn hash_op_strategy() -> impl Strategy<Value = HashOp> {
    prop_oneof![
        (field_bytes(), field_bytes()).prop_map(|(f, v)| HashOp::Set(f, v)),
        field_bytes().prop_map(HashOp::Del),
        field_bytes().prop_map(HashOp::Get),
        (field_bytes(), any::<i64>()).prop_map(|(f, d)| HashOp::IncrBy(f, d)),
    ]
}

fn pairs_as_bytes(view: &ziplist::HashView) -> Vec<(Key, Vec<u8>)> {
    let mut out = Vec::new();
    view.iter_pairs(|k, v| out.push((key_from_entry(k), entry_to_bytes(*v))));
    out
}

fn model_pairs(model: &BTreeMap<Key, Vec<u8>>) -> Vec<(Key, Vec<u8>)> {
    model.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// Applies `HINCRBY field delta` to both the real hash and the model,
/// asserting they agree. A missing field auto-vivifies at `0`; a field
/// whose current value doesn't canonicalize to a `u64` is `NotAnInteger`;
/// arithmetic that would go below `0` or past `u64::MAX` is `Underflow`/
/// `Overflow`. Per the module docs, neither error path can ever hit
/// `NeedBytes` (nothing is written before the arithmetic is known to
/// succeed) -- only the success path can, and there the model is left
/// unchanged (and `refusals` incremented) on a capacity refusal.
fn check_hincrby(
    h: &mut ziplist::HashMut,
    model: &mut BTreeMap<Key, Vec<u8>>,
    field: &[u8],
    delta: i64,
    refusals: &Cell<u32>,
) -> Result<(), TestCaseError> {
    let key = cmp_key(field);
    let base = match model.get(&key) {
        Some(val) => canonical_uint(val),
        None => Some(0),
    };
    match base {
        None => {
            let real = h
                .hincrby(field, delta)
                .expect("NotAnInteger path never writes, so capacity can't fail here");
            prop_assert_eq!(real, Err(IncrError::NotAnInteger));
        }
        Some(cur) => match model_apply_delta(cur, delta) {
            None => {
                let real = h
                    .hincrby(field, delta)
                    .expect("arithmetic-error path never writes, so capacity can't fail here");
                let expect = if delta >= 0 {
                    IncrError::Overflow
                } else {
                    IncrError::Underflow
                };
                prop_assert_eq!(real, Err(expect));
            }
            Some(new_val) => match h.hincrby(field, delta) {
                Ok(Ok(got)) => {
                    prop_assert_eq!(got, new_val);
                    model.insert(key, render_uint_vec(new_val));
                }
                Ok(Err(e)) => prop_assert!(false, "unexpected IncrError {e:?} on success path"),
                Err(_need) => {
                    refusals.set(refusals.get() + 1); // capacity refusal: model stays as-is
                }
            },
        },
    }
    Ok(())
}

/// Applies `ops` to a fresh `HashMut` over `buf` and a fresh model in
/// lockstep, asserting full-state equality after every op. `refusals` is
/// incremented on every `NeedBytes` capacity refusal encountered (both
/// `hset`'s and `hincrby`'s), so callers with a deliberately small `buf`
/// can confirm the refusal path actually ran.
fn run_hash_case(
    buf: &mut [u8],
    ops: Vec<HashOp>,
    refusals: &Cell<u32>,
) -> Result<(), TestCaseError> {
    let mut h = ziplist::HashMut::init(buf).unwrap();
    let mut model: BTreeMap<Key, Vec<u8>> = BTreeMap::new();
    for op in ops {
        match op {
            HashOp::Set(f, v) => match h.hset(&f, &v) {
                Ok(res) => {
                    let existed = model.contains_key(&cmp_key(&f));
                    prop_assert_eq!(matches!(res, HSet::Updated), existed);
                    model.insert(cmp_key(&f), v);
                }
                Err(_need) => {
                    refusals.set(refusals.get() + 1); // capacity refusal is legal; model unchanged
                }
            },
            HashOp::Del(f) => {
                prop_assert_eq!(h.hdel(&f).is_some(), model.remove(&cmp_key(&f)).is_some());
            }
            HashOp::Get(f) => {
                let got = h.view().hget(&f).unwrap().map(entry_to_bytes);
                prop_assert_eq!(got, model.get(&cmp_key(&f)).cloned());
            }
            HashOp::IncrBy(f, delta) => {
                check_hincrby(&mut h, &mut model, &f, delta, refusals)?;
            }
        }
        // full-state check every op: same length, same iteration order
        prop_assert_eq!(h.view().hlen() as usize, model.len());
        prop_assert_eq!(pairs_as_bytes(&h.view()), model_pairs(&model));
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]
    #[test]
    fn hash_matches_btreemap_model(ops in proptest::collection::vec(hash_op_strategy(), 1..200)) {
        let mut buf = vec![0u8; 8192];
        let refusals = Cell::new(0u32);
        run_hash_case(&mut buf, ops, &refusals)?;
    }
}

#[test]
fn hash_needbytes_refusals_fire_and_leave_model_unchanged() {
    let refusals = Cell::new(0u32);
    run_small_buffer_property(
        &proptest::collection::vec(hash_op_strategy(), 1..200),
        SMALL_BUF_CASES,
        |ops| {
            let mut buf = vec![0u8; SMALL_BUF];
            run_hash_case(&mut buf, ops, &refusals)
        },
    );
    assert!(
        refusals.get() > 0,
        "expected at least one hash NeedBytes refusal to fire across {SMALL_BUF_CASES} cases \
         with a {SMALL_BUF}-byte buffer; got 0 -- the model-unchanged-on-refusal property was \
         never actually exercised"
    );
}

// ---------------------------------------------------------------------
// Set model
// ---------------------------------------------------------------------

#[derive(Debug, Clone)]
enum SetOp {
    Add(Vec<u8>),
    Rem(Vec<u8>),
    IsMember(Vec<u8>),
}

fn set_op_strategy() -> impl Strategy<Value = SetOp> {
    prop_oneof![
        field_bytes().prop_map(SetOp::Add),
        field_bytes().prop_map(SetOp::Rem),
        field_bytes().prop_map(SetOp::IsMember),
    ]
}

/// Applies `ops` to a fresh `SetMut` over `buf` and a fresh model in
/// lockstep, asserting full-state equality after every op. `refusals` is
/// incremented on every `sadd` `NeedBytes` capacity refusal.
fn run_set_case(
    buf: &mut [u8],
    ops: Vec<SetOp>,
    refusals: &Cell<u32>,
) -> Result<(), TestCaseError> {
    let mut s = ziplist::SetMut::init(buf).unwrap();
    let mut model: BTreeSet<Key> = BTreeSet::new();
    for op in ops {
        match op {
            SetOp::Add(m) => {
                let already = model.contains(&cmp_key(&m));
                match s.sadd(&m) {
                    Ok(res) => {
                        prop_assert_eq!(matches!(res, SAdd::Added), !already);
                        model.insert(cmp_key(&m));
                    }
                    Err(_need) => {
                        refusals.set(refusals.get() + 1); // capacity refusal is legal; model unchanged
                    }
                }
            }
            SetOp::Rem(m) => {
                prop_assert_eq!(s.srem(&m).is_some(), model.remove(&cmp_key(&m)));
            }
            SetOp::IsMember(m) => {
                let real = s.view().sismember(&m).unwrap();
                prop_assert_eq!(real, model.contains(&cmp_key(&m)));
            }
        }
        // full-state check every op: same length, same iteration order
        prop_assert_eq!(s.view().scard() as usize, model.len());
        let mut got = Vec::new();
        s.view().iter_members(|m| got.push(key_from_entry(m)));
        let expect: Vec<_> = model.iter().cloned().collect();
        prop_assert_eq!(got, expect);
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]
    #[test]
    fn set_matches_btreeset_model(ops in proptest::collection::vec(set_op_strategy(), 1..200)) {
        let mut buf = vec![0u8; 8192];
        let refusals = Cell::new(0u32);
        run_set_case(&mut buf, ops, &refusals)?;
    }
}

#[test]
fn set_needbytes_refusals_fire_and_leave_model_unchanged() {
    let refusals = Cell::new(0u32);
    run_small_buffer_property(
        &proptest::collection::vec(set_op_strategy(), 1..200),
        SMALL_BUF_CASES,
        |ops| {
            let mut buf = vec![0u8; SMALL_BUF];
            run_set_case(&mut buf, ops, &refusals)
        },
    );
    assert!(
        refusals.get() > 0,
        "expected at least one set NeedBytes refusal to fire across {SMALL_BUF_CASES} cases \
         with a {SMALL_BUF}-byte buffer; got 0 -- the model-unchanged-on-refusal property was \
         never actually exercised"
    );
}

// ---------------------------------------------------------------------
// Zset model
// ---------------------------------------------------------------------

#[derive(Debug, Clone)]
enum ZsetOp {
    Add(Vec<u8>, u64),
    Rem(Vec<u8>),
    IncrBy(Vec<u8>, i64),
    Score(Vec<u8>),
}

/// Scores drawn from encoding-tier boundaries plus fully arbitrary `u64`s.
fn score_strategy() -> impl Strategy<Value = u64> {
    prop_oneof![
        2 => Just(0u64),
        1 => Just(1u64),
        1 => Just(250u64),
        1 => Just(251u64),
        1 => Just(65535u64),
        1 => Just(65536u64),
        1 => Just((1u64 << 24) - 1),
        1 => Just(1u64 << 24),
        1 => Just((1u64 << 56) - 1),
        1 => Just(1u64 << 56),
        1 => Just(u64::MAX),
        4 => any::<u64>(),
    ]
}

fn zset_op_strategy() -> impl Strategy<Value = ZsetOp> {
    prop_oneof![
        (field_bytes(), score_strategy()).prop_map(|(m, sc)| ZsetOp::Add(m, sc)),
        field_bytes().prop_map(ZsetOp::Rem),
        (field_bytes(), any::<i64>()).prop_map(|(m, d)| ZsetOp::IncrBy(m, d)),
        field_bytes().prop_map(ZsetOp::Score),
    ]
}

/// Applies `ZINCRBY member delta` to both the real zset and the model. A
/// missing member auto-vivifies at `0`; there's no `NotAnInteger` case
/// (scores are always stored as `Uint`), so this is a strict subset of
/// `check_hincrby`'s logic. `refusals` is incremented on a `NeedBytes`
/// capacity refusal.
fn check_zincrby(
    z: &mut ziplist::ZsetMut,
    scores: &mut HashMap<Vec<u8>, u64>,
    member: &[u8],
    delta: i64,
    refusals: &Cell<u32>,
) -> Result<(), TestCaseError> {
    let cur = scores.get(member).copied().unwrap_or(0);
    match model_apply_delta(cur, delta) {
        None => {
            let real = z
                .zincrby(member, delta)
                .expect("arithmetic-error path never writes, so capacity can't fail here");
            let expect = if delta >= 0 {
                IncrError::Overflow
            } else {
                IncrError::Underflow
            };
            prop_assert_eq!(real, Err(expect));
        }
        Some(new_val) => match z.zincrby(member, delta) {
            Ok(Ok(got)) => {
                prop_assert_eq!(got, new_val);
                scores.insert(member.to_vec(), new_val);
            }
            Ok(Err(e)) => prop_assert!(false, "unexpected IncrError {e:?} on success path"),
            Err(_need) => {
                refusals.set(refusals.get() + 1); // capacity refusal: model stays as-is
            }
        },
    }
    Ok(())
}

/// Applies `ops` to a fresh `ZsetMut` over `buf` and a fresh model in
/// lockstep, asserting full-state equality after every op. `refusals` is
/// incremented on every `NeedBytes` capacity refusal encountered (both
/// `zadd`'s and `zincrby`'s).
fn run_zset_case(
    buf: &mut [u8],
    ops: Vec<ZsetOp>,
    refusals: &Cell<u32>,
) -> Result<(), TestCaseError> {
    let mut z = ziplist::ZsetMut::init(buf).unwrap();
    let mut scores: HashMap<Vec<u8>, u64> = HashMap::new();
    for op in ops {
        match op {
            ZsetOp::Add(m, sc) => {
                let prior = scores.get(&m).copied();
                match z.zadd(&m, sc) {
                    Ok(res) => {
                        match prior {
                            None => prop_assert!(matches!(res, ZAdd::New)),
                            Some(old) if old == sc => prop_assert!(matches!(res, ZAdd::Unchanged)),
                            Some(_) => prop_assert!(matches!(res, ZAdd::ScoreChanged)),
                        }
                        scores.insert(m.clone(), sc);
                    }
                    Err(_need) => {
                        refusals.set(refusals.get() + 1); // capacity refusal is legal; model unchanged
                    }
                }
            }
            ZsetOp::Rem(m) => {
                prop_assert_eq!(z.zrem(&m).is_some(), scores.remove(&m).is_some());
            }
            ZsetOp::IncrBy(m, delta) => {
                check_zincrby(&mut z, &mut scores, &m, delta, refusals)?;
            }
            ZsetOp::Score(m) => {
                let real = z.view().zscore(&m).unwrap();
                prop_assert_eq!(real, scores.get(&m).copied());
            }
        }
        // full-state check every op: same length, same iteration order,
        // and (member, score) pairs matching zscore for every member.
        prop_assert_eq!(z.view().zcard() as usize, scores.len());
        let mut got = Vec::new();
        z.view()
            .zrange_by_rank(0, -1, false, |m, sc| got.push((key_from_entry(m), sc)));
        let mut expect: Vec<_> = scores.iter().map(|(m, &sc)| (cmp_key(m), sc)).collect();
        expect.sort_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
        prop_assert_eq!(got, expect);
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]
    #[test]
    fn zset_matches_model(ops in proptest::collection::vec(zset_op_strategy(), 1..200)) {
        let mut buf = vec![0u8; 8192];
        let refusals = Cell::new(0u32);
        run_zset_case(&mut buf, ops, &refusals)?;
    }
}

#[test]
fn zset_needbytes_refusals_fire_and_leave_model_unchanged() {
    let refusals = Cell::new(0u32);
    run_small_buffer_property(
        &proptest::collection::vec(zset_op_strategy(), 1..200),
        SMALL_BUF_CASES,
        |ops| {
            let mut buf = vec![0u8; SMALL_BUF];
            run_zset_case(&mut buf, ops, &refusals)
        },
    );
    assert!(
        refusals.get() > 0,
        "expected at least one zset NeedBytes refusal to fire across {SMALL_BUF_CASES} cases \
         with a {SMALL_BUF}-byte buffer; got 0 -- the model-unchanged-on-refusal property was \
         never actually exercised"
    );
}

// ---------------------------------------------------------------------
// List model
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Elem {
    U(u64),
    S(Vec<u8>),
}

impl Elem {
    fn as_entryval(&self) -> EntryVal<'_> {
        match self {
            Elem::U(v) => EntryVal::Uint(*v),
            Elem::S(b) => EntryVal::Str(b),
        }
    }

    fn from_entryval(v: EntryVal) -> Elem {
        match v {
            EntryVal::Uint(x) => Elem::U(x),
            EntryVal::Str(b) => Elem::S(b.to_vec()),
        }
    }
}

/// `u64`s drawn from encoding-tier boundaries plus arbitrary values, so
/// list entries exercise every immediate/`u16`/`u24`/`u56`/`u64` tag tier.
fn elem_uint_strategy() -> impl Strategy<Value = u64> {
    prop_oneof![
        2 => Just(0u64),
        1 => Just(1u64),
        1 => Just(250u64),
        1 => Just(251u64),
        1 => Just(65535u64),
        1 => Just(65536u64),
        1 => Just((1u64 << 24) - 1),
        1 => Just(1u64 << 24),
        1 => Just((1u64 << 56) - 1),
        1 => Just(1u64 << 56),
        1 => Just(u64::MAX),
        4 => any::<u64>(),
    ]
}

fn list_elem_strategy() -> impl Strategy<Value = Elem> {
    prop_oneof![
        short_string().prop_map(Elem::S),
        elem_uint_strategy().prop_map(Elem::U),
    ]
}

/// Small indices dominate (so trims/ranges/indexes land inside or just past
/// a list of up to a few hundred entries), with a thin tail of arbitrary
/// `i64`s for wildly out-of-range cases.
fn index_strategy() -> impl Strategy<Value = i64> {
    prop_oneof![9 => -12i64..=12i64, 1 => any::<i64>()]
}

#[derive(Debug, Clone)]
enum ListOp {
    PushFront(Elem),
    PushBack(Elem),
    PopFront,
    PopBack,
    Trim(i64, i64),
    Index(i64),
    Range(i64, i64),
}

fn list_op_strategy() -> impl Strategy<Value = ListOp> {
    prop_oneof![
        list_elem_strategy().prop_map(ListOp::PushFront),
        list_elem_strategy().prop_map(ListOp::PushBack),
        Just(ListOp::PopFront),
        Just(ListOp::PopBack),
        (index_strategy(), index_strategy()).prop_map(|(a, b)| ListOp::Trim(a, b)),
        index_strategy().prop_map(ListOp::Index),
        (index_strategy(), index_strategy()).prop_map(|(a, b)| ListOp::Range(a, b)),
    ]
}

/// Push-heavy variant of [`list_op_strategy`], used only by the small-buffer
/// refusal test below. `list_op_strategy`'s balanced push/pop weights make a
/// roughly driftless random walk of the list's length: measured empirically
/// (see task-9-report.md), it never once exceeded a 128-byte-usable buffer's
/// capacity across 256 small-buffer cases. Pushes outnumbering pops 4:1
/// gives positive drift, so the list's length grows past any fixed capacity
/// within the first few dozen ops of nearly every case, and (thanks to the
/// occasional pop) also keeps freeing space so `push_front`/`push_back` see
/// both a fit and a refusal repeatedly within a single case, not just one
/// then never again.
fn list_push_heavy_op_strategy() -> impl Strategy<Value = ListOp> {
    prop_oneof![
        4 => list_elem_strategy().prop_map(ListOp::PushFront),
        4 => list_elem_strategy().prop_map(ListOp::PushBack),
        1 => Just(ListOp::PopFront),
        1 => Just(ListOp::PopBack),
        1 => (index_strategy(), index_strategy()).prop_map(|(a, b)| ListOp::Trim(a, b)),
        1 => index_strategy().prop_map(ListOp::Index),
        1 => (index_strategy(), index_strategy()).prop_map(|(a, b)| ListOp::Range(a, b)),
    ]
}

/// Redis-style index normalization: negative indices count from the tail.
/// Mirrors `ListView::normalize` exactly (no clamping here; callers do
/// their own bounds checks, same as the real op).
fn model_normalize(i: i64, len: usize) -> i64 {
    if i < 0 {
        i + len as i64
    } else {
        i
    }
}

fn model_index(model: &VecDeque<Elem>, i: i64) -> Option<Elem> {
    let len = model.len();
    let idx = model_normalize(i, len);
    if idx < 0 || idx >= len as i64 {
        None
    } else {
        model.get(idx as usize).cloned()
    }
}

/// Mirrors `ListView::range`'s inclusive, clamped, possibly-empty window.
fn model_range(model: &VecDeque<Elem>, start: i64, stop: i64) -> Vec<Elem> {
    let len = model.len() as i64;
    if len == 0 {
        return Vec::new();
    }
    let start = model_normalize(start, model.len()).max(0);
    let stop = model_normalize(stop, model.len()).min(len - 1);
    if start > stop {
        return Vec::new();
    }
    (start..=stop).map(|i| model[i as usize].clone()).collect()
}

/// Mirrors `ListMut::trim`'s inclusive, clamped window: keeps
/// `[start, stop]`, or empties the list if the (normalized/clamped) window
/// is empty.
fn model_trim(model: &mut VecDeque<Elem>, start: i64, stop: i64) {
    let len = model.len() as i64;
    if len == 0 {
        return;
    }
    let start = model_normalize(start, model.len()).max(0);
    let stop = model_normalize(stop, model.len()).min(len - 1);
    if start > stop {
        model.clear();
        return;
    }
    model.truncate((stop + 1) as usize);
    for _ in 0..start {
        model.pop_front();
    }
}

/// Applies `ops` to a fresh `ListMut` over `buf` and a fresh model in
/// lockstep, asserting full-state equality after every op. `refusals` is
/// incremented on every `push_front`/`push_back` `NeedBytes` capacity
/// refusal.
fn run_list_case(
    buf: &mut [u8],
    ops: Vec<ListOp>,
    refusals: &Cell<u32>,
) -> Result<(), TestCaseError> {
    let mut l = ziplist::ListMut::init(buf).unwrap();
    let mut model: VecDeque<Elem> = VecDeque::new();
    for op in ops {
        match op {
            ListOp::PushFront(e) => {
                let ev = e.as_entryval();
                match l.push_front(&ev) {
                    Ok(_) => model.push_front(e),
                    Err(_need) => {
                        refusals.set(refusals.get() + 1); // capacity refusal is legal; model unchanged
                    }
                }
            }
            ListOp::PushBack(e) => {
                let ev = e.as_entryval();
                match l.push_back(&ev) {
                    Ok(_) => model.push_back(e),
                    Err(_need) => {
                        refusals.set(refusals.get() + 1); // capacity refusal is legal; model unchanged
                    }
                }
            }
            ListOp::PopFront => {
                let got = l.pop_front(Elem::from_entryval);
                match got {
                    Some(elem) => prop_assert_eq!(Some(elem), model.pop_front()),
                    None => prop_assert!(model.is_empty()),
                }
            }
            ListOp::PopBack => {
                let got = l.pop_back(Elem::from_entryval);
                match got {
                    Some(elem) => prop_assert_eq!(Some(elem), model.pop_back()),
                    None => prop_assert!(model.is_empty()),
                }
            }
            ListOp::Trim(start, stop) => {
                l.trim(start, stop);
                model_trim(&mut model, start, stop);
            }
            ListOp::Index(i) => {
                let real = l.view().index(i).unwrap().map(|v| Elem::from_entryval(v));
                prop_assert_eq!(real, model_index(&model, i));
            }
            ListOp::Range(start, stop) => {
                let mut real = Vec::new();
                l.view()
                    .range(start, stop, |v| real.push(Elem::from_entryval(*v)));
                prop_assert_eq!(real, model_range(&model, start, stop));
            }
        }
        // full-state check every op: same length, same iteration order
        prop_assert_eq!(l.view().len() as usize, model.len());
        let mut real_all = Vec::new();
        for i in 0..l.view().len() {
            real_all.push(Elem::from_entryval(
                l.view().index(i as i64).unwrap().unwrap(),
            ));
        }
        let expect_all: Vec<Elem> = model.iter().cloned().collect();
        prop_assert_eq!(real_all, expect_all);
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]
    #[test]
    fn list_matches_vecdeque_model(ops in proptest::collection::vec(list_op_strategy(), 1..200)) {
        let mut buf = vec![0u8; 8192];
        let refusals = Cell::new(0u32);
        run_list_case(&mut buf, ops, &refusals)?;
    }
}

#[test]
fn list_needbytes_refusals_fire_and_leave_model_unchanged() {
    let refusals = Cell::new(0u32);
    run_small_buffer_property(
        &proptest::collection::vec(list_push_heavy_op_strategy(), 1..200),
        SMALL_BUF_CASES,
        |ops| {
            let mut buf = vec![0u8; SMALL_BUF];
            run_list_case(&mut buf, ops, &refusals)
        },
    );
    assert!(
        refusals.get() > 0,
        "expected at least one list NeedBytes refusal to fire across {SMALL_BUF_CASES} cases \
         with a {SMALL_BUF}-byte buffer; got 0 -- the model-unchanged-on-refusal property was \
         never actually exercised"
    );
}
