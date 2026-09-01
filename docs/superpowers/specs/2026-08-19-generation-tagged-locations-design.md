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
proposed: [ 18-bit segment id ][ 6-bit tag ][ 20-bit offset>>3 ]
```

The tag is the low 6 bits of the segment header's existing `generation`. Note that this design *changes* when that counter advances: not on every reserve from the free queue (today's behaviour), but on the transitions that end a *used* incarnation — `Draining → Free` and the condemned `AwaitingRelease → Free`. That change is a hard prerequisite, not an optimization; see "Prerequisite" below for why the design does not work without it.

**Capacity: 262,142 segments** (from 16,777,215). The 18-bit field addresses 262,143 ids, and the top one is deliberately never issued — kept unissuable so that `Location::GHOST` (all 44 bits set) is unreachable *by construction* rather than merely implausible — so the limit a heap can actually reach is one below the field's capacity. At 1 MiB segments that is 256 GiB of heap; at the 8 MiB maximum, 2 TiB. Both are above any current deployment, and `segment_size` remains the lever if more is needed. Construction refuses an oversized heap outright, so it fails loudly at build time rather than silently aliasing ids.

> An earlier revision of this section quoted 1,048,575 for a 20-bit id field. That figure was wrong even then: it was the field's capacity, not the issuable limit, which the reserved top id put at 1,048,574. The distinction is restated above because it is the thing the reservation buys.

### Prerequisite: move the generation bump to reuse-of-a-used-segment

**This design requires a change to when `generation` advances, and does not work without it.**

Today `try_reserve` bumps on the `Free → Reserved` transition (`header.rs:303`). But `try_release` (`header.rs:309`) returns a `Reserved`/`Linking` segment straight back to `Free` with no fill, no seal, no drain and no recycle — and it is production-reachable from both chain-extension election-loser paths (`ttl_bucket.rs:353`, `:440`). So the cycle

```
Free → Reserved (generation++) → try_release → Free → Reserved (generation++) → …
```

costs nothing but a failed election, and under contended extension on one TTL bucket with a short free queue a specific id can round-trip in microseconds. The counter therefore advances at a rate **decoupled from segment lifecycles**.

Those cycles do not themselves create alias states — an election loser is never written into, so no location ever points into it at that generation — but they *consume tag space* at a rate no practical tag width can afford, because the rate is set by contention rather than by anything a location's lifetime is measured against.

**Fix: bump when a segment that was actually used becomes reusable**, and not when an unused one is handed back. That is precisely the event that invalidates previously-published locations, which is the only event the tag needs to track. Reusing a never-written segment without a bump is sound because nothing can hold a location into it.

**Where the bump lives: the header state transition, not the queue helper.** Segments reach a queue from four sites today (a fifth arrives with #63), and they do *not* share a single funnel:

| Site | Transition | Queue return | Bump? |
|---|---|---|---|
| `recycle` (`segments.rs:634-644`) | `Draining → Free` | `return_segment` | yes |
| `condemn` race-fix (`segments.rs:954-956`) | `AwaitingRelease → Free` | `return_segment` | yes |
| last reader's **guard drop** (`guard.rs:58-66`) | `AwaitingRelease → Free` | **pushes `free_queue` directly** | yes |
| `release_unused` (`segments.rs:756`) | `Reserved\|Linking → Free` | `return_segment` | **no** |
| #63's `ReleaseCondemned` arm | `AwaitingRelease → Free` | — | yes |

Putting the bump in `return_segment` (with a separate non-bumping helper for the election loser) is tempting and covers three of the four — but the guard drop **cannot** use `return_segment`: it holds only a raw pointer to the free queue, not `&Segments`, and deliberately bypasses the spare-aware helper. That path is a condemned free that must bump, so a queue-level bump would leave exactly the hole the tag exists to close.

The transitions, by contrast, are already header methods and every free path must perform one:

- `try_release_condemned()` — `AwaitingRelease → Free`. **Bumps.** One line covers the guard drop, the condemn race-fix, and #63's arm, because all three already call it.
- A new `try_release_drained()` — `Draining → Free`, replacing `recycle`'s inline `cas_metadata`. **Bumps.**
- `try_release()` — `Reserved|Linking → Free`. **Does not bump**, and its different name and different source states make the exception visible at the call site rather than living in a comment.

The distinction is then carried by which transition a path performs, which the state machine already forces it to get right, rather than by which queue helper it remembers to call.

Confirmed from #63's patch rather than assumed: its `ReleaseCondemned` variant is only constructible as the success value of `try_release_condemned()` (`header.rs:444-447` on that branch), so the transition covers it with no separate enumeration.

**Metrics consolidation is deliberately NOT taken here.** The same fact — three condemned paths sharing one transition — means `SEGMENT_RETURN`/`SEGMENT_FREE` could move from the per-site copies into `try_release_condemned`, which is what would have prevented #63's arm shipping without them. But #63 adds those increments at its own arm and ships first, so consolidating here would double-count against it. Sequence: #63 lands with per-site increments; a follow-up consolidates and deletes the copies. Whoever is second checks.

Verified against every production reader of `generation`, all of which keep their meaning:

| Reader | Purpose | Under the change |
|---|---|---|
| `CasToken` (`cas.rs`, item #24) | detect that an item was replaced | recycle precedes reuse, so a stale token still mismatches |
| `try_expand` H3 (`ttl_bucket.rs:353`, `:471`) | same-bucket ABA: tail recycled and reused as this bucket's tail | recycle still bumps before the segment can be re-linked |
| `Segments::generation` (`segments.rs:293`) | `delete`'s pin-fail snapshot guard | unchanged; it asks "was this recycled under me" |

An election-loser round trip is invisible to all three, because none of them can observe a segment that was never published into.

### Why 6 bits, stated honestly

With the bump tied to reuse-of-a-used-segment, the tag's unit is one *incarnation* of a segment id, and 6 bits distinguishes 64 of them. Aliasing — a stale location passing validation — requires the generation of **one specific segment id to advance by an exact multiple of 64 while a single thread is stalled** between reading that location and acting on it.

That is the entire claim. Three things it deliberately does **not** rest on, each of which an earlier revision of this document got wrong:

- **A lifecycle is not a segment's worth of writes.** An incarnation does not have to be *filled* to end. `drain_chain` drains the Live tail directly, so `clear()` and `expire()` retire a segment holding a single item; and a `Sealed` segment is freed as soon as `live_items()` reaches zero, which deletes alone can cause. A workload of inserts and deletes against a short TTL can cycle one id far faster than "fill 1 MiB, seal, drain, recycle" suggests. The old costing of 16 lifecycles as ~16 MiB of targeted writes was simply not true, and the width is not justified by it.
- **"Same offset" is not an independent coincidence.** Segments are append-only from a fixed start offset, so under uniform item sizes the n-th item of every incarnation lands at *exactly* the same offset. For such a workload matching offsets are the common case, not a lucky one. The offset term contributes no meaningful factor and must not be multiplied into the estimate.
- **The tag is sometimes the only remaining check.** At `get_pinned` and `acquire_item_at` a mismatch is one defence among several (the key verify, the packed-word CAS). But `remove_at` and `rollback_reservation` unlink *without* a remover pin and have no surviving slot-word term, so there the tag alone stands between a stale location and a decrement charged to the wrong incarnation. The width therefore has to stand on its own, not as the least likely factor in a product.

What survives is the residual as stated: **one thread stalled across 64 complete lifecycles of one specific segment id.** 64 is chosen as the point where that stall is long by any scheduler's standard even under a recycle-happy workload, while the price — one bit of tag halves the addressable segment count — is still comfortable at 262,142 segments (256 GiB at 1 MiB segments).

The residual is real and bounded, not zero, and is not claimed to be zero. It shrinks by 2× per bit and grows with the recycling rate of a single id. If a workload is ever found that cycles one id 64 times inside a stall window, the answer is a wider tag at proportionally fewer segments, or an incarnation counter carried outside the 44 bits — not a re-derivation of why the current width was fine all along.

This ordering matters for review: the bump change is a **prerequisite**, not an optimization. Landing the tag without it ships a tag counter advancing on failed elections, which is the "looks right, only shrinks the window" outcome this project has repeatedly rejected.

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

**Where `get_pinned`'s check sits (reconciled with #68).** The row above says *what* a mismatch means, not where it is tested, and the two are not the same question once #68's converging retry is in the loop. `acquire_item_at` already compares the tag under the reader guard, which is the only place the generation is frozen while it is read — so a stale incarnation fails the pin, and `get_pinned` needs no `resolve` of its own on the fast path. What it does need is to tell the two pin failures apart, because they take different arms: a transient not-readable state is retried *unboundedly* (a drain must be allowed to finish), while a stale tag is retried under a bound.

> **Correction (post-review).** This section originally justified that bound as "or a permanently stale entry spins that arm forever". A permanently stale hashtable entry is not reachable through the public API: nothing publishes a location at a dead generation (every packing reads the generation under a writer pin or a drain claim), and nothing survives the bump (both `→ Free` transitions run `Segment::clear` first, which sweeps every offset a published location can name). Each stale-arm firing is therefore paid for by a real recycle, so the arm terminates without the bound; what the bound actually buys is starvation-freedom under a recycle storm, plus termination for a stale entry planted directly through the crate-internal hashtable API, which is how `incarnation_tests` asserts the policy. The three write-path loops that retry a refused pin (`cas`, `numeric_update`, `try_into_numeric`) do *not* triage the two failures and retry both unboundedly; that is sound for the same reason, and is not the livelock it looks like.

That triage — one unpinned `resolve` of the failed location — lives in `relookup_after_pin_failure`, which is `#[cold] #[inline(never)]`, so the miss-and-retry policy is honoured without putting a second generation load on the hit path. It shares `attempts`/`REVALIDATE_RETRIES` with the post-pin revalidation: both bound how many times one `get` re-attempts because the world moved under it, one pin apiece.

