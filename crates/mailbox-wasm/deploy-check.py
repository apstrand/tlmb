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
import urllib.error


def fetch(url):
    request = urllib.request.Request(url, headers={"User-Agent": "mailbox-deploy-check/1"})
    with urllib.request.urlopen(request, timeout=10) as response:
        headers = {name.lower(): value for name, value in response.headers.items()}
        return response.status, headers, response.read()


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
                           ("cross-origin-embedder-policy", "credentialless")):
        if headers.get(name, "").lower() != expected:
            raise RuntimeError(f"missing {name}: expected {expected!r}")
    relay_url = urllib.parse.urlsplit(relay)
    discovery_scheme = "https" if relay_url.scheme == "wss" else "http"
    discovery_origin = f"{discovery_scheme}://{relay_url.netloc}"
    csp = headers.get("content-security-policy", "")
    connect = next((part for part in csp.split(";")
                    if part.strip().startswith("connect-src ")), "")
    allowed = connect.split()[1:]
    if discovery_origin not in allowed and f"{discovery_scheme}:" not in allowed:
        raise RuntimeError(
            f"connect-src does not allow relay discovery origin {discovery_origin!r}"
        )


def check_relay(url):
    parsed = urllib.parse.urlparse(url)
    if parsed.scheme != "wss" or not parsed.hostname:
        raise RuntimeError("relay must be a wss:// URL")
    port = parsed.port or 443
    key = base64.b64encode(os.urandom(16)).decode()
    request_target = parsed.path or "/"
    if parsed.query:
        request_target += "?" + parsed.query
    request = (f"GET {request_target} HTTP/1.1\r\n"
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


def check_image_proxy(url):
    parsed = urllib.parse.urlsplit(url)
    scheme = "https" if parsed.scheme == "wss" else "http"
    probe = urllib.parse.urlunsplit((scheme, parsed.netloc, "/_image", "", ""))
    try:
        fetch(probe)
    except urllib.error.HTTPError as error:
        status, headers = error.code, error.headers
    else:
        raise RuntimeError("relay image endpoint accepted a request without a URL")
    if status != 400:
        raise RuntimeError(f"relay image endpoint returned HTTP {status}, expected 400")
    if headers.get("cross-origin-resource-policy", "").lower() != "cross-origin":
        raise RuntimeError("relay image endpoint is missing cross-origin resource policy")


def check_discovery_proxy(url):
    parsed = urllib.parse.urlsplit(url)
    scheme = "https" if parsed.scheme == "wss" else "http"
    probe = urllib.parse.urlunsplit((scheme, parsed.netloc, "/_discover", "", ""))
    try:
        fetch(probe)
    except urllib.error.HTTPError as error:
        status, headers = error.code, error.headers
    else:
        raise RuntimeError("relay discovery endpoint accepted a request without an address")
    if status != 400:
        raise RuntimeError(f"relay discovery endpoint returned HTTP {status}, expected 400")
    if headers.get("access-control-allow-origin") != "*":
        raise RuntimeError("relay discovery endpoint is missing cross-origin access")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--site", required=True, help="public https:// site URL")
    parser.add_argument("--relay", required=True, help="public wss:// relay URL")
    parser.add_argument("--imap-host", help="configured destination to check on a shared root relay")
    parser.add_argument("--imap-port", type=int, default=993)
    args = parser.parse_args()
    check_site(args.site.rstrip("/") + "/", args.relay)
    relay = args.relay
    if args.imap_host:
        parsed = urllib.parse.urlsplit(relay)
        query = urllib.parse.parse_qs(parsed.query)
        query.update(host=[args.imap_host], port=[str(args.imap_port)])
        relay = urllib.parse.urlunsplit(parsed._replace(
            query=urllib.parse.urlencode(query, doseq=True)
        ))
    check_relay(relay)
    check_image_proxy(relay)
    check_discovery_proxy(relay)
    print("hosted deployment checks passed")


if __name__ == "__main__":
    main()
