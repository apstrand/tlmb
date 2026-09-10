#!/usr/bin/env python3
"""A submission sink on loopback. Accepts DATA, writes the bytes, talks to nobody.

Hand-rolled: Python 3.13 dropped `smtpd`/`asyncore`, and we do not want a
dependency that would pull in the network to install."""

from __future__ import annotations

import os
import socket
import sys
import time
from pathlib import Path

OUT = Path(os.environ.get("MAILBOX_E2E_SENT", "/tmp/mailbox-e2e-sent"))
PORT = int(os.environ.get("MAILBOX_E2E_SMTP_PORT", "2525"))


def handle(conn: socket.socket) -> None:
    conn.sendall(b"220 mailbox-e2e sink\r\n")
    buf = b""
    data = bytearray()
    in_data = False
    mailfrom = "unknown"
    rcpt: list[str] = []
    while True:
        chunk = conn.recv(4096)
        if not chunk:
            return
        buf += chunk
        while b"\r\n" in buf:
            line, buf = buf.split(b"\r\n", 1)
            if in_data:
                if line == b".":
                    OUT.mkdir(parents=True, exist_ok=True)
                    name = f"{int(time.time() * 1000)}-{mailfrom.replace('@', '_at_')}.eml"
                    path = OUT / name
                    path.write_bytes(bytes(data))
                    print(f"sank {path} from={mailfrom} to={','.join(rcpt)}", flush=True)
                    in_data = False
                    data.clear()
                    conn.sendall(b"250 ok\r\n")
                else:
                    if line.startswith(b"."):
                        line = line[1:]
                    data.extend(line)
                    data.extend(b"\r\n")
                continue
            verb = line.split(b" ", 1)[0].upper()
            if verb == b"EHLO":
                conn.sendall(b"250-sink\r\n250 AUTH PLAIN LOGIN\r\n")
            elif verb == b"HELO":
                conn.sendall(b"250 sink\r\n")
            elif verb == b"AUTH":
                # Accept anything. This is a loopback sink, not an MTA.
                conn.sendall(b"235 ok\r\n")
            elif verb == b"MAIL":
                mailfrom = line.decode("utf-8", "replace")
                conn.sendall(b"250 ok\r\n")
            elif verb == b"RCPT":
                rcpt.append(line.decode("utf-8", "replace"))
                conn.sendall(b"250 ok\r\n")
            elif verb == b"DATA":
                in_data = True
                conn.sendall(b"354 go\r\n")
            elif verb == b"RSET":
                mailfrom, rcpt = "unknown", []
                conn.sendall(b"250 ok\r\n")
            elif verb == b"QUIT":
                conn.sendall(b"221 bye\r\n")
                return
            else:
                conn.sendall(b"250 ok\r\n")


def main() -> None:
    sock = socket.socket()
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(("127.0.0.1", PORT))
    sock.listen(5)
    print(f"smtp sink on 127.0.0.1:{PORT} -> {OUT}", flush=True)
    try:
        while True:
            conn, _ = sock.accept()
            with conn:
                handle(conn)
    except KeyboardInterrupt:
        sys.exit(0)


if __name__ == "__main__":
    main()
