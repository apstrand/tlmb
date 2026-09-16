#!/bin/bash
# First SSH after cloud-init: Dovecot, the mail-port filter, compositor config.
set -euo pipefail

HERE=$(cd -- "$(dirname -- "$(readlink -f "$0")")" && pwd)
ROOT=${MAILBOX_E2E_ROOT:-$HOME/mailbox}

sudo useradd -r -u 5000 -d /srv/mail -s /usr/bin/nologin vmail 2>/dev/null || true
sudo mkdir -p /srv/mail
sudo chown vmail:vmail /srv/mail

# Arch is Dovecot 2.4; the L2 file is 2.3 and will not start.
sudo cp "$HERE/dovecot.conf" /etc/dovecot/dovecot.conf
# Stock Arch config includes conf.d/; our file does not, but leftover drop-ins
# must not sneak back in.
sudo mkdir -p /etc/dovecot/conf.d.disabled
if [[ -d /etc/dovecot/conf.d ]]; then
  sudo mv /etc/dovecot/conf.d/* /etc/dovecot/conf.d.disabled/ 2>/dev/null || true
fi
if ! sudo systemctl enable --now dovecot; then
  echo "dovecot failed to start:" >&2
  sudo journalctl -u dovecot --no-pager -n 80 >&2 || true
  sudo doveconf -n >&2 || true
  exit 1
fi

sudo systemctl enable --now sshd
MAILBOX_E2E_USER=$(id -un) bash "$HERE/console-setup.sh"

# Isolate has no NTP. systemd-time-wait-sync uses TimeoutStartSec=infinity,
# and the cloud image's pacman-init is After=time-sync.target Before=sshd —
# so the first isolated boot never starts sshd (connection refused, ping works).
sudo timedatectl set-ntp false >/dev/null 2>&1 || true
sudo systemctl disable --now systemd-timesyncd.service >/dev/null 2>&1 || true
sudo systemctl disable pacman-init.service >/dev/null 2>&1 || true
sudo systemctl mask systemd-time-wait-sync.service >/dev/null 2>&1 || true
sudo systemctl enable --now qemu-guest-agent >/dev/null 2>&1 || true
sudo systemctl enable --now sshd dovecot
# cloud-init can leave nftables ahead of libnftnl; the CLI then dies with
# LIBNFTNL_19 not found. Reinstall the pair before loading the ruleset.
sudo pacman -S --noconfirm libnftnl nftables >/dev/null
sudo cp "$HERE/nftables.conf" /etc/nftables.conf
if ! sudo nft -f /etc/nftables.conf; then
  echo "nftables ruleset failed to load (mail still loopback-only in config)" >&2
else
  sudo systemctl enable nftables
fi

mkdir -p "$HOME/.config/sway" "$HOME/.config/mailbox-e2e"
cp "$HERE/sway.conf" "$HOME/.config/sway/config"

# A sink that accepts submission and writes the bytes, and never talks to anyone.
chmod +x "$HERE/smtp-sink.py" "$HERE/imap-append.py" "$HERE/session-env.sh" \
  "$HERE/compositor.sh" "$HERE/capture.sh" "$HERE/record.sh" "$HERE/seed.sh" \
  "$HERE/run.sh" "$HERE/probe-isolation.sh" "$HERE/console-setup.sh" \
  "$HERE"/tests/*.sh

echo "mailbox-e2e: bootstrap done"
