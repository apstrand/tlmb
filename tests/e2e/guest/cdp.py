"""A very small Chrome DevTools Protocol client.

Stdlib only, on purpose. The guest has python and chromium and no pip; a
harness that needs a package index is a harness that stops working the day
the guest is isolated — which is every day after `mailbox-e2e isolate`.

Enough protocol to open the served page, click what a person would click,
read the DOM back, and photograph the result:

    with Browser.attach(9222) as page:
        page.goto(url)
        page.wait_for("document.querySelectorAll('#folders .folder').length > 0")
        page.click("#back")
        page.shot("/tmp/narrow.png")

`click` is a real mouse event at the element's own rendered position, not
`el.click()`: the bug this was written for was a control that was visible and
did nothing, which a synthetic DOM click would have reported as working.
"""

import base64
import json
import os
import socket
import struct
import time
import urllib.request


class BrowserError(RuntimeError):
    pass


class _Socket:
    """The four frames of RFC 6455 that a debugger connection actually uses."""

    def __init__(self, url, timeout=30):
        if not url.startswith("ws://"):
            raise BrowserError(f"not a websocket url: {url}")
        rest = url[len("ws://") :]
        hostport, _, path = rest.partition("/")
        host, _, port = hostport.partition(":")
        self.sock = socket.create_connection((host, int(port or 80)), timeout=timeout)
        self.sock.settimeout(timeout)
        key = base64.b64encode(os.urandom(16)).decode()
        self.sock.sendall(
            (
                f"GET /{path} HTTP/1.1\r\n"
                f"Host: {hostport}\r\n"
                "Upgrade: websocket\r\n"
                "Connection: Upgrade\r\n"
                f"Sec-WebSocket-Key: {key}\r\n"
                "Sec-WebSocket-Version: 13\r\n\r\n"
            ).encode()
        )
        self.buf = b""
        while b"\r\n\r\n" not in self.buf:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise BrowserError("debugger closed during handshake")
            self.buf += chunk
        head, _, self.buf = self.buf.partition(b"\r\n\r\n")
        if b"101" not in head.split(b"\r\n")[0]:
            raise BrowserError(f"debugger refused the upgrade: {head!r}")

    def _read(self, n):
        while len(self.buf) < n:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise BrowserError("debugger closed the connection")
            self.buf += chunk
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def send(self, text):
        payload = text.encode()
        head = bytearray([0x81])
        length = len(payload)
        if length < 126:
            head.append(0x80 | length)
        elif length < (1 << 16):
            head.append(0x80 | 126)
            head += struct.pack(">H", length)
        else:
            head.append(0x80 | 127)
            head += struct.pack(">Q", length)
        mask = os.urandom(4)
        head += mask
        self.sock.sendall(
            bytes(head) + bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
        )

    def recv(self):
        """One message, reassembled across continuation frames."""
        chunks = []
        while True:
            first, second = self._read(2)
            fin, opcode = first & 0x80, first & 0x0F
            length = second & 0x7F
            if length == 126:
                (length,) = struct.unpack(">H", self._read(2))
            elif length == 127:
                (length,) = struct.unpack(">Q", self._read(8))
            data = self._read(length) if length else b""
            if opcode == 0x8:
                raise BrowserError("debugger closed the connection")
            if opcode == 0x9:  # ping
                self.sock.sendall(b"\x8a\x80" + os.urandom(4))
                continue
            if opcode == 0xA:  # pong
                continue
            chunks.append(data)
            if fin:
                return b"".join(chunks).decode()

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


