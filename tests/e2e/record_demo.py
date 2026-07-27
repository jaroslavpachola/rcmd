#!/usr/bin/env python3
"""Record the README demo as an asciicast (v2) by driving the real rcmd
binary in a pty — the same trick as the e2e harness, plus timestamps.

Usage:  python3 tests/e2e/record_demo.py [binary] > docs/demo.cast
        agg --font-size 16 docs/demo.cast docs/demo.gif

The session runs in a throwaway sandbox; the recording is what a human
would plausibly type, with human-ish pauses.
"""
import fcntl
import json
import os
import pty
import re
import select
import shutil
import struct
import sys
import tempfile
import termios
import time

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = sys.argv[1] if len(sys.argv) > 1 else os.path.join(REPO, "target/release/rcmd")
COLS, ROWS = 100, 30

QUERY = re.compile(rb"\x1b\[0?c|\x1b\[([56])n")

MAIN_RS = """\
use std::io::{self, Write};

/// Greet whoever is watching the demo.
fn main() -> io::Result<()> {
    let name = std::env::args().nth(1).unwrap_or_else(|| "world".into());
    let mut out = io::stdout().lock();
    for i in 1..=3 {
        writeln!(out, "{i}: hello, {name}!")?;
    }
    Ok(())
}
"""


class Recorder:
    def __init__(self, cwd, home, args):
        self.events = []
        self.start = time.monotonic()
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.chdir(cwd)
            os.environ["HOME"] = home
            os.environ.pop("XDG_CONFIG_HOME", None)
            os.environ["SHELL"] = "/bin/bash"
            os.environ["TERM"] = "xterm-256color"
            os.execv(BIN, [BIN, *args])
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        self.drain(1.2)

    def drain(self, timeout):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            r, _, _ = select.select([self.fd], [], [], 0.02)
            if not r:
                continue
            try:
                chunk = os.read(self.fd, 65536)
            except OSError:
                return
            self.events.append(
                (time.monotonic() - self.start, chunk.decode("utf-8", "replace"))
            )
            for m in QUERY.finditer(chunk):  # answer terminal probes
                if m.group(1) == b"6":
                    os.write(self.fd, b"\x1b[1;1R")
                elif m.group(1) == b"5":
                    os.write(self.fd, b"\x1b[0n")
                else:
                    os.write(self.fd, b"\x1b[?6c")

    def key(self, keys, wait=0.6):
        os.write(self.fd, keys)
        self.drain(wait)

    def type(self, text, wait=0.6):
        # keystroke by keystroke, so the cast looks typed
        for ch in text.encode():
            os.write(self.fd, bytes([ch]))
            self.drain(0.05)
        self.drain(wait)

    def cast(self):
        header = {
            "version": 2,
            "width": COLS,
            "height": ROWS,
            "env": {"TERM": "xterm-256color", "SHELL": "/bin/bash"},
        }
        lines = [json.dumps(header)]
        for t, data in self.events:
            lines.append(json.dumps([round(t, 4), "o", data]))
        return "\n".join(lines) + "\n"


def main():
    root = tempfile.mkdtemp(prefix="rcmd-demo-")
    home = os.path.join(root, "home")
    play = os.path.join(root, "project")
    dest = os.path.join(root, "backups")
    os.makedirs(os.path.join(play, "src"))
    os.makedirs(home)
    os.makedirs(dest)
    open(os.path.join(play, "src", "main.rs"), "w").write(MAIN_RS)
    open(os.path.join(play, "src", "lib.rs"), "w").write("pub fn demo() {}\n")
    open(os.path.join(play, "README.md"), "w").write("# demo project\n")
    open(os.path.join(play, "Cargo.toml"), "w").write("[package]\nname = \"demo\"\n")
    open(os.path.join(play, "notes.txt"), "w").write(
        "rcmd — an orthodox file manager in Rust\n"
    )

    r = Recorder(play, home, (play, dest))
    DOWN, F3, F5, F9, F10 = b"\x1b[B", b"\x1b[13~", b"\x1b[15~", b"\x1b[20~", b"\x1b[21~"

    r.key(DOWN, 0.5)                     # browse a little
    r.key(DOWN, 0.5)
    r.key(b"\r", 0.8)                    # enter src/
    r.key(DOWN, 0.6)                     # -> main.rs (after ..)
    r.key(F3, 1.8)                       # syntax-colored viewer
    r.key(b"/", 0.4)
    r.type("hello", 0.3)
    r.key(b"\r", 1.6)                    # search hit, highlighted
    r.key(b"q", 0.7)                     # close the viewer
    r.key(b"\x7f", 0.7)                  # backspace: up to the project
    r.key(b"\x1b[2~", 0.35)              # mark Cargo.toml…
    r.key(b"\x1b[2~", 0.35)              # …and README.md (Insert advances)
    r.key(F5, 0.9)                       # copy dialog (dest prefilled)
    r.key(b"\r", 1.4)                    # copy to the right panel
    r.key(b"\x0f", 1.8)                  # Ctrl+O: the persistent subshell
    r.type("echo greetings from the subshell", 0.3)
    r.key(b"\r", 1.0)
    r.key(b"\x0f", 1.2)                  # back to the panels
    r.key(F9, 0.8)                       # the menu, briefly
    r.key(b"\x1b[C", 0.7)
    r.key(b"\x1b", 0.3)
    r.key(b"\x1b", 0.8)
    r.key(F10, 0.6)                      # quit
    try:
        os.waitpid(r.pid, 0)
    except ChildProcessError:
        pass

    sys.stdout.write(r.cast())
    shutil.rmtree(root)


if __name__ == "__main__":
    main()
