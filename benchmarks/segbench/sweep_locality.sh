#!/bin/bash
# Control for sweep_scaling.sh: does segment locality, rather than the key
# distribution itself, set the ceiling on skewed read scaling?
#
# mode=shuf prefills in a shuffled key order, so a zipf head spreads over many
# segments instead of packing into the few that index-ordered prefill fills
# first. Everything else is identical, so the delta isolates per-segment
# contention (the reader pin in Segments::acquire_item_at) from key contention.
#
#   CPUS=8-15 ./sweep_locality.sh
set -euo pipefail
cd "$(dirname "$0")"
BIN=${BIN:-../../target/release/segbench}
CPUS=${CPUS:-}
THREADS=${THREADS:-"1 2 4 6 8"}
OUT=${OUT:-results/locality.csv}

run() {
  if [ -n "$CPUS" ]; then taskset -c "$CPUS" "$BIN" "$@"; else "$BIN" "$@"; fi
}

mkdir -p "$(dirname "$OUT")"
echo "threads,write_pct,dist,mode,mops" > "$OUT"
for rep in 1 2 3; do
  for t in $THREADS; do
    # zipf reads are the comparison of interest; uniform is the null control
    # (prefill order must not matter) and 100% zipf writes check the write path.
    run "$t" 0 zipf 2 8 shuf >> "$OUT"
    run "$t" 0 uniform 2 8 shuf >> "$OUT"
    run "$t" 100 zipf 2 8 shuf >> "$OUT"
  done
  echo "# rep $rep done" >> "$OUT"
done
echo DONE >> "$OUT"
