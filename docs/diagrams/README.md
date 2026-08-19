# ziplist diagrams

Figures embedded by `docs/ziplist.md`. The committed source of truth for
each graph-shaped figure is its DOT file; the SVG beside it is a render.

| figure | source | provenance |
|---|---|---|
| `ziplist-block-anatomy.svg` | **generated — do not edit.** `cargo run -p ziplist --example block_anatomy > docs/diagrams/ziplist-block-anatomy.svg` | Derived from the `tests/golden.rs::hash_golden_bytes` fixture through the crate's own ops and decoder; the generator aborts if any drawn span disagrees with the bytes (including a byte-identical re-encode check). Emitted as SVG directly: a byte grid is rigid (x-position is the byte offset), and the committed artifact is exactly what the generator produced — no render step in between. |
| `ziplist-chaining.{dot,svg}` | hand-authored DOT (a design claim, not derivable from code); render with `dot -Tsvg docs/diagrams/ziplist-chaining.dot > docs/diagrams/ziplist-chaining.svg` | v1-as-shipped vs. the v2 chained shape reserved by the header's chain-root flag; dashed = reserved-not-built. |
| `ziplist-type-conventions.svg` | direct SVG, no generator | A byte-row schematic (entries in offset order per type), not a graph. Dated snapshot from the `feature/ziplist-codec` design review. |

Freshness is currently manual (regenerate when the format or fixture
changes). A future CI check regenerates `ziplist-block-anatomy.svg` and
diffs it against the committed copy: emission is deterministic (integer
geometry, fixed order, no external renderer), so the diff covers exactly
the artifact readers see. The chaining figure's DOT is the diffable
source; its SVG render depends on the graphviz version and is refreshed
on edit.
