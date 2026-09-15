# Hosted deployment

The generated site is static, but browser IMAP access needs a relay process.
The relay must be reachable as `wss://` and must be behind a TLS reverse
proxy. It supports either one fixed destination or a named allow-list of
destinations. Named routes keep the upstream choice out of browser data:

```sh
python crates/mailbox-wasm/mailbox-relay.py \
  --upstream own=imap.example.com:993 \
  --upstream google=imap.gmail.com:993 \
  --allowed-host imap.example.com --allowed-host imap.gmail.com \
  --tls-terminated
```

Use `wss://relay.example.com/` for a site serving multiple providers. The browser
adds the prepared account's IMAP host and port to account-test and sync requests.
The relay matches these against its configured upstreams and rejects any
destination outside that list. Named URLs such as `/google` or `/own` still
select a single fixed upstream for compatibility.

One browser can retain several accounts. Use **Add account** in the mailbox
toolbar; each account keeps its own credential and OPFS checkpoint. Listings
use the core's unified mailbox, and sync connects each account separately.
Use a distinct optional account name when two addresses would otherwise
produce the same account ID.

1. Build the wasm package and generate the site with the public relay URL:

   ```sh
   ./crates/mailbox-wasm/build-web.sh
   cargo run -p mailbox-cli -- site --out /srv/mailbox-site \
     --relay wss://relay.example.com/
   ```

   The `wasm-bindgen` tool version must match the Rust dependency. To use a
   separately installed matching tool, set `WASM_BINDGEN=/path/to/wasm-bindgen`
   when running the build or deployment script.

2. Install `mailbox-relay.service`, change its `ExecStart` to the desired
   `--upstream NAME=HOST:PORT` entries and `--allowed-host` values, create the
   unprivileged `mailbox-relay` user, and enable it with systemd.

3. Install `deploy/Caddyfile` as `/etc/caddy/Caddyfile`, replacing both host
   names and the static-site path. Caddy obtains certificates and forwards the
   WebSocket upgrade to `127.0.0.1:9001`.

   If you use Nginx instead, install `deploy/nginx.conf`, replace the host
   names, certificate paths, and static-site path, then validate with
   `nginx -t` before reloading. The relay location must keep HTTP/1.1 upgrade
   headers and a read timeout longer than the relay idle timeout.

4. Verify the deployment from any machine:

   ```sh
   python crates/mailbox-wasm/deploy-check.py \
     --site https://mail.example.com/ \
     --relay wss://relay.example.com/ --imap-host imap.example.com
   ```

The check validates HTTPS, COOP/COEP, the generated config, and the WSS
upgrade. It does not log in to the upstream mailbox. After it passes, open the
site, enter the IMAP/SMTP servers, and use the account Test button.

For the Wonko deployment, `push_to_wonko.sh` defaults to the shared root URL:

```sh
./deploy/push_to_wonko.sh
```

The service routes are `/zarq` for `mail.zarquon.se:993`, `/nena` for
`mail.nena.se:993`, and `/u2615` for `mail.2615.io:993`. The shared root URL
supports all three destinations according to the account being connected.
