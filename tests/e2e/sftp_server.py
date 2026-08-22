#!/usr/bin/env python3
"""Minimal SFTP server for the e2e suite (paramiko), serving the real
filesystem.

Usage: sftp_server.py PORT   (binds 127.0.0.1, prints READY when up)
$RCMD_SFTP_AUTH picks the accepted method:
  password (default) - $RCMD_SFTP_PASSWORD (default "secret")
  pubkey             - the OpenSSH .pub file at $RCMD_SFTP_PUBKEY
  interactive        - keyboard-interactive, two prompts ("fish", "chips")
"""
import base64
import os
import socket
import sys

import paramiko
from paramiko import (
    AUTH_FAILED,
    AUTH_SUCCESSFUL,
    OPEN_SUCCEEDED,
    SFTP_OK,
    SFTPAttributes,
    SFTPHandle,
    SFTPServer,
    SFTPServerInterface,
    ServerInterface,
)

PASSWORD = os.environ.get("RCMD_SFTP_PASSWORD", "secret")
AUTH = os.environ.get("RCMD_SFTP_AUTH", "password")
PUBKEY = os.environ.get("RCMD_SFTP_PUBKEY")


class Server(ServerInterface):
    def check_auth_password(self, username, password):
        if AUTH != "password":
            return AUTH_FAILED
        return AUTH_SUCCESSFUL if password == PASSWORD else AUTH_FAILED

    def check_auth_publickey(self, username, key):
        if AUTH != "pubkey" or not PUBKEY:
            return AUTH_FAILED
        blob = base64.b64decode(open(PUBKEY).read().split()[1])
        return AUTH_SUCCESSFUL if key.asbytes() == blob else AUTH_FAILED

    def check_auth_interactive(self, username, submethods):
        if AUTH != "interactive":
            return AUTH_FAILED
        q = paramiko.server.InteractiveQuery("rcmd", "two words open the door")
        q.add_prompt("Word one:", False)
        q.add_prompt("Word two:", False)
        return q

    def check_auth_interactive_response(self, responses):
        return AUTH_SUCCESSFUL if list(responses) == ["fish", "chips"] else AUTH_FAILED

    def check_channel_request(self, kind, chanid):
        return OPEN_SUCCEEDED

    def get_allowed_auths(self, username):
        return {
            "password": "password",
            "pubkey": "publickey",
            "interactive": "keyboard-interactive",
        }[AUTH]


class Handle(SFTPHandle):
    def stat(self):
        try:
            return SFTPAttributes.from_stat(os.fstat(self.readfile.fileno()))
        except OSError as e:
            return SFTPServer.convert_errno(e.errno)

    def chattr(self, attr):
        try:
            SFTPServer.set_file_attr(self.filename, attr)
            return SFTP_OK
        except OSError as e:
            return SFTPServer.convert_errno(e.errno)


def errno_or(fn):
    try:
        return fn()
    except OSError as e:
        return SFTPServer.convert_errno(e.errno)


class Sftp(SFTPServerInterface):
    def list_folder(self, path):
        def go():
            out = []
            for name in os.listdir(path):
                attr = SFTPAttributes.from_stat(os.lstat(os.path.join(path, name)))
                attr.filename = name
                out.append(attr)
            return out

        return errno_or(go)

    def stat(self, path):
        return errno_or(lambda: SFTPAttributes.from_stat(os.stat(path)))

    def lstat(self, path):
        return errno_or(lambda: SFTPAttributes.from_stat(os.lstat(path)))

    def open(self, path, flags, attr):
        mode = attr.st_mode if attr.st_mode else 0o644
        try:
            fd = os.open(path, flags, mode)
        except OSError as e:
            return SFTPServer.convert_errno(e.errno)
        if flags & os.O_WRONLY:
            fmode = "ab" if flags & os.O_APPEND else "wb"
        elif flags & os.O_RDWR:
            fmode = "a+b" if flags & os.O_APPEND else "r+b"
        else:
            fmode = "rb"
        f = os.fdopen(fd, fmode)
        handle = Handle(flags)
        handle.filename = path
        handle.readfile = f
        handle.writefile = f
        return handle

    def remove(self, path):
        return errno_or(lambda: (os.remove(path), SFTP_OK)[1])

    def rename(self, oldpath, newpath):
        return errno_or(lambda: (os.rename(oldpath, newpath), SFTP_OK)[1])

    def posix_rename(self, oldpath, newpath):
        return errno_or(lambda: (os.replace(oldpath, newpath), SFTP_OK)[1])

    def mkdir(self, path, attr):
        return errno_or(lambda: (os.mkdir(path), SFTP_OK)[1])

    def rmdir(self, path):
        return errno_or(lambda: (os.rmdir(path), SFTP_OK)[1])

    def chattr(self, path, attr):
        return errno_or(lambda: (SFTPServer.set_file_attr(path, attr), SFTP_OK)[1])

    def symlink(self, target_path, path):
        return errno_or(lambda: (os.symlink(target_path, path), SFTP_OK)[1])

    def readlink(self, path):
        return errno_or(lambda: os.readlink(path))


def main():
    port = int(sys.argv[1])
    host_key = paramiko.ECDSAKey.generate()
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(("127.0.0.1", port))
    sock.listen(8)
    print("READY", flush=True)
    transports = []
    while True:
        conn, _ = sock.accept()
        t = paramiko.Transport(conn)
        t.add_server_key(host_key)
        t.set_subsystem_handler("sftp", SFTPServer, Sftp)
        t.start_server(server=Server())
        transports.append(t)


if __name__ == "__main__":
    main()
