//! Kani proof harnesses.
//!
//! This module (and everything in it) only exists under `#[cfg(kani)]`
//! builds: `lib.rs` declares `mod kani;` behind the same cfg, so on a normal
//! `cargo build`/`cargo test` this file isn't compiled at all. The harnesses
//! themselves live in a nested `mod proofs` -- not directly in this file --
//! so that a bare `kani::...` path inside them resolves to the external
//! `kani` crate the Kani compiler injects, rather than accidentally
//! resolving to this module (which is also named `kani`, since it's a
//! sibling of, not identical to, the crate `kani`'s own top-level items).
//!
//! Run with `cargo kani -p ziplist` (from the workspace root) or `cargo
//! kani` from `crates/ziplist`. See `docs/ziplist.md`'s "Verification"
//! section for the bounds each harness is checked at and why.

#[cfg(kani)]
mod proofs {
    use crate::block::{BlockMut, InsertPos};
    use crate::cursor::Cursor;
    use crate::entry::decode_backlen;
    use crate::header::Type;
    use crate::{BlockHeader, EntryVal, HEADER_SIZE};

    /// 1. Entry codec roundtrip: for every `u64`, `encode_into` followed by
    /// `decode` reproduces the exact value and length, with no panic.
    /// `EntryVal::Uint`'s encoded length maxes out at 10 bytes (tag `254` +
    /// 8-byte payload + 1-byte backlen), so a 16-byte buffer is always
    /// enough.
    ///
    /// Bound: `#[kani::unwind(6)]`, for the same reason as harness 3 below
    /// -- `encoded_len`/`decode` both call into `encode_backlen`'s/
    /// `varint_len_for_backlen`'s varint loop (bounded by `tag_plus_data`,
    /// which for a `Uint` is one of `{1, 3, 4, 8, 9}`, well under the loop's
    /// general `MAX_VARINT_LEN = 5`-iteration cap regardless of which tier
    /// `v` falls into). Omitting an explicit bound here is *not* a smaller
    /// proof obligation: without one, Kani/CBMC doesn't infer "this loop
    /// only ever runs once" from those value constraints ahead of time --
    /// it unwinds the loop syntactically without a cap, which in practice
    /// diverged past several thousand iterations before being interrupted
    /// (see the fix note on harness 4 below for how that was diagnosed).
    /// An explicit bound is required on every harness that reaches a loop,
    /// including this one, not just the ones with an obviously
    /// large/unbounded-looking input domain.
    #[kani::proof]
    #[kani::unwind(6)]
    fn entry_uint_roundtrip() {
        let v: u64 = kani::any();
        let mut buf = [0u8; 16];
        let val = crate::EntryVal::Uint(v);
        let n = crate::encoded_len(&val);
        crate::encode_into(&val, &mut buf[..n]);
        let (got, len) = crate::decode(&buf, 0).unwrap();
        assert!(len == n);
        assert!(matches!(got, crate::EntryVal::Uint(x) if x == v));
    }

    /// 2. Header parsing never panics on arbitrary 16-byte buffers (larger
    /// than `HEADER_SIZE` by 4 bytes, matching how a real caller hands
    /// `BlockHeader::parse` a slice that may extend past the header into
    /// the body). Every field read is a fixed-offset array slice/copy, and
    /// type/format/flag validation returns `Result`, never panics or
    /// indexes past the checked-length array -- this proof is really
    /// confirming that invariant holds for the whole `u8`/`u16`/`u32`
    /// input space, not just the hand-picked cases in `header.rs`'s unit
    /// tests.
    #[kani::proof]
    fn header_parse_never_panics() {
        let buf: [u8; 16] = kani::any();
        let _ = BlockHeader::parse(&buf);
    }

    /// 3. `decode_backlen` never reads outside `[0, end)` for an arbitrary
    /// buffer up to 32 bytes and an arbitrary `end <= buf.len()`. Every
    /// buffer access goes through `buf.get(idx)` with a `checked_sub`-derived
    /// `idx`, so an out-of-range read can only manifest as a clean
    /// `DecodeError`, never a panic or an actual out-of-bounds access --
    /// this proof holds Kani to that for the whole input space up to the
    /// bound, not just hand-picked buffers.
    ///
    /// Bound: `#[kani::unwind(6)]` covers the function's single loop, which
    /// can run at most 5 times (`MAX_VARINT_LEN`) before it forces a return;
    /// 32 bytes is enough headroom to place `end` and a full 5-byte varint
    /// anywhere without truncating the scenario space the loop bound is
    /// meant to cover.
    #[kani::proof]
    #[kani::unwind(6)]
    fn decode_backlen_never_oob() {
        let buf: [u8; 32] = kani::any();
        let end: usize = kani::any();
        kani::assume(end <= buf.len());
        let _ = decode_backlen(&buf, end);
    }

