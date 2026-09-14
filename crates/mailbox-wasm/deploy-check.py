#!/usr/bin/env python3
"""Small dependency-free smoke test for a deployed hosted mailbox site."""

import argparse
import base64
import json
import os
import socket
import ssl
import urllib.parse
import urllib.request


def fetch(url):
    request = urllib.request.Request(url, headers={"User-Agent": "mailbox-deploy-check/1"})
    with urllib.request.urlopen(request, timeout=10) as response:
        return response.status, dict(response.headers), response.read()


def check_site(url, relay):
    status, headers, body = fetch(urllib.parse.urljoin(url, "config.json"))
    if status != 200:
        raise RuntimeError(f"config.json returned HTTP {status}")
    config = json.loads(body)
    if config.get("relay") != relay:
        raise RuntimeError(f"config relay is {config.get('relay')!r}, expected {relay!r}")
    status, headers, _ = fetch(url)
    if status != 200:
        raise RuntimeError(f"site returned HTTP {status}")
    for name, expected in (("cross-origin-opener-policy", "same-origin"),
                           ("cross-origin-embedder-policy", "require-corp")):
        if headers.get(name, "").lower() != expected:
            raise RuntimeError(f"missing {name}: expected {expected!r}")


def check_relay(url):
    parsed = urllib.parse.urlparse(url)
    if parsed.scheme != "wss" or not parsed.hostname:
        raise RuntimeError("relay must be a wss:// URL")
    port = parsed.port or 443
    key = base64.b64encode(os.urandom(16)).decode()
    request = (f"GET {parsed.path or '/'} HTTP/1.1\r\n"
               f"Host: {parsed.hostname}\r\nUpgrade: websocket\r\n"
               "Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n"
               f"Sec-WebSocket-Key: {key}\r\n\r\n").encode()
    context = ssl.create_default_context()
    with socket.create_connection((parsed.hostname, port), timeout=10) as raw:
        with context.wrap_socket(raw, server_hostname=parsed.hostname) as stream:
            stream.sendall(request)
            response = stream.recv(4096)
    if not response.startswith(b"HTTP/1.1 101"):
        first = response.split(b"\r\n", 1)[0].decode("latin1", "replace")
        raise RuntimeError(f"relay did not upgrade WebSocket ({first})")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--site", required=True, help="public https:// site URL")
    parser.add_argument("--relay", required=True, help="public wss:// relay URL")
    args = parser.parse_args()
    check_site(args.site.rstrip("/") + "/", args.relay)
    check_relay(args.relay)
    print("hosted deployment checks passed")


if __name__ == "__main__":
    main()
