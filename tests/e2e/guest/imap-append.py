#!/usr/bin/env python3
"""APPEND fixture-render NDJSON into Dovecot on loopback. Never leaves the machine."""

from __future__ import annotations

import argparse
import base64
import imaplib
import json
import sys


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=143)
    parser.add_argument("--user", default="e2e")
    parser.add_argument("--password", default="testpass")
    args = parser.parse_args()

    imap = imaplib.IMAP4(args.host, args.port)
    imap.login(args.user, args.password)

    created = set()
    n = 0
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        row = json.loads(line)
        folder = row.get("folder") or "INBOX"
        if folder not in created:
            try:
                imap.create(folder)
            except imaplib.IMAP4.error:
                pass
            created.add(folder)
        flags = " ".join(row.get("flags") or [])
        raw = base64.b64decode(row["bytes_base64"])
        # INTERNALDATE on APPEND wants IMAP's date grammar. The RFC5322 Date
        # header is already in the bytes; letting Dovecot stamp now is fine.
        imap.append(folder, flags or None, None, raw)
        n += 1
        print(f"appended {row.get('name')} -> {folder}", flush=True)

    imap.logout()
    print(f"appended {n} messages", flush=True)


if __name__ == "__main__":
    main()
