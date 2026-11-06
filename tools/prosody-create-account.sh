#!/bin/bash
# Privileged helper: create an XMPP account on the local Prosody.
# Usage: prosody-create-account.sh <username> <password>
# Invoked (via sudo) by the syarc-console meshlink app. Input is strictly
# validated here; never run this by hand with unvalidated args.
set -euo pipefail
USER_NAME="$1"
PASSWORD="$2"
HOST="server1.example.net"

if ! [[ "$USER_NAME" =~ ^[a-z0-9][a-z0-9._-]{2,19}$ ]]; then
    echo "BADUSERNAME" >&2; exit 2
fi
if [ ${#PASSWORD} -lt 8 ] || [ ${#PASSWORD} -gt 128 ]; then
    echo "BADPASSWORD" >&2; exit 3
fi
if prosodyctl user list "$HOST" 2>/dev/null | cut -d'@' -f1 | grep -qx "$USER_NAME"; then
    echo "EXISTS" >&2; exit 4
fi
exec prosodyctl register "$USER_NAME" "$HOST" "$PASSWORD"
