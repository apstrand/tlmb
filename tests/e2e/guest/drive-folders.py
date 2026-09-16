"""Drive the web interface's folder list, and photograph what it did.

Two things are under test, both of them §7 surface behaviour that the CLI
contract cannot show:

1. **The folder list counts unread mail**, not messages, and says both in its
   hover. The count follows reading: open an unread message, leave it open
   past the read dwell, and the folder it was in counts one fewer.
2. **On a phone-shaped screen the panes are a stack**: folders → list →
   reader, with the back arrow walking up it. The arrow was visible on the
   list pane and did nothing, because there was nothing above the list to go
   back to and the folder list was unreachable.

Usage: drive-folders.py <url> <evidence-dir> <token>
"""

import json
import sys
import time
import urllib.request

import cdp

URL, EVIDENCE, TOKEN = sys.argv[1], sys.argv[2], sys.argv[3]

# Wide enough for three panes, then a phone. 45rem is where the layout folds.
WIDE = (1280, 720)
NARROW = (420, 860)

# `mailbox.js` waits this long before a message on screen counts as read.
READ_DWELL_S = 1.5

STATE = """
(() => {
    const shown = (id) => {
        const el = document.getElementById(id);
        return el ? getComputedStyle(el).display !== "none" : false;
    };
    const back = document.getElementById("back");
    const box = back.getBoundingClientRect();
    return {
        pane: document.body.dataset.pane,
        backShown: getComputedStyle(back).display !== "none" && box.width > 0,
        backLabel: back.getAttribute("aria-label"),
        foldersShown: shown("folders"),
        listShown: shown("list-pane"),
        readerShown: shown("reader"),
        folders: [...document.querySelectorAll("#folders .folder")].map((button) => ({
            name: button.querySelector(".name").textContent,
            count: button.querySelector(".count").textContent,
            title: button.title,
        })),
        unreadRows: document.querySelectorAll("#list li.unread").length,
    };
})()
"""

transcript = []
failures = []


def check(label, ok, detail=""):
    transcript.append({"check": label, "ok": bool(ok), "detail": detail})
    mark = "ok  " if ok else "FAIL"
    print(f"{mark} {label}{(' — ' + detail) if detail else ''}", flush=True)
    if not ok:
        failures.append(label)


def note(label, detail):
    transcript.append({"note": label, "detail": detail})
    print(f"··   {label} — {detail}", flush=True)


def api_folders():
    request = urllib.request.Request(
        URL.split("?")[0] + "api/folders", headers={"X-Mailbox-Token": TOKEN}
    )
    with urllib.request.urlopen(request, timeout=10) as response:
        return json.load(response)["folders"]


def counts(page):
    # ---- the count is unread, and the hover has both -------------------

    served = api_folders()
    state = page.json(STATE)
    by_name = {folder["name"]: folder for folder in served}
    inbox_unread = sum(f["unread"] for f in served if f["special_use"] == "inbox")
    inbox_total = sum(f["total"] for f in served if f["special_use"] == "inbox")
    by_name["Inbox"] = {"unread": inbox_unread, "total": inbox_total}

    check("the folder list is populated", len(state["folders"]) > 1,
          f"{len(state['folders'])} buttons")

    for button in state["folders"]:
        served_folder = by_name.get(button["name"])
        if not served_folder:
            check(f"'{button['name']}' is a folder the API knows", False)
            continue
        unread, total = served_folder["unread"], served_folder["total"]
        want = str(unread) if unread else ""
        check(
            f"'{button['name']}' shows {want or 'no'} count, not its total {total}",
            button["count"] == want,
            f"showed {button['count']!r}, wanted {want!r}",
        )
        check(
            f"'{button['name']}' hover names unread and total",
            f"{unread} unread" in button["title"]
            and f"{total} message" in button["title"],
            button["title"],
        )

    # A mailbox where every folder happens to be fully unread could not
    # tell the two numbers apart, and this whole scenario would pass on
    # the old behaviour.
    check(
        "the scenario can tell unread from total",
        any(f["unread"] != f["total"] for f in served),
        f"{[(f['name'], f['unread'], f['total']) for f in served]}",
    )


def reading(page):
    # ---- reading mail moves the count ----------------------------------

    state = page.json(STATE)
    before = next(b for b in state["folders"] if b["name"] == "Inbox")["count"]
    check("there is unread mail on screen", state["unreadRows"] > 0,
          f"{state['unreadRows']} unread rows")
    page.click("#list li.unread")
    page.shot(f"{EVIDENCE}/folders-2-reading.png")
    time.sleep(READ_DWELL_S + 1.5)
    page.wait_for(
        "document.querySelector('#folders .folder .count').textContent"
        f" !== {json.dumps(before)}",
        timeout=10,
        what="the inbox count to follow the read",
    )
    after = page.json(STATE)["folders"][0]["count"]
    check(
        "reading a message lowers the inbox count",
        int(after or 0) == int(before or 0) - 1,
        f"{before} → {after}",
    )
    served_now = sum(
        f["unread"] for f in api_folders() if f["special_use"] == "inbox"
    )
    check(
        "and the served count agrees",
        served_now == int(after or 0),
        f"served {served_now}, shown {after}",
    )
    page.shot(f"{EVIDENCE}/folders-3-counted.png")

    # And back up again, which is also how this scenario keeps a folder
    # with unread mail in it for the narrow walk below. A message that was
    # already read when it was opened: the reader's action row is drawn
    # with the message, so the one just read still offers "Mark read".
    page.click_text("#list li:not(.unread) .subject", "Weekly digest")
    page.click_text("#reader .actions button", "Mark unread")
    page.wait_for(
        "document.querySelector('#folders .folder .count').textContent"
        f" === {json.dumps(before)}",
        timeout=10,
        what="the inbox count to come back up",
    )
    state = page.json(STATE)
    check(
        "marking it unread puts the count back",
        state["folders"][0]["count"] == before,
        f"{after or '0'} → {state['folders'][0]['count']}",
    )
    # Move off it, or the dwell reads it again while the next steps run —
    # a message left on screen is read, which is the whole point of it.
    page.click_text("#list li:not(.unread) .subject", "Re: Invoice 42")


