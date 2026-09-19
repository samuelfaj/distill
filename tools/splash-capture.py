#!/usr/bin/env python3
"""Run the pager on a pseudo-terminal and print what it drew.

The TUI takes over the screen, so a plain pipe shows nothing useful; this gives
the child a real tty with a known size, reads the escape stream for a few seconds,
strips the escapes and prints the visible screen. Used to check the welcome
banner (the mascot and the wordmark) after a branding change.

Usage: python3 tools/splash-capture.py [rows] [cols] [seconds]
"""

import fcntl
import os
import pty
import re
import select
import signal
import struct
import sys
import termios
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BINARY = os.path.join(REPO, "target", "debug", "xai-grok-pager")


def main() -> int:
    rows = int(sys.argv[1]) if len(sys.argv) > 1 else 40
    cols = int(sys.argv[2]) if len(sys.argv) > 2 else 130
    seconds = float(sys.argv[3]) if len(sys.argv) > 3 else 20.0

    pid, fd = pty.fork()
    if pid == 0:
        os.environ["TERM"] = "xterm-256color"
        os.environ["COLUMNS"] = str(cols)
        os.environ["LINES"] = str(rows)
        os.chdir(REPO)
        os.execv(BINARY, [BINARY])

    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    data = b""
    deadline = time.time() + seconds
    while time.time() < deadline:
        ready, _, _ = select.select([fd], [], [], 0.5)
        if fd in ready:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            data += chunk
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    os.waitpid(pid, 0)

    text = data.decode("utf-8", errors="replace")
    text = re.sub(r"\x1b\][^\x07\x1b]*(\x07|\x1b\\)", "", text)
    text = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", text)
    text = re.sub(r"\x1b[()][A-Z0-9]", "", text)
    text = re.sub(r"\x1b[=>]", "", text)
    text = text.replace("\r\n", "\n").replace("\r", "\n")
    lines = [line.rstrip() for line in text.split("\n")]
    print(f"--- {len(data)} bytes, {len(lines)} lines, pty {rows}x{cols} ---")
    print("\n".join(line for line in lines if line.strip())[-6000:])
    return 0


if __name__ == "__main__":
    sys.exit(main())
