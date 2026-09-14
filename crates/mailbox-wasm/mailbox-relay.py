#!/usr/bin/env python3
"""Bounded WebSocket-to-TCP relay for the hosted mailbox site.

TLS is terminated in the browser. This process only forwards ciphertext and
must be placed behind a TLS/WSS reverse proxy. ``--tls-terminated`` is an
explicit deployment acknowledgement that prevents accidental plain exposure.
"""

import argparse
import asyncio
import base64
import hashlib
import struct

GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
ALLOWED_PORTS = (143, 993, 465, 587)


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


async def handle(web_reader, web_writer, target, policy, slots):
    tcp_writer = None
    try:
        async with slots:
            request = await asyncio.wait_for(
                web_reader.readuntil(b"\r\n\r\n"), policy["idle_timeout"]
            )
            headers = {}
            for line in request.decode("latin1").split("\r\n")[1:]:
                if ":" in line:
                    name, value = line.split(":", 1)
                    headers[name.lower()] = value.strip()
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
    target_host, target_port = args.target
    if target_port not in args.allowed_port:
        raise SystemExit(f"target port {target_port} is not in --allowed-port")
    if args.allowed_host and target_host not in args.allowed_host:
        raise SystemExit(f"target host {target_host!r} is not in --allowed-host")
    policy = {"max_bytes": args.max_bytes, "idle_timeout": args.idle_timeout}
    slots = asyncio.Semaphore(args.max_connections)
    server = await asyncio.start_server(
        lambda reader, writer: handle(reader, writer, args.target, policy, slots),
        *args.listen,
    )
    addresses = ", ".join(str(sock.getsockname()) for sock in server.sockets)
    print(f"mailbox relay on {addresses} -> {target_host}:{target_port} "
          f"(max {args.max_bytes} bytes, {args.idle_timeout}s idle, "
          f"{args.max_connections} connections)")
    async with server:
        await server.serve_forever()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--listen", type=endpoint, default=("127.0.0.1", 9001))
    parser.add_argument("--target", type=endpoint, required=True)
    parser.add_argument("--allowed-host", action="append", default=[])
    parser.add_argument(
        "--allowed-port", action="append", type=int,
        help="permitted destination port (repeatable; defaults to IMAP/SMTP ports)",
    )
    parser.add_argument("--max-bytes", type=int, default=64 * 1024 * 1024)
    parser.add_argument("--idle-timeout", type=float, default=120.0)
    parser.add_argument("--max-connections", type=int, default=100)
    parser.add_argument("--tls-terminated", action="store_true", required=True,
                        help="acknowledge that a TLS/WSS reverse proxy fronts this listener")
    args = parser.parse_args()
    if args.allowed_port is None:
        args.allowed_port = list(ALLOWED_PORTS)
    if any(not 1 <= port <= 65535 for port in args.allowed_port):
        parser.error("allowed ports must be between 1 and 65535")
    if args.max_bytes <= 0 or args.idle_timeout <= 0 or args.max_connections <= 0:
        parser.error("limits must be positive")
    asyncio.run(run(args))


if __name__ == "__main__":
    main()
