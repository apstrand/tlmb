#!/usr/bin/env bash
# Imports the release signing key into a fresh GnuPG home, unlocked.
#
#   MAILBOX_FLATPAK_GPG_PRIVATE_KEY=… MAILBOX_FLATPAK_GPG_PASSPHRASE=… \
#       packaging/flatpak/ci-import-key.sh <gnupg-home>
#
# The key comes in as an ASCII-armoured secret key. Signing goes through
# gpgme, which would ask a pinentry for the passphrase, and there is nobody to
# answer one in CI — so the passphrase is preset in this home's agent, which
# build.sh stops when it is done. Prints the key's fingerprint.

set -euo pipefail

home="${1:?usage: ci-import-key.sh <gnupg-home>}"
: "${MAILBOX_FLATPAK_GPG_PRIVATE_KEY:?set MAILBOX_FLATPAK_GPG_PRIVATE_KEY to the armoured secret key}"
passphrase="${MAILBOX_FLATPAK_GPG_PASSPHRASE:-}"

mkdir -p "$home"
chmod 700 "$home"
printf 'allow-preset-passphrase\n' >"$home/gpg-agent.conf"

gpg --homedir "$home" --batch --quiet --pinentry-mode loopback --passphrase "$passphrase" \
    --import <<<"$MAILBOX_FLATPAK_GPG_PRIVATE_KEY" 2>/dev/null

fingerprint="$(gpg --homedir "$home" --list-secret-keys --with-colons \
    | awk -F: '/^fpr/ { print $10; exit }')"
[[ -n $fingerprint ]] || { echo "no secret key was imported" >&2; exit 1; }

if [[ -n $passphrase ]]; then
    preset="$(gpgconf --list-dirs libexecdir)/gpg-preset-passphrase"
    gpg-connect-agent --homedir "$home" /bye >/dev/null
    # Every keygrip, since which of the key's parts signs is the key's choice.
    gpg --homedir "$home" --list-secret-keys --with-colons --with-keygrip \
        | awk -F: '/^grp/ { print $10 }' \
        | while read -r grip; do
            "$preset" --homedir "$home" --preset --passphrase "$passphrase" "$grip"
        done
fi

echo "$fingerprint"