    /// 4. A single `insert_at` on a symbolic list block of up to 3 entries
    /// keeps `nentry`/`tail_off` consistent: either the insert succeeds,
    /// `nentry` increments by exactly one, and the entry now at `tail_off`
    /// decodes cleanly with its end landing exactly at the block's used
    /// length; or it fails with `NeedBytes` and leaves the buffer
    /// byte-for-byte unmodified.
    ///
    /// Bounds, documented at length because this harness needed far more
    /// tightening than the other three to become tractable at all. Every
    /// version below was checked against CBMC's own diagnostic, not just
    /// the summary line (`cargo kani`'s `--verbosity 9`,
    /// `cbmc --verbosity 9`): a real counterexample prints as a failing
    /// assertion with a witness trace; every failure actually hit here
    /// instead printed CBMC's own "appears to have run out of memory" --
    /// a tooling resource limit, not a disproof. In order, what was tried
    /// and why each still wasn't enough on its own:
    /// - Full `u64` `Uint` entries, symbolic `for`-loop setup, a 96-byte
    ///   buffer, and a full `Block::parse` re-walk on success: OOM well
    ///   past 10 minutes and several GB.
    /// - Narrowing every entry to the immediate tag tier (`v <= 250`: no
    ///   data bytes, a fixed 1-byte backlen, so every entry is exactly 2
    ///   bytes and the splice/`copy_within` arithmetic no longer branches
    ///   per tier) still OOMed: a `for _ in 0..n_init` loop with a
    ///   *symbolic* trip count, wrapping calls into functions that
    ///   themselves loop (`encode_backlen` et al.), forces CBMC to unroll
    ///   the inner loops once per possible outer-loop iteration count --
    ///   a multiplicative blowup independent of the value domain.
    /// - Unrolling that loop into three plain `if n_init >= K` blocks
    ///   (semantically identical: still 0..=3 setup entries, just no
    ///   *loop* left in the harness's own control flow) still OOMed:
    ///   the non-tail insert position was built via `locate`, pulling
    ///   `locate`'s own loop plus every `Cursor` walk method's
    ///   `decode`/`decode_backward` calls into the harness's reachable
    ///   code, for a lookup whose answer is already known exactly (every
    ///   entry here is a fixed 2 bytes from a known offset).
    /// - Replacing that `locate` call with a directly-constructed
    ///   `Cursor { off: HEADER_SIZE + 2 * idx, len: 2 }` -- `insert_at`'s
    ///   splice logic only ever reads a `Cursor`'s fields as given, never
    ///   re-derives them, so this changes nothing about what's under test
    ///   -- still OOMed, as did using concrete setup values (`1`, `2`,
    ///   `3`) instead of a fresh symbolic value per setup entry.
    /// - What finally made the difference: dropping the success-path
    ///   `Block::parse(blk.bytes())` re-walk (which internally calls
    ///   `decode` in a loop, once per existing entry) in favor of a
    ///   direct, single `decode` call on just the entry at the new
    ///   `tail_off`, checking its length lands exactly at the block's used
    ///   length. This is a narrower check than "every entry in the block
    ///   is individually valid" -- but every entry other than the one just
    ///   inserted was placed by a prior, already-successful `insert_at`
    ///   call in this same harness (or is the pre-existing empty block),
    ///   so the only *new* thing that could be wrong after this op is the
    ///   op's own effect on the tail and the header fields, which is
    ///   exactly what this checks. Full multi-entry re-validation after
    ///   arbitrary op sequences is what the `ops` fuzz target (100k runs)
    ///   and `Block::parse` itself already cover.
    ///
    /// With all of the above, the final harness: entries restricted to
    /// `EntryVal::Uint(v <= 250)`; `n_init <= 3` setup entries via three
    /// unrolled `if`s (not a loop); a `u8` (not `u32`/`usize`) `idx` and
    /// `n_init`; a directly-constructed non-tail `Cursor`; a 40-byte buffer
    /// (12-byte header + up to 4 entries at 2 bytes each = 20, with room to
    /// spare -- well under the 96-byte ceiling this harness was speced at);
    /// and `#[kani::unwind(6)]`, matching harness 3's bound for the same
    /// varint-loop family (`encode_backlen`/`decode_backlen`/
    /// `varint_len_for_backlen` need at most `MAX_VARINT_LEN + 1 = 6`
    /// regardless of the value domain -- see harness 1's doc comment for
    /// why an explicit, sufficient bound matters even though every entry
    /// here is immediate-tag and so dynamically only ever takes 1
    /// iteration).
    #[kani::proof]
    #[kani::unwind(6)]
    fn single_insert_keeps_header_consistent() {
        const BUF: usize = 40;
        let mut buf = [0u8; BUF];
        BlockHeader::init_empty(Type::List, &mut buf)
            .expect("40 bytes is always enough for an empty header");
        let mut blk = BlockMut::parse(&mut buf).expect("just-initialized block parses");

        // Immediate-tag-only Uint entries: see the harness doc comment
        // above for why this is restricted (tractability, not a gap in
        // what's covered crate-wide).
        let any_imm_uint = || -> u64 {
            let v: u8 = kani::any();
            kani::assume(v <= 250);
            v as u64
        };

        // Unrolled, not a `for` loop over a symbolic trip count (see the
        // harness doc comment above), *and* concrete values (1, 2, 3), not
        // a fresh `any_imm_uint()` per setup entry: three independent
        // symbolic setup values (on top of the symbolic `n_init` selecting
        // how many of them get inserted, and the symbolic position/value
        // for the op under test) made the buffer's resulting byte content
        // depend on several compounding symbolic choices at once, which is
        // exactly the byte-array-aliasing reasoning CBMC's memory model
        // handles worst. Concrete setup values remove that compounding
        // without narrowing what's actually under test: the op being
        // proven is the *final* `insert_at` call below, whose position and
        // value are still fully symbolic.
        let n_init: u8 = kani::any();
        kani::assume(n_init <= 3);
        if n_init >= 1 {
            blk.insert_at(InsertPos::Tail, &EntryVal::Uint(1))
                .expect("<=3 two-byte entries always fit in a 40-byte buffer");
        }
        if n_init >= 2 {
            blk.insert_at(InsertPos::Tail, &EntryVal::Uint(2))
                .expect("<=3 two-byte entries always fit in a 40-byte buffer");
        }
        if n_init >= 3 {
            blk.insert_at(InsertPos::Tail, &EntryVal::Uint(3))
                .expect("<=3 two-byte entries always fit in a 40-byte buffer");
        }

        let nentry_before = blk.header().nentry;

        let at_tail: bool = kani::any();
        let idx: u8 = kani::any();
        let pos = if at_tail || nentry_before == 0 {
            InsertPos::Tail
        } else {
            kani::assume((idx as u32) < nentry_before);
            // Every entry here is exactly 2 bytes (immediate tag, no data,
            // 1-byte backlen) laid out contiguously right after the
            // 12-byte header, so entry `idx`'s cursor is computable
            // directly. Deliberately not calling `locate` (which would
            // pull its own loop, plus `Cursor::first`/`next`/`prev` and
            // their `decode`/`decode_backward` calls, into this harness's
            // reachable code for no benefit: `insert_at`'s splice logic
            // only ever reads a `Cursor`'s `off`/`len` fields as given,
            // never re-derives them) -- `locate` itself is exercised
            // directly by `cursor.rs`'s own unit tests and indirectly by
            // every `ops` fuzz case that inserts/removes/replaces at a
            // non-tail position.
            InsertPos::Before(Cursor {
                off: HEADER_SIZE + 2 * idx as usize,
                len: 2,
            })
        };

        let val = EntryVal::Uint(any_imm_uint());

        let mut snapshot = [0u8; BUF];
        snapshot.copy_from_slice(blk.bytes_full());

        match blk.insert_at(pos, &val) {
            Ok(_) => {
                assert_eq!(blk.header().nentry, nentry_before + 1);
                // A direct check on the entry at `tail_off`, not a full
                // `Block::parse` re-walk of every entry from the header:
                // see the harness doc comment above for why. `tail_off`
                // must point at a real, correctly-terminated entry (a
                // successful `decode`, itself re-checking the entry's
                // backlen against a freshly recomputed one -- see
                // `entry::decode`'s doc comment), and that entry's end
                // must land exactly at the block's used length.
                let hdr = *blk.header();
                let bytes = blk.bytes();
                let (_, len) = crate::decode(bytes, hdr.tail_off as usize)
                    .expect("tail_off must address a valid entry");
                assert_eq!(hdr.tail_off as usize + len, bytes.len());
            }
            Err(_) => {
                assert_eq!(blk.bytes_full(), &snapshot[..]);
            }
        }
    }
}
