# The L2 server (§14.2): a real Dovecot, in-tree so its configuration is part of
# the repository rather than part of someone's laptop.
#
# Debian's package is used rather than a prebuilt image because the *config* is
# the interesting part: CONDSTORE, QRESYNC, SPECIAL-USE, and the folder layout
# below are what the sync engine's fast paths key off, and a test server that
# quietly stops advertising one of them would silently move every test onto the
# slow path and still pass.
FROM debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends dovecot-imapd \
 && rm -rf /var/lib/apt/lists/*

# One system user owns every test mailbox. Tests isolate by *account*, not by
# uid: §14's reset is per-account, and a fresh account is a fresh directory.
RUN useradd -r -u 5000 -d /srv/mail -s /usr/sbin/nologin vmail \
 && mkdir -p /srv/mail \
 && chown vmail:vmail /srv/mail

COPY dovecot.conf /etc/dovecot/dovecot.conf

EXPOSE 143
CMD ["/usr/sbin/dovecot", "-F"]
