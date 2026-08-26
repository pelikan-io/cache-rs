#!/bin/bash
# Read, write, and mixed scaling of one shared Arc<Segcache> across N threads.
#
#   CPUS=8-15 ./sweep_scaling.sh      # pin to a homogeneous core set (recommended)
#   ./sweep_scaling.sh                # no pinning
#
# Pin to cores of ONE type. On a hybrid CPU an unpinned sweep mixes performance
# cores, their SMT siblings, and efficiency cores on a single "threads" axis,
# which makes a scaling curve unreadable.
set -euo pipefail
cd "$(dirname "$0")"
BIN=${BIN:-../../target/release/segbench}
CPUS=${CPUS:-}
THREADS=${THREADS:-"1 2 4 6 8"}
OUT=${OUT:-results/scaling.csv}

run() {
  if [ -n "$CPUS" ]; then taskset -c "$CPUS" "$BIN" "$@"; else "$BIN" "$@"; fi
}

mkdir -p "$(dirname "$OUT")"
echo "threads,write_pct,dist,mode,mops" > "$OUT"
for rep in 1 2 3; do
  for t in $THREADS; do
    for w in 0 50 100; do
      for d in uniform zipf; do
        run "$t" "$w" "$d" 2 8 >> "$OUT"
      done
    done
  done
  echo "# rep $rep done" >> "$OUT"
done
echo DONE >> "$OUT"
