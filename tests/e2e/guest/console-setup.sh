#!/bin/bash
# VGA/serial that a human can actually use. The SPICE window is a getty, not
# the test run — tests happen over SSH. Autologin + a banner so the console
# is a shell with logs, not a locked prompt.
set -euo pipefail

USER_NAME=${MAILBOX_E2E_USER:-mailbox}

echo "${USER_NAME}:mailbox" | sudo chpasswd
sudo passwd -u "$USER_NAME" >/dev/null 2>&1 || true

sudo mkdir -p /etc/systemd/system/getty@tty1.service.d
sudo tee /etc/systemd/system/getty@tty1.service.d/autologin.conf >/dev/null <<EOF
[Service]
ExecStart=
ExecStart=-/sbin/agetty --autologin ${USER_NAME} --noclear %I \$TERM
EOF

sudo systemctl enable sshd serial-getty@ttyS0
sudo systemctl start sshd serial-getty@ttyS0 || true
sudo systemctl daemon-reload
sudo systemctl restart getty@tty1 || true

sudo tee /etc/profile.d/mailbox-e2e.sh >/dev/null <<'EOF'
# Only the physical/serial consoles — not SSH.
case $(tty) in
/dev/tty1|/dev/ttyS0|/dev/console) ;;
*) return 0 ;;
esac
echo
echo "mailbox-e2e — this screen is a debug shell, not the test run."
echo "TUI/GUI run over SSH (tmux / headless Sway). Evidence is pulled to the host."
echo
echo "  sshd:    $(systemctl is-active sshd 2>/dev/null || echo unknown)"
echo "  address: $(ip -4 -o addr show scope global | awk '{print $4}' | paste -sd' ' -)"
echo "  journalctl -u sshd -u cloud-init -n 40 --no-pager"
echo
EOF
sudo chmod 644 /etc/profile.d/mailbox-e2e.sh

echo "mailbox-e2e: console autologin on tty1, password mailbox, serial-getty on ttyS0"
