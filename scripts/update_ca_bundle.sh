#!/usr/bin/env bash
# Replace assets/certs/ca-certificates.crt with the Mozilla root bundle curl.se
# extracted on DATE, after checking it against the checksum curl.se publishes.
#
#     scripts/update_ca_bundle.sh 2026-08-13
#
# The bundle is committed rather than fetched by the build, so an offline
# checkout still builds images that verify TLS servers.
set -euo pipefail

DATE="${1:?usage: update_ca_bundle.sh YYYY-MM-DD (see https://curl.se/docs/caextract.html)}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BASE="${CA_BUNDLE_BASE_URL:-https://curl.se/ca}"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

curl -fsSL "$BASE/cacert-${DATE}.pem" -o "$TMP/bundle.pem"
curl -fsSL "$BASE/cacert-${DATE}.pem.sha256" -o "$TMP/bundle.sha256"
want="$(cut -d' ' -f1 "$TMP/bundle.sha256")"
got="$(sha256sum "$TMP/bundle.pem" | cut -d' ' -f1)"
if [ "$want" != "$got" ]; then
    echo "update_ca_bundle: checksum mismatch: curl.se says $want, got $got" >&2
    exit 1
fi
count="$(grep -c 'BEGIN CERTIFICATE' "$TMP/bundle.pem")"
install -m 0644 "$TMP/bundle.pem" "$REPO_ROOT/assets/certs/ca-certificates.crt"
echo "update_ca_bundle: $count roots as of $DATE ($got)"
