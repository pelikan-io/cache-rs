# ziplist diagrams

Figures embedded by `docs/ziplist.md`. The committed source of truth for
each graph-shaped figure is its DOT file; the SVG beside it is a render.

| figure | source | provenance |
|---|---|---|
| `ziplist-block-anatomy.{dot,svg}` | **generated — do not edit.** `cargo run -p ziplist --example block_anatomy > docs/diagrams/ziplist-block-anatomy.dot` | Derived from the `tests/golden.rs::hash_golden_bytes` fixture through the crate's own ops and decoder; the generator aborts if any drawn span disagrees with the bytes (including a byte-identical re-encode check). |
| `ziplist-chaining.{dot,svg}` | hand-authored DOT (a design claim, not derivable from code) | v1-as-shipped vs. the v2 chained shape reserved by the header's chain-root flag; dashed = reserved-not-built. |
| `ziplist-type-conventions.svg` | direct SVG, no DOT source | A byte-row schematic (entries in offset order per type), not a graph — a layout engine adds nothing over positional truth here. Dated snapshot from the `feature/ziplist-codec` design review. |

Render any DOT source with:

```sh
dot -Tsvg docs/diagrams/<name>.dot > docs/diagrams/<name>.svg
```

Freshness is currently manual (regenerate when the format or fixture
changes). A future CI check should diff the regenerated
`ziplist-block-anatomy.dot` against the committed one — DOT is
generator-owned text, stable across graphviz versions, unlike rendered
SVG bytes.
