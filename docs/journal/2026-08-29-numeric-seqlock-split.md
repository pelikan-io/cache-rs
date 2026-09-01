---
status: shipped
opened: 2026-08-29
updated: 2026-08-29
---

# Split keyvalue's integrity feature: numeric-seqlock by default, CRC opt-in

## Goal

Stop paying a whole-item CRC32 on every insert in builds that never check
a CRC. Profiling during the 2026-08-27 concurrency-verification effort
(entry [2026-08-27-concurrency-verification-hardening]) found
`crc32fast::Hasher::update` was ~27% of set-bench samples on plain main —
because segcache force-enabled keyvalue's `integrity`, whose feature
bundled two unrelated things: the numeric seqlock writer-lock discipline
(load-bearing — the cas publish path excludes in-place increments by
holding `lock_numeric_version`, which only excludes writers that take the
lock) and the magic/CRC diagnostics (never read outside debug tooling).

Direction set by Brian: the numeric/cas correctness half becomes a
default feature; integrity (CRC + magic) becomes opt-in.

## Decision Criteria

- cas/incr linearization behavior unchanged in every build segcache
  ships: writers still serialize on the version word.
- Bench gate, same-path A/B: set improves materially (the CRC was the
  cost), get unchanged, incr not worse (its locked update loses the CRC
  recompute).
- Every feature combination keeps a green lane: default (6-byte header,
  no CRC), `integrity`/`debug` (12-byte header + CRC), keyvalue
  no-default-features (lock-free numerics, no gate API).

## Scope

keyvalue (feature split, cfg re-gating), segcache manifest (requires
`numeric-seqlock`), one layout-coupled integration test. Versions:
keyvalue 0.3.1 -> 0.4.0 (default-behavior change for keyvalue users:
numeric updates go from lock-free to seqlocked by default), segcache
0.4.4 -> 0.4.5.

## Design and Implementation

- keyvalue features: `numeric-seqlock` (new, default) carries the locked
  numeric-update path, `lock_numeric_version`, and `NumericVersionGuard`;
  `integrity = ["numeric-seqlock", "crc32fast"]` adds magic + CRC (value
  and CRC change as one seqlocked unit, so the implication is required);
  the lock-free fetch-op path remains for `no-default-features` users.
  `NumericVersionGuard::update` writes the CRC only under `integrity`.
- segcache requires `keyvalue/numeric-seqlock` (explicitly, not via
  default) and its own `integrity` feature maps to keyvalue's as before.
  Net effect on default segcache builds: no CRC per insert, 6 bytes of
  header back per item, incr's locked update drops the CRC recompute.
- The default test lane now exercises the 6-byte-header layout for the
  first time; it caught `fifo_evicts_oldest_segment_first`, which
  hand-tuned `segment_size = 264` against the old always-on-CRC layout.
  Rewritten layout-proof: uniform items with the segment size computed
  from `keyvalue::item_size` (keyvalue added as a dev-dependency;
  features unify with the lib's), so "three items fill a segment" holds
  in every combination. The other quote-based integration tests pass
  under both layouts unchanged.

## Evidence

- Profiling attribution: `sample` leaf counts, both main and the parked
  racy-bytes branch, ~27% in `crc32fast::Hasher::update` on set/1b/1b
  (recorded in issue #91's investigation).
- Bench gate: same-path interleaved A/B vs main (results in the PR).
- Full matrix: workspace default, debug/integrity, keyvalue x3 cfgs,
  loom 32, shuttle 7, Kani (both keyvalue cfgs + segcache), TSan, fuzz
  smoke, clippy all-features + default, fmt.

## Outcome

Shipped (PR pending merge at close; evidence complete). Same-path
interleaved A/B min-of-N vs main — after a false start where the branch
side accidentally measured a STALE binary left in target/ by the parked
racy-bytes work (the dep change gives this branch a different artifact
hash; binaries must be taken from the exact path `cargo bench --no-run`
prints, never by name):

| bench | main | branch | delta |
|---|---|---|---|
| set/1b/1b | 57.3ns | 41.7ns | -27.3% |
| set/1b/64b | 63.2ns | 45.4ns | -28.2% |
| set/255b/16384b | 2299ns | 484ns | -78.9% |
| incr/hot_counter | 76.5ns | 57.9ns | -24.3% |
| get_hit/1b | 40.4ns | 41.1ns | ~noise |
| get_hit/255b | 85.5ns | 83.6ns | ~noise |

The ~27% profiling prediction recovered exactly on small sets; large
values were dominated by hashing the value itself. Full matrix green:
workspace default (the 6-byte-header layout's first CI exposure — it
caught one layout-hand-tuned test, rewritten layout-proof), debug 158,
keyvalue x3 shapes, loom 32, shuttle 7, Kani 2+2+11, clippy both lanes,
fmt. Adversarial review verified all three feature shapes sound and
found the verification-surface gaps (keyvalue integrity tests had lost
their CI lane; default-layout kani run; stale docs) — all fixed.

## Deferred or Reopen Items

- None planned beyond the PR.

## Appendix: Skills Invoked

- `engineering-journal` — this record.
- `pr-adversarial-review` — pre-PR review.
