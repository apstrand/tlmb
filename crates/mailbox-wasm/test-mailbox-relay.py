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

    def test_discovery_route_accepts_one_email_address(self):
        self.assertEqual(
            relay.discovery_address("/_discover?address=me%40example.com"),
            "me@example.com",
        )
        self.assertIsNone(relay.discovery_address("/?host=imap.example.com&port=993"))
        for request in ("/_discover", "/_discover?address=", "/_discover?address=a&address=b"):
            with self.subTest(request=request), self.assertRaises(ValueError):
                relay.discovery_address(request)

    def test_thunderbird_autoconfig_is_translated_for_the_setup_form(self):
        document = b"""<?xml version="1.0"?>
        <clientConfig><emailProvider>
          <incomingServer type="imap"><hostname>imap.example.com</hostname><port>993</port>
            <socketType>SSL</socketType><username>%EMAILADDRESS%</username></incomingServer>
          <outgoingServer type="smtp"><hostname>smtp.example.com</hostname><port>587</port>
            <socketType>STARTTLS</socketType><username>%EMAILLOCALPART%</username></outgoingServer>
        </emailProvider></clientConfig>"""
        found = relay.parse_autoconfig(document, "peter@example.com", "autoconfig")
        self.assertEqual(found["imap_host"], "imap.example.com")
        self.assertEqual(found["imap_security"], "implicit")
        self.assertEqual(found["imap_user"], "peter@example.com")
        self.assertEqual(found["smtp_user"], "peter")
        self.assertEqual(found["imap_source"], "autoconfig")

    def test_dns_srv_response_is_parsed(self):
        transaction = 42
        question = relay.encode_dns_name("_imaps._tcp.example.com") + b"\x00\x21\x00\x01"
        target = relay.encode_dns_name("mail.example.com")
        record = b"\xc0\x0c" + b"\x00\x21\x00\x01\x00\x00\x00\x3c"
        record += len(b"\x00\x0a\x00\x05\x03\xe1" + target).to_bytes(2, "big")
        record += b"\x00\x0a\x00\x05\x03\xe1" + target
        packet = (transaction.to_bytes(2, "big") + b"\x81\x80\x00\x01\x00\x01\x00\x00\x00\x00"
                  + question + record)
        self.assertEqual(
            relay.parse_dns_records(packet, transaction, 33),
            [(10, -5, "mail.example.com", 993)],
        )

    def test_exchange_autodiscover_is_translated_for_the_setup_form(self):
        document = b"""<Autodiscover><Response><Account>
          <Protocol><Type>IMAP</Type><Server>imap.example.com</Server>
            <Port>143</Port><SSL>off</SSL><Encryption>TLS</Encryption></Protocol>
          <Protocol><Type>SMTP</Type><Server>smtp.example.com</Server>
            <Port>465</Port><SSL>on</SSL></Protocol>
        </Account></Response></Autodiscover>"""
        found = relay.parse_autodiscover(document)
        self.assertEqual(found["imap_security"], "starttls")
        self.assertEqual(found["smtp_security"], "implicit")
        self.assertEqual(found["imap_source"], "autodiscover")


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

    async def test_discovery_response_is_json_and_cross_origin_readable(self):
        reader = asyncio.StreamReader()
        reader.feed_data(("GET /_discover?address=me%40example.com HTTP/1.1\r\n"
                          "Host: relay.example\r\n\r\n").encode())
        reader.feed_eof()
        from unittest.mock import AsyncMock, Mock
        writer = Mock(drain=AsyncMock(), wait_closed=AsyncMock())
        answer = {"known": True, "imap_host": "imap.example.com"}
        with patch.object(relay, "discover_servers", new=AsyncMock(
            return_value=answer
        )) as discover:
            await relay.handle(reader, writer, TARGETS, {
                "idle_timeout": 1, "max_bytes": 1024, "max_image_bytes": 512,
                "max_discovery_bytes": 2048,
            }, asyncio.Semaphore(1))
        discover.assert_awaited_once_with("me@example.com", 2048, 1)
        response = writer.write.call_args.args[0]
        self.assertTrue(response.startswith(b"HTTP/1.1 200 OK"))
        self.assertIn(b"Content-Type: application/json", response)
        self.assertIn(b"Access-Control-Allow-Origin: *", response)
        self.assertTrue(response.endswith(b'{"known":true,"imap_host":"imap.example.com"}'))


class ImageSafetyTests(unittest.IsolatedAsyncioTestCase):
    async def test_private_image_addresses_are_rejected(self):
        private = [(socket.AF_INET, socket.SOCK_STREAM, 6, "", ("127.0.0.1", 443))]
        with patch.object(relay.asyncio.get_running_loop(), "getaddrinfo",
                          new=AsyncMock(return_value=private)):
            with self.assertRaisesRegex(ValueError, "non-public"):
                await relay.public_address("localhost", 443)


class DiscoveryTests(unittest.IsolatedAsyncioTestCase):
    async def test_domain_autoconfig_wins_before_dns(self):
        document = b"""<clientConfig><emailProvider>
          <incomingServer type="imap"><hostname>imap.example.com</hostname><port>993</port>
            <socketType>SSL</socketType><username>%EMAILADDRESS%</username></incomingServer>
          <outgoingServer type="smtp"><hostname>smtp.example.com</hostname><port>587</port>
            <socketType>STARTTLS</socketType></outgoingServer>
        </emailProvider></clientConfig>"""
        with patch.object(relay, "fetch_document", new=AsyncMock(
            return_value=(200, document)
        )), patch.object(relay, "dns_records", new=AsyncMock()) as dns:
            found = await relay.discover_servers("me@example.com", 4096, 2)
        self.assertEqual(found["imap_host"], "imap.example.com")
        self.assertEqual(found["imap_source"], "autoconfig")
        dns.assert_not_awaited()

    async def test_srv_records_fill_both_servers_after_autoconfig_misses(self):
        async def records(name, qtype, timeout):
            if name.startswith("_imaps"):
                return [(0, 0, "mail.example.com", 993)]
            if name.startswith("_submission"):
                return [(0, 0, "submit.example.com", 587)]
            return []

        with patch.object(relay, "fetch_document", new=AsyncMock(
            return_value=(404, b"")
        )), patch.object(relay, "dns_records", new=AsyncMock(side_effect=records)):
            found = await relay.discover_servers("me@example.com", 4096, 2)
        self.assertEqual(found["imap_host"], "mail.example.com")
        self.assertEqual(found["smtp_host"], "submit.example.com")
        self.assertEqual(found["imap_source"], "srv")
        self.assertTrue(found["authoritative"])

    async def test_invalid_address_is_rejected_without_network(self):
        with patch.object(relay, "fetch_document", new=AsyncMock()) as fetch:
            with self.assertRaisesRegex(ValueError, "valid email"):
                await relay.discover_servers("localhost", 4096, 2)
        fetch.assert_not_awaited()


if __name__ == "__main__":
    unittest.main()
