#!/usr/bin/env python3
"""End-to-end tests: drive the real rcmd binary in a pseudo-terminal.

Usage:  python3 tests/e2e/run.py [path-to-rcmd-binary]
        (defaults to target/debug/rcmd relative to the repo root)

Each test runs in a fresh tempdir with an isolated $HOME, so the user's
real config is never touched. Exits non-zero if any check fails.
"""
import fcntl
import os
import pty
import re
import select
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tarfile
import tempfile
import termios
import time

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = sys.argv[1] if len(sys.argv) > 1 else os.path.join(REPO, "target/debug/rcmd")

COLS, ROWS = 120, 30
FAILURES = []

# generous waits: CI runners are slow
STEP = float(os.environ.get("RCMD_E2E_STEP", "0.5"))

# RCMD_E2E_SUBSHELL=1 runs the whole suite with the persistent subshell
# on (commands run inside it, no "Press Enter" pause); default is off.
SUBSHELL = os.environ.get("RCMD_E2E_SUBSHELL", "0") == "1"

# Terminal queries a shell may block on (fish probes at every prompt);
# a real terminal answers these, so the harness must too.
QUERY = re.compile(rb"\x1b\[0?c|\x1b\[([56])n")

signal.alarm(900)  # hard cap for the whole suite (the scale test is slow)


class Session:
    def __init__(self, cwd, home, args=(), shell="/bin/sh", subshell=None):
        self.buf = b""
        want = SUBSHELL if subshell is None else subshell
        cfg = os.path.join(home, ".config", "rcmd", "config.toml")
        os.makedirs(os.path.dirname(cfg), exist_ok=True)
        line = "subshell = %s\n" % ("true" if want else "false")
        if os.path.exists(cfg):
            text = open(cfg).read()
            if "subshell" not in text:
                # prepend: the flag must come before any [[table]]
                open(cfg, "w").write(line + text)
        else:
            open(cfg, "w").write(line)
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.chdir(cwd)
            os.environ["HOME"] = home
            os.environ.pop("XDG_CONFIG_HOME", None)
            os.environ.pop("SSH_AUTH_SOCK", None)  # keep sftp auth deterministic
            os.environ["SHELL"] = shell
            os.environ["TERM"] = "xterm-256color"
            os.execv(BIN, [BIN, *args])
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        os.kill(self.pid, signal.SIGWINCH)
        self.drain(STEP * 2)

    def drain(self, timeout):
        end = time.time() + timeout
        while time.time() < end:
            r, _, _ = select.select([self.fd], [], [], 0.05)
            if r:
                try:
                    chunk = os.read(self.fd, 65536)
                except OSError:
                    return
                self.buf += chunk
                for m in QUERY.finditer(chunk):  # act like a real terminal
                    if m.group(1) == b"6":
                        os.write(self.fd, b"\x1b[1;1R")
                    elif m.group(1) == b"5":
                        os.write(self.fd, b"\x1b[0n")
                    else:
                        os.write(self.fd, b"\x1b[?6c")

    def send(self, keys, wait=None):
        os.write(self.fd, keys)
        self.drain(wait if wait is not None else STEP)

    def quit(self):
        self.send(b"\x1b[21~", wait=STEP * 2)  # F10
        try:
            os.waitpid(self.pid, 0)
        except ChildProcessError:
            pass
        # forkpty masters are inheritable: close, or they pile up in
        # every later child (which shifts its fd numbering)
        try:
            os.close(self.fd)
        except OSError:
            pass

    def screen(self):
        grid = [[" "] * COLS for _ in range(ROWS)]
        row = col = 0
        text = self.buf.decode("utf-8", "replace")
        i = 0
        while i < len(text):
            ch = text[i]
            if ch == "\x1b":
                m = re.match(r"\x1b\[(\d+);(\d+)H", text[i:])
                if m:
                    row, col = int(m.group(1)) - 1, int(m.group(2)) - 1
                    i += m.end()
                    continue
                m = re.match(r"\x1b\[[0-9;?]*[a-zA-Z]|\x1b[()][A-Z0-9]|\x1b.", text[i:])
                i += m.end() if m else 1
                continue
            if ch == "\r":
                col = 0
            elif ch == "\n":
                row = min(row + 1, ROWS - 1)
            elif ch >= " ":
                if 0 <= row < ROWS and 0 <= col < COLS:
                    grid[row][col] = ch
                col += 1
            i += 1
        return "\n".join("".join(r).rstrip() for r in grid)


def check(name, cond, detail=""):
    tag = "PASS" if cond else "FAIL"
    print(f"{tag} {name}" + (f"  ({detail})" if detail and not cond else ""))
    if not cond:
        FAILURES.append(name)


def sandbox():
    root = tempfile.mkdtemp(prefix="rcmd-e2e-")
    play = os.path.join(root, "play")
    home = os.path.join(root, "home")
    os.makedirs(play)
    os.makedirs(home)
    return root, play, home


# Key escape sequences
F3, F5, F7, F8 = b"\x1b[13~", b"\x1b[15~", b"\x1b[18~", b"\x1b[19~"
SF8 = b"\x1b[19;2~"
DOWN, END, HOME_K, INSERT = b"\x1b[B", b"\x1b[F", b"\x1b[H", b"\x1b[2~"


def test_smoke():
    root, play, home = sandbox()
    open(os.path.join(play, "hello.txt"), "w").write("hi\n")
    s = Session(play, home)
    scr = s.screen()
    check("smoke: panels render", "Modify time" in scr and "hello.txt" in scr)
    check("smoke: keybar present", "10Quit" in scr)
    s.quit()
    shutil.rmtree(root)


def test_fileops():
    root, play, home = sandbox()
    for name in ("a.txt", "b.txt"):
        open(os.path.join(play, name), "w").write(name + "\n")
    s = Session(play, home)
    s.send(F7)
    s.send(b"sub\r")                       # mkdir sub
    s.send(END)                            # -> b.txt
    s.send(INSERT + b"\x1b[A" + INSERT)    # mark b.txt, up, mark a.txt
    s.send(F5)
    s.send(b"sub\r", wait=STEP * 3)        # copy both into sub/
    s.send(END)                            # -> b.txt
    s.send(SF8)                            # permanent delete
    s.send(b"y", wait=STEP * 3)
    s.quit()
    check("fileops: mkdir", os.path.isdir(os.path.join(play, "sub")))
    check("fileops: copy", open(os.path.join(play, "sub/a.txt")).read() == "a.txt\n")
    check("fileops: delete", not os.path.exists(os.path.join(play, "b.txt")))
    shutil.rmtree(root)


