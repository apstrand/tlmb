"""Regression checks for selecting configured destinations on one relay URL."""

import asyncio
import importlib.util
from pathlib import Path
import socket
import unittest
from unittest.mock import AsyncMock, patch

spec = importlib.util.spec_from_file_location(
    "mailbox_relay", Path(__file__).with_name("mailbox-relay.py")
)
relay = importlib.util.module_from_spec(spec)
spec.loader.exec_module(relay)

TARGETS = {"/one": ("imap.one.example", 993), "/two": ("imap.two.example", 143)}


class RoutingTests(unittest.TestCase):
    def test_shared_site_selects_each_configured_account(self):
        for route, (host, port) in TARGETS.items():
            self.assertEqual(relay.select_target(f"/?host={host}&port={port}", TARGETS),
                             TARGETS[route])
        self.assertEqual(relay.select_target("/?host=IMAP.ONE.EXAMPLE.&port=993", TARGETS),
                         TARGETS["/one"])

    def test_named_and_legacy_routes_remain_fixed(self):
        self.assertEqual(relay.select_target("/two/?host=other&port=993", TARGETS),
                         TARGETS["/two"])
        self.assertEqual(relay.select_target("/", {"/": TARGETS["/one"]}),
                         TARGETS["/one"])

    def test_unconfigured_and_ambiguous_destinations_are_rejected(self):
        for request in ("/", "/unknown", "/?host=localhost&port=993",
                        "/?host=imap.one.example&port=22", "/?host=imap.one.example",
                        "/?host=imap.one.example&port=bad",
                        "/?host=imap.one.example&host=imap.two.example&port=993"):
            with self.subTest(request=request), self.assertRaises(ValueError):
                relay.select_target(request, TARGETS)

    def test_image_route_accepts_one_encoded_url(self):
        self.assertEqual(
            relay.image_url("/_image?url=https%3A%2F%2Fimages.example%2Fa.png%3Fx%3D1"),
            "https://images.example/a.png?x=1",
        )
        self.assertIsNone(relay.image_url("/?host=imap.one.example&port=993"))
        for request in ("/_image", "/_image?url=", "/_image?url=a&url=b"):
            with self.subTest(request=request), self.assertRaises(ValueError):
                relay.image_url(request)


class HandshakeTests(unittest.IsolatedAsyncioTestCase):
    async def test_root_handshake_connects_only_to_matching_upstream(self):
        for host, port in TARGETS.values():
            reader = asyncio.StreamReader()
            reader.feed_data((f"GET /?host={host}&port={port} HTTP/1.1\r\n"
                              "Upgrade: websocket\r\nSec-WebSocket-Key: test\r\n\r\n").encode())
            reader.feed_eof()
            from unittest.mock import AsyncMock, Mock
            writer = Mock(drain=AsyncMock(), wait_closed=AsyncMock())
            tcp_reader = asyncio.StreamReader()
            tcp_reader.feed_eof()
            tcp_writer = Mock(drain=AsyncMock(), wait_closed=AsyncMock())
            with patch.object(relay.asyncio, "open_connection", new=AsyncMock(
                return_value=(tcp_reader, tcp_writer)
            )) as connect:
                await relay.handle(reader, writer, TARGETS,
                                   {"idle_timeout": 1, "max_bytes": 1024}, asyncio.Semaphore(1))
                connect.assert_awaited_once_with(host, port)
            self.assertTrue(writer.write.call_args.args[0].startswith(b"HTTP/1.1 101"))

    async def test_image_response_is_bounded_and_cross_origin_embeddable(self):
        reader = asyncio.StreamReader()
        reader.feed_data(("GET /_image?url=https%3A%2F%2Fimages.example%2Fa.png HTTP/1.1\r\n"
                          "Host: relay.example\r\n\r\n").encode())
        reader.feed_eof()
        from unittest.mock import AsyncMock, Mock
        writer = Mock(drain=AsyncMock(), wait_closed=AsyncMock())
        with patch.object(relay, "fetch_image", new=AsyncMock(
            return_value=("image/png", b"png")
        )) as fetch:
            await relay.handle(reader, writer, TARGETS, {
                "idle_timeout": 1, "max_bytes": 1024, "max_image_bytes": 512,
            }, asyncio.Semaphore(1))
        fetch.assert_awaited_once_with("https://images.example/a.png", 512, 1)
        response = writer.write.call_args.args[0]
        self.assertTrue(response.startswith(b"HTTP/1.1 200 OK"))
        self.assertIn(b"Cross-Origin-Resource-Policy: cross-origin", response)
        self.assertTrue(response.endswith(b"png"))


class ImageSafetyTests(unittest.IsolatedAsyncioTestCase):
    async def test_private_image_addresses_are_rejected(self):
        private = [(socket.AF_INET, socket.SOCK_STREAM, 6, "", ("127.0.0.1", 443))]
        with patch.object(relay.asyncio.get_running_loop(), "getaddrinfo",
                          new=AsyncMock(return_value=private)):
            with self.assertRaisesRegex(ValueError, "non-public"):
                await relay.public_address("localhost", 443)


if __name__ == "__main__":
    unittest.main()
