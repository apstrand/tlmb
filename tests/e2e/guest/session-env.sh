#!/bin/bash
# Source this in the guest to talk to the compositor the way a desktop session would.
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"
if [[ -z ${WAYLAND_DISPLAY:-} ]]; then
  for sock in "$XDG_RUNTIME_DIR"/wayland-*; do
    [[ -S $sock ]] && export WAYLAND_DISPLAY=$(basename "$sock") && break
  done
fi
if [[ -z ${SWAYSOCK:-} ]]; then
  for sock in "$XDG_RUNTIME_DIR"/sway-ipc.*; do
    [[ -S $sock ]] && export SWAYSOCK=$sock && break
  done
fi
export MAILBOX_CONFIG="${MAILBOX_CONFIG:-$HOME/.config/mailbox/config.toml}"
export MAILBOX_STATE_DIR="${MAILBOX_STATE_DIR:-$HOME/.local/share/mailbox}"
export MAILBOX_PASSWORD_E2E="${MAILBOX_PASSWORD_E2E:-testpass}"
export MAILBOX_NOTIFIER=none
export MAILBOX_CLOCK="${MAILBOX_CLOCK:-2026-09-01T12:00:00Z}"
export MAILBOX_OPENER=/bin/true
export GDK_BACKEND="${GDK_BACKEND:-wayland}"
export WINIT_UNIX_BACKEND="${WINIT_UNIX_BACKEND:-wayland}"
# Headless Sway has no GPU; WebKitGTK otherwise dies at first paint.
export WEBKIT_DISABLE_DMABUF_RENDERER="${WEBKIT_DISABLE_DMABUF_RENDERER:-1}"
export WLR_BACKENDS="${WLR_BACKENDS:-headless}"
export WLR_RENDERER="${WLR_RENDERER:-pixman}"
export WLR_LIBINPUT_NO_DEVICES="${WLR_LIBINPUT_NO_DEVICES:-1}"
