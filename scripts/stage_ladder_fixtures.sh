#!/usr/bin/env bash
set -euo pipefail

# Stage what `toolchain_test` fetches from on the self-hosting root.
#
# Usage: stage_ladder_fixtures.sh <dir>
#
#   git/greeting.git   a bare repository of one crate, which cargo fetches
#                      through libgit2; made at a fixed date, so reproducible.
#   registry/          a sparse registry of one crate, which the guest serves
#                      over loopback TLS for libcurl and Mbed TLS: `www` is
#                      served, `ca.pem` is the root to trust,
#                      `server.der`/`server.key` (raw P-256 scalar) the
#                      served identity. The keys are fresh each time and the
#                      root's key is discarded.

SELF="stage_ladder_fixtures"
DIR="${1:?usage: stage_ladder_fixtures.sh <dir>}"
REGISTRY_ORIGIN="https://127.0.0.1:4433"

die() {
    echo "$SELF: $*" >&2
    exit 1
}

command -v git >/dev/null 2>&1 || die "git is required to stage the git fixture"
command -v openssl >/dev/null 2>&1 || die "openssl is required to stage the registry"
rm -rf "$DIR"
mkdir -p "$DIR"
DIR="$(cd "$DIR" && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

crate() {
    local dir="$1" says="$2"
    mkdir -p "$dir/src"
    printf '%s\n' '[package]' 'name = "greeting"' 'version = "0.1.0"' 'edition = "2021"' >"$dir/Cargo.toml"
    printf '%s\n' 'pub fn greeting() -> &'"'"'static str {' "    \"$says\"" '}' >"$dir/src/lib.rs"
}

crate "$WORK/git" "fetched through libgit2"
mkdir -p "$DIR/git"
(
    cd "$WORK/git"
    export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
    export GIT_AUTHOR_NAME="The SlopOS Authors" GIT_AUTHOR_EMAIL="ladder@slopos.invalid"
    export GIT_COMMITTER_NAME="$GIT_AUTHOR_NAME" GIT_COMMITTER_EMAIL="$GIT_AUTHOR_EMAIL"
    export GIT_AUTHOR_DATE="2026-01-01T00:00:00Z" GIT_COMMITTER_DATE="2026-01-01T00:00:00Z"
    git init -q -b main . &&
        git add Cargo.toml src/lib.rs &&
        git commit -q -m "greeting: the ladder's git fixture" &&
        git clone -q --bare --no-hardlinks "$WORK/git" "$DIR/git/greeting.git"
) || die "could not make the git fixture in $DIR/git/greeting.git"

reg="$DIR/registry"
www="$reg/www"
mkdir -p "$www/index/gr/ee" "$www/crates"
crate "$WORK/greeting-0.1.0" "fetched over https"
tar --sort=name --mtime=@1767225600 --owner=0 --group=0 --numeric-owner \
    --mode=u=rwX,go=rX --format=ustar -C "$WORK" -cf - greeting-0.1.0 |
    gzip -9n >"$www/crates/greeting-0.1.0.crate" || die "could not pack the registry's crate"
cksum="$(sha256sum "$www/crates/greeting-0.1.0.crate" | cut -d' ' -f1)"
printf '{"dl":"%s/crates/{crate}-{version}.crate"}\n' "$REGISTRY_ORIGIN" >"$www/index/config.json"
printf '{"name":"greeting","vers":"0.1.0","deps":[],"cksum":"%s","features":{},"yanked":false}\n' \
    "$cksum" >"$www/index/gr/ee/greeting"

cnf="$WORK/openssl.cnf"
cat >"$cnf" <<'EOF'
[req]
distinguished_name = dn
prompt = no
[dn]
[root]
basicConstraints = critical, CA:TRUE
keyUsage = critical, keyCertSign, cRLSign
subjectKeyIdentifier = hash
[leaf]
basicConstraints = critical, CA:FALSE
keyUsage = critical, digitalSignature
extendedKeyUsage = serverAuth
subjectAltName = IP:127.0.0.1, DNS:localhost
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid
EOF
{
    openssl ecparam -name prime256v1 -genkey -noout -out "$WORK/root.key" &&
        openssl ecparam -name prime256v1 -genkey -noout -out "$WORK/server.key" &&
        openssl req -x509 -new -config "$cnf" -extensions root -key "$WORK/root.key" \
            -subj "/CN=SlopOS Ladder Test Root" -days 3650 -sha256 -out "$reg/ca.pem" &&
        openssl req -new -config "$cnf" -key "$WORK/server.key" -subj "/CN=127.0.0.1" \
            -out "$WORK/server.csr" &&
        openssl x509 -req -in "$WORK/server.csr" -CA "$reg/ca.pem" -CAkey "$WORK/root.key" \
            -set_serial 2 -days 3650 -sha256 -extfile "$cnf" -extensions leaf \
            -outform DER -out "$reg/server.der" &&
        openssl ec -in "$WORK/server.key" -no_public -outform DER -out "$WORK/server.sec1"
} >/dev/null 2>&1 || die "openssl could not make the registry's certificates"
# RFC 5915's ECPrivateKey: SEQUENCE, version 1, then the scalar as a 32-byte
# OCTET STRING at offset 7.
sec1="$WORK/server.sec1"
[ "$(od -An -tx1 -j2 -N5 "$sec1" | tr -d ' \n')" = "0201010420" ] || die "$sec1 is not a SEC1 P-256 private key"
head -c 39 "$sec1" | tail -c 32 >"$reg/server.key"
