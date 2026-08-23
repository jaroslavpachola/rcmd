#!/usr/bin/env python3
"""End-to-end tests: drive the real rcmd binary in a pseudo-terminal.

Usage:  python3 tests/e2e/run.py [path-to-rcmd-binary]
        (defaults to target/debug/rcmd relative to the repo root)

Each test runs in a fresh tempdir with an isolated $HOME, so the user's
real config is never touched. Exits non-zero if any check fails.
"""
import fcntl
import gzip
import io
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
            os.environ.pop("XDG_STATE_HOME", None)  # state.toml stays in $HOME
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
BACKSPACE = b"\x7f"


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

    # R4: copy INTO the tar - other panel (out/) holds a new file; F5
    # from there with the tar path as destination rewrites the archive
    open(os.path.join(play, "out", "fresh.txt"), "w").write("packed later\n")
    s.send(b"\t")                       # -> right panel (out/)
    s.send(b"\x12")                     # reload to see fresh.txt
    s.send(b"\x13fresh\r", wait=STEP)   # quick search -> fresh.txt
    s.send(F5)
    s.send(b"\x15")                     # clear the prefilled destination
    s.send(os.path.join(play, "b.tar.gz").encode() + b"://\r", wait=STEP * 4)
    check("archive: packed into tar", wait_for(s, "done -"))
    s.quit()
    with tarfile.open(os.path.join(play, "b.tar.gz")) as t:
        names = t.getnames()
        packed = t.extractfile("fresh.txt").read()
    check("archive: tar holds old and new",
          "inside.txt" in names and packed == b"packed later\n")
    shutil.rmtree(root)


def write_newc(members):
    """A cpio "newc" stream, built by hand so the fixture needs no tool.

    Each member is (name, st_mode, nlink, ino, data).
    """
    out = bytearray()
    for name, mode, nlink, ino, data in list(members) + [("TRAILER!!!", 0, 1, 0, b"")]:
        raw = name.encode() + b"\0"
        out += b"070701"
        for value in (ino, mode, 0, 0, nlink, 1700000000, len(data),
                      3, 4, 0, 0, len(raw), 0):
            out += b"%08X" % value
        out += raw
        out += b"\0" * (-len(out) % 4)
        out += data
        out += b"\0" * (-len(out) % 4)
    return bytes(out)


def test_cpio():
    """cpio archives: browse, view and extract - including a hard link,
    whose own record carries no bytes at all."""
    root, play, home = sandbox()
    os.makedirs(os.path.join(play, "out"))
    stream = write_newc([
        ("sub", 0o040755, 1, 1, b""),
        ("sub/inner.txt", 0o100644, 1, 2, b"deep\n"),
        ("hello.txt", 0o100644, 1, 3, b"from the cpio\n"),
        ("point", 0o120777, 1, 4, b"hello.txt"),
        # the alias comes first and is empty; the bytes ride with real.txt
        ("alias.txt", 0o100644, 2, 9, b""),
        ("real.txt", 0o100644, 2, 9, b"shared bytes\n"),
    ])
    with gzip.open(os.path.join(play, "box.cpio.gz"), "wb") as f:
        f.write(stream)

    s = Session(play, home, args=(play, os.path.join(play, "out")))
    s.send(b"\x13box\r", wait=STEP)     # quick search -> box.cpio.gz
    s.send(b"\r", wait=STEP * 2)        # enter the archive
    scr = s.screen()
    check("cpio: entered", "box.cpio.gz://" in scr)
    check("cpio: listing", "hello.txt" in scr and "sub" in scr and "point" in scr)
    s.send(F8)                          # must refuse
    check("cpio: read-only", "read-only" in s.screen())
    s.send(b"\x13hello\r", wait=STEP)   # -> hello.txt
    s.send(F3, wait=STEP * 2)
    check("cpio: F3 views member", "from the cpio" in s.screen())
    s.send(b"q")
    s.send(b"\x13alias\r", wait=STEP)   # -> alias.txt, the empty record
    s.send(F5)
    s.send(b"\r", wait=STEP * 3)        # extract into out/
    extracted = os.path.join(play, "out", "alias.txt")
    check("cpio: hard link extracts the shared bytes",
          wait_for(s, "done -") and open(extracted).read() == "shared bytes\n")
    s.quit()
    shutil.rmtree(root)


def test_cmdarchive():
    """rar/7z browsing through external tools (7z family / unrar)."""
    packer = None
    if shutil.which("rar"):
        packer, ext = "rar", "rar"
    elif shutil.which("7za") or shutil.which("7z"):
        packer, ext = (shutil.which("7za") and "7za") or "7z", "7z"
    if packer is None:
        print("SKIP cmdarchive (no rar/7z binary)")
        return
    root, play, home = sandbox()
    os.makedirs(os.path.join(play, "out"))
    src = os.path.join(root, "src")
    os.makedirs(os.path.join(src, "sub"))
    open(os.path.join(src, "hello.txt"), "w").write("packed content\n")
    open(os.path.join(src, "sub", "inner.txt"), "w").write("deep\n")
    box = os.path.join(play, "box." + ext)
    args = ["a", "-idq"] if packer == "rar" else ["a", "-bd"]
    subprocess.run([packer, *args, box, "hello.txt", "sub"],
                   cwd=src, check=True, capture_output=True)

    s = Session(play, home, args=(play, os.path.join(play, "out")))
    s.send(b"\x13box\r", wait=STEP)     # quick search -> box.rar/.7z
    s.send(b"\r", wait=STEP * 3)        # enter the archive
    scr = s.screen()
    check("cmdarchive: entered", ("box." + ext + "://") in scr)
    check("cmdarchive: listing", "hello.txt" in scr and "sub" in scr)
    s.send(F8)                          # must refuse
    check("cmdarchive: read-only", "read-only" in s.screen())
    s.send(b"\x13hello\r", wait=STEP)   # -> hello.txt
    s.send(F3, wait=STEP * 2)
    check("cmdarchive: F3 views member", "packed content" in s.screen())
    s.send(b"q")
    s.send(F5)
    s.send(b"\r", wait=STEP * 3)        # extract into out/
    extracted = os.path.join(play, "out", "hello.txt")
    check("cmdarchive: F5 extracts",
          wait_for(s, "done -") and open(extracted).read() == "packed content\n")
    s.quit()
    shutil.rmtree(root)


def write_ar(members):
    """An `ar` archive: an 8-byte magic and a 60-byte header per member,
    each padded to an even offset."""
    out = bytearray(b"!<arch>\n")
    for name, data in members:
        out += ("%-16s%-12d%-6d%-6d%-8s%-10d" % (name, 1700000000, 0, 0, "100644",
                                                 len(data))).encode()
        out += b"\x60\n"
        out += data
        if len(data) % 2:
            out += b"\n"
    return bytes(out)


def write_targz(files):
    """A gzipped tar of (name, mode, bytes) triples, built in memory."""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as t:
        for name, mode, data in files:
            info = tarfile.TarInfo(name)
            info.size = len(data)
            info.mode = mode
            info.mtime = 1700000000
            t.addfile(info, io.BytesIO(data))
    return buf.getvalue()


def test_deb():
    """A Debian package browses as one tree: the version stamp, the
    control half and the installed half."""
    root, play, home = sandbox()
    os.makedirs(os.path.join(play, "out"))
    control = write_targz([
        ("./control", 0o644, b"Package: hello\nVersion: 1.0\nArchitecture: all\n"),
        ("./postinst", 0o755, b"#!/bin/sh\nexit 0\n"),
    ])
    data = write_targz([
        ("./usr/share/doc/hello/README", 0o644, b"installed by the package\n"),
    ])
    open(os.path.join(play, "hello_1.0_all.deb"), "wb").write(write_ar([
        ("debian-binary", b"2.0\n"),
        ("control.tar.gz", control),
        ("data.tar.gz", data),
    ]))

    s = Session(play, home, args=(play, os.path.join(play, "out")))
    s.send(b"\x13hello\r", wait=STEP)   # quick search -> the package
    s.send(b"\r", wait=STEP * 2)        # enter it
    scr = s.screen()
    check("deb: entered", "hello_1.0_all.deb://" in scr)
    check("deb: both halves listed", "CONTROL" in scr and "CONTENTS" in scr)
    check("deb: the version stamp rides along", "debian-binary" in scr)

    # the root lists .., CONTENTS, CONTROL, debian-binary in that order
    s.send(DOWN + DOWN)                 # -> CONTROL
    s.send(b"\r", wait=STEP * 2)
    scr = s.screen()
    check("deb: control files listed", "control" in scr and "postinst" in scr)
    s.send(DOWN)                        # -> control
    s.send(F3, wait=STEP * 2)
    check("deb: F3 reads the control file", "Package: hello" in s.screen())
    s.send(b"q")

    s.send(BACKSPACE, wait=STEP)        # back to the package root
    s.send(HOME_K + DOWN)               # -> CONTENTS
    s.send(b"\r", wait=STEP * 2)
    check("deb: the installed tree opens", "usr" in s.screen())
    for _ in range(4):                  # usr/ share/ doc/ hello/
        s.send(DOWN)
        s.send(b"\r", wait=STEP)
    s.send(DOWN)                        # -> README
    s.send(F5)
    s.send(b"\r", wait=STEP * 3)
    extracted = os.path.join(play, "out", "README")
    check("deb: F5 extracts an installed file",
          wait_for(s, "done -") and open(extracted).read() == "installed by the package\n")
    s.quit()
    shutil.rmtree(root)


