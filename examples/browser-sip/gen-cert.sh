#!/usr/bin/env bash
# Generate the WSS certificate for the browser-sip lab.
#
# Prefers mkcert (https://github.com/FiloSottile/mkcert) because its CA
# is trusted by your browser automatically — zero interstitials. Falls
# back to a plain openssl self-signed cert; the README explains the
# one-time browser-trust click that needs.
#
# Output: certs/wss-cert.pem + certs/wss-key.pem, SANs for
# localhost / 127.0.0.1 — matching lab.toml's [sip.wss] block.
set -euo pipefail
cd "$(dirname "$0")/certs"

if command -v mkcert >/dev/null 2>&1; then
    mkcert -install >/dev/null
    mkcert -cert-file wss-cert.pem -key-file wss-key.pem localhost 127.0.0.1
    echo "mkcert certificate written; your browser already trusts it."
else
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 \
        -keyout wss-key.pem -out wss-cert.pem -days 825 -nodes \
        -subj "/CN=siphon-ai browser-sip lab" \
        -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" 2>/dev/null
    echo "self-signed certificate written."
    echo "Browser trust (one-time): with the daemon running, open"
    echo "  https://127.0.0.1:8443/"
    echo "and click through the warning (Advanced -> Proceed). The page"
    echo "itself will error — that's fine; the exception it records is"
    echo "what lets wss://127.0.0.1:8443 connect."
fi
chmod 600 wss-key.pem
