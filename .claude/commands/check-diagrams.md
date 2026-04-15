Find all fenced code blocks in markdown files under `docs/` that contain box-drawing
characters (─ │ ┌ ┐ └ ┘ ┬ ┴ ├ ┤ ┼). For each diagram found:

1. Check that every row of an outer box has its right-edge character (│, ┐, ┘) at the
   same column index as the box's top-right corner (┐).

2. Check that every vertical connector chain (┬ → │ → ┼ → │ → ▼) shares the same
   column index across all rows.

3. Check that inner boxes (nested ┌/┐ pairs) have consistent right edges within
   themselves.

Run these checks by writing a small Python script inline that parses each diagram and
reports misalignments with the file path, line number, expected column, and actual column.

If any misalignments are found, fix them by adjusting whitespace so all edges line up,
then re-run the check to confirm. Do not change any text content — only adjust spaces
between content and the right-edge box character.