def write_rpm(tags, payload):
    """A package: the lead, a signature header to step over, the header
    that describes it, then the payload. `tags` is {tag: str}."""
    def header(entries):
        index, store = bytearray(), bytearray()
        for tag, value in entries.items():
            at = len(store)
            store += value.encode() + b"\0"
            index += struct.pack(">IIII", tag, 6, at, 1)
        return (b"\x8e\xad\xe8\x01" + b"\0" * 4
                + struct.pack(">II", len(entries), len(store)) + index + store)

    out = bytearray(b"\xed\xab\xee\xdb")
    out += b"\0" * (96 - len(out))
    out += header({1000: "sig"})
    out += b"\0" * (-len(out) % 8)      # the next header is 8-byte aligned
    out += header(tags)
    return bytes(out) + payload


def test_rpm():
    """An RPM package: the tags read as a file, the payload as a tree."""
    root, play, home = sandbox()
    os.makedirs(os.path.join(play, "out"))
    stream = write_newc([
        ("./usr/bin/hello", 0o100755, 1, 1, b"#!/bin/sh\necho hi\n"),
        ("./usr/share/doc/hello/README", 0o100644, 1, 2, b"shipped by the rpm\n"),
    ])
    open(os.path.join(play, "hello-1.0-3.noarch.rpm"), "wb").write(write_rpm({
        1000: "hello", 1001: "1.0", 1002: "3", 1004: "a fixture package",
        1022: "noarch", 1124: "cpio", 1125: "gzip",
    }, gzip.compress(stream)))

    s = Session(play, home, args=(play, os.path.join(play, "out")))
    s.send(b"\x13hello\r", wait=STEP)   # quick search -> the package
    s.send(b"\r", wait=STEP * 2)
    scr = s.screen()
    check("rpm: entered", "hello-1.0-3.noarch.rpm://" in scr)
    check("rpm: both halves listed", "CONTROL" in scr and "CONTENTS" in scr)

    s.send(DOWN + DOWN)                 # .., CONTENTS, CONTROL
    s.send(b"\r", wait=STEP * 2)        # into CONTROL/
    check("rpm: the header is a file", "header" in s.screen())
    s.send(DOWN)
    s.send(F3, wait=STEP * 2)
    scr = s.screen()
    check("rpm: F3 reads the header", "Name" in scr and "hello" in scr)
    check("rpm: the summary is there", "a fixture package" in scr)
    s.send(b"q")

    s.send(BACKSPACE, wait=STEP)
    s.send(HOME_K + DOWN)               # -> CONTENTS
    s.send(b"\r", wait=STEP * 2)
    check("rpm: the payload tree opens", "usr" in s.screen())
    s.send(DOWN)
    s.send(b"\r", wait=STEP)            # usr/
    s.send(DOWN)
    s.send(b"\r", wait=STEP)            # bin/
    s.send(DOWN)                        # -> hello
    s.send(F5)
    s.send(b"\r", wait=STEP * 3)
    extracted = os.path.join(play, "out", "hello")
    check("rpm: F5 extracts a payload file",
          wait_for(s, "done -") and open(extracted).read() == "#!/bin/sh\necho hi\n")
    s.quit()
    shutil.rmtree(root)


def test_iso():
    """An ISO 9660 image browses like any other archive - with Rock
    Ridge names, since a disc authored on Unix carries them."""
    tool = None
    for candidate in ("xorriso", "genisoimage", "mkisofs"):
        if shutil.which(candidate):
            tool = candidate
            break
    if tool is None:
        print("SKIP iso (no xorriso/genisoimage/mkisofs)")
        return
    root, play, home = sandbox()
    os.makedirs(os.path.join(play, "out"))
    src = os.path.join(root, "src")
    os.makedirs(os.path.join(src, "docs"))
    open(os.path.join(src, "readme.txt"), "w").write("burned to the disc\n")
    open(os.path.join(src, "docs", "manual.md"), "w").write("# manual\n")
    cmd = [tool] + (["-as", "mkisofs"] if tool == "xorriso" else [])
    subprocess.run(cmd + ["-R", "-J", "-o", os.path.join(play, "disc.iso"), src],
                   check=True, capture_output=True)

    s = Session(play, home, args=(play, os.path.join(play, "out")))
    s.send(b"\x13disc\r", wait=STEP)    # quick search -> disc.iso
    s.send(b"\r", wait=STEP * 2)
    scr = s.screen()
    check("iso: entered", "disc.iso://" in scr)
    check("iso: rock ridge names", "readme.txt" in scr and "docs" in scr)
    s.send(F8)                          # must refuse
    check("iso: read-only", "read-only" in s.screen())
    s.send(END)                         # -> readme.txt
    s.send(F3, wait=STEP * 2)
    check("iso: F3 views a file", "burned to the disc" in s.screen())
    s.send(b"q")
    s.send(F5)
    s.send(b"\r", wait=STEP * 3)
    extracted = os.path.join(play, "out", "readme.txt")
    check("iso: F5 extracts",
          wait_for(s, "done -") and open(extracted).read() == "burned to the disc\n")
    s.quit()
    shutil.rmtree(root)


def test_patch():
    """A patch browses as the tree it would apply to, each entry holding
    that one file's hunks."""
    root, play, home = sandbox()
    os.makedirs(os.path.join(play, "out"))
    open(os.path.join(play, "change.patch"), "w").write(
        "diff --git a/src/main.rs b/src/main.rs\n"
        "--- a/src/main.rs\n"
        "+++ b/src/main.rs\n"
        "@@ -1 +1 @@\n"
        "-the old line\n"
        "+the new line\n"
        "diff --git a/docs/readme.md b/docs/readme.md\n"
        "--- a/docs/readme.md\n"
        "+++ b/docs/readme.md\n"
        "@@ -1 +1 @@\n"
        "-old title\n"
        "+new title\n")

    s = Session(play, home, args=(play, os.path.join(play, "out")))
    s.send(b"\x13change\r", wait=STEP)  # quick search -> change.patch
    s.send(b"\r", wait=STEP * 2)
    scr = s.screen()
    check("patch: entered", "change.patch://" in scr)
    check("patch: paths became directories", "docs" in scr and "src" in scr)

    s.send(DOWN + DOWN + DOWN)          # .., docs, src -> src
    s.send(b"\r", wait=STEP * 2)
    check("patch: the file is inside its directory", "main.rs" in s.screen())
    s.send(DOWN)
    s.send(F3, wait=STEP * 2)
    scr = s.screen()
    check("patch: F3 shows only this file's hunks",
          "the new line" in scr and "old title" not in scr)
    s.send(b"q")
    s.send(F5)
    s.send(b"\r", wait=STEP * 3)
    extracted = os.path.join(play, "out", "main.rs")
    check("patch: F5 writes the slice out",
          wait_for(s, "done -")
          and "+the new line" in open(extracted).read()
          and "readme" not in open(extracted).read())
    s.quit()
    shutil.rmtree(root)


def test_mbox():
    """An mbox browses as its messages, numbered and with the subject
    decoded out of its RFC 2047 wrapper."""
    root, play, home = sandbox()
    os.makedirs(os.path.join(play, "out"))
    open(os.path.join(play, "inbox.mbox"), "w").write(
        "From alice@example.com Mon Aug 23 10:00:00 2026\n"
        "From: Alice <alice@example.com>\n"
        "Subject: the first message\n"
        "\n"
        "Hello Bob, this is the body.\n"
        "\n"
        "From bob@example.com Mon Aug 23 11:00:00 2026\n"
        "From: Bob <bob@example.com>\n"
        "Subject: =?UTF-8?B?YSByZXBseQ==?=\n"
        "\n"
        "And a reply.\n")

    s = Session(play, home, args=(play, os.path.join(play, "out")))
    s.send(b"\x13inbox\r", wait=STEP)   # quick search -> inbox.mbox
    s.send(b"\r", wait=STEP * 2)
    scr = s.screen()
    check("mbox: entered", "inbox.mbox://" in scr)
    check("mbox: messages are numbered", "0001 the first message" in scr)
    check("mbox: the encoded subject is readable", "0002 a reply" in scr)

    s.send(DOWN)                        # -> the first message
    s.send(F3, wait=STEP * 2)
    scr = s.screen()
    check("mbox: F3 shows the message", "Hello Bob, this is the body." in scr)
    check("mbox: without the mbox separator line", "From alice@example.com Mon" not in scr)
    s.send(b"q")
    s.send(F5)
    s.send(b"\r", wait=STEP * 3)
    extracted = os.path.join(play, "out", "0001 the first message")
    check("mbox: F5 writes the message out",
          wait_for(s, "done -")
          and open(extracted).read().startswith("From: Alice"))
    s.quit()
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
        # (Help, User menu, Quick search, Hotlist, Directory tree, Find)
        s.send(b"\x1b[20~")                 # F9
        s.send(b"\x1b[C" * 2)               # Left -> File -> Command
        s.send(DOWN * 5)                    # -> Find file...
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
    # keybar: the "9 PullDn" button opens the menu (on Left, as in MC)
    s.send(click(66, 30))
    check("mouse: keybar opens menu", "Brief listing" in s.screen())
    # menu bar: switch to Command, whose title x comes from the bar itself
    titles = s.screen().split("\n")[0]
    s.send(click(titles.index("Command") + 1, 1))
    check("mouse: menu bar switches", "Find file..." in s.screen())
    s.send(click(100, 15))              # outside: closes the menu
    check("mouse: click outside closes menu", "Find file..." not in s.screen())
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