def test_cmdline():
    root, play, home = sandbox()
    os.makedirs(os.path.join(play, "sub"))
    open(os.path.join(play, "sub", "deep-target.txt"), "w").write("x\n")
    wdfile = os.path.join(root, "lastdir")
    s = Session(play, home, args=("-P", wdfile, play))
    s.send(b"ls sub/de")
    s.send(b"\t", wait=STEP)            # Tab completes the path
    check("cmdline: tab completion", "ls sub/deep-target.txt" in s.screen())
    s.send(b"\x1b", wait=STEP)          # clear the line (Esc Esc: real Esc)
    s.send(b"\x1b", wait=STEP)
    if SUBSHELL:
        # runs inside the subshell; panels return by themselves
        s.send(b"touch made.txt\r", wait=STEP * 4)
    else:
        s.send(b"touch made.txt\r", wait=STEP * 2)   # runs command, then pause
        s.send(b"\r", wait=STEP * 2)                 # return from pause
    s.send(b"cd sub\r")
    s.quit()
    check("cmdline: command ran", os.path.isfile(os.path.join(play, "made.txt")))
    check(
        "cmdline: -P wrote last dir",
        os.path.isfile(wdfile) and open(wdfile).read() == os.path.join(play, "sub"),
    )
    shutil.rmtree(root)


def test_viewer():
    root, play, home = sandbox()
    with open(os.path.join(play, "big.txt"), "w") as f:
        for i in range(200):
            f.write(f"line {i:04}\n")
        f.write("FINDME here\n")
    open(os.path.join(play, "code.rs"), "w").write(
        'fn main() {\n    let greeting = "hello";\n    println!("{greeting}");\n}\n'
    )
    s = Session(play, home)
    s.send(DOWN)                        # -> big.txt
    s.send(F3, wait=STEP * 2)
    check("viewer: opens", "line 0000" in s.screen())
    s.send(b"/")
    s.send(b"findme\r", wait=STEP * 2)  # case-insensitive search
    check("viewer: search", "FINDME here" in s.screen())
    # the matched substring is styled on its own: a style change sits
    # between the match and the rest of the line in the raw stream
    check("viewer: match span highlighted",
          re.search(rb"FINDME(?:\x1b\[[0-9;]*m)+ here", s.buf))
    s.send(b"\x1b[14~")                 # F4 hex
    check("viewer: hex", re.search(r"00000000  .*\|line", s.screen()))
    s.send(b"\x1b[14~")                 # back to text
    s.send(b"f", wait=STEP)             # follow mode (R3): tail -f
    check("viewer: follow tag and jump to end", "[follow]" in s.screen()
          and "FINDME here" in s.screen())
    with open(os.path.join(play, "big.txt"), "a") as f:
        f.write("APPENDED tail line\n")
    check("viewer: follow picks up appends", wait_for(s, "APPENDED tail line"))
    s.send(b"f")                        # stop following
    s.send(b"q")
    # syntax highlighting: a .rs file emits RGB color runs, plain .txt
    # (checked above) never did
    s.send(b"\x13code\r", wait=STEP)    # quick search -> code.rs
    mark = len(s.buf)
    s.send(F3, wait=STEP * 2)
    check("viewer: syntax colors on .rs",
          b"[38;2;" in s.buf[mark:] and "greeting" in s.screen())
    s.send(b"q")
    s.quit()
    shutil.rmtree(root)


def test_archive():
    root, play, home = sandbox()
    os.makedirs(os.path.join(play, "out"))
    src = os.path.join(root, "src")
    os.makedirs(src)
    open(os.path.join(src, "inside.txt"), "w").write("from the archive\n")
    with tarfile.open(os.path.join(play, "b.tar.gz"), "w:gz") as t:
        t.add(os.path.join(src, "inside.txt"), arcname="inside.txt")
    s = Session(play, home, args=(play, os.path.join(play, "out")))
    s.send(DOWN + DOWN)                 # .., out, b.tar.gz -> b.tar.gz
    s.send(b"\r", wait=STEP * 2)        # enter archive
    check("archive: entered", "b.tar.gz://" in s.screen())
    s.send(F8)                          # must refuse
    check("archive: read-only", "read-only" in s.screen())
    s.send(END)                         # -> inside.txt
    s.send(F5)
    s.send(b"\r", wait=STEP * 3)        # extract to out/
    extracted = os.path.join(play, "out/inside.txt")
    check(
        "archive: extracted",
        os.path.isfile(extracted) and open(extracted).read() == "from the archive\n",
    )

    # R4: copy INTO the tar — other panel (out/) holds a new file; F5
    # from there with the tar path as destination rewrites the archive
    open(os.path.join(play, "out", "fresh.txt"), "w").write("packed later\n")
    s.send(b"\t")                       # -> right panel (out/)
    s.send(b"\x12")                     # reload to see fresh.txt
    s.send(b"\x13fresh\r", wait=STEP)   # quick search -> fresh.txt
    s.send(F5)
    s.send(b"\x15")                     # clear the prefilled destination
    s.send(os.path.join(play, "b.tar.gz").encode() + b"://\r", wait=STEP * 4)
    check("archive: packed into tar", wait_for(s, "done —"))
    s.quit()
    with tarfile.open(os.path.join(play, "b.tar.gz")) as t:
        names = t.getnames()
        packed = t.extractfile("fresh.txt").read()
    check("archive: tar holds old and new",
          "inside.txt" in names and packed == b"packed later\n")
    shutil.rmtree(root)


def test_find():
    root, play, home = sandbox()
    os.makedirs(os.path.join(play, "sub"))
    open(os.path.join(play, "needle-top.txt"), "w").write("x\n")
    open(os.path.join(play, "sub", "needle-deep.txt"), "w").write("x\n")
    open(os.path.join(play, "sub", "other.txt"), "w").write("x\n")
    # a gitignored tree: skipped by default, searched when toggled off
    gitted = bool(shutil.which("git"))
    if gitted:
        os.makedirs(os.path.join(play, "junk"))
        open(os.path.join(play, "junk", "needle-hidden.txt"), "w").write("x\n")
        open(os.path.join(play, ".gitignore"), "w").write("junk/\n")
        subprocess.run(["git", "-C", play, "init", "-q"], check=True)

    def find(keys):
        # F9 -> Command -> Find file...
        # (Help, User menu, Quick search, Hotlist, Find)
        s.send(b"\x1b[20~")                 # F9
        s.send(b"\x1b[C")                   # Right -> Command
        s.send(DOWN + DOWN + DOWN + DOWN)   # -> Find file...
        s.send(b"\r")                       # open find dialog
        s.send(b"\x15")                     # Ctrl+U clears the "*" prefill
        s.send(keys)
        s.send(b"\r", wait=STEP * 3)        # search

    s = Session(play, home)
    find(b"needle*")
    scr = s.screen()
    check("find: results panelized", "find: needle*" in scr)
    check("find: nested match with rel path", "sub/needle-deep.txt" in scr)
    check("find: match count", "2 match(es)" in scr)
    check("find: non-match absent", "other.txt" not in scr.replace("sub/other", ""))
    if gitted:
        check("find: gitignored tree skipped", "needle-hidden" not in scr)
        find(b"needle*" + b"\t\t" + b" ")   # Tab to the checkbox, untick
        scr = s.screen()
        check("find: unticked finds ignored", "junk/needle-hidden.txt" in scr)
        check("find: unticked count", "3 match(es)" in scr)
    s.quit()
    shutil.rmtree(root)


