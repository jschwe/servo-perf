#!/bin/sh
# Generate a self-signed TLS cert for the localhost fixtures. Idempotent.
set -eu
dir="$(dirname "$0")"
if [ -f "$dir/cert.pem" ] && [ -f "$dir/key.pem" ]; then
    exit 0
fi
openssl req -x509 -newkey rsa:2048 \
    -keyout "$dir/key.pem" -out "$dir/cert.pem" \
    -days 365 -nodes -subj "/CN=localhost" 2>/dev/null
chmod 600 "$dir/key.pem"
echo "generated $dir/{cert,key}.pem"
