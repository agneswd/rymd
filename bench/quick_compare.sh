#!/usr/bin/env bash
# Quick hyperfine comparison across all synthetic trees.
set -euo pipefail
cd "$(dirname "$0")/.."
GDU="${GDU:-/tmp/opencode/gdu}"
DUA="${DUA:-$HOME/.cargo/bin/dua}"
RUNS="${RUNS:-7}"
TREES=(many_small wide tiny_dirs deep mixed sparse_hardlinks)

for t in "${TREES[@]}"; do
    P="/home/elias/rymd-bench-trees/$t"
    [ -d "$P" ] || continue
    echo "=== $t ==="
    hyperfine --style basic --runs "$RUNS" --warmup 1 \
        "./target/release/rymd-bench '$P'" \
        "'$GDU' -n -p -c '$P' > /dev/null" \
        "'$DUA' aggregate '$P' > /dev/null" 2>&1 | grep -E "Time|±|faster|slower|\.\.\." | head -14
done