def test_compare():
    root, play, home = sandbox()
    left = os.path.join(play, "left")
    right = os.path.join(play, "right")
    os.makedirs(left)
    os.makedirs(right)
    for d in (left, right):
        open(os.path.join(d, "same.txt"), "w").write("identical\n")
        os.utime(os.path.join(d, "same.txt"), (1_700_000_000, 1_700_000_000))
    open(os.path.join(left, "only-left.txt"), "w").write("l\n")
    open(os.path.join(left, "differs.txt"), "w").write("short\n")
    open(os.path.join(right, "differs.txt"), "w").write("much longer content\n")
    s = Session(play, home, args=(left, right))
    s.send(b"\x18")                     # Ctrl+X
    s.send(b"d", wait=STEP * 2)         # compare
    scr = s.screen()
    check("compare: difference count", "2 difference(s) marked" in scr)
    check("compare: marked summary shown", "file(s)" in scr)
    s.quit()
    shutil.rmtree(root)


def test_watch():
    root, play, home = sandbox()
    open(os.path.join(play, "first.txt"), "w").write("x\n")
    s = Session(play, home)
    check("watch: baseline renders", "first.txt" in s.screen())
    open(os.path.join(play, "appeared.txt"), "w").write("y\n")
    deadline = time.time() + 8
    while time.time() < deadline and "appeared.txt" not in s.screen():
        s.drain(0.5)
    check("watch: external create auto-reloads", "appeared.txt" in s.screen())
    s.quit()
    shutil.rmtree(root)


def test_dirsize():
    root, play, home = sandbox()
    os.makedirs(os.path.join(play, "sub"))
    open(os.path.join(play, "sub", "a"), "w").write("12345")
    open(os.path.join(play, "sub", "b"), "w").write("1234567")
    s = Session(play, home)
    s.send(DOWN)                        # -> sub
    s.send(b"\x00", wait=STEP * 2)      # Ctrl+Space
    deadline = time.time() + 8
    while time.time() < deadline and "12 bytes in 2 file(s)" not in s.screen():
        s.drain(0.5)
    check("dirsize: recursive size reported", "12 bytes in 2 file(s)" in s.screen())
    s.quit()
    shutil.rmtree(root)


ALT_LEFT, ALT_RIGHT, ALT_UP = b"\x1b[1;3D", b"\x1b[1;3C", b"\x1b[1;3A"


def click(col, row):
    """SGR left-button press+release at 1-based (col, row)."""
    return b"\x1b[<0;%d;%dM\x1b[<0;%d;%dm" % (col, row, col, row)


def wheel(col, row, down=True):
    return b"\x1b[<%d;%d;%dM" % (65 if down else 64, col, row)


def test_history():
    root, play, home = sandbox()
    os.makedirs(os.path.join(play, "one"))
    os.makedirs(os.path.join(play, "two"))
    s = Session(play, home)
    s.send(b"cd one\r")
    s.send(b"cd ../two\r")
    check("history: cd landed", play + "/two" in s.screen())
    s.send(ALT_LEFT)
    check("history: back", play + "/one" in s.screen())
    s.send(ALT_LEFT)
    scr = s.screen()
    check(
        "history: back to start",
        play + "/one" not in scr and play + "/two" not in scr,
    )
    s.send(ALT_RIGHT)
    check("history: forward", play + "/one" in s.screen())
    s.send(ALT_UP)
    check("history: alt+up opens hotlist", "Directory hotlist" in s.screen())
    # recent directories (R3): visited dirs listed below the pinned ones
    check("history: hotlist lists recent dirs", "Recent:" in s.screen()
          and play + "/two" in s.screen())
    s.send(b"\r", wait=STEP)           # first recent row = most recent: two
    check("history: recent entry cds", play + "/two" in s.screen()
          and "Directory hotlist" not in s.screen())
    s.quit()
    shutil.rmtree(root)


def test_quickview():
    root, play, home = sandbox()
    open(os.path.join(play, "poem.txt"), "w").write("roses are red\nviolets too\n")
    open(os.path.join(play, "prose.txt"), "w").write("second file content\n")
    s = Session(play, home)
    s.send(b"\x18")                     # Ctrl+X ...
    s.send(b"q")                        # ... Q -> quick view
    s.send(DOWN)                        # -> poem.txt
    scr = s.screen()
    check(
        "quickview: preview follows cursor",
        "Quick view" in scr and "roses are red" in scr,
    )
    s.send(DOWN)                        # -> prose.txt
    check("quickview: switches file", "second file content" in s.screen())
    s.send(b"\t")                       # focus the preview
    s.send(F4, wait=STEP)               # R4: hex mode
    check("quickview: hex dump",           # "se" of "second" in hex
          "00000000  73 65" in s.screen())
    s.send(F4, wait=STEP)               # back to text
    check("quickview: hex off", "00000000" not in s.screen())
    s.send(b"\t")                       # focus back to the listing
    s.send(b"\x18q")                    # toggle off
    check("quickview: toggles off", "Quick view" not in s.screen())
    s.quit()
    shutil.rmtree(root)


def test_mouse():
    root, play, home = sandbox()
    os.makedirs(os.path.join(play, "subdir"))
    open(os.path.join(play, "subdir", "inner.txt"), "w").write("x\n")
    open(os.path.join(play, "file1.txt"), "w").write("alpha\nbeta\n")
    s = Session(play, home, args=(play, os.path.join(play, "subdir")))

    # wheel scrolls the hovered (left) panel's cursor: .. -> file1.txt
    s.send(wheel(10, 4))
    check("mouse: wheel moves cursor", "file1.txt" in status_line(s))
    # click focuses the right panel and puts the cursor on the row
    s.send(click(62, 4))
    check("mouse: click focuses+selects", "inner.txt" in status_line(s))
    # keybar: the "9 PullDn" button opens the menu
    s.send(click(66, 30))
    check("mouse: keybar opens menu", "Make directory..." in s.screen())
    s.send(click(21, 1))                # menu bar: switch to Sort
    check("mouse: menu bar switches", "By modify time" in s.screen())
    s.send(click(100, 15))              # outside: closes the menu
    check("mouse: click outside closes menu", "By modify time" not in s.screen())
    # editor: a click places the cursor (line 2, past the end of "beta")
    s.send(click(10, 5))                # focus left panel, cursor file1.txt
    s.send(F4, wait=STEP * 2)
    s.send(click(6, 3))
    check("mouse: editor click sets cursor", "2:5" in s.screen())
    s.send(F10, wait=STEP * 2)
    # double-click enters a directory
    s.send(click(10, 4) + click(10, 4))
    check("mouse: double-click enters", play + "/subdir" in s.screen())
    s.quit()
    shutil.rmtree(root)