def option_downs(scr, label):
    """Down presses needed to reach `label` in the options form, counted
    from the first setting. Keeps these tests working as the form grows
    a section at a time."""
    rows = []
    for line in scr.split("\n"):
        text = line.strip()
        if any(mark in text for mark in ("[x]", "[ ]", "(*)", "( )", "(Left/Right)")):
            rows.append(text)
    for i, row in enumerate(rows):
        if label in row:
            return i
    raise AssertionError(f"{label!r} not found in the options form: {rows}")


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

    # long listing via F9 -> Left (the panel menu F9 opens on)
    s.send(b"\x1b[20~")                # F9
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
    for _ in range(option_downs(s.screen(), "Lynx-like motion")):
        s.send(DOWN)                   # -> lynx row
    s.send(b" ")                       # check it
    check("mcdepth: form checkbox", "[x] Lynx-like motion" in s.screen())
    s.send(b"\r")                      # OK applies live
    time.sleep(0.5)                    # ... and writes through to disk
    # write-through lands in the state file; config.toml is the user's
    state_path = os.path.join(home, ".local", "state", "rcmd", "state.toml")
    check("mcdepth: options write through", "lynx = true" in open(state_path).read())
    s2 = Session(play, home)           # second instance, soon-stale memory
    s.send(HOME_K + DOWN)              # cursor -> docs/
    s.send(b"\x1b[C")                  # lynx Right enters it
    check("mcdepth: lynx right enters", "/docs" in s.screen().split("\n")[0][:60])
    s.send(b"\x1b[D")                  # lynx Left goes to the parent
    check("mcdepth: lynx left up", "/docs" not in s.screen().split("\n")[0][:60])
    s.send(b"\x1b[20~")
    s.send(b"o")
    s.send(b"p", wait=STEP)
    for _ in range(option_downs(s.screen(), "Lynx-like motion")):
        s.send(DOWN)
    s.send(b" " + b"\r")               # uncheck, OK
    s.send(HOME_K + DOWN)
    s.send(b"\x1b[C")                  # Right is a no-op once more
    check("mcdepth: lynx toggles off", "/docs" not in s.screen().split("\n")[0][:60])
    # the second instance never saw the toggles; its exit must save only
    # panel state, not clobber the options another instance applied
    s2.quit()
    check("mcdepth: exit does not clobber", "lynx = false" in open(state_path).read())
    s.quit()
    shutil.rmtree(root)


def test_escmeta():
    root, play, home = sandbox()
    open(os.path.join(play, "read.me"), "w").write("esc meta works\n")
    s = Session(play, home)
    # Esc then 9 (separate writes, so crossterm sees a lone Esc) = F9.
    # The follow-up must land inside esc_timeout_ms (250 ms by default),
    # so these sends drain briefly instead of a full STEP.
    s.send(b"\x1b", wait=0.05)
    s.send(b"9")
    check("escmeta: Esc 9 opens the menu", "Brief listing" in s.screen())
    # Esc Esc = a real Escape: closes the menu
    s.send(b"\x1b", wait=0.05)
    s.send(b"\x1b")
    check("escmeta: Esc Esc escapes", "Brief listing" not in s.screen())
    # Esc 3 on a file = F3 viewer
    s.send(DOWN)                       # cursor -> read.me
    s.send(b"\x1b", wait=0.05)
    s.send(b"3")
    check("escmeta: Esc 3 views", "esc meta works" in s.screen())
    s.send(b"q")
    # Esc t = Alt+T (cycle listing: full -> long)
    s.send(b"\x1b", wait=0.05)
    s.send(b"t")
    check("escmeta: Esc t is Alt+T", "Owner" in header_line(s)[:60])
    s.quit()

    # esc_timeout_ms raises the window for people who type the prefix by
    # hand: at MC's 1000 ms a half-second gap still reaches F9
    cfgdir = os.path.join(home, ".config", "rcmd")
    os.makedirs(cfgdir, exist_ok=True)
    cfg = os.path.join(cfgdir, "config.toml")
    open(cfg, "w").write("esc_timeout_ms = 1000\n" + open(cfg).read())
    s = Session(play, home)
    s.send(b"\x1b", wait=STEP)         # a slow, deliberate follow-up
    s.send(b"9")
    check("escmeta: esc_timeout_ms widens the window",
          "Brief listing" in s.screen())
    s.send(b"\x1b", wait=0.05)
    s.send(b"\x1b")
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
    # the lists open on the entry's own owner, so Set is a no-op chown
    s.send(b"\t" * 3)                       # users -> groups -> recurse -> Set
    s.send(b"\r", wait=STEP * 2)
    check("cxops: chown self ok", "chown: 1 item(s)" in status_line(s), status_line(s))

    s.send(b"\x18s", wait=STEP)             # C-x s
    check("cxops: symlink dialog", "Symlink" in s.screen())
    s.send(b"\r", wait=STEP * 2)            # accept "target.txt-link"
    link = os.path.join(play, "target.txt-link")
    # C-x s is MC's *absolute* symlink; C-x v is the relative one
    check("cxops: symlink created",
          os.path.islink(link) and os.readlink(link) == os.path.join(play, "target.txt"),
          os.readlink(link) if os.path.islink(link) else "not a link")
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
    s.send(b"\r", wait=STEP * 2)            # copy to dest/ - blocks on the fifo
    check("jobs: progress dialog", "copy 1 item" in s.screen())
    s.send(b"b", wait=STEP)                 # detach
    check("jobs: status shows background job",
          "1 job(s) running" in s.screen())
    s.send(b"\x18j", wait=STEP)             # C-x j
    scr = s.screen()
    check("jobs: list shows it", "Jobs" in scr and "copy 1 item" in scr)
    s.send(b"\r", wait=STEP)                # Enter: foreground again
    check("jobs: foregrounded", "b - background" in s.screen())
    s.send(b"b", wait=STEP)                 # detach again
    s.send(F10, wait=STEP)                  # quit must refuse
    check("jobs: quit refused while running", "still running" in s.screen())
    fd = os.open(os.path.join(play, "pipe.dat"), os.O_WRONLY)
    os.write(fd, b"data!")
    os.close(fd)                            # EOF -> the copy completes
    check("jobs: finishes in background", wait_for(s, "done -"))
    copied = os.path.join(dest, "pipe.dat")
    check("jobs: payload arrived",
          os.path.isfile(copied) and open(copied, "rb").read() == b"data!")
    s.quit()
    shutil.rmtree(root)


def test_bulk_rename():
    """R3: bulk rename - edit names in the editor, preview, apply."""
    root, play, home = sandbox()
    for name in ("aaa.txt", "bbb.txt", "ccc.txt"):
        open(os.path.join(play, name), "w").write(name + "\n")
    s = Session(play, home)
    s.send(b"+")                            # select group dialog
    s.send(b"\r", wait=STEP)                # "*" marks all files
    s.send(b"\x1b[20~")                     # F9 (opens on Left, as in MC)
    s.send(b"f")                            # -> File, by its title letter
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
        '[[view]]\n'
        'match = "*.dat"\n'
        'run = "tr a-z A-Z < %f"\n'
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

    # [[view]] filter: F3 shows the command's stdout, Shift+F3 the raw file
    open(os.path.join(play, "message.dat"), "w").write("filtered view content\n")
    s.send(b"\x12", wait=STEP)          # Ctrl+R reload
    s.send(b"\x13message\r", wait=STEP) # quick search -> message.dat
    s.send(F3, wait=STEP * 2)
    check("extensibility: [[view]] filter output",
          "FILTERED VIEW CONTENT" in s.screen())
    s.send(b"q")
    s.send(b"\x1b[13;2~", wait=STEP * 2)  # Shift+F3: raw
    check("extensibility: Shift+F3 raw view",
          "filtered view content" in s.screen())
    s.send(b"q")
    s.quit()
    shutil.rmtree(root)


def test_brief():
    """PLAN4 S1: MC's brief listing shows names in several columns,
    filled column by column so Down stays "the next file"."""
    root, play, home = sandbox()
    cfgdir = os.path.join(home, ".config", "rcmd")
    os.makedirs(cfgdir)
    open(os.path.join(cfgdir, "config.toml"), "w").write('listing = "brief"\n')
    for i in range(40):
        open(os.path.join(play, "f%02d.txt" % i), "w").write("x\n")
    s = Session(play, home)
    scr = s.screen().split("\n")

    # two columns of names, filled downwards: the top row carries the
    # first entry of each column, not the first two entries
    top = next(line for line in scr if "f00.txt" in line)
    check("brief: two columns", top.count("Name") == 0 and "f00.txt" in top, top)
    header = next(line for line in scr if "Name" in line)
    check("brief: a header per column", header.count("Name") >= 2, header)
    check("brief: filled column by column", "f01.txt" not in top, top)

    # Down moves to the file drawn underneath
    s.send(DOWN)
    check("brief: down is the next file", "f00.txt" in status_line(s), status_line(s))

    # clicking in the second column selects that file
    second = [line for line in scr if "f23.txt" in line]
    check("brief: second column filled", bool(second), str(scr[2:4]))
    col2_x = second[0].index("f23.txt") + 2
    s.send(click(col2_x, 3))
    check("brief: click maps to the right column", "f23.txt" in status_line(s), status_line(s))
    s.quit()
    shutil.rmtree(root)


