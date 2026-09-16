#!/usr/bin/env python3
"""Bounded network relay for the hosted mailbox site.

Mailbox TLS is terminated in the browser; its WebSocket route only forwards
ciphertext. The ``/_image`` route fetches explicitly revealed remote message
images while rejecting non-public destinations. This process must be placed
behind a TLS/WSS reverse proxy. ``--tls-terminated`` is an explicit deployment
acknowledgement that prevents accidental plain exposure.
"""

import argparse
import asyncio
import base64
import hashlib
import ipaddress
import socket
import ssl
import struct
from urllib.parse import parse_qs, urljoin, urlsplit

GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
ALLOWED_PORTS = (143, 993, 465, 587)
IMAGE_ROUTE = "/_image"
MAX_HTTP_HEADERS = 64 * 1024
MAX_REDIRECTS = 4


def endpoint(value):
    host, separator, port = value.rpartition(":")
    if not separator or not host:
        raise argparse.ArgumentTypeError("expected HOST:PORT")
    try:
        port = int(port)
    except ValueError as error:
        raise argparse.ArgumentTypeError("port must be an integer") from error
    if not 1 <= port <= 65535:
        raise argparse.ArgumentTypeError("port must be between 1 and 65535")
    return host.strip("[]"), port


def upstream(value):
    name, separator, destination = value.partition("=")
    if not separator or not name or not name.isidentifier():
        raise argparse.ArgumentTypeError("expected NAME=HOST:PORT")
    return name, endpoint(destination)


def select_target(request_target, targets):
    """Select only configured destinations, including on a shared root URL."""
    parsed = urlsplit(request_target)
    route = parsed.path.rstrip("/") or "/"
    query = parse_qs(parsed.query, keep_blank_values=True)
    if route == "/" and ("host" in query or "port" in query):
        if len(query.get("host", [])) != 1 or len(query.get("port", [])) != 1:
            raise ValueError("relay destination needs one host and one port")
        try:
            port = int(query["port"][0])
        except ValueError as error:
            raise ValueError("invalid relay destination port") from error
        host = query["host"][0].lower().rstrip(".")
        for target in targets.values():
            if (target[0].lower().rstrip("."), target[1]) == (host, port):
                return target
        raise ValueError("relay destination is not configured")
    target = targets.get(route)
    if target is None:
        raise ValueError(f"unknown relay route {route!r}")
    return target


def image_url(request_target):
    parsed = urlsplit(request_target)
    if parsed.path != IMAGE_ROUTE:
        return None
    values = parse_qs(parsed.query, keep_blank_values=True).get("url", [])
    if len(values) != 1 or not values[0]:
        raise ValueError("image request needs one URL")
    return values[0]


async def public_address(host, port):
    try:
        addresses = await asyncio.get_running_loop().getaddrinfo(
            host, port, type=socket.SOCK_STREAM
        )
    except socket.gaierror as error:
        raise ValueError("image host could not be resolved") from error
    if not addresses:
        raise ValueError("image host could not be resolved")
    ips = {item[4][0].split("%", 1)[0] for item in addresses}
    if any(not ipaddress.ip_address(ip).is_global for ip in ips):
        raise ValueError("image URL resolves to a non-public address")
    return addresses[0][4][0]


async def read_image_body(reader, headers, limit, timeout):
    body = bytearray()

    async def append(chunk):
        body.extend(chunk)
        if len(body) > limit:
            raise ValueError("image exceeds relay byte cap")

    if "chunked" in headers.get("transfer-encoding", "").lower():
        while True:
            line = await asyncio.wait_for(reader.readline(), timeout)
            if len(line) > 128:
                raise ValueError("invalid chunk header")
            try:
                length = int(line.split(b";", 1)[0].strip(), 16)
            except ValueError as error:
                raise ValueError("invalid chunk header") from error
            if length == 0:
                while await asyncio.wait_for(reader.readline(), timeout) not in (b"\r\n", b"\n", b""):
                    pass
                break
            await append(await asyncio.wait_for(reader.readexactly(length), timeout))
            if await asyncio.wait_for(reader.readexactly(2), timeout) != b"\r\n":
                raise ValueError("invalid chunk ending")
    elif "content-length" in headers:
        try:
            length = int(headers["content-length"])
        except ValueError as error:
            raise ValueError("invalid image content length") from error
        if length < 0 or length > limit:
            raise ValueError("image exceeds relay byte cap")
        await append(await asyncio.wait_for(reader.readexactly(length), timeout))
    else:
        while True:
            chunk = await asyncio.wait_for(reader.read(64 * 1024), timeout)
            if not chunk:
                break
            await append(chunk)
    return bytes(body)


