#!/usr/bin/env python3
"""Drives `mailbox tui` in tmux and checks that the page keys page.

The `paging` scenario numbers every subject ("Paging 001" is the newest) and
every line of one long letter ("line 001" …), so where the cursor is and how
far the reader scrolled can be read straight off the screen.

Checks land in tui-paging-checks.json beside the pane captures."""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

EVIDENCE = Path(os.environ.get("EVIDENCE", Path.home() / "mailbox-e2e-out"))
PANE = "mailbox-tui"
ROWS, COLS = 40, 120
# Header, status and prompt take a row each; the rest is the list or message.
CONTENT = ROWS - 3

SGR = re.compile(r"\x1b\[([0-9;:]*)m")
CHECKS: list[dict] = []


def tmux(*args: str) -> str:
    return subprocess.run(
        ["tmux", *args], check=True, capture_output=True, text=True
    ).stdout


def screen(escapes: bool = False) -> list[str]:
    flags = "-pe" if escapes else "-p"
    return tmux("capture-pane", "-t", PANE, flags).split("\n")[:ROWS]


def settle() -> list[str]:
    """Waits until two captures agree, so a slow repaint is not read half done."""
    last = screen(True)
    for _ in range(40):
        time.sleep(0.1)
        now = screen(True)
        if now == last:
            return now
        last = now
    return last


def keys(*names: str) -> list[str]:
    for name in names:
        tmux("send-keys", "-t", PANE, name)
    return settle()


def reversed_on(line: str) -> bool:
    """True when some SGR on the line turns on reverse video (7), not counting
    a 7 that is part of a 38;5;n / 48;2;r;g;b colour."""
    for m in SGR.finditer(line):
        params = [p for p in re.split("[;:]", m.group(1)) if p != ""]
        i = 0
        while i < len(params):
            p = params[i]
            if p in ("38", "48", "58") and i + 1 < len(params):
                i += 3 if params[i + 1] == "5" else 5
                continue
            if p == "7":
                return True
            i += 1
    return False


def cursor(lines: list[str]) -> int | None:
    """The number of the "Paging NNN" subject on the reversed list row."""
    for line in lines[1 : 1 + CONTENT]:
        if reversed_on(line):
            m = re.search(r"Paging (\d{3})", SGR.sub("", line))
            if m:
                return int(m.group(1))
    return None


def top_line(lines: list[str]) -> int | None:
    """The first "line NNN" of the long letter that is on screen."""
    for line in lines[1 : 1 + CONTENT]:
        m = re.search(r"\bline (\d{3})\b", SGR.sub("", line))
        if m:
            return int(m.group(1))
    return None


def save(name: str, lines: list[str]) -> None:
    (EVIDENCE / f"tui-paging-{name}.txt").write_text("\n".join(lines))


def check(name: str, ok: bool, detail: str) -> None:
    CHECKS.append({"check": name, "ok": ok, "detail": detail})
    print(f"  {'PASS' if ok else 'FAIL'}  {name}: {detail}", flush=True)


def start() -> None:
    subprocess.run(["tmux", "kill-session", "-t", PANE], capture_output=True)
    tmux("new-session", "-d", "-s", PANE, "-x", str(COLS), "-y", str(ROWS))
    env = " ".join(
        f"{k}={os.environ[k]}"
        for k in ("MAILBOX_CONFIG", "MAILBOX_STATE_DIR", "MAILBOX_CLOCK")
        if k in os.environ
    )
    tmux("send-keys", "-t", PANE,
         f"export {env} MAILBOX_PASSWORD_E2E=testpass MAILBOX_NOTIFIER=none", "Enter")
    tmux("send-keys", "-t", PANE, "mailbox tui", "Enter")
    for _ in range(75):
        if cursor(screen(True)) is not None:
            return
        time.sleep(0.2)
    save("start-timeout", screen(True))
    sys.exit("the list never showed a selected 'Paging' row")


def list_paging(down: str, up: str, label: str) -> None:
    # A page is what fits, give or take a line or two of context. One row is
    # a `j`, not a page — that is the bug this was written for.
    lo, hi = CONTENT // 2, CONTENT
    first = cursor(keys("g"))
    save(f"list-{label}-0-top", screen(True))
    one = cursor(keys(down))
    save(f"list-{label}-1-down", screen(True))
    two = cursor(keys(down))
    save(f"list-{label}-2-down", screen(True))
    back = cursor(keys(up))
    home = cursor(keys(up))
    save(f"list-{label}-3-up-up", screen(True))
    if None in (first, one, two, back, home):
        check(f"list {label}", False,
              f"cursor not readable: top={first} down={one} down={two} up={back} up={home}")
        return
    step = one - first
    check(f"list {label} pages down", lo <= step <= hi,
          f"Paging {first:03} -> {one:03}: moved {step} rows, a page is {lo}..{hi}")
    check(f"list {label} pages down again", two - one == step,
          f"-> {two:03}: moved {two - one} rows")
    check(f"list {label} pages back up", back == one and home == first,
          f"{up} twice: -> {back:03} -> {home:03}, expected {one:03} -> {first:03}")


def message_paging(down: str, up: str, label: str) -> None:
    lo, hi = CONTENT // 2, CONTENT
    # The long letter is the oldest, so it is at the bottom of the list.
    keys("G")
    opened = keys("Enter")
    save(f"message-{label}-0-open", opened)
    first = top_line(opened)
    if first is None:
        check(f"message {label}", False, "the long letter did not open")
        keys("Escape")
        return
    one = top_line(keys(down))
    save(f"message-{label}-1-down", screen(True))
    home = top_line(keys(up))
    save(f"message-{label}-2-up", screen(True))
    step = (one or first) - first
    check(f"message {label} scrolls down", lo <= step <= hi,
          f"top line {first:03} -> {one}: scrolled {step} lines, a page is {lo}..{hi}")
    check(f"message {label} scrolls back up", home == first,
          f"top line back to {home}, expected {first:03}")
    keys("Escape")


def main() -> int:
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    try:
        start()
        print(f"terminal {COLS}x{ROWS}, {CONTENT} content rows", flush=True)
        for down, up, label in (("NPage", "PPage", "PageDown-PageUp"),
                                ("C-f", "C-b", "C-f-C-b")):
            list_paging(down, up, label)
            message_paging(down, up, label)
        tmux("send-keys", "-t", PANE, "q")
        time.sleep(0.3)
    finally:
        # A TUI left running holds the binary the next sync copies over.
        subprocess.run(["tmux", "kill-session", "-t", PANE], capture_output=True)

    (EVIDENCE / "tui-paging-checks.json").write_text(json.dumps(CHECKS, indent=2))
    failed = [c for c in CHECKS if not c["ok"]]
    print(f"tui-paging: {len(CHECKS) - len(failed)}/{len(CHECKS)} checks passed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