def test_layout():
    """PLAN4 S1: MC's Layout settings - split direction and size, and the
    optional menu bar / status line / command line / key bar."""
    root, play, home = sandbox()
    cfgdir = os.path.join(home, ".config", "rcmd")
    os.makedirs(cfgdir)
    open(os.path.join(cfgdir, "config.toml"), "w").write(
        'split = "horizontal"\n'
        "split_ratio = 30\n"
        "show_menubar = true\n"
        "show_keybar = false\n"
    )
    open(os.path.join(play, "hello.txt"), "w").write("hi\n")
    s = Session(play, home)
    scr = s.screen().split("\n")

    # the menu bar is drawn across the top, the key bar is gone
    check("layout: menu bar drawn", "File" in scr[0] and "Command" in scr[0], scr[0])
    check("layout: key bar hidden", not any("10Quit" in line for line in scr), scr[-1])

    # horizontal split: the two panel frames are stacked, so the panel
    # border appears on more than the usual two rows
    tops = [i for i, line in enumerate(scr) if line.startswith("\u250c")]
    check("layout: panels stacked", len(tops) == 2, str(tops))
    # ...and the 30% top panel is the shorter of the two
    check("layout: split ratio honoured", tops[1] - tops[0] < ROWS // 2, str(tops))

    # clicking a menu-bar title opens that menu (click() is 1-based, and
    # the title's column comes from the bar itself - Left is first now)
    s.send(click(scr[0].index("File") + 1, 1), wait=STEP)
    check("layout: menu bar is clickable", "Make directory..." in s.screen())
    s.send(b"\x1b", wait=STEP)
    s.send(b"\x1b", wait=STEP)

    # switch back to a vertical split from the options form
    s.send(b"\x1b[20~")
    s.send(b"o")
    s.send(b"p", wait=STEP)
    check("layout: form has a Layout section", "Layout" in s.screen(), s.screen())
    check("layout: ratio row shows the split", "30%" in s.screen(), s.screen())
    s.send(b" ", wait=STEP)                 # row 1 = Split radio -> vertical
    s.send(b"\r", wait=STEP)
    scr = s.screen().split("\n")
    tops = [i for i, line in enumerate(scr) if line.startswith("\u250c")]
    check("layout: back to a vertical split", len(tops) == 1, str(tops))
    statepath = os.path.join(home, ".local", "state", "rcmd", "state.toml")
    check("layout: saved to state", 'split = "vertical"' in open(statepath).read())

    # mini status: a per-panel row describing that panel's cursor entry
    s.send(b"\x1b[20~")
    s.send(b"o")
    s.send(b"p", wait=STEP)
    for _ in range(option_downs(s.screen(), "Mini status")):
        s.send(DOWN)
    s.send(b" ")
    s.send(b"\r", wait=STEP)
    s.send(DOWN)                            # active panel -> hello.txt
    framed = [line for line in s.screen().split("\n") if line.startswith("\u2502")]
    # side by side, one screen row carries both minis: the active panel
    # describes hello.txt while the other still sits on ".."
    both = [line for line in framed if "hello.txt" in line and "UP--DIR" in line]
    check("layout: each panel has its own mini status", len(both) == 1, str(framed[-3:]))
    check("layout: mini status shows permissions", "rw" in both[0] if both else False)
    s.quit()
    shutil.rmtree(root)


def test_tree():
    """PLAN4 S1: mc's directory tree. In the Command-menu dialog Enter
    moves *this* panel and closes; in the tree listing mode Enter moves
    the *other* panel and the figure stays put."""
    root, play, home = sandbox()
    for path in ["alpha/one", "alpha/two", "beta/deep", "gamma"]:
        os.makedirs(os.path.join(play, path))
    open(os.path.join(play, "file.txt"), "w").write("x\n")
    s = Session(play, home)

    def figure(screen, left=0, right=COLS):
        """Tree-figure lines inside a column range - the dialog and the
        tree panel both sit on top of a listing that must not be read as
        part of the figure."""
        cut = [ln[left:right] for ln in screen.split("\n")]
        return [ln for ln in cut if "├─" in ln or "└─" in ln]

    def dialog(screen):
        lines = screen.split("\n")
        title = next((ln for ln in lines if "Directory tree" in ln), "")
        start = title.index("┌") if "┌" in title else 0
        return figure(screen, start, start + 60)

    # F9 -> Command -> Directory tree...
    # (Help, User menu, Quick search, Hotlist, *Directory tree*)
    s.send(b"\x1b[20~")
    s.send(b"\x1b[C" * 2)
    s.send(DOWN * 4 + b"\r")
    scr = s.screen()
    check("tree: dialog opens", "Directory tree" in scr, scr[:120])
    fig = dialog(scr)
    check("tree: figure drawn", len(fig) > 3, str(fig[:2]))
    check("tree: the panel's directory is revealed", any("play" in ln for ln in fig))
    check("tree: its subdirectories are open", any("alpha" in ln for ln in fig))
    check("tree: directories only", not any("file.txt" in ln for ln in fig), str(fig[:3]))

    # F4 flips mc's navigation mode; the hint line names the next one
    s.send(F4)
    check("tree: F4 goes static", "F4 static" in s.screen())
    s.send(F4)
    check("tree: F4 goes back to dynamic", "F4 dynamic" in s.screen())

    # type-to-search, then Enter takes *this* panel to the selection
    s.send(b"b")
    check("tree: search string shown", "search: b" in s.screen())
    s.send(b"\r")
    scr = s.screen()
    check("tree: dialog closed", "Directory tree" not in scr)
    check("tree: Enter cd'd this panel", "play/beta" in scr, scr[:120])
    check("tree: and only this one", scr.count("play/beta") < 3, scr[:240])

    # the listing mode: F9 -> Left (Brief, Full, Long, User, *Tree*)
    s.send(b"\x1b[20~")
    s.send(DOWN * 4 + b"\r")
    scr = s.screen()
    left_half = [ln[:60] for ln in scr.split("\n")]
    check("tree: listing mode draws the figure", len(figure(scr, 0, 60)) > 3, scr[:120])
    check("tree: no listing header left", "Modify time" not in left_half[1], left_half[1])
    check("tree: the other panel keeps its listing", "Modify time" in scr.split("\n")[1][60:])

    # Enter moves the *other* panel and stays in the tree. The cursor
    # sits on beta (the panel's directory), so Up lands on its sibling.
    s.send(b"\x1b[A")
    s.send(b"\r")
    scr = s.screen()
    check("tree: mode survives Enter", len(figure(scr, 0, 60)) > 3, scr[:120])
    check("tree: Enter moved the other panel", "play/alpha" in scr.split("\n")[0][60:],
          scr.split("\n")[0][60:])
    check("tree: this panel did not move", "play/beta" in scr.split("\n")[0][:60],
          scr.split("\n")[0][:60])

    # Ctrl+S searches the figure - in a tree view mc keeps plain
    # characters for the command line until the search is switched on
    s.send(b"\x13")
    s.send(b"g")
    check("tree: Ctrl+S searches the figure", "Search: g" in status_line(s), status_line(s))
    s.send(b"\r")                  # end the search
    s.send(b"\r")                  # Enter on the match
    scr = s.screen()
    check("tree: the search landed on gamma", "play/gamma" in scr.split("\n")[0][60:],
          scr.split("\n")[0][60:])
    s.quit()
    shutil.rmtree(root)


def test_userformat():
    """PLAN4 S1: mc's user-defined listing format - `listing = "user"`
    draws whatever `listing_format` asks for, in mc's own little
    language."""
    root, play, home = sandbox()
    cfgdir = os.path.join(home, ".config", "rcmd")
    os.makedirs(cfgdir)
    open(os.path.join(cfgdir, "config.toml"), "w").write(
        'listing = "user"\n'
        'listing_format = "half name | size:7 | type mode:3"\n'
    )
    os.makedirs(os.path.join(play, "subdir"))
    os.chmod(os.path.join(play, "subdir"), 0o750)
    open(os.path.join(play, "one.txt"), "w").write("x" * 300)
    s = Session(play, home)
    scr = s.screen().split("\n")
    header, left = scr[1][:59], [ln[:59] for ln in scr]

    check("userformat: fields become columns",
          "Name" in header and "Size" in header and "T" in header, header)
    check("userformat: | draws a rule", header.count("│") >= 2, header)
    dir_row = next((ln for ln in left if "subdir" in ln), "")
    check("userformat: the octal mode is not clipped", "750" in dir_row, dir_row)
    check("userformat: type marks the directory", "/" in dir_row, dir_row)
    file_row = next((ln for ln in left if "one.txt" in ln), "")
    check("userformat: sizes line up right", "    300" in file_row, file_row)
    s.quit()

    # a field nobody knows costs one column, not the panel
    home2 = os.path.join(root, "home2")
    os.makedirs(os.path.join(home2, ".config", "rcmd"))
    open(os.path.join(home2, ".config", "rcmd", "config.toml"), "w").write(
        'listing = "user"\nlisting_format = "half name colour size"\n'
    )
    s = Session(play, home2)
    check("userformat: an unknown field warns", "colour" in status_line(s), status_line(s))
    check("userformat: and the rest still draws", "one.txt" in s.screen())
    s.quit()

    # `full` asks for the whole width, so only the active panel is drawn
    home3 = os.path.join(root, "home3")
    os.makedirs(os.path.join(home3, ".config", "rcmd"))
    open(os.path.join(home3, ".config", "rcmd", "config.toml"), "w").write(
        'listing = "user"\n'
        'listing_format = "full perm space owner space size space name"\n'
    )
    s = Session(play, home3)
    scr = s.screen().split("\n")
    check("userformat: full takes the whole width", scr[0].count("┌") == 1, scr[0])
    check("userformat: and shows its fields", "Perms" in scr[1] and "Owner" in scr[1], scr[1])
    s.quit()
    shutil.rmtree(root)


def test_highlight():
    """PLAN4 S1: MC's filehighlight as TOML - [[highlight]] paints
    entries by name or by kind, first rule wins."""
    root, play, home = sandbox()
    cfgdir = os.path.join(home, ".config", "rcmd")
    os.makedirs(cfgdir)
    open(os.path.join(cfgdir, "config.toml"), "w").write(
        '[[highlight]]\nmatch = "*.tar.gz"\ncolor = "brightred"\n\n'
        '[[highlight]]\nmatch = "*.gz"\ncolor = "green"\n\n'
        '[[highlight]]\ntype = "exe"\ncolor = "magenta"\nbold = true\n\n'
        '[[highlight]]\nmatch = "*.bad"\ncolor = "chartreuse"\n'
    )
    open(os.path.join(play, "archive.tar.gz"), "w").write("x")
    open(os.path.join(play, "data.gz"), "w").write("x")
    open(os.path.join(play, "plain.txt"), "w").write("x")
    open(os.path.join(play, "script.sh"), "w").write("#!/bin/sh\n")
    os.chmod(os.path.join(play, "script.sh"), 0o755)
    s = Session(play, home)

    def sgr(needle):
        """The escape codes still in force where `needle` was drawn -
        screen() strips them, so the raw stream is where colour lives."""
        raw = s.buf.decode("utf-8", "replace")
        i = raw.find(needle)
        return raw[max(0, i - 40):i] if i >= 0 else ""

    # 38;5;N is how ratatui writes a named colour: 9 bright red, 2 green,
    # 5 magenta, 7 the panel's own grey
    check("highlight: a glob rule paints the name", "38;5;9" in sgr("archive.tar.gz"),
          repr(sgr("archive.tar.gz")))
    check("highlight: the first matching rule wins", "38;5;2" not in sgr("archive.tar.gz"),
          repr(sgr("archive.tar.gz")))
    check("highlight: later rules still apply", "38;5;2" in sgr("data.gz"),
          repr(sgr("data.gz")))
    check("highlight: a type rule paints by kind", "38;5;5" in sgr("script.sh"),
          repr(sgr("script.sh")))
    check("highlight: bold is honoured", "\x1b[1m" in sgr("script.sh"), repr(sgr("script.sh")))
    check("highlight: unmatched entries keep the panel colour",
          "38;5;7" in sgr("plain.txt"), repr(sgr("plain.txt")))
    check("highlight: an unknown colour warns", "chartreuse" in status_line(s), status_line(s))
    s.quit()
    shutil.rmtree(root)


def test_panelmenus():
    """PLAN4 S1: mc's menu structure - Left and Right act on their own
    panel whichever one has the focus, with File, Command and Options
    between them."""
    root, play, home = sandbox()
    other = os.path.join(root, "other")
    os.makedirs(other)
    open(os.path.join(play, "left.txt"), "w").write("x\n")
    open(os.path.join(other, "right.txt"), "w").write("hello from the right\n")
    s = Session(play, home, args=(play, other))

    s.send(b"\x1b[20~")                     # F9 opens on the Left menu
    titles = s.screen().split("\n")[0]
    for title in ("Left", "File", "Command", "Options", "Right"):
        check("panelmenus: %s in the bar" % title.lower(), title in titles, titles)

    # an entry letter beats a title letter, so the panel menus leave f,
    # c, o and r alone - the other menus stay one keystroke away
    for letter, entry in ((b"f", "Make directory..."), (b"c", "Find file..."),
                          (b"o", "Panel options...")):
        s.send(letter)
        check("panelmenus: %s reachable by letter" % entry.split()[0].lower(),
              entry in s.screen(), s.screen().split("\n")[2])
        s.send(b"\x1b")
        s.send(b"\x1b[20~")

    # Right menu, Brief listing: the right panel loses its columns and
    # the left keeps them, though the left panel is the one with focus
    s.send(b"r")
    s.send(b"b", wait=STEP)
    hdr = header_line(s)
    check("panelmenus: the right menu hit the right panel",
          "Modify time" not in hdr[60:], hdr[60:])
    check("panelmenus: the left panel is untouched", "Modify time" in hdr[:60], hdr[:60])
    check("panelmenus: focus follows the menu you used",
          "/other$" in s.screen().split("\n")[-2], s.screen().split("\n")[-2])

    # Left menu, Quick view: the *left* panel becomes the preview, so
    # the focus lands on the right one, which is doing the browsing
    s.send(b"\x1b[20~")
    s.send(b"q", wait=STEP * 2)
    s.send(DOWN, wait=STEP * 2)             # off ".." and onto the file
    scr = s.screen()
    check("panelmenus: quick view took the left panel",
          "hello from the right" in "".join(ln[:60] for ln in scr.split("\n")), scr[:200])
    check("panelmenus: the browsing panel keeps the focus",
          "/other$" in scr.split("\n")[-2], scr.split("\n")[-2])

    # ...and the same entry again puts the panel back
    s.send(b"\x1b[20~")
    s.send(b"q", wait=STEP)
    check("panelmenus: quick view toggles back off",
          "hello from the right" not in s.screen(), s.screen()[:200])
    s.quit()
    shutil.rmtree(root)


def test_overwrite():
    """PLAN4 S2: MC's overwrite prompt - both files on screen, and the
    Append / Reget / Update / Size-differs answers behind it."""
    root, play, home = sandbox()
    other = os.path.join(root, "other")
    os.makedirs(other)
    open(os.path.join(play, "log.txt"), "w").write("second\n")
    open(os.path.join(other, "log.txt"), "w").write("first\n")
    s = Session(play, home, args=(play, other))

    s.send(DOWN)                            # onto log.txt
    s.send(F5, wait=STEP)                   # copy to the other panel
    s.send(b"\r", wait=STEP * 2)            # ...which already has one
    scr = s.screen()
    check("overwrite: the prompt names the file", "File exists" in scr and "log.txt" in scr)
    check("overwrite: both files are on screen",
          "source" in scr and "target" in scr, scr[:200])
    for label in ("Overwrite", "Append", "Reget", "Skip", "All", "Update",
                  "Size differs", "None", "Abort"):
        check("overwrite: %s offered" % label.lower(), "[ %s ]" % label in scr, scr[:400])

    # Append puts the source on the end of what is already there
    s.send(b"\x1b[C")                       # -> Append
    s.send(b"\r", wait=STEP * 3)
    check("overwrite: append kept both halves",
          open(os.path.join(other, "log.txt")).read() == "first\nsecond\n",
          repr(open(os.path.join(other, "log.txt")).read()))

    # Update: answered once, it compares mtimes - here the target is the
    # newer file, so it stays put
    open(os.path.join(play, "keep.txt"), "w").write("older source\n")
    open(os.path.join(other, "keep.txt"), "w").write("newer target\n")
    os.utime(os.path.join(play, "keep.txt"), (1_000_000, 1_000_000))
    os.utime(os.path.join(other, "keep.txt"), (2_000_000, 2_000_000))
    s.send(b"\x12")                         # Ctrl+R: see the new file
    s.send(HOME_K, wait=STEP)               # ".." , then the first entry
    s.send(DOWN, wait=STEP)
    check("overwrite: on keep.txt", "keep.txt" in status_line(s), status_line(s))
    s.send(F5, wait=STEP)
    s.send(b"\r", wait=STEP * 2)
    check("overwrite: the prompt is back", "File exists" in s.screen())
    s.send(b"\x1b[B")                       # Down: onto the "all files" row
    s.send(b"\x1b[C")                       # -> Update
    s.send(b"\r", wait=STEP * 3)
    check("overwrite: update left the newer target alone",
          open(os.path.join(other, "keep.txt")).read() == "newer target\n",
          repr(open(os.path.join(other, "keep.txt")).read()))
    s.quit()
    shutil.rmtree(root)


def test_copyform():
    """PLAN4 S2: MC's copy/move form - the destination, the switches that
    change what a copy does, and a Background button."""
    root, play, home = sandbox()
    other = os.path.join(root, "other")
    os.makedirs(other)
    open(os.path.join(play, "a.txt"), "w").write("hi\n")
    os.utime(os.path.join(play, "a.txt"), (1_000_000, 1_000_000))
    open(os.path.join(play, "b.txt"), "w").write("bye\n")
    s = Session(play, home, args=(play, other))

    s.send(DOWN)                            # onto a.txt
    s.send(F5, wait=STEP)
    scr = s.screen()
    check("copyform: the form opens", "Copy" in scr and "/other" in scr, scr[:200])
    for label in ("Preserve attributes", "Follow links", "Dive into subdirs",
                  "Stable symlinks"):
        check("copyform: %s offered" % label.split()[0].lower(), label in scr, scr[:400])
    check("copyform: rcmd's defaults are the careful ones",
          "[x] Preserve attributes" in scr and "[ ] Follow links" in scr, scr[:400])
    check("copyform: OK/Background/Cancel", "[ Background ]" in scr, scr[:400])

    # Cancel really cancels: down to the buttons, along to Cancel, Enter
    # (the form opens on the destination, with four boxes below it)
    s.send(DOWN * 5)
    s.send(b"\x1b[C" * 2)
    s.send(b"\r", wait=STEP * 2)
    check("copyform: cancel copied nothing",
          not os.path.exists(os.path.join(other, "a.txt")))

    # Preserve off: the copy gets its own timestamp, not the source's
    s.send(F5, wait=STEP)
    s.send(DOWN)                            # -> Preserve attributes
    s.send(b" ")
    check("copyform: space flips the box", "[ ] Preserve attributes" in s.screen(),
          s.screen()[:400])
    s.send(b"\r", wait=STEP * 3)
    copied = os.path.join(other, "a.txt")
    check("copyform: OK ran the copy", os.path.exists(copied))
    check("copyform: preserve off leaves a fresh mtime",
          os.path.exists(copied) and os.stat(copied).st_mtime > 1_000_000,
          str(os.stat(copied).st_mtime if os.path.exists(copied) else "missing"))

    # Background: the job detaches, the panels come straight back
    s.send(DOWN, wait=STEP)                 # onto b.txt
    if "b.txt" not in status_line(s):
        s.send(DOWN, wait=STEP)
    s.send(F5, wait=STEP)
    s.send(DOWN * 5)
    s.send(b"\x1b[C")                       # -> Background
    s.send(b"\r", wait=STEP * 3)
    scr = s.screen()
    check("copyform: background leaves no dialog up", "[ Background ]" not in scr, scr[:200])
    check("copyform: and the copy still happened",
          os.path.exists(os.path.join(other, "b.txt")))
    s.quit()
    shutil.rmtree(root)


def test_masks():
    """PLAN4 S2: MC's mask copy/rename - the source mask picks which
    files take part, the destination's wildcards rename them."""
    root, play, home = sandbox()
    other = os.path.join(root, "other")
    os.makedirs(other)
    open(os.path.join(play, "foo.tar.gz"), "w").write("tarball\n")
    open(os.path.join(play, "notes.txt"), "w").write("notes\n")
    s = Session(play, home, args=(play, other))

    s.send(b"+")                            # select group...
    s.send(b"\r", wait=STEP)                # ..."*" marks both files
    s.send(F5, wait=STEP)
    check("masks: the form asks for a mask first", "mask" in s.screen(), s.screen()[:300])
    check("masks: it starts as catch-all", "mask *" in s.screen(), s.screen()[:300])

    s.send(b"\x1b[A")                       # up to the mask row
    s.send(b"\x15")                         # Ctrl+U clears it
    s.send(b"*.tar.gz")
    s.send(DOWN)                            # back to the destination
    s.send(END)
    s.send(b"*.tgz")
    scr = s.screen()
    check("masks: both fields show", "*.tar.gz" in scr and "*.tgz" in scr, scr[:300])
    s.send(b"\r", wait=STEP * 3)

    check("masks: the match was renamed on the way",
          os.path.exists(os.path.join(other, "foo.tgz")),
          str(sorted(os.listdir(other))))
    check("masks: under its new name only",
          not os.path.exists(os.path.join(other, "foo.tar.gz")))
    check("masks: and the rest was left alone",
          not os.path.exists(os.path.join(other, "notes.txt")),
          str(sorted(os.listdir(other))))
    s.quit()
    shutil.rmtree(root)


def test_chmod():
    """PLAN4 S2: MC's chmod window - twelve bits as check boxes, the
    octal beside them, and its three ways of spending them."""
    root, play, home = sandbox()
    for name, mode in (("a.sh", 0o754), ("b.sh", 0o600)):
        open(os.path.join(play, name), "w").write("#!/bin/sh\n")
        os.chmod(os.path.join(play, name), mode)
    s = Session(play, home)

    s.send(DOWN)                            # onto a.sh
    s.send(b"\x18c", wait=STEP)             # Ctrl+X c
    scr = s.screen()
    check("chmod: the matrix opens", "Chmod" in scr and "Permissions" in scr, scr[:200])
    check("chmod: the file section names what changes",
          "a.sh" in scr and "owner" in scr, scr[:400])
    check("chmod: it starts from the file's own mode", "0754" in scr, scr[:400])
    check("chmod: the bits are drawn", "[x] read    owner" in scr and
          "[ ] write   group" in scr, scr[:600])

    # Space on a bit rewrites the octal: 0754 + group write = 0774
    # (the dialog opens on the octal field, so the bits are above it)
    s.send(b"\x1b[A" * 5)                   # -> write   group
    s.send(b" ")
    check("chmod: space flips a bit", "[x] write   group" in s.screen(), s.screen()[:600])
    check("chmod: and the octal follows", "octal 774" in s.screen(), s.screen()[:600])
    s.send(b"\r", wait=STEP * 2)             # Set
    check("chmod: Set applied it",
          oct(os.stat(os.path.join(play, "a.sh")).st_mode & 0o777) == "0o774",
          oct(os.stat(os.path.join(play, "a.sh")).st_mode & 0o777))

    # typing an octal moves the boxes the other way
    s.send(b"\x18c", wait=STEP)             # opens on the octal field
    s.send(b"\x15")                         # Ctrl+U clears it
    s.send(b"640")
    check("chmod: the octal moves the boxes", "[ ] exec    owner" in s.screen(),
          s.screen()[:600])
    s.send(b"\r", wait=STEP * 2)
    check("chmod: and Set wrote that mode",
          oct(os.stat(os.path.join(play, "a.sh")).st_mode & 0o777) == "0o640")

    # "Clear marked" takes the *checked* bits off every marked file and
    # leaves each file's other bits alone. a.sh is 0640, b.sh is 0600,
    # and the boxes come from the cursor entry - b.sh - so 0600 comes
    # off both: b.sh empties, a.sh keeps the group-read bit it alone had
    s.send(HOME_K)
    s.send(DOWN + INSERT + INSERT)          # mark a.sh and b.sh
    s.send(b"\x18c", wait=STEP)
    check("chmod: the boxes come from the cursor entry", "0600" in s.screen(),
          s.screen()[:400])
    s.send(DOWN * 2)                        # -> past recurse, to the buttons
    s.send(b"\x1b[C" * 2)                   # -> Clear marked
    s.send(b"\r", wait=STEP * 3)
    a = os.stat(os.path.join(play, "a.sh")).st_mode & 0o777
    b = os.stat(os.path.join(play, "b.sh")).st_mode & 0o777
    check("chmod: clear marked cleared only those bits",
          a == 0o040 and b == 0o000, "%o %o" % (a, b))
    s.quit()
    shutil.rmtree(root)


def test_chown():
    """PLAN4 S2: MC's chown window - the system's users and groups as
    two pick lists, with the entry's own owner preselected."""
    import getpass
    root, play, home = sandbox()
    open(os.path.join(play, "target.txt"), "w").write("x\n")
    s = Session(play, home)
    me = getpass.getuser()

    s.send(DOWN)                            # onto target.txt
    s.send(b"\x18o", wait=STEP)             # Ctrl+X o
    scr = s.screen()
    check("chown: the window opens", "Chown" in scr, scr[:200])
    check("chown: both lists are headed", "User" in scr and "Group" in scr, scr[:400])
    # the user column, read straight off the screen: the lists scroll to
    # centre the entry's own owner, so what is visible is around "me"
    lines = scr.split("\n")
    head = next(ln for ln in lines if "User" in ln and "Group" in ln)
    at = head.index("User")
    column = [ln[at:at + 16].strip() for ln in lines[lines.index(head) + 1:]][:12]
    names = [n for n in column if n]
    check("chown: the user column lists accounts", len(names) >= 5, str(names))
    check("chown: with the entry's own owner among them", me in names, str(names))
    check("chown: the file section names what changes",
          "target.txt" in scr and me in scr, scr[:600])
    check("chown: it says how many entries", "1 item(s)" in scr, scr[:600])

    # Tab walks user list -> group list -> buttons, and Esc backs out
    s.send(b"\t" * 3)                       # users -> groups -> recurse -> buttons
    check("chown: tab reaches the buttons", "[ Set ]" in s.screen(), s.screen()[:600])
    s.send(b"\x1b", wait=STEP)
    check("chown: esc closed it", "Chown" not in s.screen())

    # Set with the entry's own owner is a no-op chown that still runs
    s.send(b"\x18o", wait=STEP)
    s.send(b"\t" * 3)
    s.send(b"\r", wait=STEP * 2)
    check("chown: Set ran", "chown: 1 item(s)" in status_line(s), status_line(s))
    s.quit()
    shutil.rmtree(root)


def test_confirmations():
    """PLAN4 S2: MC's confirmation set - dropping a hotlist entry and
    Enter running an opener now ask, and both are toggles."""
    root, play, home = sandbox()
    cfgdir = os.path.join(home, ".config", "rcmd")
    os.makedirs(cfgdir)
    open(os.path.join(cfgdir, "config.toml"), "w").write(
        'confirm_execute = true\n\n'
        '[[open]]\nmatch = "*.run"\nrun = "touch ran.marker"\n'
    )
    open(os.path.join(play, "thing.run"), "w").write("x\n")
    s = Session(play, home)

    # the hotlist: 'a' adds this directory, 'd' now asks before dropping
    s.send(b"\x1c", wait=STEP)              # Ctrl+\ hotlist
    s.send(b"a")
    check("confirm: hotlist entry added", "play" in s.screen(), s.screen()[:400])
    s.send(b"d")
    scr = s.screen()
    check("confirm: dropping asks first", "Hotlist" in scr and "Drop" in scr, scr[:400])
    s.send(b"n", wait=STEP)                 # No puts the hotlist back
    check("confirm: no keeps the entry",
          "Directory hotlist" in s.screen() and "play" in s.screen(), s.screen()[:400])
    s.send(b"d")
    s.send(b"y", wait=STEP)
    scr = s.screen()
    check("confirm: yes dropped it and came back",
          "Directory hotlist" in scr and "empty" in scr, scr[:400])
    s.send(b"\x1b", wait=STEP)

    # Enter on a file with an opener asks before running it
    s.send(DOWN)
    s.send(b"\r", wait=STEP)
    check("confirm: execute asks", "Execute" in s.screen() and "touch" in s.screen(),
          s.screen()[:400])
    s.send(b"n", wait=STEP)
    check("confirm: no did not run it",
          not os.path.exists(os.path.join(play, "ran.marker")))
    s.send(b"\r", wait=STEP)
    s.send(b"y", wait=STEP * 4)
    check("confirm: yes ran it", os.path.exists(os.path.join(play, "ran.marker")),
          str(sorted(os.listdir(play))))

    # and both are in the options form
    s.send(b"\x1b[20~")
    s.send(b"o")
    s.send(b"p", wait=STEP)
    scr = s.screen()
    check("confirm: the form offers both toggles",
          "hotlist entry" in scr and "opener" in scr, scr[:600])
    s.send(b"\x1b", wait=STEP)
    s.quit()
    shutil.rmtree(root)


def test_links():
    """PLAN4 S2: MC's four link commands - C-x l hard, C-x s absolute,
    C-x v relative, C-x C-s to change where a link points."""
    root, play, home = sandbox()
    open(os.path.join(play, "orig.txt"), "w").write("payload\n")
    s = Session(play, home)
    s.send(DOWN)                            # onto orig.txt

    # C-x v: a relative symlink, named by the form's second row
    s.send(b"\x18v", wait=STEP)
    scr = s.screen()
    check("links: the form names both halves",
          "points at" in scr and "named" in scr, scr[:400])
    check("links: relative keeps it short", "points at orig.txt" in scr, scr[:400])
    s.send(b"\x15")                         # Ctrl+U over the suggested name
    s.send(b"rel.txt")
    s.send(b"\r", wait=STEP * 2)
    rel = os.path.join(play, "rel.txt")
    check("links: relative symlink created",
          os.path.islink(rel) and os.readlink(rel) == "orig.txt",
          os.readlink(rel) if os.path.islink(rel) else "missing")

    # C-x l: a hard link - a real file sharing the original's inode
    s.send(HOME_K)
    s.send(DOWN, wait=STEP)                 # back onto orig.txt
    if "orig.txt" not in status_line(s):
        s.send(DOWN, wait=STEP)
    s.send(b"\x18l", wait=STEP)
    check("links: the hard link form opens", "Hard link" in s.screen(), s.screen()[:300])
    s.send(b"\x15")
    s.send(b"hard.txt")
    s.send(b"\r", wait=STEP * 2)
    hard = os.path.join(play, "hard.txt")
    check("links: hard link created",
          os.path.exists(hard) and not os.path.islink(hard))
    check("links: and it is the same file",
          os.path.exists(hard) and
          os.stat(hard).st_ino == os.stat(os.path.join(play, "orig.txt")).st_ino)

    # C-x C-s: change where the relative link points
    s.send(HOME_K, wait=STEP)
    for _ in range(6):
        if "rel.txt" in status_line(s):
            break
        s.send(DOWN, wait=STEP)
    check("links: on the symlink", "rel.txt" in status_line(s), status_line(s))
    s.send(b"\x18\x13", wait=STEP)          # C-x C-s
    scr = s.screen()
    check("links: edit shows the current target",
          "Edit symlink" in scr and "orig.txt" in scr, scr[:300])
    s.send(b"\x15")
    s.send(b"hard.txt")
    s.send(b"\r", wait=STEP * 2)
    check("links: the link was retargeted",
          os.path.islink(rel) and os.readlink(rel) == "hard.txt",
          os.readlink(rel) if os.path.islink(rel) else "missing")
    s.quit()
    shutil.rmtree(root)


def test_recursive_attrs():
    """PLAN4 S2: MC's advanced chown - a recursive chmod/chown runs as a
    job, and reaches everything under the directory."""
    root, play, home = sandbox()
    os.makedirs(os.path.join(play, "tree/sub"))
    open(os.path.join(play, "tree/sub/deep.txt"), "w").write("x\n")
    os.chmod(os.path.join(play, "tree/sub/deep.txt"), 0o644)
    s = Session(play, home)

    s.send(DOWN)                            # onto tree/
    s.send(b"\x18c", wait=STEP)             # Ctrl+X c, on the octal field
    s.send(b"\x15")
    s.send(b"750")
    s.send(DOWN)                            # -> recurse into directories
    check("recattrs: the box is there", "recurse into directories" in s.screen(),
          s.screen()[:600])
    s.send(b" ")
    check("recattrs: and it ticks", "[x] recurse into directories" in s.screen(),
          s.screen()[:600])
    s.send(DOWN)                            # -> the buttons
    s.send(b"\r", wait=STEP * 4)            # Set

    deep = os.path.join(play, "tree/sub/deep.txt")
    check("recattrs: the change reached the bottom",
          oct(os.stat(deep).st_mode & 0o777) == "0o750",
          oct(os.stat(deep).st_mode & 0o777))
    check("recattrs: and the directory itself",
          oct(os.stat(os.path.join(play, "tree")).st_mode & 0o777) == "0o750")

    # the chown window carries the same switch
    s.send(b"\x18o", wait=STEP)
    check("recattrs: chown offers it too", "recurse into directories" in s.screen(),
          s.screen()[:600])
    s.send(b"\t" * 2)                       # -> the recurse row
    s.send(b" ")
    check("recattrs: space ticks it", "[x] recurse into directories" in s.screen(),
          s.screen()[:600])
    s.send(b"\x1b", wait=STEP)
    s.quit()
    shutil.rmtree(root)


def test_mcimport():
    """PLAN4 S0: `rcmd --import-mc DIR` converts mc's menu, mc.ext and
    keymap into an rcmd config fragment on stdout, warning on stderr
    about anything it cannot express."""
    root, play, home = sandbox()
    mcdir = os.path.join(home, "mc")
    os.makedirs(mcdir)
    open(os.path.join(mcdir, "menu"), "w").write(
        "# menu\n"
        "+ f \\.tar\\.gz$\n"
        "a\tExtract here\n"
        "\ttar xzf %f\n"
    )
    open(os.path.join(mcdir, "mc.ext"), "w").write(
        "shell/.md\n"
        "\tOpen=glow %f\n"
        "\tView=%view{ascii} cat %f\n"
        "type/^ELF\n"
        "\tOpen=objdump -d %f\n"
    )
    open(os.path.join(mcdir, "mc.keymap"), "w").write(
        "[panel]\nCopy = f5; ctrl-c\nShowHidden = alt-dot\n"
    )
    proc = subprocess.run([BIN, "--import-mc", mcdir], capture_output=True, text=True)
    out, err = proc.stdout, proc.stderr
    check("mcimport: exits cleanly", proc.returncode == 0, err)
    check("mcimport: menu entry became a command",
          '[[commands]]' in out and "Extract here" in out and "tar xzf %f" in out, out)
    check("mcimport: mc.ext became an opener", 'match = "*.md"' in out and "glow %f" in out, out)
    check("mcimport: View became a [[view]] rule", "[[view]]" in out and "cat %f" in out, out)
    check("mcimport: keymap converted", '"f5" = "copy"' in out and '"alt+." =' in out, out)
    check("mcimport: warns about type/ matchers", "type/" in err, err)
    check("mcimport: config.toml untouched",
          not os.path.exists(os.path.join(home, ".config", "rcmd", "config.toml")))

    # what it prints must be loadable as an rcmd config
    cfgdir = os.path.join(home, ".config", "rcmd")
    os.makedirs(cfgdir, exist_ok=True)
    open(os.path.join(cfgdir, "config.toml"), "w").write(out)
    s = Session(play, home)
    check("mcimport: output loads as a config", "Modify time" in s.screen())
    s.send(b"\x1b[12~", wait=STEP)          # F2 user menu
    check("mcimport: imported command is in the F2 menu", "Extract here" in s.screen(), s.screen())
    s.send(b"\x1b", wait=STEP)
    s.send(b"\x1b", wait=STEP)
    s.quit()
    shutil.rmtree(root)


def test_keycontexts():
    """PLAN4 S0: [keys.viewer] / [keys.editor] rebind inside the viewer
    and the editor; bare [keys] entries still bind in the panel."""
    root, play, home = sandbox()
    cfgdir = os.path.join(home, ".config", "rcmd")
    os.makedirs(cfgdir)
    open(os.path.join(cfgdir, "config.toml"), "w").write(
        '[keys]\n'
        '"ctrl+y" = "swap-panels"\n'
        '\n'
        '[keys.viewer]\n'
        '"ctrl+w" = "wrap"\n'
        '"ctrl+k" = "quit"\n'
        '\n'
        '[keys.editor]\n'
        '"ctrl+q" = "quit"\n'
    )
    long_line = "viewer " + "wide " * 40
    open(os.path.join(play, "wide.txt"), "w").write(long_line + "\n")
    s = Session(play, home)

    # panel context still works from the bare table
    s.send(DOWN)                            # cursor -> wide.txt
    s.send(F3, wait=STEP)
    check("keycontexts: viewer opened", "viewer wide" in s.screen())
    # Ctrl+W now wraps, and the default F2 still does too
    s.send(b"\x17", wait=STEP)               # Ctrl+W
    wrapped = s.screen()
    check("keycontexts: [keys.viewer] rebind works",
          wrapped.count("wide wide") > 1, wrapped[-200:])
    s.send(b"\x0b", wait=STEP)               # Ctrl+K = quit (rebound)
    check("keycontexts: rebound quit leaves the viewer",
          "Modify time" in s.screen())

    # editor: Ctrl+Q quits (rebound), F2 still saves
    s.send(b"\x1b[14~", wait=STEP * 2)       # F4 edit
    check("keycontexts: editor opened", "wide.txt" in s.screen())
    s.send(b"\x11", wait=STEP * 2)           # Ctrl+Q
    check("keycontexts: rebound editor quit", "Modify time" in s.screen())
    s.quit()
    shutil.rmtree(root)


def test_options():
    """PLAN4 S0: one grouped options dialog with MC's setting surface,
    including the confirmation toggles."""
    root, play, home = sandbox()
    open(os.path.join(play, "gone.txt"), "w").write("x\n")
    s = Session(play, home)

    # F9 o p reaches the form; it now has sections
    s.send(b"\x1b[20~")
    s.send(b"o")
    s.send(b"p", wait=STEP)
    scr = s.screen()
    check("options: form opens with sections",
          "Confirmation" in scr and "Appearance" in scr and "Panel" in scr)
    check("options: confirmation toggles present",
          "Ask before deleting" in scr and "Ask before quitting" in scr)

    # walk to "Ask before deleting" and switch it off, then OK
    for _ in range(option_downs(s.screen(), "Ask before deleting")):
        s.send(DOWN)
    scr = s.screen()
    check("options: cursor skips the headings", "[x] Ask before deleting" in scr, scr)
    s.send(b" ", wait=STEP)
    check("options: toggle flips the box", "[ ] Ask before deleting" in s.screen())
    s.send(b"\r", wait=STEP)                # OK applies and writes through

    statepath = os.path.join(home, ".local", "state", "rcmd", "state.toml")
    check("options: written to state", "confirm_delete = false" in open(statepath).read())

    # with the question off, F8 deletes straight away (no dialog)
    s.send(HOME_K + DOWN)                   # cursor -> gone.txt
    s.send(F8, wait=STEP * 3)
    check("options: confirm_delete off deletes at once",
          not os.path.exists(os.path.join(play, "gone.txt")))

    # turn "Ask before quitting" on and check F10 asks
    s.send(b"\x1b[20~")
    s.send(b"o")
    s.send(b"p", wait=STEP)
    for _ in range(option_downs(s.screen(), "Ask before quitting")):
        s.send(DOWN)
    scr = s.screen()
    check("options: reached the quit toggle", "[ ] Ask before quitting" in scr, scr)
    s.send(b" ")
    s.send(b"\r", wait=STEP)
    s.send(b"\x1b[21~", wait=STEP)          # F10
    check("options: confirm_exit asks", "Quit rcmd?" in s.screen())
    s.send(b"y", wait=STEP * 2)             # ...and Yes really quits
    try:
        os.waitpid(s.pid, 0)
    except ChildProcessError:
        pass
    os.close(s.fd)
    check("options: confirm_exit yes quits", True)
    shutil.rmtree(root)


def test_keysbatch():
    """PLAN4 S0 small keys: M-h history (persisted), M-p/M-n, M-a, cd -,
    C-x !, command-line macros, and the shortened Esc timeout."""
    root, play, home = sandbox()
    os.makedirs(os.path.join(play, "one"))
    os.makedirs(os.path.join(play, "two"))
    open(os.path.join(play, "marker.txt"), "w").write("x\n")
    s = Session(play, home)

    # command-line macros: %d expands to the panel directory
    s.send(b"echo %d > macro.out\r", wait=STEP * 3)
    if not SUBSHELL:
        s.send(b"\r", wait=STEP * 2)
    out = os.path.join(play, "macro.out")
    check(
        "keys: %d expands on the command line",
        os.path.isfile(out) and play in open(out).read(),
    )

    # cd - returns to the previous directory
    s.send(b"cd one\r")
    check("keys: cd landed", play + "/one" in s.screen())
    s.send(b"cd -\r")
    check("keys: cd - goes back", play + "/one" not in s.screen() and play in s.screen())

    # M-a pastes the panel path onto the command line
    s.send(b"\x1ba", wait=STEP)
    check("keys: M-a pastes the path", play in s.screen().split("\n")[-2])

    # a lone Esc clears the line within the 250 ms timeout (was 1 s).
    # assert on text rcmd itself painted: the harness ignores the
    # clear-screen sequence, so stale shell output on that row is not
    # a reliable thing to test against.
    s.send(b"zzmarkerzz", wait=STEP)
    check("keys: typing reaches the line", "zzmarkerzz" in s.screen())
    s.send(b"\x1b", wait=STEP)          # single Esc, no follow-up key
    check("keys: lone Esc clears the line", "zzmarkerzz" not in s.screen())

    # M-p walks history backwards (same as C-p)
    s.send(b"\x1bp", wait=STEP)
    check("keys: M-p recalls history", "cd -" in s.screen().split("\n")[-2])
    s.send(b"\x1b", wait=STEP)

    # M-h lists the history and Enter puts an entry back on the line
    s.send(b"\x1bh", wait=STEP)
    check("keys: M-h opens the history", "Command history" in s.screen())
    s.send(b"\r", wait=STEP)
    check("keys: M-h picks an entry", "cd -" in s.screen().split("\n")[-2])
    s.send(b"\x1b", wait=STEP)

    # C-x ! panelizes a command's output
    s.send(b"\x18!", wait=STEP)
    check("keys: C-x ! opens panelize", "Panelize" in s.screen())
    s.send(b"echo marker.txt\r", wait=STEP * 2)
    check("keys: C-x ! panelized", "cmd: echo marker.txt" in s.screen())
    s.send(b"\x12", wait=STEP)          # Ctrl+R restores the listing
    s.quit()

    # history survived the session, in the state file
    statepath = os.path.join(home, ".local", "state", "rcmd", "state.toml")
    st = open(statepath).read()
    check("keys: history persisted to state", "cmd_history" in st and "cd one" in st, st)

    s = Session(play, home)
    s.send(b"\x1bh", wait=STEP)
    check("keys: history restored next run", "cd one" in s.screen())
    # Enter closes the dialog by picking a line. (A lone Esc would arm
    # the meta prefix for a second and swallow quit()'s F10.)
    s.send(b"\r", wait=STEP)
    s.quit()
    shutil.rmtree(root)


def test_configstate():
    """PLAN4 S0: config.toml is the user's and never rewritten; everything
    rcmd changes lives in $XDG_STATE_HOME/rcmd/state.toml."""
    root, play, home = sandbox()
    cfgdir = os.path.join(home, ".config", "rcmd")
    os.makedirs(cfgdir)
    cfgpath = os.path.join(cfgdir, "config.toml")
    open(cfgpath, "w").write(
        "# hand-written config: this comment must survive\n"
        'theme = "mc"\n'
        "show_hidden = false\n"
        'sort_key = "mtime"\n'
        "\n"
        "[[hotlist]]\n"
        'label = "home"\n'
        'path = "%s"\n' % home
    )
    open(os.path.join(play, "visible.txt"), "w").write("x\n")
    open(os.path.join(play, ".secret"), "w").write("x\n")
    s = Session(play, home)
    # the harness prepends its own subshell line before rcmd ever starts
    written = open(cfgpath).read()
    statepath = os.path.join(home, ".local", "state", "rcmd", "state.toml")

    # migration: the state keys still in config.toml seed state.toml once
    check("config/state: state file created at startup", os.path.isfile(statepath))
    st = open(statepath).read() if os.path.isfile(statepath) else ""
    check(
        "config/state: state keys migrated out of the config",
        'sort_key = "mtime"' in st and "show_hidden = false" in st,
        st,
    )
    # ...and theme is not one of them: it stays the user's to set
    check("config/state: non-state keys not migrated", "theme" not in st, st)

    # the merged view still drives behaviour (hidden files stay hidden)
    scr = s.screen()
    check(
        "config/state: config still applies",
        "visible.txt" in scr and ".secret" not in scr,
    )

    # a hotlist edit goes to state, never to the user's file
    s.send(ALT_UP, wait=STEP)              # hotlist (Ctrl+\ equivalent)
    # the pinned entry points at $HOME, so it renders abbreviated as ~
    check(
        "config/state: hotlist read from config",
        "Directory hotlist" in s.screen() and "home" in s.screen(),
        s.screen()[-400:],
    )
    s.send(b"a", wait=STEP)                # add the current directory
    s.send(b"\r", wait=STEP)               # Enter on a row closes the dialog
    s.quit()

    st = open(statepath).read()
    check("config/state: hotlist saved to state", play in st, st)
    check(
        "config/state: user config untouched",
        open(cfgpath).read() == written and "must survive" in open(cfgpath).read(),
    )

    # second run: state wins, and the added hotlist entry is still there
    s = Session(play, home)
    s.send(ALT_UP, wait=STEP)
    check("config/state: state survives a restart", play in s.screen())
    s.send(b"\r", wait=STEP)
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
    s.send(F10, wait=STEP * 2)          # quit (no confirm - just saved)
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

    # R4: F5/F6 block ops - duplicate the first line, then cut one copy
    s.send(F4, wait=STEP * 2)
    s.send(F5)                          # no selection: duplicate line 1
    s.send(F6)                          # cut the duplicate (clipboard)
    s.send(b"\x16")                     # Ctrl+V pastes it back
    s.send(F2)
    s.send(F10, wait=STEP * 2)
    check("editor: F5 duplicated the line",
          open(path).read().count("alpha") == 2)

    # R4: Alt+W soft-wrap - the tail of a long line becomes visible
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
        print("SKIP sftp (no python with paramiko - pip install paramiko)")
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
            wait_for(s, "done -")
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
            wait_for(s, "done -")
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
        print("SKIP sftp-auth (no python with paramiko - pip install paramiko)")
        return
    keygen = shutil.which("ssh-keygen")
    if keygen is None:
        print("SKIP sftp-auth (no ssh-keygen)")
        return
    root, play, home = sandbox()
    remote = os.path.join(root, "remote")
    os.makedirs(remote)
    open(os.path.join(remote, "server.txt"), "w").write("from the server\n")

    # an encrypted PEM key in the sandbox home - rcmd must ask for its
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
        s.send(b"wrong\r", wait=STEP * 2)    # rejected - the prompt returns
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
        # (and no passphrase prompt - publickey is not on offer)
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
        # rcmd waits out slow shell startups - compinit and the like)
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
        test_cmdarchive,
        test_cpio,
        test_deb,
        test_rpm,
        test_iso,
        test_patch,
        test_mbox,
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
        test_brief,
        test_layout,
        test_tree,
        test_userformat,
        test_highlight,
        test_panelmenus,
        test_overwrite,
        test_copyform,
        test_masks,
        test_chmod,
        test_chown,
        test_confirmations,
        test_links,
        test_recursive_attrs,
        test_mcimport,
        test_keycontexts,
        test_options,
        test_keysbatch,
        test_configstate,
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
