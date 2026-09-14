#!/bin/sh
# The app's command: the window. Everything else is `--command=mailbox`.
exec /app/bin/mailbox gui "$@"