async def fetch_image(url, limit, timeout):
    for redirects in range(MAX_REDIRECTS + 1):
        parsed = urlsplit(url)
        if parsed.scheme not in ("http", "https") or not parsed.hostname:
            raise ValueError("image URL must use HTTP or HTTPS")
        port = parsed.port or (443 if parsed.scheme == "https" else 80)
        if port not in (80, 443):
            raise ValueError("image URL uses a disallowed port")
        address = await public_address(parsed.hostname, port)
        context = ssl.create_default_context() if parsed.scheme == "https" else None
        reader, writer = await asyncio.wait_for(asyncio.open_connection(
            address, port, ssl=context,
            server_hostname=parsed.hostname if context else None,
        ), timeout)
        try:
            path = parsed.path or "/"
            if parsed.query:
                path += "?" + parsed.query
            default_port = 443 if parsed.scheme == "https" else 80
            host = parsed.hostname if port == default_port else f"{parsed.hostname}:{port}"
            writer.write((
                f"GET {path} HTTP/1.1\r\nHost: {host}\r\n"
                "User-Agent: The-Last-Mailbox-Image-Relay/1\r\n"
                "Accept: image/*\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n"
            ).encode("ascii"))
            await asyncio.wait_for(writer.drain(), timeout)
            raw = await asyncio.wait_for(reader.readuntil(b"\r\n\r\n"), timeout)
            if len(raw) > MAX_HTTP_HEADERS:
                raise ValueError("image response headers are too large")
            lines = raw.decode("latin1").split("\r\n")
            status_parts = lines[0].split(" ", 2)
            if len(status_parts) < 2 or not status_parts[1].isdigit():
                raise ValueError("invalid image response")
            status = int(status_parts[1])
            headers = {}
            for line in lines[1:]:
                if ":" in line:
                    name, value = line.split(":", 1)
                    headers[name.lower()] = value.strip()
            if status in (301, 302, 303, 307, 308):
                location = headers.get("location")
                if not location or redirects == MAX_REDIRECTS:
                    raise ValueError("too many image redirects")
                url = urljoin(url, location)
                continue
            if status != 200:
                raise ValueError(f"image server returned HTTP {status}")
            content_type = headers.get("content-type", "").split(";", 1)[0].lower()
            if not content_type.startswith("image/"):
                raise ValueError("image URL did not return an image")
            return content_type, await read_image_body(reader, headers, limit, timeout)
        finally:
            writer.close()
            await writer.wait_closed()
    raise ValueError("too many image redirects")


def http_response(status, reason, body=b"", content_type="text/plain; charset=utf-8",
                  cache_control="private, max-age=86400"):
    return (
        f"HTTP/1.1 {status} {reason}\r\n"
        f"Content-Type: {content_type}\r\nContent-Length: {len(body)}\r\n"
        "Cross-Origin-Resource-Policy: cross-origin\r\n"
        "Access-Control-Allow-Origin: *\r\n"
        "X-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n"
        f"Cache-Control: {cache_control}\r\nConnection: close\r\n\r\n"
    ).encode() + body


async def read_frame(reader, max_bytes):
    first, second = await reader.readexactly(2)
    opcode = first & 0x0F
    length = second & 0x7F
    if length == 126:
        length = struct.unpack("!H", await reader.readexactly(2))[0]
    elif length == 127:
        length = struct.unpack("!Q", await reader.readexactly(8))[0]
    if length > max_bytes:
        raise ValueError("WebSocket frame exceeds relay byte cap")
    mask = await reader.readexactly(4) if second & 0x80 else None
    payload = bytearray(await reader.readexactly(length))
    if mask:
        for index in range(length):
            payload[index] ^= mask[index % 4]
    return opcode, bytes(payload)


def frame(payload):
    length = len(payload)
    header = bytes([0x82])
    if length < 126:
        return header + bytes([length]) + payload
    if length <= 0xFFFF:
        return header + bytes([126]) + struct.pack("!H", length) + payload
    return header + bytes([127]) + struct.pack("!Q", length) + payload


async def pipe_web_to_tcp(web_reader, tcp_writer, max_bytes, idle_timeout):
    total = 0
    while True:
        opcode, payload = await asyncio.wait_for(
            read_frame(web_reader, max_bytes), idle_timeout
        )
        if opcode == 8:
            return
        if opcode == 9:
            continue
        if opcode != 2:
            raise ValueError("the relay accepts binary frames only")
        total += len(payload)
        if total > max_bytes:
            raise ValueError("connection exceeds relay byte cap")
        tcp_writer.write(payload)
        await asyncio.wait_for(tcp_writer.drain(), idle_timeout)


async def pipe_tcp_to_web(tcp_reader, web_writer, max_bytes, idle_timeout):
    total = 0
    while True:
        chunk = await asyncio.wait_for(tcp_reader.read(64 * 1024), idle_timeout)
        if not chunk:
            return
        total += len(chunk)
        if total > max_bytes:
            raise ValueError("connection exceeds relay byte cap")
        web_writer.write(frame(chunk))
        await asyncio.wait_for(web_writer.drain(), idle_timeout)


