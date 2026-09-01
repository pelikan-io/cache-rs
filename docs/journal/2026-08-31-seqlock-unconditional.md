---
status: shipped
opened: 2026-08-31
updated: 2026-08-31
---

# The numeric seqlock is not a feature

## Goal

Follow-up to [2026-08-29-numeric-seqlock-split], direction from Brian:
the seqlock writer-lock discipline should be keyvalue's NORMAL behavior,
not a feature flag. As a default feature it was still a correctness
knob — any downstream building with `default-features = false` would
silently lose cas-vs-incr linearization. Structure over discipline: a
guarantee that must always hold should not be expressible as off.

## Design and Implementation

Deleted the `numeric-seqlock` feature and the lock-free fetch-op arms
(zero consumers existed). `fetch_wrapping_add`/`fetch_saturating_sub`
always take the locked path; `lock_numeric_version` and
`NumericVersionGuard` are unconditional API; `integrity = ["crc32fast"]`
no longer needs an implication. segcache's dependency drops its feature
list; the CI `--no-default-features` keyvalue lane (added two days ago
for the third shape) is removed — there are two shapes again, and
no-default-features now behaves identically to default. The manifest
comment records WHY there is no feature, so it doesn't get reintroduced
as a "flexibility" cleanup.

## Evidence

Gate green across all shapes: keyvalue default/integrity/
no-default-features (15/18/15), workspace 16 lanes, segcache debug 158,
loom 32, shuttle 7, kani keyvalue both layouts, clippy all-features +
default, fmt. No behavioral change in any previously-tested shape — the
diff deletes the one shape (lock-free numerics) nothing used.

## Outcome

Shipped in the PR carrying this entry. keyvalue stays 0.4.0 — the
version was bumped for the split two days ago and remains unreleased,
so the two changes ship as one 0.4.0.

## Deferred or Reopen Items

None.

## Appendix: Skills Invoked

- `engineering-journal` — this record.
