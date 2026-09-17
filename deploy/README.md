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
Browser sync fetches only the newest 100 inbox headers and server counts for
all folders. Opening another folder fetches one recent header batch; **Load more
headers** fetches another batch in the current folder. Bodies load when opened.
Folder badges show total messages, with unread counts in their tooltips.
The list indicates when older mail has not been downloaded. Deletes update
locally and queue a move to Trash for the next sync.

**Discover servers** uses the relay's bounded `/_discover` endpoint. The relay
tries the domain's HTTPS autoconfiguration, Mozilla ISPDB, Exchange
Autodiscover, DNS SRV records, and finally MX-based conventional names. It
accepts only public HTTPS destinations, caps downloaded configuration documents
at 1 MiB, and returns no credentials.
Discovery does not expand the relay's IMAP allow-list: the resulting IMAP host
and port must still match one of the configured `--upstream` destinations.

Remote message images load through the relay's bounded `/_image` endpoint only
after the reader chooses **Load images**. The endpoint is needed for image hosts
that send `Cross-Origin-Resource-Policy: same-origin`; it accepts only public
HTTP(S) destinations on ports 80 and 443, follows the same checks after every
redirect, requires an image response, and caps each body at 10 MiB by default.
The frame's CSP still blocks every remote image until the explicit choice.
Tracking URLs can still tell their sender that the message was read, although
the image host sees the relay's network address rather than the browser's.
When two addresses have the same local part, the first keeps the short account
ID and the next automatically uses its complete email address. The optional
account-name field can still override either ID.

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

The check validates HTTPS, COOP/COEP, the generated config, the WSS upgrade,
and the image and discovery endpoints. It does not log in to the upstream
mailbox. After it passes, open the site and use **Discover servers** or enter
the settings manually, then use the account Test button.

For the Wonko deployment, the GitHub Actions workflow `.github/workflows/hosted-deploy.yml`
publishes on version tags (`v*`) and can also be started with **Run workflow**.
Configure these repository secrets: `MAILBOX_DEPLOY_SSH_KEY`,
`MAILBOX_DEPLOY_KNOWN_HOSTS`, `MAILBOX_DEPLOY_HOST`, `MAILBOX_DEPLOY_USER`,
`MAILBOX_DEPLOY_SITE_URL`, `MAILBOX_DEPLOY_RELAY_URL`, and
`MAILBOX_DEPLOY_IMAP_HOST`. `MAILBOX_DEPLOY_RELAY_DIR` and
`MAILBOX_DEPLOY_SITE_DIR` are optional when the defaults (`web/relay` and
`web/tlmb`) are used. OAuth client secrets use the `MAILBOX_*_CLIENT_ID` and
`MAILBOX_*_CLIENT_SECRET` names described in `Release.md`.

For a local deployment, `push_to_wonko.sh` remains available and defaults to the shared root URL:

```sh
./deploy/push_to_wonko.sh
```

The service routes are `/zarq` for `mail.zarquon.se:993`, `/nena` for
`mail.nena.se:993`, and `/u2615` for `mail.2615.io:993`. The shared root URL
supports all three destinations according to the account being connected.

The Linux desktop app has its own static site on the same host —
`tlmb-flat.2615.io`, from `web/tlmb-flat/` — holding the signed Flatpak update
repository. It is unrelated to the web client and is published by
`packaging/flatpak/build.sh --release`; see `Release.md` and
`nginx-flatpak.conf`.
