#!/usr/bin/env bash
# Deterministic synthetic benchmark trees.
# Usage: make_trees.sh [BASE_DIR]
set -euo pipefail
BASE="${1:-$HOME/rymd-bench-trees}"
mkdir -p "$BASE"

# 1. many-small: 100k files of ~1 KiB spread over 100 directories.
mk() { # mk <name> <fn>
    local name="$1"; shift
    local dir="$BASE/$name"
    if [ -e "$dir/.done" ]; then echo "skip $name"; return; fi
    mkdir -p "$dir"
    "$@" "$dir"
    touch "$dir/.done"
    echo "built $name"
}

many_small() {
    local d="$1"
    python3 - "$d" <<'EOF'
import os, sys
base = sys.argv[1]
blob = b"x" * 1024
for i in range(100):
    dd = os.path.join(base, f"d{i:03d}")
    os.mkdir(dd)
    for j in range(1000):
        with open(os.path.join(dd, f"f{j:04d}.dat"), "wb") as f:
            f.write(blob)
EOF
}

wide() {
    local d="$1"
    python3 - "$d" <<'EOF'
import os, sys
base = sys.argv[1]
os.mkdir(os.path.join(base, "wide"))
for j in range(300_000):
    with open(os.path.join(base, "wide", f"file{j:06d}.dat"), "wb") as f:
        f.write(b"y")
EOF
}

tiny_dirs() {
    local d="$1"
    python3 - "$d" <<'EOF'
import os, sys
base = sys.argv[1]
root = os.path.join(base, "manydirs")
os.mkdir(root)
for i in range(50_000):
    dd = os.path.join(root, f"dir{i:05d}")
    os.mkdir(dd)
    with open(os.path.join(dd, "a.txt"), "wb") as f:
        f.write(b"a")
    if i % 2 == 0:
        with open(os.path.join(dd, "b.txt"), "wb") as f:
            f.write(b"bb")
EOF
}

deep() {
    local d="$1"
    python3 - "$d" <<'EOF'
import os, sys
base = sys.argv[1]
cur = base
for i in range(600):
    cur = os.path.join(cur, "d")
    os.mkdir(cur)
    with open(os.path.join(cur, "leaf.txt"), "wb") as f:
        f.write(b"deep leaf content")
EOF
}

mixed() {
    local d="$1"
    python3 - "$d" <<'EOF'
import os, sys, random
base = sys.argv[1]
random.seed(42)
names = ["src", "docs", "assets", "vendor", "target", ".git", "tests", "node_modules"]
def build(path, depth, width):
    os.makedirs(path, exist_ok=True)
    for i in range(random.randint(2, width)):
        kind = random.random()
        n = os.path.join(path, f"{random.randrange(1<<26):08x}")
        if kind < 0.55 or depth >= 6:
            with open(n + random.choice([".rs", ".ts", ".md", ".png", ".json"]), "wb") as f:
                f.write(bytes(random.getrandbits(8) for _ in range(random.randint(0, 64 * 1024))))
        else:
            build(n, depth + 1, width)
for top in names:
    build(os.path.join(base, top), 0, 6)
EOF
}

sparse_hardlinks() {
    local d="$1"
    python3 - "$d" <<'EOF'
import os, sys
base = sys.argv[1]
os.makedirs(os.path.join(base, "sparse"), exist_ok=True)
os.makedirs(os.path.join(base, "linked"), exist_ok=True)
for i in range(8):
    p = os.path.join(base, "sparse", f"s{i}.bin")
    with open(p, "wb") as f:
        f.truncate(32 * 1024 * 1024)
        f.seek(16 * 1024 * 1024)
        f.write(b"\0" * 4096)
orig = os.path.join(base, "linked", "orig.dat")
with open(orig, "wb") as f:
    f.write(b"h" * (5 * 1024 * 1024))
for i in range(9):
    os.link(orig, os.path.join(base, "linked", f"link{i}.dat"))
EOF
}

mk many_small many_small
mk wide wide
mk tiny_dirs tiny_dirs
mk deep deep
mk mixed mixed
mk sparse_hardlinks sparse_hardlinks
echo "all trees ready under $BASE"