def test_sortclick():
    """R4: clicking a column header sorts by it, again reverses."""
    root, play, home = sandbox()
    open(os.path.join(play, "aaa.txt"), "w").write("x" * 4000)
    open(os.path.join(play, "zzz.txt"), "w").write("tiny\n")
    s = Session(play, home)

    def row(n):
        return s.screen().split("\n")[n]

    check("sortclick: name order first", "aaa.txt" in row(3) and "zzz.txt" in row(4))
    s.send(click(42, 2), wait=STEP)     # Size header (left panel)
    check("sortclick: size ascending", "zzz.txt" in row(3))
    s.send(click(42, 2), wait=STEP)     # same header again: reverse
    check("sortclick: size reversed", "aaa.txt" in row(3))
    s.send(click(10, 2), wait=STEP)     # Name header restores name order
    check("sortclick: back to name", "aaa.txt" in row(3) and "zzz.txt" in row(4))
    s.quit()
    shutil.rmtree(root)


def status_line(s):
    return s.screen().split("\n")[-3]


def header_line(s):
    """The panel column-header row (borders row 0, header row 1)."""
    return s.screen().split("\n")[1]


def test_mcdepth():
    import getpass

    root, play, home = sandbox()
    os.makedirs(os.path.join(play, "docs"))
    open(os.path.join(play, "notes.txt"), "w").write("hello\n")
    s = Session(play, home)
    check("mcdepth: free space in footer", " free " in s.screen())

    # info pane on the other panel follows the cursor
    s.send(b"\x18")                    # Ctrl+X ...
    s.send(b"i")                       # ... i -> info pane
    s.send(END)                        # cursor -> notes.txt
    scr = s.screen()
    check("mcdepth: info pane opens", "Inode:" in scr and "regular file" in scr)
    check("mcdepth: info owner resolved", getpass.getuser() in scr)
    s.send(b"\x18i")                   # off

    # long listing via F9 -> View
    s.send(b"\x1b[20~")                # F9
    s.send(b"\x1b[C\x1b[C\x1b[C")      # File -> Command -> Sort -> View
    s.send(DOWN + DOWN + b"\r")        # Brief, Full, *Long*
    check("mcdepth: long listing headers", "Owner" in header_line(s)[:60])
    check("mcdepth: long listing owner", getpass.getuser() in s.screen())

    # active long panel = MC's one-panel view: full width, other hidden
    check("mcdepth: long is one-panel", "Modify time" not in header_line(s))
    s.send(b"\t")                      # Tab to the non-long panel: split is
    hdr = header_line(s)               # back, the long one squeezed but seen
    check("mcdepth: one-panel per-focus", "Owner" in hdr[:60] and "Modify time" in hdr)
    s.send(b"\t")                      # back: one-panel view again
    check("mcdepth: one-panel returns", "Modify time" not in header_line(s))

    # brief listing hides everything but names (left panel only)
    s.send(b"\x1b[20~")
    s.send(b"\x1b[C\x1b[C\x1b[C")
    s.send(b"\r")                      # *Brief*
    hdr = header_line(s)
    check("mcdepth: brief listing", "Size" not in hdr[:60] and "Size" in hdr[60:])

    # Alt+T cycles the format like MC (brief -> full)
    s.send(b"\x1bt")
    check("mcdepth: alt+t cycles listing", "Size" in header_line(s)[:60])

    # Alt+o opens the dir under the cursor in the other panel; Alt+i syncs
    s.send(HOME_K + DOWN)              # cursor -> docs/
    s.send(b"\x1bo")
    check("mcdepth: alt+o", play + "/docs" in s.screen())
    # Ctrl+U swaps the panels (MC), and again swaps back
    s.send(b"\x15")
    check("mcdepth: ctrl+u swaps", "/docs" in s.screen().split("\n")[0][:60])
    s.send(b"\x15")
    s.send(b"\x1bi")
    check("mcdepth: alt+i", play + "/docs" not in s.screen())

    # panel options form, reached via MC-style menu hotkey letters:
    # F9, then "o" (Options title), then "p" (Panel options...)
    s.send(b"\x1b[20~")
    s.send(b"o")
    s.send(b"p")
    check("mcdepth: options form opens", "Lynx-like motion" in s.screen())
    s.send(DOWN)                       # -> lynx row
    s.send(b" ")                       # check it
    check("mcdepth: form checkbox", "[x] Lynx-like motion" in s.screen())
    s.send(b"\r")                      # OK applies live
    time.sleep(0.5)                    # ... and writes through to disk
    cfg_path = os.path.join(home, ".config", "rcmd", "config.toml")
    check("mcdepth: options write through", "lynx = true" in open(cfg_path).read())
    s2 = Session(play, home)           # second instance, soon-stale memory
    s.send(HOME_K + DOWN)              # cursor -> docs/
    s.send(b"\x1b[C")                  # lynx Right enters it
    check("mcdepth: lynx right enters", "/docs" in s.screen().split("\n")[0][:60])
    s.send(b"\x1b[D")                  # lynx Left goes to the parent
    check("mcdepth: lynx left up", "/docs" not in s.screen().split("\n")[0][:60])
    s.send(b"\x1b[20~")
    s.send(b"o")
    s.send(b"p")
    s.send(DOWN + b" " + b"\r")        # uncheck, OK
    s.send(HOME_K + DOWN)
    s.send(b"\x1b[C")                  # Right is a no-op once more
    check("mcdepth: lynx toggles off", "/docs" not in s.screen().split("\n")[0][:60])
    # the second instance never saw the toggles; its exit must save only
    # panel state, not clobber the options another instance applied
    s2.quit()
    check("mcdepth: exit does not clobber", "lynx = false" in open(cfg_path).read())
    s.quit()
    shutil.rmtree(root)


def test_escmeta():
    root, play, home = sandbox()
    open(os.path.join(play, "read.me"), "w").write("esc meta works\n")
    s = Session(play, home)
    # Esc then 9 (separate writes, so crossterm sees a lone Esc) = F9
    s.send(b"\x1b")
    s.send(b"9")
    check("escmeta: Esc 9 opens the menu", "Make directory..." in s.screen())
    # Esc Esc = a real Escape: closes the menu
    s.send(b"\x1b")
    s.send(b"\x1b")
    check("escmeta: Esc Esc escapes", "Make directory..." not in s.screen())
    # Esc 3 on a file = F3 viewer
    s.send(DOWN)                       # cursor -> read.me
    s.send(b"\x1b")
    s.send(b"3")
    check("escmeta: Esc 3 views", "esc meta works" in s.screen())
    s.send(b"q")
    # Esc t = Alt+T (cycle listing: full -> long)
    s.send(b"\x1b")
    s.send(b"t")
    check("escmeta: Esc t is Alt+T", "Owner" in header_line(s)[:60])
    s.quit()
    shutil.rmtree(root)


