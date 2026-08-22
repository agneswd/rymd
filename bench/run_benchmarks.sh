#!/usr/bin/env bash
# Linux scanner benchmark suite.
#
# Runs Rymd's production scanner against gdu, dua and diskonaut over the
# synthetic trees built by make_trees.sh plus any extra paths given as
# arguments. Results land in bench/results/<timestamp>/.
#
# Categories (see docs/performance.md):
#   - FULL TREE: every tool walks and aggregates the entire tree
#   - RAW LOWER BOUND: gdu --summarize skips most of what an interactive
#     tool must retain; shown for context only
set -euo pipefail
cd "$(dirname "$0")/.."

OUT="bench/results/$(date +%Y%m%d-%H%M%S)"
mkdir -p "$OUT"
GDU="${GDU:-/tmp/opencode/gdu}"
DUA="${DUA:-$HOME/.cargo/bin/dua}"
DISKONAUT_PY="bench/bench_diskonaut.py"
RYMD="./target/release/rymd-bench"

TREES=(
    "${RYMD_TREES:-$HOME/rymd-bench-trees}"/many_small
    "${RYMD_TREES:-$HOME/rymd-bench-trees}"/wide
    "${RYMD_TREES:-$HOME/rymd-bench-trees}"/tiny_dirs
    "${RYMD_TREES:-$HOME/rymd-bench-trees}"/deep
    "${RYMD_TREES:-$HOME/rymd-bench-trees}"/mixed
    "${RYMD_TREES:-$HOME/rymd-bench-trees}"/sparse_hardlinks
)
TREES+=("$@")   # extra paths, e.g. $HOME

RUNS="${RUNS:-7}"
WARMUP=1

echo "# environment" > "$OUT/env.txt"
uname -a >> "$OUT/env.txt"
grep "model name" /proc/cpuinfo | head -1 >> "$OUT/env.txt"
nproc >> "$OUT/env.txt"
"$GDU" --version | head -1 >> "$OUT/env.txt" || true
"$DUA" --version >> "$OUT/env.txt" || true
~/.cargo/bin/diskonaut --version 2>&1 | head -1 >> "$OUT/env.txt" || true
./target/release/rymd-bench --version 2>/dev/null || true
rustc --version >> "$OUT/env.txt"

for TREE in "${TREES[@]}"; do
    [ -d "$TREE" ] || { echo "skip missing $TREE"; continue; }
    NAME="$(basename "$TREE")"
    echo "=== $NAME ==="
    DIR="$OUT/$NAME"
    mkdir -p "$DIR"

    hyperfine --style basic --runs "$RUNS" --warmup "$WARMUP" \
        --export-json "$DIR/rymd.json" \
        "$RYMD '$TREE'" 2>&1 | tee "$DIR/log.txt"

    hyperfine --style basic --runs "$RUNS" --warmup "$WARMUP" \
        --export-json "$DIR/gdu-full.json" \
        "'$GDU' -n -p -c '$TREE' > /dev/null" 2>&1 | tee -a "$DIR/log.txt"

    # Raw traversal lower bound: totals only, nothing retained.
    hyperfine --style basic --runs "$RUNS" --warmup "$WARMUP" \
        --export-json "$DIR/gdu-summarize.json" \
        "'$GDU' -s -n -p -c '$TREE' > /dev/null" 2>&1 | tee -a "$DIR/log.txt"

    hyperfine --style basic --runs "$RUNS" --warmup "$WARMUP" \
        --export-json "$DIR/dua.json" \
        "'$DUA' aggregate '$TREE' > /dev/null" 2>&1 | tee -a "$DIR/log.txt"

    if [ "${SKIP_DISKONAUT:-0}" != "1" ]; then
        python3 "$DISKONAUT_PY" "$TREE" > "$DIR/diskonaut-runs.txt" || true
        for i in $(seq 2 "$RUNS"); do
            python3 "$DISKONAUT_PY" "$TREE" >> "$DIR/diskonaut-runs.txt" || true
        done
    fi
done

python3 bench/summarize_results.py "$OUT"
echo "results in $OUT"