## 3. Scope

**In:** the layout change, `pack_location`/`unpack_location`/`resolve`, the validation sites above, the build-time capacity assertion, and the documentation of the reconstruction precondition at the two rebuild sites.

**Out:**
- **Release-side token identity** (mux's #63 defect 2) ships in #63, where it is load-bearing for that fix's correctness. If this design lands later, its tag can widen or replace that one; it must not block it.
- **Eliminating the unpinned unlinks** (the `raced_old` / `rollback_reservation` restructure). Tagging makes those *safe*; eliminating them fixes the separate missed-decrement accounting gap. Different problems, separately reviewable — tracked on its own.
- Any change to the pin/drain protocols.

## 4. Testing

- **Loom, using #67's `KeyOracle` fixture** — this is the payoff of that work. Model recycle-and-refill producing a stale location and assert every consumer rejects it, exhaustively rather than by stress. Every new model must be proven to fail against neutered code, per #67's discipline.

  *Built as* `table.rs`'s `loom_stale_incarnation_unlink_cannot_take_the_refilled_entry`. The oracle's cells now address with the production `pack_location` (cell as segment id, offset 0), so a model asserting on the tag is built out of the real projection; `KeyOracle::recycle_and_refill` encodes the drain → recycle → re-reserve → refill ordering. The racer is `Segcache::delete`'s unpinned pin-fail arm, with its generation snapshot deliberately omitted so the assertion is that the *tag alone* suffices. Proven to fail against `tag_for_generation` returning a constant, in four peeled layers (premise guard, the consumer sweep, both invariants).

  Deliberately **not** modeled: the pinned consumers (`get_pinned`'s revalidation, `insert`/`replace_at`'s `cas_location_at`, `copy_into`'s relink, `clear`'s liveness check). Each of them holds a reader pin, a remover pin, or a drain claim on the very segment whose recycle would make its location stale, so a model that recycled underneath them would manufacture a state production cannot reach — the fixture's own faithfulness rule. Their rejection of a stale location is covered deterministically by `incarnation_tests` and `revalidation_tests` instead.
- **Bit-layout unit tests** — round-trip across the id/tag/offset boundaries, the `Location::GHOST` sentinel still distinguishable, and the capacity assertion firing at the configured limit.
- **A deterministic reconstruction test** for `copy_into`/`s3fifo_promote_from`: relocation must still relink after the change. A silent failure here degrades merges to no-ops without failing anything, so it is asserted directly rather than inferred from throughput.
- **Benchmark A/B** — `get` and `set` against main. The unpack gains a mask and compare on the read path; #60 showed a 1-2% regression can appear from a change that looks free, so this is reported, not assumed.
- **TSan** — re-run #61's invocation. The expectation to state honestly: this design permits validating a location *before* dereferencing it, which is what could eventually retire the racing read in `verify` — but whether the report set actually shrinks is measured, not claimed in advance.