def test_aliases():
    """R3 MC alias batch: M-c, M-y, S-F4, S-F5, C-x t/p, C-l."""
    root, play, home = sandbox()
    os.makedirs(os.path.join(play, "sub"))
    open(os.path.join(play, "aaa-first.txt"), "w").write("x\n")
    s = Session(play, home)

    s.send(b"\x1bc", wait=STEP)             # M-c
    check("aliases: M-c opens quick cd", "Quick cd" in s.screen())
    s.send(b"sub\r", wait=STEP)
    check("aliases: quick cd lands", play + "/sub" in s.screen())
    s.send(b"\x1by", wait=STEP)             # M-y = history back
    check("aliases: M-y goes back", play + "/sub" not in s.screen())

    s.send(b"\x1b[14;2~", wait=STEP)        # Shift+F4
    check("aliases: S-F4 prompts for a name", "Edit new file" in s.screen())
    s.send(b"fresh.txt\r", wait=STEP)
    s.send(b"hello")
    s.send(F2, wait=STEP)                   # save
    s.send(F10, wait=STEP * 2)              # close the editor
    fresh = os.path.join(play, "fresh.txt")
    check("aliases: S-F4 created the file",
          os.path.isfile(fresh) and open(fresh).read() == "hello")

    s.send(END)                             # -> fresh.txt (last entry)
    s.send(b"\x1b[15;2~", wait=STEP)        # Shift+F5
    check("aliases: S-F5 in-place dialog", "in place" in s.screen())
    s.send(b"\x15")                         # clear the prefilled name
    s.send(b"fresh-copy.txt\r", wait=STEP * 3)
    copy = os.path.join(play, "fresh-copy.txt")
    check("aliases: in-place copy", os.path.isfile(copy)
          and open(copy).read() == "hello")

    s.send(END)                             # -> fresh.txt again
    s.send(b"echo ")
    s.send(b"\x18t", wait=STEP)             # C-x t: tagged names
    check("aliases: C-x t pastes the name", "echo fresh.txt" in s.screen())
    s.send(b"\x18p", wait=STEP)             # C-x p: panel path
    check("aliases: C-x p pastes the path",
          "echo fresh.txt " + play in s.screen())
    s.send(b"\x0c", wait=STEP)              # C-l repaint
    check("aliases: C-l keeps the screen", "echo fresh.txt" in s.screen())
    s.quit()
    shutil.rmtree(root)


def test_cxops():
    """R4: C-x c chmod, C-x o chown, C-x s symlink."""
    import pwd
    root, play, home = sandbox()
    open(os.path.join(play, "target.txt"), "w").write("x\n")
    s = Session(play, home)
    s.send(END)                             # -> target.txt

    s.send(b"\x18c", wait=STEP)             # C-x c
    check("cxops: chmod dialog", "Chmod" in s.screen())
    s.send(b"\x15")                         # clear the prefilled mode
    s.send(b"600\r", wait=STEP * 2)
    mode = os.stat(os.path.join(play, "target.txt")).st_mode & 0o777
    check("cxops: chmod applied", mode == 0o600, f"mode {oct(mode)}")

    s.send(b"\x18o", wait=STEP)             # C-x o
    check("cxops: chown dialog", "Chown" in s.screen())
    me = pwd.getpwuid(os.getuid()).pw_name
    s.send(me.encode() + b"\r", wait=STEP * 2)   # chown to self: allowed
    check("cxops: chown self ok", "chown: 1 item(s)" in s.screen())

    s.send(b"\x18s", wait=STEP)             # C-x s
    check("cxops: symlink dialog", "Symlink" in s.screen())
    s.send(b"\r", wait=STEP * 2)            # accept "target.txt-link"
    link = os.path.join(play, "target.txt-link")
    check("cxops: symlink created",
          os.path.islink(link) and os.readlink(link) == "target.txt")
    s.quit()
    shutil.rmtree(root)


def test_jobs():
    """R4 job queue: background a job, list it, foreground it, finish.
    The copy source is a FIFO, so the job blocks deterministically
    until the test opens the writing end."""
    root, play, home = sandbox()
    dest = os.path.join(play, "dest")
    os.makedirs(dest)
    os.mkfifo(os.path.join(play, "pipe.dat"))
    s = Session(play, home, args=(play, dest))
    s.send(b"\x13pipe\r", wait=STEP)        # quick search -> pipe.dat
    s.send(F5)
    s.send(b"\r", wait=STEP * 2)            # copy to dest/ — blocks on the fifo
    check("jobs: progress dialog", "copy 1 item" in s.screen())
    s.send(b"b", wait=STEP)                 # detach
    check("jobs: status shows background job",
          "1 job(s) running" in s.screen())
    s.send(b"\x18j", wait=STEP)             # C-x j
    scr = s.screen()
    check("jobs: list shows it", "Jobs" in scr and "copy 1 item" in scr)
    s.send(b"\r", wait=STEP)                # Enter: foreground again
    check("jobs: foregrounded", "b — background" in s.screen())
    s.send(b"b", wait=STEP)                 # detach again
    s.send(F10, wait=STEP)                  # quit must refuse
    check("jobs: quit refused while running", "still running" in s.screen())
    fd = os.open(os.path.join(play, "pipe.dat"), os.O_WRONLY)
    os.write(fd, b"data!")
    os.close(fd)                            # EOF -> the copy completes
    check("jobs: finishes in background", wait_for(s, "done —"))
    copied = os.path.join(dest, "pipe.dat")
    check("jobs: payload arrived",
          os.path.isfile(copied) and open(copied, "rb").read() == b"data!")
    s.quit()
    shutil.rmtree(root)


def test_bulk_rename():
    """R3: bulk rename — edit names in the editor, preview, apply."""
    root, play, home = sandbox()
    for name in ("aaa.txt", "bbb.txt", "ccc.txt"):
        open(os.path.join(play, name), "w").write(name + "\n")
    s = Session(play, home)
    s.send(b"+")                            # select group dialog
    s.send(b"\r", wait=STEP)                # "*" marks all files
    s.send(b"\x1b[20~")                     # F9 (File menu opens)
    s.send(b"b", wait=STEP)                 # Bulk rename (entry hotkey)
    check("bulk: editor opens with numbered names",
          "bulk rename" in s.screen() and "aaa.txt" in s.screen())
    s.send(END)                             # end of line 1: "0<TAB>aaa.txt"
    s.send(b"\x7f" * 7)                     # backspace away "aaa.txt"
    s.send(b"zzz.txt")
    s.send(DOWN + HOME_K)                   # line 2
    s.send(F8, wait=STEP)                   # editor: delete line (bbb.txt)
    s.send(F2, wait=STEP)                   # save
    s.send(F10, wait=STEP * 2)              # close -> preview
    scr = s.screen()
    check("bulk: preview lists the rename",
          "aaa.txt" in scr and "zzz.txt" in scr and "1 rename(s)" in scr)
    check("bulk: preview lists the delete", "delete bbb.txt" in scr)
    check("bulk: nothing happened yet",
          os.path.isfile(os.path.join(play, "aaa.txt")))
    s.send(b"y", wait=STEP * 3)
    deadline = time.time() + 8
    while time.time() < deadline and os.path.exists(os.path.join(play, "bbb.txt")):
        s.drain(0.3)
    check("bulk: rename applied",
          open(os.path.join(play, "zzz.txt")).read() == "aaa.txt\n"
          and not os.path.exists(os.path.join(play, "aaa.txt")))
    check("bulk: delete applied", not os.path.exists(os.path.join(play, "bbb.txt")))
    check("bulk: untouched file kept",
          open(os.path.join(play, "ccc.txt")).read() == "ccc.txt\n")
    s.quit()
    shutil.rmtree(root)


