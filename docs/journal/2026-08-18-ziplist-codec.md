---
status: shipped
opened: 2026-08-18
updated: 2026-08-18
beta_skills: [review-guide, architecture-diagram]
---

# Ziplist codec: compact byte-format collections for cache storage

## Goal

Give pelikan's RDS server a storage representation for Redis collections
(list, hash, set, sorted set) that fits segcache's flat item model: one
compact block format as the permanent representation, following the
Twitter hybridlist / Pelikan-C ziplist lineage rather than Redis's
convert-to-pointer-structures design. This crate is Plan A of three; the
segcache in-place-update extension (Plan B) and the pelikan RESP
entrystore (Plan C) follow separately.

## Decision Criteria

- Format frozen before any consumer stores durable data: byte-exact golden
  tests over every encoding tier.
- All mutations all-or-nothing (`Result<Fit, NeedBytes>`, buffer untouched
  on refusal).
- `#![no_std]`, `#![forbid(unsafe_code)]`, zero dependencies; decode never
  panics on arbitrary bytes.
- Chain-ready: v2 root/chunk chaining must need no format break.

## Scope

New crate `crates/data-structure/ziplist` (nested for future compact
structures), its fuzz workspace, CI fuzz/kani jobs, `docs/ziplist.md`, and
generated figures under `docs/diagrams/`. No existing crate touched.

## Evidence

- PR: pelikan-io/cache-rs#45 (branched from main at 42b6a46, rebased onto
  main after #47; 15 implementation commits from `5c735e4` scaffold through
  `e4fc12d` golden-tier freezes, plus restructure/figure/sweep commits).
- Tests: `cargo test -p ziplist` — 49 pass (35 unit, 6 golden, 8
  model-based with 512 proptest cases per type and small-buffer variants
  whose refusal counters assert the `NeedBytes` path fires).
- Fuzz: `decode`, `ops`, `typed_ops` targets; 100,000 local runs each,
  zero findings; CI smoke jobs added.
- Kani: 4/4 harnesses verified (`cargo kani -p ziplist`); harness 4's
  tightened bounds and their CBMC memory history documented in
  `src/kani.rs`.
- Review trail: every task passed an independent spec+quality review;
  two real bugs found and fixed in review loops — ZREVRANGE partial-window
  indexing (`2a885d4`) and the initially-unexercised refusal path in the
  model tests (`2894667`). Final whole-branch review drove the tier golden
  freezes and a docs honesty fix (`e4fc12d`).

## Design and Implementation

12-byte LE header (`type`, type-owned `format`, `flags` with a reserved
chain-root bit, `nentry`, `tail_off`); uniform small-int-optimized entry
codec (tags 0..=250 immediate, u16/u24/u56/u64 tiers, varint-length
strings) with a backward-read varint backlen that never cascades; per-type
semantics are ordering/pairing conventions over identical entries, walked
by one splice engine. Sorted-set scores are u64 integers — a documented
Redis deviation. Design record: pelikan repo,
`docs/superpowers/specs/2026-08-18-ziplist-collections-design.md`
(branch `spec/ziplist-collections`).

Dead ends kept for the record: graphviz-routed layout for the byte-anatomy
figure (clusters/edges broke the position-is-offset claim; final figure is
deterministic direct-SVG emitted by `examples/block_anatomy.rs`, which
derives every span through the crate's own decoder and aborts on
disagreement); a Python mini-decoder as figure generator (second source of
truth, replaced by the Rust example).

## Outcome

Shipped on PR #45: the codec crate with frozen formats, full verification
stack, format docs, and generated figures. Merge pending review; the
branch's verification evidence is recorded above.

## Derived Documents

`docs/ziplist.md` (normative format reference) and `docs/diagrams/`
(anatomy figure regenerated via
`cargo run -p ziplist --example block_anatomy`; freshness check is
regenerate-and-diff on the SVG artifact itself).

## Deferred or Reopen Items

Carried to Plan C (list in the pelikan spec's "Plan C carry-list"
section): typed wrap-existing-buffer constructors and their re-validation
contract; `size_hint`/`avg_entry_bytes` for the slack policy; decode
leniency toward non-minimal encodings (revisit if encoded-byte identity
ever matters). Plan B (segcache `update`/capacity/seqlock) needs its own
design session against the engine's concurrency machinery. CI freshness
job for the generated figure is named in `docs/diagrams/README.md` but not
yet wired. A `format-layout-diagram` skill was proposed upstream-of-repo
(see Skill Feedback).

## Skill Feedback

### review-guide (beta)

- **Friction** — asked to draft PR #45's body; the rule "cite `path:line`
  pinned to a commit" rots within the PR's own lifetime: five follow-up
  commits (fix waves, restructure) landed after drafting, so the pinned
  citations point at a superseded tree even before merge. Done instead:
  kept the commit pin and accepted staleness. The instruction could say
  how to treat citations on a branch that will keep moving.
- **Confirmation** — the publish test and proportionality rule held at
  both extremes: full guide for #45, four-sentence body for #47.
  "Read every line you cite, at the moment you cite it" caught a citation
  about to be written from memory. Numbered look-out items re-marked at
  their evidence sites gave the reviewer a working reading order.

### architecture-diagram (beta)

- **Friction** — asked for a byte-format anatomy figure; the skill scopes
  itself to the build/runtime duo and `dataflow-diagram` to
  pipelines/DAGs, so neither applied and the figure was built from
  scratch. Done instead: single-use-chart mode with the family's
  principles (derive-never-draw, fail-loud, verify-the-rendering).
  Proposed upstream: a `format-layout-diagram` sibling for memory/wire
  formats — position is the byte offset (no layout engine), spans from
  golden fixtures decoded by the shipping codec, freshness =
  deterministic regenerate-and-diff on the artifact readers see.
- **Confirmation** — derive-never-draw and verify-the-rendering both
  earned their keep: the generator's asserts bound the figure to the
  codec, and rasterizing before commit caught two real defects (`--`
  inside an XML comment is illegal; a `max-width/height:auto` root style
  collapsed the canvas to zero height in cairosvg).

## Appendix: Skills Invoked

- `superpowers:brainstorming` — design exploration for the spec.
- `superpowers:writing-plans` — Plan A implementation plan.
- `superpowers:subagent-driven-development` — 10 tasks, per-task review
  loops, final whole-branch review.
- `superpowers:finishing-a-development-branch` — test-verify + PR menu.
- `review-guide` (beta) — PR #45 and #47 bodies.
- `sweep-comments` — pre-PR comment sweep (caught a lying `tail_off` doc).
- `architecture-diagram` (beta) — applicability check; single-use
  principles for the anatomy figure.
- `dataflow-diagram` — applicability check; DOT-source conventions used
  for the chaining figure.
- `recommend-skills` / `seed-skill-template` — skill adoption survey and
  the engineering-journal instance (PR #47).
