# Generation-tagged locations (issue #50)

**Status:** design for review 2026-08-19 (not yet built)
**Issue:** pelikan-io/cache-rs#50. Related: #61 (TSan gate), #63/#64 (release-side token identity — shipping separately), #67 (loom oracle fixture this design is verified with).

## 1. Problem

A `Location` is `(segment id, offset)` and carries no incarnation identity. A segment can be drained, recycled, re-reserved and refilled while a thread holds a location naming it, so a stale location can silently resolve to a *different item at the same address*. Sites where that matters today:

- **Unpinned unlinks** — `Segcache::insert`'s `raced_old` arm and `rollback_reservation` unlink without a remover pin, so a recycled-and-reused incarnation can be pinned and decremented (misdirected accounting, or a `max_item_offset` assert).
- **`try_replace_existing`'s `DifferentKey` conclusion** and `cas_location_at`'s stale `SlotRef` — both currently rest on "an unchanged slot word proves the bytes were this entry's", with a documented ABA residual.
- **`delete`'s pin-fail arm** — already mitigated with a generation snapshot plus post-remove re-verify, i.e. the pattern this design generalizes and makes structural.
- **#61's TSan gate** is impossible while `verify` reads segment bytes whose validity is established only *after* the read.

## 2. Design

### Layout

`Location` keeps its 44 bits, repartitioned inside the 24-bit segment-id field:

```
current:  [ 24-bit segment id ][ 20-bit offset>>3 ]
proposed: [ 20-bit segment id ][ 4-bit tag ][ 20-bit offset>>3 ]
```

The tag is the low 4 bits of the segment header's existing `generation`, which increments on every reserve from the free queue.

**Capacity:** 1,048,575 segments (from 16,777,215). At 1 MiB segments that is 1 TiB of heap; at the 8 MiB maximum, 8 TiB. Both are far above any current deployment, and `segment_size` remains the lever if more is needed. Construction asserts the configured segment count fits, so an oversized heap fails loudly at build time rather than silently aliasing.

**Why 4 bits is sufficient, and why the argument does not depend on deployment churn:** the tag's unit is a *full segment lifecycle*, not an operation. A segment's generation advances only when it is reserved from the free queue, which requires it to have been filled, sealed, drained and recycled first. Aliasing therefore needs **16 complete fill-and-recycle cycles of one specific segment inside a single thread's stall window** — 16 MiB of writes targeted at that segment while one thread is descheduled — *on top of* the coincidences already required (same offset, same slot, matching tag bits in the packed word). Widening to 8 bits buys another factor of 16 against a term that is already the least likely in the product, at the cost of 16× the segment count.

### The mechanism is mostly free, because the tag rides inside the packed slot word

Every hashtable slot is `tag(12) | freq(8) | location(44)`, and every mutation of a published entry is already a compare-exchange against the *whole* word: `cas_location_at`, `try_cas_in_bucket`, `try_unlink_in_bucket`, `try_to_ghost_in_bucket`, `try_replace_existing`. Putting the incarnation tag inside the location means all of those validate it **with no new code and no extra load** — a stale-incarnation CAS simply fails, and each site's existing failure arm already handles that. This is the main argument for tagging the location rather than carrying identity beside it.

### The reconstruction constraint (the part that needs care)

`pack_location` has 5 production call sites and `unpack_location` has 7. Critically, **no site unpacks a location and repacks it**, so the `Metadata::pack()` round-trip trap found in #63 — where an unrelated RMW silently zeroed a tag parked in unused bits — does not recur here.

But two sites *reconstruct* a location from parts in order to CAS against a published one:

- `Segment::copy_into` — `pack_location(src.id(), offset)` as the `old_loc` for its relink CAS
- `Segments::s3fifo_promote_from` — the same shape

If reconstruction cannot reproduce the published tag, those CASes fail permanently and merge/promotion silently stops relocating. Therefore:

> **`pack_location` takes the generation as an explicit parameter.** It must not read the header internally.

Explicitness is what keeps the reconstruction sites honest: each must state *which incarnation* it means. Both are sound as written because the drain owns the segment, so its generation cannot advance underneath them — but that becomes a stated precondition rather than an accident. A `Location` remains otherwise opaque: no arithmetic, no field surgery, and `unpack_location` continues to return `(id, offset)` for addressing while a separate accessor exposes the tag for validation.

### Validation policy

A new `Segments::resolve(location) -> Option<(NonZeroU32, usize)>` compares the location's tag against the live header generation and returns `None` on mismatch. Call sites and their mismatch behaviour:

| Site | On mismatch |
|---|---|
| `get_pinned` (post-lookup addressing) | treat as a miss and retry the lookup — the entry is stale by definition |
| `remove_at` / the unpinned-unlink arms | skip the decrement; the incarnation that owned it is gone and its counters were reset wholesale |
| `acquire_item_at` | fail the pin, which existing callers already handle |
| eviction/merge relocation | skip the item, exactly as a lost relink CAS does today |

No site treats a mismatch as an error to surface; a stale location always means "this is no longer yours", which every caller already has a path for.

## 3. Scope

**In:** the layout change, `pack_location`/`unpack_location`/`resolve`, the validation sites above, the build-time capacity assertion, and the documentation of the reconstruction precondition at the two rebuild sites.

**Out:**
- **Release-side token identity** (mux's #63 defect 2) ships in #63, where it is load-bearing for that fix's correctness. If this design lands later, its tag can widen or replace that one; it must not block it.
- **Eliminating the unpinned unlinks** (the `raced_old` / `rollback_reservation` restructure). Tagging makes those *safe*; eliminating them fixes the separate missed-decrement accounting gap. Different problems, separately reviewable — tracked on its own.
- Any change to the pin/drain protocols.

## 4. Testing

- **Loom, using #67's `KeyOracle` fixture** — this is the payoff of that work. Model recycle-and-refill producing a stale location and assert every consumer rejects it, exhaustively rather than by stress. Every new model must be proven to fail against neutered code, per #67's discipline.
- **Bit-layout unit tests** — round-trip across the id/tag/offset boundaries, the `Location::GHOST` sentinel still distinguishable, and the capacity assertion firing at the configured limit.
- **A deterministic reconstruction test** for `copy_into`/`s3fifo_promote_from`: relocation must still relink after the change. A silent failure here degrades merges to no-ops without failing anything, so it is asserted directly rather than inferred from throughput.
- **Benchmark A/B** — `get` and `set` against main. The unpack gains a mask and compare on the read path; #60 showed a 1-2% regression can appear from a change that looks free, so this is reported, not assumed.
- **TSan** — re-run #61's invocation. The expectation to state honestly: this design permits validating a location *before* dereferencing it, which is what could eventually retire the racing read in `verify` — but whether the report set actually shrinks is measured, not claimed in advance.
