#!/usr/bin/env python3
"""Local SFTP lab, for checking the file panel without a real server.

Serves SSH on 127.0.0.1:2244 with a password-authenticated user and an SFTP
subsystem chrooted to a scratch directory. The shell channel stays open and
echoes what it receives, because the client requests a PTY and a shell before
it opens the SFTP channel -- closing either would retire the session handle.

    python3 -m pip install asyncssh
    python3 tools/sftp_lab.py

    # in another terminal
    LAB_USER=lab LAB_PASS=labpass ./target/debug/sftpprobe

The host key is pinned to <root>/../hostkey across restarts: the client's
TOFU check rejects a changed key (correctly), which would make reconnect
testing impossible.
"""
import asyncio
import os
import pathlib
import random
import sys

import asyncssh

ROOT = pathlib.Path(os.environ.get("LAB_ROOT", "/tmp/sftp-lab/root"))
HOSTKEY = ROOT.parent / "hostkey"
PORT = int(os.environ.get("LAB_PORT", "2244"))
USER = os.environ.get("LAB_USER", "lab")
PASSWORD = os.environ.get("LAB_PASS", "labpass")
BIG = 4 * 1024 * 1024


class LabSFTP(asyncssh.SFTPServer):
    def __init__(self, chan):
        super().__init__(chan, chroot=str(ROOT))


class LabSSH(asyncssh.SSHServer):
    def begin_auth(self, username):
        return True

    def password_auth_supported(self):
        return True

    def validate_password(self, username, password):
        return username == USER and password == PASSWORD

    def public_key_auth_supported(self):
        return True

    def validate_public_key(self, username, key):
        # Any key is accepted: the point is exercising the client's SFTP
        # channel, not its authentication policy.
        return username == USER


async def hold_shell(process):
    try:
        process.stdout.write("lab shell\r\n")
        while True:
            line = await process.stdin.readline()
            if not line:
                break
            text = line.decode(errors="replace").rstrip("\r\n")
            process.stdout.write("ECHO:" + text + "\r\n")
    except (asyncio.CancelledError, ConnectionError):
        pass


def host_key():
    if HOSTKEY.exists():
        return asyncssh.read_private_key(HOSTKEY)
    key = asyncssh.generate_private_key("ssh-ed25519")
    HOSTKEY.write_bytes(key.export_private_key())
    return key


def seed():
    ROOT.mkdir(parents=True, exist_ok=True)
    sub = ROOT / "sub"
    if not sub.exists():
        sub.mkdir()
    (ROOT / "hello.txt").write_text("hello from the sftp lab\n")
    (sub / "nested.txt").write_text("nested file\n")
    big = ROOT / "big.bin"
    if not big.exists() or big.stat().st_size != BIG:
        random.seed(7)
        big.write_bytes(bytes(random.getrandbits(8) for _ in range(BIG)))


async def main():
    seed()
    await asyncssh.create_server(
        LabSSH,
        "127.0.0.1",
        PORT,
        server_host_keys=[host_key()],
        sftp_factory=LabSFTP,
        process_factory=hold_shell,
    )
    print(f"lab ready on 127.0.0.1:{PORT} user={USER} pass={PASSWORD} root={ROOT}", flush=True)
    await asyncio.Event().wait()


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        sys.exit(0)
