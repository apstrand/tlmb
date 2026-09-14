#!/usr/bin/env bash
# Prepares a fresh Ubuntu machine — a GitHub runner — to run build.sh.
#
# flatpak-builder comes from apt here, not from Flathub's org.flatpak.Builder
# as on a desktop: that one runs each build step on the host through the
# Flatpak session helper, which needs a session bus, a system bus and a
# runtime directory that a runner may not have. The runtime, SDK and Rust
# extension are left to build.sh's --install-deps-from.

set -euo pipefail

sudo=()
[[ $(id -u) == 0 ]] || sudo=(sudo)

"${sudo[@]}" apt-get update -q
"${sudo[@]}" apt-get install -y -q --no-install-recommends \
    flatpak flatpak-builder ostree gnupg dbus dbus-user-session git ca-certificates

# Ubuntu 24.04 has AppArmor refuse unprivileged user namespaces, and
# bubblewrap — so every Flatpak build and every `flatpak run` — needs them.
"${sudo[@]}" sysctl -w kernel.apparmor_restrict_unprivileged_userns=0 2>/dev/null || true

flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