def test_extensibility():
    root, play, home = sandbox()
    cfgdir = os.path.join(home, ".config", "rcmd")
    os.makedirs(cfgdir)
    open(os.path.join(cfgdir, "config.toml"), "w").write(
        '[[open]]\n'
        'match = "*.txt"\n'
        'run = "cp %f opened_copy"\n'
        '\n'
        '[[commands]]\n'
        'name = "write marker"\n'
        'run = "echo hello-%d > marker.out"\n'
        '\n'
        '[[commands]]\n'
        'name = "list tagged"\n'
        'run = "echo %t > tagged.out"\n'
        'key = "ctrl+g"\n'
    )
    open(os.path.join(play, "notes.txt"), "w").write("data\n")
    s = Session(play, home)

    # Enter on a matching file runs the opener (quietly, no pause)
    s.send(DOWN)                       # cursor -> notes.txt
    s.send(b"\r", wait=STEP * 3)
    copy = os.path.join(play, "opened_copy")
    check(
        "extensibility: opener ran on Enter",
        os.path.isfile(copy) and open(copy).read() == "data\n",
    )

    # F2 user menu lists commands and runs the selection
    s.send(b"\x1b[12~")                # F2
    scr = s.screen()
    check("extensibility: user menu", "User menu" in scr and "write marker" in scr)
    s.send(b"\r", wait=STEP * 3)       # run "write marker" (pauses)
    if not SUBSHELL:
        s.send(b"\r", wait=STEP * 2)   # return from the pause
    marker = os.path.join(play, "marker.out")
    check(
        "extensibility: %d expanded",
        os.path.isfile(marker) and play in open(marker).read(),
    )

    # a command bound via key = "ctrl+g" sees %t (marked files);
    # listing by now: .., marker.out, notes.txt, opened_copy
    s.send(HOME_K + DOWN + DOWN + INSERT)  # mark notes.txt
    s.send(b"\x07", wait=STEP * 3)     # Ctrl+G
    if not SUBSHELL:
        s.send(b"\r", wait=STEP * 2)
    tagged = os.path.join(play, "tagged.out")
    check(
        "extensibility: %t + key binding",
        os.path.isfile(tagged) and "notes.txt" in open(tagged).read(),
    )
    s.quit()
    shutil.rmtree(root)


def test_git():
    if not shutil.which("git"):
        print("SKIP git: no git binary")
        return
    root, play, home = sandbox()

    def git(*args):
        subprocess.run(
            ["git", "-C", play, "-c", "user.email=t@e2e", "-c", "user.name=t", *args],
            capture_output=True,
            check=True,
        )

    git("init", "-b", "main", ".")
    open(os.path.join(play, "tracked.txt"), "w").write("one\n")
    open(os.path.join(play, ".gitignore"), "w").write("*.log\n")
    git("add", ".")
    git("commit", "-m", "init")
    open(os.path.join(play, "tracked.txt"), "w").write("two\n")   # M
    open(os.path.join(play, "fresh.txt"), "w").write("x\n")       # ?
    open(os.path.join(play, "build.log"), "w").write("x\n")       # !

    s = Session(play, home)
    connected = wait_for(s, "[main]", timeout=10)
    scr = s.screen()
    check("git: branch in the title", connected)
    check("git: modified mark", "M tracked.txt" in scr)
    check("git: untracked mark", "? fresh.txt" in scr)
    check("git: ignored mark", "! build.log" in scr)
    check("git: clean file unmarked", "M .gitignore" not in scr)
    s.quit()
    shutil.rmtree(root)


F2, F4, F6, F10 = b"\x1b[12~", b"\x1b[14~", b"\x1b[17~", b"\x1b[21~"


def test_editor():
    root, play, home = sandbox()
    path = os.path.join(play, "notes.txt")
    open(path, "w").write("alpha\nbeta\n")
    s = Session(play, home)
    s.send(DOWN)                        # -> notes.txt
    s.send(F4, wait=STEP * 2)           # internal editor
    scr = s.screen()
    check("editor: opens with content", "alpha" in scr and "notes.txt" in scr)
    s.send(b"\x1b[1;5F")                # Ctrl+End -> end of buffer
    s.send(b"gamma")
    check("editor: modified flag", "[+]" in s.screen())
    s.send(F2, wait=STEP * 2)           # save
    check("editor: saved note", "saved" in s.screen())
    s.send(F10, wait=STEP * 2)          # quit (no confirm — just saved)
    check("editor: file written", open(path).read() == "alpha\nbeta\ngamma")

    # replace all via F4-in-editor, then quit-confirm discard path
    s.send(F4, wait=STEP * 2)
    s.send(F4)                          # replace prompt
    s.send(b"beta\r")                   # pattern
    s.send(b"BETA\r")                   # replacement -> confirm dialog
    check("editor: replace asks", "Replace?" in s.screen())
    s.send(b"a", wait=STEP)             # All
    check("editor: replaced note", "1 replaced" in s.screen())
    s.send(F2)                          # save the replacement
    s.send(F10, wait=STEP * 2)
    check("editor: replace-all wrote", "BETA" in open(path).read())

    # R4: $1 capture groups in the replacement
    s.send(F4, wait=STEP * 2)
    s.send(F4)                          # replace prompt
    s.send(b"(BET)(A)\r")               # pattern with two groups
    s.send(b"$2-$1\r")                  # replacement using both
    s.send(b"a", wait=STEP)             # All
    s.send(F2)                          # save
    s.send(F10, wait=STEP * 2)
    check("editor: capture groups", "A-BET" in open(path).read())

    # R4: F5/F6 block ops — duplicate the first line, then cut one copy
    s.send(F4, wait=STEP * 2)
    s.send(F5)                          # no selection: duplicate line 1
    s.send(F6)                          # cut the duplicate (clipboard)
    s.send(b"\x16")                     # Ctrl+V pastes it back
    s.send(F2)
    s.send(F10, wait=STEP * 2)
    check("editor: F5 duplicated the line",
          open(path).read().count("alpha") == 2)

    # R4: Alt+W soft-wrap — the tail of a long line becomes visible
    long_path = os.path.join(play, "wide.txt")
    open(long_path, "w").write("HEAD" + "x" * 150 + "WRAPTAIL\n")
    s.send(b"\x12", wait=STEP)          # Ctrl+R reload the panel
    s.send(b"\x1bs")                    # quick search...
    s.send(b"wide\r", wait=STEP)        # ...to wide.txt
    s.send(F4, wait=STEP * 2)
    check("editor: long line clipped", "WRAPTAIL" not in s.screen())
    s.send(b"\x1bw", wait=STEP)         # Alt+W wrap on
    check("editor: soft-wrap shows the tail", "WRAPTAIL" in s.screen())
    s.send(b"\x1bw", wait=STEP)         # wrap off again
    check("editor: wrap toggles back", "WRAPTAIL" not in s.screen())
    s.send(F10, wait=STEP * 2)

    s.send(b"\x1bs")                    # back to notes.txt
    s.send(b"notes\r", wait=STEP)
    s.send(F4, wait=STEP * 2)           # reopen
    s.send(b"junk")                     # modify
    s.send(F10)                         # quit -> unsaved-changes dialog
    check("editor: quit confirms", "Unsaved changes" in s.screen())
    s.send(b"d", wait=STEP * 2)         # discard
    check("editor: discard kept file", "junk" not in open(path).read())
    s.quit()
    shutil.rmtree(root)


