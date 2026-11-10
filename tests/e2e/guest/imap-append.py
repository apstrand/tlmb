#!/usr/bin/env python3
"""APPEND fixture-render NDJSON into Dovecot on loopback. Never leaves the machine."""

from __future__ import annotations

import argparse
import base64
import imaplib
import json
import sys


def empty_every_folder(imap):
    """Removes what earlier scenarios appended.

    A scenario is the whole mailbox, and the interfaces sync as they open: mail
    left on the server by the last run would arrive in this one and move the
    list under the test.
    """
    status, folders = imap.list()
    if status != "OK":
        return
    for entry in folders or []:
        text = entry.decode() if isinstance(entry, bytes) else str(entry)
        if "\\Noselect" in text:
            continue
        # `(flags) "delim" name`, the name possibly quoted.
        name = text.rsplit(" ", 1)[-1] if not text.endswith('"') else '"' + text.rsplit(' "', 1)[-1]
        if imap.select(name)[0] != "OK":
            continue
        if imap.search(None, "ALL")[1][0]:
            imap.store("1:*", "+FLAGS.SILENT", "(\\Deleted)")
            imap.expunge()
    if imap.state == "SELECTED":
        imap.close()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=143)
    parser.add_argument("--user", default="e2e")
    parser.add_argument("--password", default="testpass")
    args = parser.parse_args()

    imap = imaplib.IMAP4(args.host, args.port)
    imap.login(args.user, args.password)
    empty_every_folder(imap)

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