def narrow(page):
    # ---- the same mailbox on a phone ------------------------------------

    # A message is open from the step above, so the stack is at the reader:
    # narrowing the window shows the pane you were already on.
    page.viewport(*NARROW, mobile=True)
    time.sleep(0.8)
    state = page.json(STATE)
    page.shot(f"{EVIDENCE}/folders-4-narrow-reader.png")
    check(
        "narrow shows one pane at a time",
        state["readerShown"] and not state["listShown"]
        and not state["foldersShown"],
        f"pane={state['pane']}",
    )
    check("the back arrow is offered", state["backShown"],
          f"label={state['backLabel']!r}")
    check("and it names the pane above", state["backLabel"] == "Back to the list",
          f"{state['backLabel']!r}")

    page.click("#back")
    state = page.json(STATE)
    page.shot(f"{EVIDENCE}/folders-5-narrow-list.png")
    check("back from the reader returns to the list",
          state["pane"] == "list" and state["listShown"],
          f"pane={state['pane']}")
    check("the arrow now points at the folders",
          state["backShown"] and state["backLabel"] == "Back to the folders",
          f"shown={state['backShown']} label={state['backLabel']!r}")

    page.click("#back")
    state = page.json(STATE)
    page.shot(f"{EVIDENCE}/folders-6-narrow-folders.png")
    check(
        "back from the list reaches the folder list",
        state["pane"] == "folders" and state["foldersShown"],
        f"pane={state['pane']}",
    )
    check("the folder list still counts unread",
          any(button["count"] for button in state["folders"]),
          f"{[(b['name'], b['count']) for b in state['folders']]}")
    check("and it agrees with what the server is asked for",
          {f["name"]: str(f["unread"] or "") for f in api_folders()
           if f["special_use"] != "inbox"}
          | {"Inbox": str(sum(f["unread"] for f in api_folders()
                              if f["special_use"] == "inbox") or "")}
          == {b["name"]: b["count"] for b in state["folders"]},
          f"{[(b['name'], b['count']) for b in state['folders']]}")
    check("there is nothing above the folders", not state["backShown"],
          "the arrow is hidden at the top of the stack")

    # Down the stack again, by hand, the way a person would. The inbox,
    # because a folder with no mail cannot show that its list opened.
    page.click("#folders .folder")
    state = page.json(STATE)
    page.shot(f"{EVIDENCE}/folders-7-narrow-folder-opened.png")
    check("choosing a folder opens its list", state["pane"] == "list",
          f"pane={state['pane']}")

    page.wait_for("document.querySelectorAll('#list li').length > 0",
                  what="that folder's messages")
    page.click("#list li")
    state = page.json(STATE)
    page.shot(f"{EVIDENCE}/folders-8-narrow-reader.png")
    check("opening a message shows the reader", state["pane"] == "reader",
          f"pane={state['pane']}")

    page.click("#back")
    page.click("#back")
    state = page.json(STATE)
    page.shot(f"{EVIDENCE}/folders-9-narrow-back-at-top.png")
    check("two arrows walk back up to the folders", state["pane"] == "folders",
          f"pane={state['pane']}")

    note("the walk up the stack", "reader → list → folders")
    time.sleep(1)  # a last beat, so the video does not cut on the click


def main():
    with cdp.Browser.attach() as page:
        page.viewport(*WIDE)
        page.goto(URL)
        page.wait_for(
            "document.querySelectorAll('#folders .folder').length > 0",
            what="the folder list",
        )
        page.wait_for("document.querySelectorAll('#list li').length > 0",
                      what="the message list")
        # The page opens the newest message by itself, and a message left on
        # screen is read after the dwell. Wait that out before reading any
        # count, or the first number in this scenario is one the app is still
        # in the middle of changing.
        time.sleep(READ_DWELL_S + 1.5)
        page.shot(f"{EVIDENCE}/folders-1-wide.png")

        # Each phase stands on its own: one broken behaviour should report
        # itself and leave the others still able to say what they found,
        # rather than ending the run at the first surprise.
        for name, phase in (("the counts", counts), ("reading", reading),
                            ("the phone layout", narrow)):
            try:
                phase(page)
            except cdp.BrowserError as error:
                check(f"{name} ran to the end", False, str(error))

    with open(f"{EVIDENCE}/folders-checks.json", "w") as handle:
        json.dump(transcript, handle, indent=2)
    if failures:
        print(f"\n{len(failures)} check(s) failed: {', '.join(failures)}",
              file=sys.stderr)
        return 1
    passed = sum(1 for item in transcript if item.get("ok"))
    print(f"\nfolders: {passed} checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