class Browser:
    """A CDP session against one page target."""

    def __init__(self, ws_url, timeout=30):
        self.ws = _Socket(ws_url, timeout=timeout)
        self.next_id = 0
        self.timeout = timeout
        self.call("Page.enable")
        self.call("Runtime.enable")

    # -- lifecycle ----------------------------------------------------------

    @classmethod
    def attach(cls, port=9222, timeout=30):
        """Wait for chromium's debugger and attach to its first page."""
        deadline = time.time() + timeout
        last = None
        while time.time() < deadline:
            try:
                with urllib.request.urlopen(
                    f"http://127.0.0.1:{port}/json/list", timeout=2
                ) as response:
                    targets = json.load(response)
                for target in targets:
                    if target.get("type") == "page" and target.get(
                        "webSocketDebuggerUrl"
                    ):
                        return cls(target["webSocketDebuggerUrl"])
                last = "no page target yet"
            except Exception as error:  # noqa: BLE001 - chromium is still booting
                last = error
            time.sleep(0.25)
        raise BrowserError(f"no chromium debugger on {port}: {last}")

    def close(self):
        self.ws.close()

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()

    # -- protocol -----------------------------------------------------------

    def call(self, method, **params):
        self.next_id += 1
        want = self.next_id
        self.ws.send(json.dumps({"id": want, "method": method, "params": params}))
        deadline = time.time() + self.timeout
        while time.time() < deadline:
            message = json.loads(self.ws.recv())
            if message.get("id") != want:
                continue  # an event; this client does not subscribe to any
            if "error" in message:
                raise BrowserError(f"{method}: {message['error'].get('message')}")
            return message.get("result", {})
        raise BrowserError(f"{method}: no reply in {self.timeout}s")

    # -- the page -----------------------------------------------------------

    def goto(self, url, settle=0.5):
        self.call("Page.navigate", url=url)
        self.wait_for("document.readyState === 'complete'")
        time.sleep(settle)

    def eval(self, expression):
        """Evaluate JS and return the value, awaiting a promise if it is one."""
        result = self.call(
            "Runtime.evaluate",
            expression=expression,
            returnByValue=True,
            awaitPromise=True,
        )
        details = result.get("exceptionDetails")
        if details:
            text = details.get("exception", {}).get("description") or details.get("text")
            raise BrowserError(f"page threw: {text}")
        return result.get("result", {}).get("value")

    def json(self, expression):
        """Evaluate JS whose value is structured, via JSON to keep it exact."""
        return json.loads(self.eval(f"JSON.stringify({expression})"))

    def wait_for(self, expression, timeout=20, what=None):
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self.eval(f"Boolean({expression})"):
                return True
            time.sleep(0.1)
        raise BrowserError(f"timed out waiting for {what or expression}")

    RECT_JS = """
        (() => {
            const el = document.querySelector(SELECTOR);
            if (!el) return null;
            const r = el.getBoundingClientRect();
            return { x: r.x, y: r.y, w: r.width, h: r.height,
                     shown: r.width > 0 && r.height > 0 };
        })()
    """

    def rect(self, selector):
        box = self.json(self.RECT_JS.replace("SELECTOR", json.dumps(selector)))
        if not box:
            raise BrowserError(f"no element matches {selector}")
        return box

    def click(self, selector, settle=0.4):
        """A real mouse click at the element's rendered centre."""
        box = self.rect(selector)
        if not box["shown"]:
            raise BrowserError(f"{selector} has no box to click — is it hidden?")
        x, y = box["x"] + box["w"] / 2, box["y"] + box["h"] / 2
        for kind in ("mousePressed", "mouseReleased"):
            self.call(
                "Input.dispatchMouseEvent",
                type=kind,
                x=x,
                y=y,
                button="left",
                clickCount=1,
            )
        time.sleep(settle)

    MARK_JS = """
        (() => {
            document.querySelectorAll("[data-cdp-target]")
                .forEach((node) => node.removeAttribute("data-cdp-target"));
            const el = [...document.querySelectorAll(SELECTOR)]
                .find((node) => node.textContent.trim() === TEXT);
            if (!el) return false;
            el.setAttribute("data-cdp-target", "1");
            return true;
        })()
    """

    def click_text(self, selector, text, settle=0.4):
        """Click the one element matching `selector` whose label is `text`."""
        found = self.eval(
            self.MARK_JS.replace("SELECTOR", json.dumps(selector)).replace(
                "TEXT", json.dumps(text)
            )
        )
        if not found:
            raise BrowserError(f"no {selector} reads {text!r}")
        self.click("[data-cdp-target]", settle=settle)

    def viewport(self, width, height, mobile=False):
        """Resize the page the way a phone would, media queries and all."""
        self.call(
            "Emulation.setDeviceMetricsOverride",
            width=width,
            height=height,
            deviceScaleFactor=1,
            mobile=mobile,
        )
        time.sleep(0.3)

    def shot(self, path):
        data = self.call("Page.captureScreenshot", format="png")["data"]
        os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
        with open(path, "wb") as handle:
            handle.write(base64.b64decode(data))
        if os.path.getsize(path) == 0:
            raise BrowserError(f"empty screenshot: {path}")
        return path