async def handle(web_reader, web_writer, targets, policy, slots):
    tcp_writer = None
    try:
        async with slots:
            request = await asyncio.wait_for(
                web_reader.readuntil(b"\r\n\r\n"), policy["idle_timeout"]
            )
            request_line = request.decode("latin1").split("\r\n", 1)[0].split(" ", 2)
            if len(request_line) != 3 or request_line[0] != "GET":
                raise ValueError("invalid relay request line")
            headers = {}
            for line in request.decode("latin1").split("\r\n")[1:]:
                if ":" in line:
                    name, value = line.split(":", 1)
                    headers[name.lower()] = value.strip()
            requested_image = image_url(request_line[1])
            if requested_image is not None:
                try:
                    content_type, body = await fetch_image(
                        requested_image, policy["max_image_bytes"], policy["idle_timeout"]
                    )
                    web_writer.write(http_response(200, "OK", body, content_type))
                except ValueError as error:
                    body = str(error).encode()
                    web_writer.write(http_response(
                        400, "Bad Request", body, cache_control="no-store"
                    ))
                await web_writer.drain()
                return
            target = select_target(request_line[1], targets)
            key = headers.get("sec-websocket-key")
            if not key or headers.get("upgrade", "").lower() != "websocket":
                raise ValueError("not a WebSocket upgrade")
            tcp_reader, tcp_writer = await asyncio.wait_for(
                asyncio.open_connection(*target), policy["idle_timeout"]
            )
            accept = base64.b64encode(
                hashlib.sha1((key + GUID).encode()).digest()
            ).decode()
            web_writer.write(
                ("HTTP/1.1 101 Switching Protocols\r\n"
                 "Upgrade: websocket\r\nConnection: Upgrade\r\n"
                 f"Sec-WebSocket-Accept: {accept}\r\n\r\n").encode()
            )
            await web_writer.drain()
            tasks = [
                asyncio.create_task(pipe_web_to_tcp(web_reader, tcp_writer, policy["max_bytes"], policy["idle_timeout"])),
                asyncio.create_task(pipe_tcp_to_web(tcp_reader, web_writer, policy["max_bytes"], policy["idle_timeout"])),
            ]
            done, pending = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
            for task in pending:
                task.cancel()
            for task in done:
                task.result()
    except (asyncio.IncompleteReadError, ConnectionError, asyncio.TimeoutError):
        pass
    except Exception as error:
        print(f"relay connection failed: {error}")
    finally:
        if tcp_writer:
            tcp_writer.close()
            await tcp_writer.wait_closed()
        web_writer.close()
        await web_writer.wait_closed()


async def run(args):
    targets = {}
    if args.target is not None:
        targets["/"] = args.target
    for name, target in args.upstream or []:
        route = f"/{name}"
        if route in targets:
            raise SystemExit(f"duplicate relay route {route}")
        targets[route] = target
    if not targets:
        raise SystemExit("provide --target or at least one --upstream NAME=HOST:PORT")
    for target_host, target_port in targets.values():
        if target_port not in args.allowed_port:
            raise SystemExit(f"target port {target_port} is not in --allowed-port")
        if args.allowed_host and target_host not in args.allowed_host:
            raise SystemExit(f"target host {target_host!r} is not in --allowed-host")
    policy = {"max_bytes": args.max_bytes, "max_image_bytes": args.max_image_bytes,
              "idle_timeout": args.idle_timeout}
    slots = asyncio.Semaphore(args.max_connections)
    server = await asyncio.start_server(
        lambda reader, writer: handle(reader, writer, targets, policy, slots),
        *args.listen,
    )
    addresses = ", ".join(str(sock.getsockname()) for sock in server.sockets)
    routes = ", ".join(f"{route} -> {host}:{port}" for route, (host, port) in targets.items())
    print(f"mailbox relay on {addresses} ({routes}) "
          f"(max {args.max_bytes} bytes, {args.idle_timeout}s idle, "
          f"{args.max_connections} connections)")
    async with server:
        await server.serve_forever()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--listen", type=endpoint, default=("127.0.0.1", 9001))
    parser.add_argument("--target", type=endpoint,
                        help="single upstream, exposed at the root route (legacy form)")
    parser.add_argument("--upstream", action="append", type=upstream,
                        help="named fixed upstream NAME=HOST:PORT, exposed at /NAME")
    parser.add_argument("--allowed-host", action="append", default=[])
    parser.add_argument(
        "--allowed-port", action="append", type=int,
        help="permitted destination port (repeatable; defaults to IMAP/SMTP ports)",
    )
    parser.add_argument("--max-bytes", type=int, default=64 * 1024 * 1024)
    parser.add_argument("--max-image-bytes", type=int, default=10 * 1024 * 1024)
    parser.add_argument("--idle-timeout", type=float, default=120.0)
    parser.add_argument("--max-connections", type=int, default=100)
    parser.add_argument("--tls-terminated", action="store_true", required=True,
                        help="acknowledge that a TLS/WSS reverse proxy fronts this listener")
    args = parser.parse_args()
    if args.allowed_port is None:
        args.allowed_port = list(ALLOWED_PORTS)
    if any(not 1 <= port <= 65535 for port in args.allowed_port):
        parser.error("allowed ports must be between 1 and 65535")
    if (args.max_bytes <= 0 or args.max_image_bytes <= 0 or
            args.idle_timeout <= 0 or args.max_connections <= 0):
        parser.error("limits must be positive")
    asyncio.run(run(args))


if __name__ == "__main__":
    main()
