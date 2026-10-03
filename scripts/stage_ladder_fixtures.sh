#!/usr/bin/env bash
set -euo pipefail

# Stage what `toolchain_test` fetches from on the self-hosting root.
#
# Usage: stage_ladder_fixtures.sh <dir> [<sshd dir>]
#
#   git/greeting.git   a bare repository of one crate, which cargo fetches
#                      through libgit2; made at a fixed date, so reproducible.
#   registry/          a sparse registry of one crate, which the guest serves
#                      over loopback TLS for libcurl and Mbed TLS: `www` is
#                      served, `ca.pem` is the root to trust,
#                      `server.der`/`server.key` (raw P-256 scalar) the
#                      served identity. The keys are fresh each time and the
#                      root's key is discarded.
#   ssh/               with <sshd dir>: the guest's half of git over ssh to
#                      the host at 10.0.2.4 — `id_ed25519` (mode 0600, as ssh
#                      demands of a key its user owns) and its `.pub`, a
#                      `known_hosts` naming the host key, and `user`, the
#                      host account it logs in as.
#
# <sshd dir> is the host's half, which stays on the host: the host key,
# `authorized_keys` holding the guest's key alone, `git-only`, the forced
# command, and `sshd_config`. qemu_run.sh runs `sshd -i -f <sshd
# dir>/sshd_config` per connection, logging to `<sshd dir>/sshd.log` and
# naming the checkout and the push repository in `LADDER_CHECKOUT` and
# `LADDER_PUSH`; `git-only` runs `git-upload-pack` on the first for
# `/checkout`, `git-receive-pack` on the second for `/push`, and refuses
# anything else. Both keys are fresh each time; the directory is 0700 and
# `authorized_keys` 0600, so `StrictModes no` loses nothing and spares sshd
# walking `builddir`'s parents.

SELF="stage_ladder_fixtures"
DIR="${1:?usage: stage_ladder_fixtures.sh <dir> [<sshd dir>]}"
SSHD="${2:-}"
REGISTRY_ORIGIN="https://127.0.0.1:4433"
SSH_PEER_ADDR="10.0.2.4"

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

[ -n "$SSHD" ] || exit 0
command -v ssh-keygen >/dev/null 2>&1 || die "ssh-keygen is required to stage the ssh fixture"
GIT="$(command -v git)"
HOST_USER="$(id -un)"
rm -rf "$SSHD"
mkdir -p "$SSHD"
chmod 0700 "$SSHD"
SSHD="$(cd "$SSHD" && pwd)"
case "$SSHD$GIT$HOST_USER" in
    *[[:space:]%\"\'\\]*) die "sshd_config cannot name $SSHD, $GIT or the user $HOST_USER" ;;
esac
ssh="$DIR/ssh"
mkdir -p "$ssh"
{
    ssh-keygen -q -t ed25519 -N '' -C "slopos-ladder-host" -f "$SSHD/ssh_host_ed25519_key" &&
        ssh-keygen -q -t ed25519 -N '' -C "slopos-ladder-guest" -f "$ssh/id_ed25519"
} </dev/null >/dev/null || die "ssh-keygen could not make the ssh fixture's keys"
chmod 0600 "$ssh/id_ed25519"
printf '%s\n' "$HOST_USER" >"$ssh/user"
printf '%s %s\n' "$SSH_PEER_ADDR" "$(cut -d' ' -f1-2 "$SSHD/ssh_host_ed25519_key.pub")" >"$ssh/known_hosts"
printf 'restrict %s\n' "$(cat "$ssh/id_ed25519.pub")" >"$SSHD/authorized_keys"

cat >"$SSHD/git-only" <<EOF
#!/bin/sh
# Generated by scripts/$SELF.sh — do not edit.
case "\$SSH_ORIGINAL_COMMAND" in
    "git-upload-pack '/checkout'")
        [ -d "\${LADDER_CHECKOUT:-}" ] || { echo "ladder sshd: no checkout to serve" >&2; exit 1; }
        exec "$GIT" upload-pack --strict "\$LADDER_CHECKOUT"
        ;;
    "git-receive-pack '/push'")
        [ -d "\${LADDER_PUSH:-}" ] || { echo "ladder sshd: no push repository" >&2; exit 1; }
        exec "$GIT" receive-pack "\$LADDER_PUSH"
        ;;
esac
echo "ladder sshd: refused \${SSH_ORIGINAL_COMMAND:-a shell}" >&2
exit 1
EOF
chmod 0700 "$SSHD/git-only"

cat >"$SSHD/sshd_config" <<EOF
# Generated by scripts/$SELF.sh — do not edit.
HostKey $SSHD/ssh_host_ed25519_key
AuthorizedKeysFile $SSHD/authorized_keys
AuthorizedPrincipalsFile none
AllowUsers $HOST_USER
AuthenticationMethods publickey
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
HostbasedAuthentication no
UsePAM no
PermitRootLogin prohibit-password
StrictModes no
ForceCommand $SSHD/git-only
AcceptEnv GIT_PROTOCOL
DisableForwarding yes
PermitTTY no
PermitTunnel no
PermitUserRC no
PermitUserEnvironment no
PidFile none
PrintMotd no
LoginGraceTime 30
MaxAuthTries 2
MaxSessions 1
EOF
chmod 0600 "$SSHD/sshd_config" "$SSHD/authorized_keys"
