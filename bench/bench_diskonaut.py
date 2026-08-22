#!/usr/bin/env python3
"""Run diskonaut against a path inside a pty and report scan wall time.

diskonaut is a pure TUI without a batch mode, so we drive it through a
pseudo-terminal: spawn it, wait until its scan-complete screen appears,
then quit. Prints the elapsed milliseconds on stdout.

Usage: bench_diskonaut.py <path>
"""

import fcntl
import os
import pty
import select
import struct
import sys
import termios
import time

DISKONAUT = os.environ.get("DISKONAUT_BIN") or os.path.expanduser(
    "~/.cargo/bin/diskonaut"
)

# Strings diskonaut only draws once scanning has finished and the main
# screen (file list + keys footer) is up.
DONE_MARKERS = ("backspace",)  # only drawn in the post-scan keys footer


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: bench_diskonaut.py <path>", file=sys.stderr)
        return 2
    target = os.path.realpath(sys.argv[1])

    pid, fd = pty.fork()
    if pid == 0:
        os.environ["TERM"] = "xterm-2560"
        os.execvp(DISKONAUT, [DISKONAUT, target])

    # Give the TUI a real window size; without it diskonaut never renders.
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))

    start = time.monotonic()
    done_at = None
    buf = b""
    deadline = start + 600.0
    while time.monotonic() < deadline:
        r, _, _ = select.select([fd], [], [], 1.0)
        if not r:
            continue
        try:
            chunk = os.read(fd, 65536)
        except OSError:
            break
        if not chunk:
            break
        buf += chunk
        text = buf.decode("utf-8", "replace").lower()
        if any(m in text for m in DONE_MARKERS):
            done_at = time.monotonic()
            break

    elapsed_ms = ((done_at or time.monotonic()) - start) * 1000.0

    try:
        os.write(fd, b"q")
        time.sleep(0.3)
        os.write(fd, b"q")
    except OSError:
        pass

    try:
        os.close(fd)
    except OSError:
        pass
    try:
        os.waitpid(pid, 0)
    except ChildProcessError:
        pass

    print(f"{elapsed_ms:.1f}")
    return 0 if done_at is not None else 1


if __name__ == "__main__":
    sys.exit(main())