def wait_for(s, needle, timeout=10):
    deadline = time.time() + timeout
    while time.time() < deadline and needle not in s.screen():
        s.drain(0.3)
    return needle in s.screen()


def sftp_python():
    """An interpreter that can run the paramiko test server, if any."""
    for py in ("python3.12", "python3.11", "python3", sys.executable):
        path = shutil.which(py)
        if path and subprocess.run(
            [path, "-c", "import paramiko"], capture_output=True
        ).returncode == 0:
            return path
    return None


def test_sftp():
    if os.environ.get("RCMD_E2E_SFTP") == "0":
        print("SKIP sftp (RCMD_E2E_SFTP=0)")
        return
    py = sftp_python()
    if py is None:
        print("SKIP sftp (no python with paramiko — pip install paramiko)")
        return
    root, play, home = sandbox()
    remote = os.path.join(root, "remote")
    os.makedirs(remote)
    open(os.path.join(remote, "server.txt"), "w").write("from the server\n")
    open(os.path.join(play, "upload.txt"), "w").write("to the server\n")

    probe = socket.socket()
    probe.bind(("127.0.0.1", 0))
    port = probe.getsockname()[1]
    probe.close()
    server = subprocess.Popen(
        [py, os.path.join(os.path.dirname(os.path.abspath(__file__)), "sftp_server.py"),
         str(port)],
        env={**os.environ, "RCMD_SFTP_PASSWORD": "secret"},
        stdout=subprocess.PIPE,
    )
    try:
        assert server.stdout.readline().strip() == b"READY", "sftp server failed to start"

        s = Session(play, home)
        s.send(f"cd sftp://tester@127.0.0.1:{port}{remote}\r".encode(), wait=STEP * 2)
        check("sftp: host key dialog", wait_for(s, "Unknown host"))
        s.send(b"y")                        # trust & save
        check("sftp: password prompt", wait_for(s, "SSH authentication"))
        s.send(b"secret\r", wait=STEP * 2)
        connected = wait_for(s, "server.txt", timeout=15)
        check("sftp: connected and listed", connected and "sftp://tester@" in s.screen())

        s.send(END)                         # -> server.txt
        s.send(F5)
        s.send(b"\r", wait=STEP * 4)        # download into local panel dir
        downloaded = os.path.join(play, "server.txt")
        check(
            "sftp: download via F5",
            wait_for(s, "done —")
            and os.path.isfile(downloaded)
            and open(downloaded).read() == "from the server\n",
        )

        s.send(b"\t")                       # -> local panel
        s.send(END)                         # -> upload.txt
        s.send(F5)                          # dest prefilled with the sftp URL
        s.send(b"\r", wait=STEP * 4)
        uploaded = os.path.join(remote, "upload.txt")
        check(
            "sftp: upload via F5",
            wait_for(s, "done —")
            and os.path.isfile(uploaded)
            and open(uploaded).read() == "to the server\n",
        )

        s.send(b"\t")                       # -> remote panel
        s.send(HOME_K + DOWN)               # .. -> server.txt
        s.send(F4, wait=STEP * 2)           # edit a scratch copy internally
        check("sftp: remote edit opens", "from the server" in s.screen())
        s.send(b"X")                        # prepend a byte
        s.send(F2, wait=STEP)               # save the scratch copy
        s.send(F10, wait=STEP * 3)          # close -> upload back
        deadline = time.time() + 8
        remote_file = os.path.join(remote, "server.txt")
        while time.time() < deadline and open(remote_file).read() != "Xfrom the server\n":
            s.drain(0.3)
        check("sftp: edit uploaded back", open(remote_file).read() == "Xfrom the server\n")

        s.send(F7)
        s.send(b"made-remotely\r", wait=STEP * 3)
        check("sftp: remote mkdir", os.path.isdir(os.path.join(remote, "made-remotely")))

        s.send(END)                         # -> upload.txt on the server
        s.send(F8)
        check("sftp: delete asks server-side", wait_for(s, "from the server?"))
        s.send(b"y", wait=STEP * 4)
        deadline = time.time() + 8
        while time.time() < deadline and os.path.exists(uploaded):
            s.drain(0.3)
        check("sftp: remote delete", not os.path.exists(uploaded))

        # R4: Ctrl+Space directory size over sftp (dir made server-side
        # only now, so the earlier position-coupled steps stay put)
        os.makedirs(os.path.join(remote, "deep"))
        open(os.path.join(remote, "deep", "one.bin"), "w").write("x" * 10)
        open(os.path.join(remote, "deep", "two.bin"), "w").write("y" * 6)
        s.send(b"\x12", wait=STEP)          # Ctrl+R reload the listing
        s.send(b"\x13deep\r", wait=STEP)    # quick search -> deep/
        s.send(b"\x00", wait=STEP)          # Ctrl+Space
        check("sftp: remote dir size", wait_for(s, "deep: 16 bytes in 2 file(s)"))

        khfile = os.path.join(home, ".ssh", "known_hosts")
        check(
            "sftp: host key saved",
            os.path.isfile(khfile) and "127.0.0.1" in open(khfile).read(),
        )
        s.quit()
    finally:
        server.terminate()
        server.wait()
    shutil.rmtree(root)


