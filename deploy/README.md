# Hosted deployment

The generated site is static, but browser IMAP access needs a relay process.
The relay must be reachable as `wss://` and must be behind a TLS reverse
proxy. It is a fixed-destination pipe: run one instance for each allowed
upstream endpoint, or replace it with a policy-aware multi-tenant service.

1. Build the wasm package and generate the site with the public relay URL:

   ```sh
   ./crates/mailbox-wasm/build-web.sh
   cargo run -p mailbox-cli -- site --out /srv/mailbox-site \
     --relay wss://relay.example.com/
   ```

2. Install `mailbox-relay.service`, change its `--target` and
   `--allowed-host`, create the unprivileged `mailbox-relay` user, and enable
   it with systemd.

3. Install `deploy/Caddyfile` as `/etc/caddy/Caddyfile`, replacing both host
   names and the static-site path. Caddy obtains certificates and forwards the
   WebSocket upgrade to `127.0.0.1:9001`.

4. Verify the deployment from any machine:

   ```sh
   python crates/mailbox-wasm/deploy-check.py \
     --site https://mail.example.com/ \
     --relay wss://relay.example.com/
   ```

The check validates HTTPS, COOP/COEP, the generated config, and the WSS
upgrade. It does not log in to the upstream mailbox. After it passes, open the
site, enter the IMAP/SMTP servers, and use the account Test button.
