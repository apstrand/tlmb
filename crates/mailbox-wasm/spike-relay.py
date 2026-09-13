#!/usr/bin/env python3
"""Throwaway Stage 0 WebSocket-to-TCP relay.

The destination is fixed at startup, so this probe cannot be used as an open
proxy. It intentionally has no production deployment features; `mailbox relay`
and its policy/limits belong to Stage 4 if this spike passes.
"""

import argparse
import asyncio
import base64
import hashlib
import struct


GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
MAX_BYTES = 8 * 1024 * 1024


def endpoint(value):
    host, separator, port = value.rpartition(":")
    if not separator or not host:
        raise argparse.ArgumentTypeError("expected HOST:PORT")
    try:
        return host.strip("[]"), int(port)
    except ValueError as error:
        raise argparse.ArgumentTypeError("port must be an integer") from error


async def read_frame(reader):
    first, second = await reader.readexactly(2)
    opcode = first & 0x0F
    masked = bool(second & 0x80)
    length = second & 0x7F
    if length == 126:
        length = struct.unpack("!H", await reader.readexactly(2))[0]
    elif length == 127:
        length = struct.unpack("!Q", await reader.readexactly(8))[0]
    if length > MAX_BYTES:
        raise ValueError("WebSocket frame exceeds spike byte cap")
    mask = await reader.readexactly(4) if masked else None
    payload = bytearray(await reader.readexactly(length))
    if mask:
        for index in range(length):
            payload[index] ^= mask[index % 4]
    return opcode, bytes(payload)


def frame(payload, opcode=2):
    length = len(payload)
    header = bytes([0x80 | opcode])
    if length < 126:
        return header + bytes([length]) + payload
    if length <= 0xFFFF:
        return header + bytes([126]) + struct.pack("!H", length) + payload
    return header + bytes([127]) + struct.pack("!Q", length) + payload


async def websocket_to_tcp(web_reader, tcp_writer):
    total = 0
    while True:
        opcode, payload = await read_frame(web_reader)
        if opcode == 8:
            return
        if opcode == 9:
            # Pings are not expected during the short probe. Ignoring one is
            # safe and keeps the data path strictly TLS bytes.
            continue
        if opcode != 2:
            raise ValueError("the relay accepts binary frames only")
        total += len(payload)
        if total > MAX_BYTES:
            raise ValueError("connection exceeds spike byte cap")
        tcp_writer.write(payload)
        await tcp_writer.drain()


async def tcp_to_websocket(tcp_reader, web_writer):
    total = 0
    while chunk := await tcp_reader.read(64 * 1024):
        total += len(chunk)
        if total > MAX_BYTES:
            raise ValueError("connection exceeds spike byte cap")
        web_writer.write(frame(chunk))
        await web_writer.drain()


async def handle(web_reader, web_writer, target):
    tcp_writer = None
    try:
        request = await web_reader.readuntil(b"\r\n\r\n")
        headers = {}
        for line in request.decode("latin1").split("\r\n")[1:]:
            if ":" in line:
                name, value = line.split(":", 1)
                headers[name.lower()] = value.strip()
        key = headers.get("sec-websocket-key")
        if not key or headers.get("upgrade", "").lower() != "websocket":
            raise ValueError("not a WebSocket upgrade")

        tcp_reader, tcp_writer = await asyncio.wait_for(
            asyncio.open_connection(*target), timeout=10
        )
        accept = base64.b64encode(hashlib.sha1((key + GUID).encode()).digest()).decode()
        web_writer.write(
            (
                "HTTP/1.1 101 Switching Protocols\r\n"
                "Upgrade: websocket\r\n"
                "Connection: Upgrade\r\n"
                f"Sec-WebSocket-Accept: {accept}\r\n\r\n"
            ).encode()
        )
        await web_writer.drain()

        tasks = [
            asyncio.create_task(websocket_to_tcp(web_reader, tcp_writer)),
            asyncio.create_task(tcp_to_websocket(tcp_reader, web_writer)),
        ]
        done, pending = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
        for task in pending:
            task.cancel()
        for task in done:
            task.result()
    except (asyncio.IncompleteReadError, ConnectionError):
        pass
    except Exception as error:
        print(f"relay connection failed: {error}")
    finally:
        if tcp_writer:
            tcp_writer.close()
            await tcp_writer.wait_closed()
        web_writer.close()
        await web_writer.wait_closed()


async def run(listen, target):
    server = await asyncio.start_server(
        lambda reader, writer: handle(reader, writer, target), *listen
    )
    addresses = ", ".join(str(sock.getsockname()) for sock in server.sockets)
    print(f"spike relay on {addresses} -> {target[0]}:{target[1]}")
    async with server:
        await server.serve_forever()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--listen", type=endpoint, default=("127.0.0.1", 9001))
    parser.add_argument("--target", type=endpoint, required=True)
    args = parser.parse_args()
    asyncio.run(run(args.listen, args.target))


if __name__ == "__main__":
    main()
