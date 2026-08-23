#!/usr/bin/env python3
"""A minimal FTP server for the e2e suite and rcmd-core's own tests.

Stdlib only - pyftpdlib is not something a test should need installed.
It speaks the subset rcmd uses: USER/PASS, PWD, CWD, CDUP, TYPE, PASV,
EPSV, LIST, MLSD, NLST, RETR, STOR, DELE, MKD, RMD, RNFR/RNTO, SIZE, MDTM,
MFMT, SITE CHMOD, QUIT. One thread per control connection.

Usage: ftp_server.py ROOT [PORT]   - prints "READY <port>" on stdout.
"""
import os
import socket
import stat
import sys
import threading
import time

USER, PASSWORD = "tester", "secret"


class Session(threading.Thread):
    def __init__(self, conn, root):
        super().__init__(daemon=True)
        self.conn = conn
        self.root = os.path.realpath(root)
        self.cwd = "/"
        self.rest = None
        self.pasv = None
        self.rename_from = None
        self.authed = False

    # --- plumbing -----------------------------------------------------
    def send(self, text):
        self.conn.sendall((text + "\r\n").encode())

    def local(self, path):
        """Map a client path onto the served tree, refusing escapes."""
        if not path.startswith("/"):
            path = self.cwd.rstrip("/") + "/" + path
        full = os.path.realpath(os.path.join(self.root, path.lstrip("/")))
        if full != self.root and not full.startswith(self.root + os.sep):
            return None
        return full

    def open_data(self):
        sock, self.pasv = self.pasv, None
        if sock is None:
            self.send("425 Use PASV first")
            return None
        conn, _ = sock.accept()
        sock.close()
        return conn

    # --- listings -----------------------------------------------------
    def list_lines(self, full, machine):
        names = sorted(os.listdir(full)) if os.path.isdir(full) else []
        for name in names:
            path = os.path.join(full, name)
            st = os.lstat(path)
            when = time.strftime("%Y%m%d%H%M%S", time.gmtime(st.st_mtime))
            if machine:
                if stat.S_ISDIR(st.st_mode):
                    kind, size = "dir", f"sizd={st.st_size};"
                elif stat.S_ISLNK(st.st_mode):
                    kind, size = "OS.unix=slink:" + os.readlink(path), "size=0;"
                else:
                    kind, size = "file", f"size={st.st_size};"
                yield (f"type={kind};{size}modify={when};"
                       f"UNIX.mode={oct(stat.S_IMODE(st.st_mode))[2:].zfill(4)}; {name}")
            else:
                mode = stat.filemode(st.st_mode)
                shown = time.strftime("%b %d %H:%M", time.gmtime(st.st_mtime))
                suffix = " -> " + os.readlink(path) if stat.S_ISLNK(st.st_mode) else ""
                yield (f"{mode} 1 ftp ftp {st.st_size:>8} {shown} {name}{suffix}")

    # --- the loop -----------------------------------------------------
    def run(self):
        try:
            self.send("220 rcmd test server")
            rfile = self.conn.makefile("r", encoding="utf-8", newline="\r\n")
            for line in rfile:
                line = line.rstrip("\r\n")
                if not line:
                    continue
                verb, _, arg = line.partition(" ")
                if not self.handle(verb.upper(), arg):
                    break
        except OSError:
            pass
        finally:
            try:
                self.conn.close()
            except OSError:
                pass

    def handle(self, verb, arg):
        if verb == "USER":
            self.send("331 Password required" if arg == USER else "530 No such user")
            return True
        if verb == "PASS":
            self.authed = arg == PASSWORD
            self.send("230 Logged in" if self.authed else "530 Wrong password")
            return True
        if not self.authed:
            self.send("530 Log in first")
            return True
        if verb == "QUIT":
            self.send("221 Bye")
            return False
        if verb == "TYPE":
            self.send("200 Type set")
        elif verb == "PWD":
            self.send(f'257 "{self.cwd}" is the current directory')
        elif verb in ("CWD", "CDUP"):
            target = ".." if verb == "CDUP" else arg
            full = self.local(target)
            if full and os.path.isdir(full):
                rel = os.path.relpath(full, self.root)
                self.cwd = "/" if rel == "." else "/" + rel
                self.send("250 Directory changed")
            else:
                self.send("550 No such directory")
        elif verb == "PASV":
            sock = socket.socket()
            sock.bind(("127.0.0.1", 0))
            sock.listen(1)
            self.pasv = sock
            port = sock.getsockname()[1]
            self.send(f"227 Entering Passive Mode (127,0,0,1,{port >> 8},{port & 255})")
        elif verb == "EPSV":
            sock = socket.socket()
            sock.bind(("127.0.0.1", 0))
            sock.listen(1)
            self.pasv = sock
            self.send(f"229 Entering Extended Passive Mode (|||{sock.getsockname()[1]}|)")
        elif verb in ("LIST", "MLSD", "NLST"):
            full = self.local(arg or self.cwd)
            if full is None or not os.path.isdir(full):
                self.send("550 No such directory")
                return True
            data = self.open_data()
            if data is None:
                return True
            self.send("150 Here it comes")
            if verb == "NLST":
                body = "".join(n + "\r\n" for n in sorted(os.listdir(full)))
            else:
                body = "".join(l + "\r\n" for l in self.list_lines(full, verb == "MLSD"))
            data.sendall(body.encode())
            data.close()
            self.send("226 Listing sent")
        elif verb == "RETR":
            full = self.local(arg)
            if full is None or not os.path.isfile(full):
                self.send("550 No such file")
                return True
            data = self.open_data()
            if data is None:
                return True
            self.send("150 Here it comes")
            with open(full, "rb") as f:
                data.sendall(f.read())
            data.close()
            self.send("226 Transfer complete")
        elif verb == "STOR":
            full = self.local(arg)
            if full is None:
                self.send("550 Bad path")
                return True
            data = self.open_data()
            if data is None:
                return True
            self.send("150 Send it")
            with open(full, "wb") as f:
                while True:
                    chunk = data.recv(65536)
                    if not chunk:
                        break
                    f.write(chunk)
            data.close()
            self.send("226 Stored")
        elif verb == "DELE":
            full = self.local(arg)
            try:
                os.remove(full)
                self.send("250 Deleted")
            except OSError:
                self.send("550 Cannot delete")
        elif verb == "MKD":
            full = self.local(arg)
            try:
                os.mkdir(full)
                self.send(f'257 "{arg}" created')
            except OSError:
                self.send("550 Cannot create")
        elif verb == "RMD":
            full = self.local(arg)
            try:
                os.rmdir(full)
                self.send("250 Removed")
            except OSError:
                self.send("550 Cannot remove")
        elif verb == "RNFR":
            self.rename_from = self.local(arg)
            self.send("350 Ready for RNTO")
        elif verb == "RNTO":
            try:
                os.rename(self.rename_from, self.local(arg))
                self.send("250 Renamed")
            except OSError:
                self.send("550 Cannot rename")
        elif verb == "SIZE":
            full = self.local(arg)
            try:
                self.send(f"213 {os.path.getsize(full)}")
            except OSError:
                self.send("550 No such file")
        elif verb == "MDTM":
            full = self.local(arg)
            try:
                self.send("213 " + time.strftime("%Y%m%d%H%M%S",
                                                 time.gmtime(os.path.getmtime(full))))
            except OSError:
                self.send("550 No such file")
        elif verb == "MFMT":
            when, _, path = arg.partition(" ")
            full = self.local(path)
            try:
                stamp = time.mktime(time.strptime(when, "%Y%m%d%H%M%S")) - time.timezone
                os.utime(full, (stamp, stamp))
                self.send("213 Modify=" + when + "; " + path)
            except (OSError, ValueError):
                self.send("550 Cannot set time")
        elif verb == "SITE":
            what, _, rest = arg.partition(" ")
            mode, _, path = rest.partition(" ")
            if what.upper() != "CHMOD":
                self.send("500 Unknown SITE command")
                return True
            try:
                os.chmod(self.local(path), int(mode, 8))
                self.send("200 Mode set")
            except (OSError, ValueError):
                self.send("550 Cannot set mode")
        else:
            self.send("500 Unknown command")
        return True


def main():
    root = sys.argv[1]
    port = int(sys.argv[2]) if len(sys.argv) > 2 else 0
    server = socket.socket()
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(("127.0.0.1", port))
    server.listen(8)
    print(f"READY {server.getsockname()[1]}", flush=True)
    while True:
        conn, _ = server.accept()
        Session(conn, root).start()


if __name__ == "__main__":
    main()
