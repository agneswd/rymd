#!/usr/bin/env bash
# Linux scanner benchmark suite.
#
# Runs Rymd's production scanner against gdu, dua and diskonaut over the
# synthetic trees built by make_trees.sh plus any extra paths given as
# arguments. Results land in bench/results/<timestamp>/.
#
# Categories (see docs/performance.md):
#   - Rymd: full in-memory GUI model (arena, child links, aggregates)
#   - GDU full-tree: gdu with --depth to construct and retain full tree
#   - GDU lightweight: gdu -n (lightweight aggregate traversal, does not retain full tree)
#   - DUA aggregate: dua aggregate (aggregate traversal, does not retain interactive tree)
#   - GDU summarize: gdu -s (totals only, raw traversal lower bound)
set -euo pipefail
cd "$(dirname "$0")/.."

OUT="bench/results/$(date +%Y%m%d-%H%M%S)"
mkdir -p "$OUT"
GDU="${GDU:-$(command -v gdu 2>/dev/null || echo /tmp/opencode/gdu)}"
DUA="${DUA:-$(command -v dua 2>/dev/null || echo "$HOME/.cargo/bin/dua")}"
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

    # Rymd: full in-memory GUI model
    hyperfine --style basic --runs "$RUNS" --warmup "$WARMUP" \
        --export-json "$DIR/rymd.json" \
        "$RYMD '$TREE'" 2>&1 | tee "$DIR/log.txt"

    # GDU full tree: forces construction of full retained hierarchy
    hyperfine --style basic --runs "$RUNS" --warmup "$WARMUP" \
        --export-json "$DIR/gdu-tree.json" \
        "'$GDU' -n -p -c --depth 1000 '$TREE' > /dev/null" 2>&1 | tee -a "$DIR/log.txt"

    # GDU lightweight: non-interactive aggregate traversal (does not retain full tree)
    hyperfine --style basic --runs "$RUNS" --warmup "$WARMUP" \
        --export-json "$DIR/gdu-light.json" \
        "'$GDU' -n -p -c '$TREE' > /dev/null" 2>&1 | tee -a "$DIR/log.txt"

    # DUA aggregate: aggregate traversal (not full interactive tree)
    hyperfine --style basic --runs "$RUNS" --warmup "$WARMUP" \
        --export-json "$DIR/dua-aggregate.json" \
        "'$DUA' aggregate '$TREE' > /dev/null" 2>&1 | tee -a "$DIR/log.txt"

    # GDU summarize: raw traversal lower bound (totals only)
    hyperfine --style basic --runs "$RUNS" --warmup "$WARMUP" \
        --export-json "$DIR/gdu-summarize.json" \
        "'$GDU' -s -n -p -c '$TREE' > /dev/null" 2>&1 | tee -a "$DIR/log.txt"

    if [ "${SKIP_DISKONAUT:-0}" != "1" ]; then
        python3 "$DISKONAUT_PY" "$TREE" > "$DIR/diskonaut-runs.txt" || true
        for i in $(seq 2 "$RUNS"); do
            python3 "$DISKONAUT_PY" "$TREE" >> "$DIR/diskonaut-runs.txt" || true
        done
    fi
done

python3 bench/summarize_results.py "$OUT"
echo "results in $OUT"
