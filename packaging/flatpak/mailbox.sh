#!/bin/sh
# `mailbox`, inside the Flatpak.
#
# The sandbox points XDG_CONFIG_HOME and XDG_DATA_HOME into ~/.var/app, which
# would give the packaged app a mailbox of its own: a second config, a second
# store, and a second sync engine fighting the first for the same account. The
# manifest grants the host's `mailbox` directories instead, and this points
# the binary at them, so the window, a `mailbox` on the host, and
# `flatpak run --command=mailbox com.thelastmailbox.flat tui` are one mailbox.
#
# Flatpak exports the host's own XDG variables as HOST_XDG_* when they are set,
# which is also how it resolved the `xdg-config` and `xdg-data` grants.

config_home="${HOST_XDG_CONFIG_HOME:-$HOME/.config}"
data_home="${HOST_XDG_DATA_HOME:-$HOME/.local/share}"

export MAILBOX_CONFIG="${MAILBOX_CONFIG:-$config_home/mailbox/config.toml}"
export MAILBOX_STATE_DIR="${MAILBOX_STATE_DIR:-$data_home/mailbox}"

exec /app/libexec/mailbox "$@"