def test_sftp_auth():
    """R2: passphrase-protected key + keyboard-interactive auth."""
    if os.environ.get("RCMD_E2E_SFTP") == "0":
        print("SKIP sftp-auth (RCMD_E2E_SFTP=0)")
        return
    py = sftp_python()
    if py is None:
        print("SKIP sftp-auth (no python with paramiko — pip install paramiko)")
        return
    keygen = shutil.which("ssh-keygen")
    if keygen is None:
        print("SKIP sftp-auth (no ssh-keygen)")
        return
    root, play, home = sandbox()
    remote = os.path.join(root, "remote")
    os.makedirs(remote)
    open(os.path.join(remote, "server.txt"), "w").write("from the server\n")

    # an encrypted PEM key in the sandbox home — rcmd must ask for its
    # passphrase instead of falling through to password auth
    sshdir = os.path.join(home, ".ssh")
    os.makedirs(sshdir)
    key = os.path.join(sshdir, "id_ecdsa")
    subprocess.run(
        [keygen, "-q", "-t", "ecdsa", "-m", "PEM", "-N", "opensesame", "-f", key],
        check=True,
    )

    def serve(auth, extra_env=None):
        probe = socket.socket()
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
        probe.close()
        server = subprocess.Popen(
            [py, os.path.join(os.path.dirname(os.path.abspath(__file__)),
                              "sftp_server.py"), str(port)],
            env={**os.environ, "RCMD_SFTP_AUTH": auth, **(extra_env or {})},
            stdout=subprocess.PIPE,
        )
        assert server.stdout.readline().strip() == b"READY", "sftp server failed to start"
        return server, port

    server, port = serve("pubkey", {"RCMD_SFTP_PUBKEY": key + ".pub"})
    try:
        s = Session(play, home)
        s.send(f"cd sftp://tester@127.0.0.1:{port}{remote}\r".encode(), wait=STEP * 2)
        check("sftp auth: host key dialog", wait_for(s, "Unknown host"))
        s.send(b"y")
        check("sftp auth: passphrase prompt", wait_for(s, "passphrase for"))
        s.send(b"wrong\r", wait=STEP * 2)    # rejected — the prompt returns
        s.send(b"opensesame\r")
        check("sftp auth: key connected after retry",
              wait_for(s, "server.txt", timeout=15))
        s.quit()
    finally:
        server.terminate()
        server.wait()

    server, port = serve("interactive")
    try:
        s = Session(play, home)
        s.send(f"cd sftp://tester@127.0.0.1:{port}{remote}\r".encode(), wait=STEP * 2)
        check("sftp auth: interactive host key", wait_for(s, "Unknown host"))
        s.send(b"y")
        # server sends two prompts in one round; each gets its own dialog
        # (and no passphrase prompt — publickey is not on offer)
        check("sftp auth: first challenge", wait_for(s, "Word one:"))
        check("sftp auth: no passphrase detour", "passphrase" not in s.screen())
        s.send(b"fish\r")
        check("sftp auth: second challenge", wait_for(s, "Word two:"))
        s.send(b"chips\r")
        check("sftp auth: interactive connected",
              wait_for(s, "server.txt", timeout=15))
        s.quit()
    finally:
        server.terminate()
        server.wait()
    shutil.rmtree(root)


def test_scale():
    if os.environ.get("RCMD_E2E_SCALE") == "0":
        print("SKIP scale (RCMD_E2E_SCALE=0)")
        return
    root, play, home = sandbox()
    big = os.path.join(play, "big")
    os.makedirs(big)
    for i in range(100_000):
        os.close(os.open(os.path.join(big, f"f{i:06}.dat"), os.O_CREAT | os.O_WRONLY, 0o644))
    t_start = time.time()
    s = Session(big, home)
    deadline = time.time() + 90
    while time.time() < deadline and "f000000.dat" not in s.screen():
        s.drain(1.0)
    check("scale: 100k listing loads", "f000000.dat" in s.screen())
    print(f"     (100k load visible after {time.time() - t_start:.1f}s)")
    s.send(END, wait=1.0)
    deadline = time.time() + 15
    while time.time() < deadline and "f099999.dat" not in s.screen():
        s.drain(0.5)
    check("scale: End reaches last of 100k", "f099999.dat" in s.screen())
    s.quit()
    shutil.rmtree(root)


def wait_buf(s, needle, timeout=40, start=0):
    """Poll the raw pty stream for `needle` (slow shells, loaded CI)."""
    deadline = time.time() + timeout
    while time.time() < deadline and needle not in s.buf[start:]:
        s.drain(0.3)
    return needle in s.buf[start:]


def test_subshell():
    """R1 per-shell scenarios: forced subshell=true in every suite mode."""
    shells = ["/bin/sh"]
    for name in ("bash", "zsh", "fish"):
        path = shutil.which(name)
        if path:
            shells.append(path)
        else:
            print(f"SKIP subshell {name}: not installed")
    for shell in shells:
        name = os.path.basename(shell)
        root, play, home = sandbox()
        os.makedirs(os.path.join(play, "followme"))
        if name == "fish":
            # pre-create so first-run completion generation is skipped
            os.makedirs(os.path.join(home, ".local/share/fish/generated_completions"))
        if name == "zsh":
            # CI runners have group-writable zsh dirs; Ubuntu's global
            # compinit then blocks on an interactive security prompt
            open(os.path.join(home, ".zshenv"), "w").write("skip_global_compinit=1\n")
        s = Session(play, home, shell=shell, subshell=True)

        # a typed command runs in the subshell, panels come right back
        # ('' splits the marker so the echoed command line can't match;
        # rcmd waits out slow shell startups — compinit and the like)
        s.send(b"echo AA''BB\r")
        check(
            f"subshell {name}: typed command ran",
            wait_buf(s, b"AABB"),
            detail=repr(s.buf[-400:]),
        )
        check(
            f"subshell {name}: auto-returned to panels",
            wait_for(s, "10Quit", timeout=10),
        )

        # Ctrl+O into the shell, cd there, Ctrl+O back: the panel follows
        s.send(b"\x0f", wait=STEP * 2)
        s.send(b"cd followme\r", wait=STEP * 2)
        s.send(b"\x0f", wait=STEP * 2)
        check(
            f"subshell {name}: panel follows the shell cwd",
            wait_for(s, play + "/followme", timeout=10),
        )

        # a panel cd syncs back into the shell before the next command
        s.send(b"cd ..\r", wait=STEP * 2)
        mark = len(s.buf)
        s.send(b"echo P''WD=$PWD\r")
        check(
            f"subshell {name}: shell follows the panel cwd",
            wait_buf(s, b"PWD=" + play.encode(), start=mark),
        )

        # exit respawns the shell (with a note on the output screen)
        s.send(b"\x0f", wait=STEP * 2)
        s.send(b"exit\r")
        check(f"subshell {name}: exit respawns", wait_buf(s, b"respawned"))
        s.send(b"\x0f", wait=STEP * 2)
        s.quit()
        shutil.rmtree(root, ignore_errors=True)


def main():
    if not os.path.isfile(BIN):
        print(f"FAIL binary not found: {BIN} (run `cargo build` first)")
        sys.exit(2)
    for test in (
        test_smoke,
        test_fileops,
        test_cmdline,
        test_viewer,
        test_archive,
        test_find,
        test_compare,
        test_watch,
        test_dirsize,
        test_history,
        test_quickview,
        test_mouse,
        test_sortclick,
        test_mcdepth,
        test_escmeta,
        test_aliases,
        test_bulk_rename,
        test_jobs,
        test_cxops,
        test_extensibility,
        test_git,
        test_editor,
        test_subshell,
        test_sftp,
        test_sftp_auth,
        test_scale,
    ):
        test()
    if FAILURES:
        print(f"\n{len(FAILURES)} failure(s): {', '.join(FAILURES)}")
        sys.exit(1)
    print("\nall e2e tests passed")


if __name__ == "__main__":
    main()
