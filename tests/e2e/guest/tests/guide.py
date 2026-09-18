"""Exercise and photograph the short user guide.

Every image used by docs/USER-GUIDE.md is made here.  The checks alongside
the screenshots keep them from becoming a gallery of states the app can no
longer reach.

Usage: guide.py setup|mail <url> <evidence-dir> <token>
"""

import json
import os
import sys
import time
import urllib.parse
import urllib.request

import cdp


PHASE, URL, EVIDENCE, TOKEN = sys.argv[1:]


def check(label, ok, detail=""):
    print(("ok  " if ok else "FAIL") + label + (f" — {detail}" if detail else ""), flush=True)
    if not ok:
        raise cdp.BrowserError(label + (f": {detail}" if detail else ""))


def api(path):
    request = urllib.request.Request(
        urllib.parse.urljoin(URL, path), headers={"X-Mailbox-Token": TOKEN}
    )
    with urllib.request.urlopen(request, timeout=10) as response:
        return json.load(response)


def api_post(path, body):
    request = urllib.request.Request(
        urllib.parse.urljoin(URL, path),
        data=json.dumps(body).encode(),
        headers={"X-Mailbox-Token": TOKEN, "Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=25) as response:
        return json.load(response)


def setup(page):
    page.viewport(1280, 900)
    page.goto(URL)
    page.wait_for("document.body.dataset.state === 'setup'", what="first-run setup")
    for selector in ("#setup-address", "#setup-password", "#setup-add", "#setup-test"):
        check(f"setup exposes {selector}", page.rect(selector)["shown"])
    page.fill("#setup-address", "you@example.com")
    page.fill("#setup-password", "not-a-real-password")
    page.fill("#setup-id", "personal")
    check("server overrides are available", page.rect("#setup-servers")["shown"])
    page.shot(f"{EVIDENCE}/guide-setup.png")

    # The fixture IMAP server deliberately speaks plaintext on loopback. The
    # public form has no insecure-transport switch, so exercise its two routes
    # with the same account plus the test-only `none` transport carried by the
    # request. This proves Test and Add all the way through login and config
    # persistence without teaching the real UI to offer a dangerous checkbox.
    account = {
        "id": "e2e",
        "address": "peter@example.com",
        "password": "testpass",
        "imap_host": "127.0.0.1",
        "imap_port": 143,
        "imap_user": "e2e",
        "imap_security": "none",
        "smtp_host": "127.0.0.1",
        "smtp_port": 2525,
        "smtp_user": "e2e",
        "smtp_security": "none",
    }
    tested = api_post("api/accounts/test", account)
    check("Test logs in without saving", tested["login_user"] == "e2e")
    check("Test reports the mailbox", tested["folders"] > 0)
    added = api_post("api/accounts", account)
    check("Add persists the checked account", added["id"] == "e2e")
    page.goto(URL)
    page.wait_for("document.body.dataset.state === 'mail'", timeout=10, what="the added account to open")


def mail(page):
    page.viewport(1280, 800)
    page.goto(URL)
    page.wait_for("document.querySelectorAll('#list li').length >= 4", what="seeded inbox")
    page.wait_for("document.querySelector('#reader h1')", what="an open message")

    subjects = page.json("[...document.querySelectorAll('#list .subject')].map(n => n.textContent)")
    check("the inbox lists the fixture mail", "Invoice 42" in subjects and "Weekly digest" in subjects)
    check("the reader shows a message body", page.rect("#reader iframe.body")["shown"])
    page.shot(f"{EVIDENCE}/guide-inbox.png")

    page.click("#folders-tree")
    check("tree view is selected", page.eval("document.querySelector('#folders-tree').getAttribute('aria-selected') === 'true'"))
    check("the account tree is populated", page.eval("document.querySelectorAll('#folder-view details').length > 0"))
    page.shot(f"{EVIDENCE}/guide-folders.png")

    page.click("#compose")
    check("Write opens compose", page.eval("document.querySelector('#compose-window').open"))
    page.fill("#compose-to", "alice@example.com")
    page.fill("#compose-subject", "A short test message")
    page.fill("#compose-body", "This draft is filled in by the end-to-end guide test.")
    page.click("#compose-cc-toggle")
    check("Cc/Bcc can be expanded", page.rect("#cc-field")["shown"] and page.rect("#bcc-field")["shown"])
    page.shot(f"{EVIDENCE}/guide-compose.png")
    page.click("#compose-discard")

    page.click("#help")
    shortcuts = page.eval("document.querySelector('#help-window').textContent")
    for text in ("Next message", "Archive", "Move to trash", "Toggle read", "Write message"):
        check(f"shortcut help names {text}", text in shortcuts)
    page.shot(f"{EVIDENCE}/guide-shortcuts.png")
    page.click("#help-close")

    # Exercise the actions the guide calls out, not merely their labels.
    before = page.eval("document.querySelector('#list li[aria-selected=true]').classList.contains('unread')")
    page.key("u")
    page.wait_for(
        f"document.querySelector('#list li[aria-selected=true]').classList.contains('unread') === {str(not before).lower()}",
        what="u to toggle read state",
    )
    check("u toggles read state", True)

    selected = page.eval("document.querySelector('#reader h1').textContent")
    page.key("d")
    page.wait_for("document.querySelector('#status').textContent === 'moved to trash'", what="d to move to trash")
    trashed = [m["subject"] for m in api("api/ls?q=in%3Atrash&limit=20")["messages"]]
    check("d moves the selected message to trash", selected in trashed, repr(trashed))

    selected = page.eval("document.querySelector('#reader h1').textContent")
    page.key("s")
    page.wait_for("document.querySelector('#status').textContent === 'moved to archive'", what="s to archive")
    archived = [m["subject"] for m in api("api/ls?q=in%3Aarchive&limit=20")["messages"]]
    check("s archives the selected message", selected in archived, repr(archived))

    selected = page.eval("document.querySelector('#reader h1').textContent")
    page.click_text("#reader .actions button", "Spam")
    page.wait_for("document.querySelector('#status').textContent === 'moved to junk'", what="Spam action")
    junk = [m["subject"] for m in api("api/ls?q=in%3Ajunk&limit=20")["messages"]]
    check("Spam moves the selected message to junk", selected in junk, repr(junk))


def main():
    with cdp.Browser.attach(int(os.environ.get("MAILBOX_E2E_CDP_PORT", "9222"))) as page:
        (setup if PHASE == "setup" else mail)(page)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except cdp.BrowserError as error:
        print(f"guide {PHASE} FAILED: {error}", file=sys.stderr)
        raise SystemExit(1)
