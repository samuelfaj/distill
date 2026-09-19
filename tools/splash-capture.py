#!/usr/bin/env python3
"""Run the pager on a pseudo-terminal and print what it drew.

The TUI takes over the screen, so a plain pipe shows nothing useful; this gives
the child a real tty with a known size, reads the escape stream for a few seconds,
strips the escapes and prints the visible screen. Used to check the welcome
banner (the mascot and the wordmark) after a branding change, and to drive the
welcome menu to check that a row really opens its notice.

Usage: python3 tools/splash-capture.py [rows] [cols] [seconds] [keys]
       keys: comma-separated, e.g. `esc,down,down,down,enter`. Each key is sent
       after the screen settles, and everything drawn is printed at the end.
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

# Extra argv for the child, e.g. `SPLASH_ARGS="--effort high"` to check a CLI
# path that a bare launch never reaches.
EXTRA_ARGS = os.environ.get("SPLASH_ARGS", "").split()

# Keys the welcome screen reacts to, as the bytes a terminal sends.
KEY_BYTES = {
    "esc": b"\x1b",
    "enter": b"\r",
    "down": b"\x1b[B",
    "up": b"\x1b[A",
    "tab": b"\t",
}


def strip_screen(data: bytes) -> list[str]:
    """The escape stream reduced to the lines it painted."""
    text = data.decode("utf-8", errors="replace")
    text = re.sub(r"\x1b\][^\x07\x1b]*(\x07|\x1b\\)", "", text)
    text = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", text)
    text = re.sub(r"\x1b[()][A-Z0-9]", "", text)
    text = re.sub(r"\x1b[=>]", "", text)
    text = text.replace("\r\n", "\n").replace("\r", "\n")
    return [line.rstrip() for line in text.split("\n")]


def drain(fd: int, seconds: float) -> bytes:
    """Read whatever the child draws for `seconds`."""
    data = b""
    deadline = time.time() + seconds
    while time.time() < deadline:
        ready, _, _ = select.select([fd], [], [], 0.25)
        if fd in ready:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            data += chunk
    return data


def main() -> int:
    rows = int(sys.argv[1]) if len(sys.argv) > 1 else 40
    cols = int(sys.argv[2]) if len(sys.argv) > 2 else 130
    seconds = float(sys.argv[3]) if len(sys.argv) > 3 else 20.0
    keys = [key.strip() for key in sys.argv[4].split(",")] if len(sys.argv) > 4 else []

    pid, fd = pty.fork()
    if pid == 0:
        os.environ["TERM"] = "xterm-256color"
        os.environ["COLUMNS"] = str(cols)
        os.environ["LINES"] = str(rows)
        os.chdir(REPO)
        os.execv(BINARY, [BINARY, *EXTRA_ARGS])

    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    data = drain(fd, seconds)
    for key in keys:
        # `text=...` types literal bytes (starting a session needs a prompt),
        # `wait=N` gives the child N more seconds, anything else is a named key.
        if key.startswith("text="):
            os.write(fd, key[len("text=") :].encode())
        elif key.startswith("wait="):
            data += drain(fd, float(key[len("wait=") :]))
            continue
        else:
            os.write(fd, KEY_BYTES[key])
        data += drain(fd, 1.0)
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    os.waitpid(pid, 0)

    lines = strip_screen(data)
    print(f"--- {len(data)} bytes, {len(lines)} lines, pty {rows}x{cols} ---")
    print("\n".join(line for line in lines if line.strip())[-6000:])
    return 0


if __name__ == "__main__":
    sys.exit(main())
