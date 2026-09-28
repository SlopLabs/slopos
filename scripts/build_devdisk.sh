#!/usr/bin/env bash
set -euo pipefail

# Build the dev disk: the volume a cross-built toolchain lands on.
#
# Usage: build_devdisk.sh <image_path> <build_dir>
#
# The layout is a target sysroot — `lib/`, `include/`, `include/c++/v1/`,
# `licenses/` — plus, on a new volume, `src/slopos` with the toolchain
# `TOOLCHAIN_STAGE` holds at `src/slopos/third_party/rust-slopos`. A guest
# that mounts this volume has everything `x86_64-unknown-slopos` needs to
# compile and link a program, which the ISO's read-only appliance root
# deliberately does not carry.
#
# The image is `VERITY=off PRESERVE_FS_IMAGE=1`, which is the whole difference
# between this and the shipped root: a dev disk is a workbench, so the guest
# writes to it and a rebuild must not discard what the guest wrote. The volume
# therefore holds a *fresh* sysroot only on the run that creates it; after that
# `build_fs_image.sh` preserves what is there. Discard one with
# `rm -f <image> <image>.stamp`.
#
# Image building itself is `build_fs_image.sh`'s: this script stages a
# directory, hands it over as `FS_POPULATE_DIR`, and then writes a
# `SLOPOS-DEVDISK` marker at the volume root, which `devdisk_test` reads back
# in the guest.
#
# Environment:
#   DEV_DISK_SIZE - volume size (default: 4G). A cross-built toolchain with
#                   clang is ~0.9 GB, the seeded source ~0.05 GB, and each
#                   kernel variant built in the guest ~0.41 GB. A preserved
#                   smaller volume is grown in place with `resize2fs`.
#   DEV_DISK_INODE_RATIO - bytes of volume per inode (default: 16384, the
#                   mke2fs default). The C++ headers alone are ~1000 files.
#   TOOLCHAIN_STAGE - a cross-built toolchain prefix, staged on a new volume
#                   and graded against a preserved one. Unset stages the
#                   sysroot alone, which is what a run before the toolchain
#                   exists wants.
#
# A new volume is also seeded with `src/slopos`: the committed HEAD, the
# vendored crates, a `.cargo/config.toml` that reads them with no registry, and
# `.slopos-base`, the commit `scripts/export_devdisk.sh` diffs the guest's
# edits against. A preserved volume keeps the guest's tree, so the marker
# records only that it is there.
#
# A new volume also carries `git/greeting.git`, a bare repository of one crate
# that `devdisk_test` fetches through libgit2, made at a fixed date so it is
# reproducible; and `registry/`, a sparse registry of one crate it serves over
# loopback TLS for libcurl and OpenSSL: `www` is served, `ca.pem` is the root
# to trust, `server.der`/`server.key` (raw P-256 scalar) the served identity.
# The keys are fresh per volume and the root's key is discarded.
#
# The volume is labelled `slopos-dev`: the guest's disk letters are probe
# order, so the boot finds it with `mount=LABEL=slopos-dev:/devel`.

SELF="build_devdisk"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

IMAGE_PATH="${1:?Usage: build_devdisk.sh <image_path> <build_dir>}"
BUILD_DIR="${2:?Usage: build_devdisk.sh <image_path> <build_dir>}"

DEV_DISK_SIZE="${DEV_DISK_SIZE:-4G}"
DEV_DISK_LABEL="slopos-dev"
DEV_DISK_INODE_RATIO="${DEV_DISK_INODE_RATIO:-16384}"

USERLAND_TARGET="${USERLAND_TARGET:-x86_64-unknown-slopos}"
RELEASE_DIR="${BUILD_DIR}/target/${USERLAND_TARGET}/release"
CXX_DIR="${REPO_ROOT}/third_party/slopos-cxx"
STAGE="${BUILD_DIR}/devdisk-stage"
MARKER="SLOPOS-DEVDISK"

die() {
    echo "$SELF: $*" >&2
    exit 1
}

missing() {
    echo "$SELF: $1 is missing" >&2
    echo "  The dev disk is a target sysroot; without it the volume is an empty" >&2
    echo "  workbench that fails at link time inside the guest instead of here." >&2
    echo "  Build it:   $2" >&2
    exit 1
}

# `libc.a` is the one staged library `build_userland.sh` leaves in cargo's
# output directory rather than copying to the build directory, so both are
# searched and the error names the build directory either way.
#
# Answers through a global rather than on stdout: `missing` inside a command
# substitution exits only the subshell, and the caller would carry on staging
# an empty path.
staged_lib() {
    local name="$1" candidate
    for candidate in "${BUILD_DIR}/${name}" "${RELEASE_DIR}/${name}"; do
        if [ -f "$candidate" ]; then
            STAGED_LIB="$candidate"
            return 0
        fi
    done
    missing "${BUILD_DIR}/${name}" "scripts/build_userland.sh --test '${BUILD_DIR}'"
}

rm -rf "$STAGE"
mkdir -p "$STAGE/lib" "$STAGE/include/c++/v1"

for lib in libc.so libc.a libbuiltins.a crt0.o libc++.so; do
    staged_lib "$lib"
    cp -a "$STAGED_LIB" "$STAGE/lib/$lib"
done
[ -f "$CXX_DIR/lib/libc++.a" ] ||
    missing "$CXX_DIR/lib/libc++.a" "scripts/make_slopos_cxx.sh '${RELEASE_DIR}'"
cp -a "$CXX_DIR/lib/libc++.a" "$STAGE/lib/libc++.a"

[ -d "${REPO_ROOT}/slibc/include" ] || die "slibc/include is not a directory"
cp -a "${REPO_ROOT}/slibc/include/." "$STAGE/include/"

[ -d "$CXX_DIR/include/c++/v1" ] ||
    missing "$CXX_DIR/include/c++/v1" "scripts/make_slopos_cxx.sh '${RELEASE_DIR}'"
cp -a "$CXX_DIR/include/c++/v1/." "$STAGE/include/c++/v1/"

[ -d "$CXX_DIR/licenses" ] ||
    missing "$CXX_DIR/licenses" "scripts/make_slopos_cxx.sh '${RELEASE_DIR}'"
cp -a "$CXX_DIR/licenses" "$STAGE/licenses"

VENDOR_REL="$(. "$SCRIPT_DIR/lib/toolchain_pin.sh" && tp_vendor_rel "$REPO_ROOT")" ||
    die ".cargo/vendor.toml names no vendored-sources directory"

seed_source() {
    local base src
    base="$(git -C "$REPO_ROOT" rev-parse --verify HEAD 2>/dev/null)" ||
        die "the dev disk seeds src/slopos from git HEAD, and $REPO_ROOT is not a git checkout"
    git -C "$REPO_ROOT" diff --quiet HEAD -- Cargo.lock .cargo rust-toolchain.toml toolchain/PIN ||
        die "Cargo.lock, .cargo/ or the toolchain pin differ from HEAD; commit or stash them, since the vendored crates must be the ones src/slopos names"
    [ -z "$(git -C "$REPO_ROOT" status --porcelain --untracked-files=no)" ] ||
        echo "$SELF: the working tree has uncommitted changes; src/slopos is cut from HEAD ($base) without them" >&2
    "$SCRIPT_DIR/make_vendor.sh"
    src="$STAGE/src/slopos"
    mkdir -p "$src/.cargo" "$src/$(dirname "$VENDOR_REL")"
    git -C "$REPO_ROOT" archive --format=tar "$base" | tar -x -C "$src"
    cp -a "$REPO_ROOT/$VENDOR_REL" "$src/$VENDOR_REL"
    {
        git -C "$REPO_ROOT" show "$base:.cargo/config.toml"
        echo
        git -C "$REPO_ROOT" show "$base:.cargo/vendor.toml"
    } >"$src/.cargo/config.toml"
    echo "$base" >"$src/.slopos-base"
}
[ -f "$IMAGE_PATH" ] || seed_source

GIT_FIXTURE_REL="git/greeting.git"
stage_git_fixture() {
    local work="$BUILD_DIR/devdisk-git"
    command -v git >/dev/null 2>&1 || die "git is required to stage $GIT_FIXTURE_REL"
    rm -rf "$work"
    mkdir -p "$work/src" "$STAGE/$(dirname "$GIT_FIXTURE_REL")"
    printf '%s\n' '[package]' 'name = "greeting"' 'version = "0.1.0"' 'edition = "2021"' \
        >"$work/Cargo.toml"
    printf '%s\n' 'pub fn greeting() -> &'"'"'static str {' '    "fetched through libgit2"' '}' \
        >"$work/src/lib.rs"
    (
        cd "$work"
        export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
        export GIT_AUTHOR_NAME="The SlopOS Authors" GIT_AUTHOR_EMAIL="devdisk@slopos.invalid"
        export GIT_COMMITTER_NAME="$GIT_AUTHOR_NAME" GIT_COMMITTER_EMAIL="$GIT_AUTHOR_EMAIL"
        export GIT_AUTHOR_DATE="2026-01-01T00:00:00Z" GIT_COMMITTER_DATE="2026-01-01T00:00:00Z"
        git init -q -b main . &&
            git add Cargo.toml src/lib.rs &&
            git commit -q -m "greeting: the dev disk's git fixture" &&
            git clone -q --bare --no-hardlinks "$work" "$STAGE/$GIT_FIXTURE_REL"
    ) || die "could not make the git fixture in $STAGE/$GIT_FIXTURE_REL"
    rm -rf "$work"
}
[ -f "$IMAGE_PATH" ] || stage_git_fixture

REGISTRY_REL="registry"
REGISTRY_ORIGIN="https://127.0.0.1:4433"
stage_registry() {
    local work="$BUILD_DIR/devdisk-registry" reg="$STAGE/$REGISTRY_REL" www cksum cnf sec1
    command -v openssl >/dev/null 2>&1 || die "openssl is required to stage $REGISTRY_REL"
    www="$reg/www"
    rm -rf "$work"
    mkdir -p "$work/greeting-0.1.0/src" "$www/index/gr/ee" "$www/crates"
    printf '%s\n' '[package]' 'name = "greeting"' 'version = "0.1.0"' 'edition = "2021"' \
        >"$work/greeting-0.1.0/Cargo.toml"
    printf '%s\n' 'pub fn greeting() -> &'"'"'static str {' '    "fetched over https"' '}' \
        >"$work/greeting-0.1.0/src/lib.rs"
    tar --sort=name --mtime=@1767225600 --owner=0 --group=0 --numeric-owner \
        --mode=u=rwX,go=rX --format=ustar -C "$work" -cf - greeting-0.1.0 |
        gzip -9n >"$www/crates/greeting-0.1.0.crate" ||
        die "could not pack the registry's crate"
    cksum="$(sha256sum "$www/crates/greeting-0.1.0.crate" | cut -d' ' -f1)"
    printf '{"dl":"%s/crates/{crate}-{version}.crate"}\n' "$REGISTRY_ORIGIN" \
        >"$www/index/config.json"
    printf '{"name":"greeting","vers":"0.1.0","deps":[],"cksum":"%s","features":{},"yanked":false}\n' \
        "$cksum" >"$www/index/gr/ee/greeting"

    cnf="$work/openssl.cnf"
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
        openssl ecparam -name prime256v1 -genkey -noout -out "$work/root.key" &&
            openssl ecparam -name prime256v1 -genkey -noout -out "$work/server.key" &&
            openssl req -x509 -new -config "$cnf" -extensions root -key "$work/root.key" \
                -subj "/CN=SlopOS Dev Disk Test Root" -days 3650 -sha256 -out "$reg/ca.pem" &&
            openssl req -new -config "$cnf" -key "$work/server.key" -subj "/CN=127.0.0.1" \
                -out "$work/server.csr" &&
            openssl x509 -req -in "$work/server.csr" -CA "$reg/ca.pem" -CAkey "$work/root.key" \
                -set_serial 2 -days 3650 -sha256 -extfile "$cnf" -extensions leaf \
                -outform DER -out "$reg/server.der" &&
            openssl ec -in "$work/server.key" -no_public -outform DER -out "$work/server.sec1"
    } >/dev/null 2>&1 || die "openssl could not make the registry's certificates in $work"
    # RFC 5915's ECPrivateKey: SEQUENCE, version 1, then the scalar as a
    # 32-byte OCTET STRING at offset 7.
    sec1="$work/server.sec1"
    [ "$(od -An -tx1 -j2 -N5 "$sec1" | tr -d ' \n')" = "0201010420" ] ||
        die "$sec1 is not a SEC1 P-256 private key"
    head -c 39 "$sec1" | tail -c 32 >"$reg/server.key"
    rm -rf "$work"
}
[ -f "$IMAGE_PATH" ] || stage_registry

# Where the host keeps its owned sysroot, so the tree's scripts find the
# guest's toolchain where they find the host's.
TOOLCHAIN_REL="src/slopos/third_party/rust-slopos"
if [ -n "${TOOLCHAIN_STAGE:-}" ] && [ ! -f "$IMAGE_PATH" ]; then
    [ -d "$TOOLCHAIN_STAGE" ] ||
        die "TOOLCHAIN_STAGE='$TOOLCHAIN_STAGE' is not a directory"
    mkdir -p "$STAGE/$TOOLCHAIN_REL"
    cp -alf "$TOOLCHAIN_STAGE/." "$STAGE/$TOOLCHAIN_REL/" 2>/dev/null ||
        cp -af "$TOOLCHAIN_STAGE/." "$STAGE/$TOOLCHAIN_REL/"
fi

echo "$SELF: staged $(du -sh "$STAGE" | cut -f1) for $IMAGE_PATH"

FS_IMAGE_SIZE="$DEV_DISK_SIZE" \
FS_INODE_RATIO="$DEV_DISK_INODE_RATIO" \
FS_LABEL="$DEV_DISK_LABEL" \
VERITY=off \
PRESERVE_FS_IMAGE=1 \
FS_POPULATE_DIR="$STAGE" \
    "$SCRIPT_DIR/build_fs_image.sh" "$IMAGE_PATH" "$BUILD_DIR"

# The inventory is measured on the finished volume, never on the stage. A
# preserved image is not repopulated, and `build_fs_image.sh` refreshes
# `/lib/libc.so` on one anyway, so a marker written from the stage would
# state the size of a library the volume does not hold and fail the guest
# test for a discrepancy that is the marker's own.
image_size() {
    debugfs -R "stat /$1" "$IMAGE_PATH" 2>/dev/null |
        sed -n 's/^User:.*Size: \([0-9]\{1,\}\).*/\1/p' | head -n 1
}

image_holds_dir() {
    debugfs -R "stat /$1" "$IMAGE_PATH" 2>/dev/null | grep -q 'Type: directory'
}

# A preserved dev disk that predates a library is the one case this cannot
# describe: naming it is the whole point, and the fix is the developer's to
# take, as `build_fs_image.sh` does with every image it will not delete.
stale() {
    echo "" >&2
    echo "$SELF: $IMAGE_PATH does not carry $1" >&2
    echo "  It was preserved from an earlier build that staged something else," >&2
    echo "  so the marker cannot describe it." >&2
    echo "  Discard it: rm -f '$IMAGE_PATH' '$IMAGE_PATH.stamp'" >&2
    exit 1
}

# Subdirectories get a `dir` line rather than a recursive walk: `lib/rustlib`
# is thousands of files, and one `debugfs` call each would cost more than the
# image build. Symlinks are skipped, since the guest's stat follows them. A
# toolchain entry whose size differs from the one staged this run is a
# preserved volume carrying an older toolchain.
inventory() {
    local rel_dir="$1" local_dir="$2" check_size="$3" entry rel size
    [ -d "$local_dir" ] || return 0
    for entry in "$local_dir"/*; do
        [ -L "$entry" ] && continue
        rel="$rel_dir/$(basename "$entry")"
        if [ -d "$entry" ]; then
            image_holds_dir "$rel" || stale "$rel"
            printf 'dir %s\n' "$rel"
            continue
        fi
        [ -f "$entry" ] || continue
        size="$(image_size "$rel")"
        [ -n "$size" ] || stale "$rel"
        if [ "$check_size" -eq 1 ] && [ "$size" != "$(stat -c %s "$entry")" ]; then
            stale "the toolchain staged at $TOOLCHAIN_STAGE ($rel differs)"
        fi
        printf 'file %s %s\n' "$size" "$rel"
    done
}

MARKER_FILE="${BUILD_DIR}/devdisk-marker.txt"
{
    echo "$MARKER 1"
    echo "target ${USERLAND_TARGET}"
    for rel in include include/c++/v1 licenses; do
        image_holds_dir "$rel" || stale "$rel"
        printf 'dir %s\n' "$rel"
    done
    if image_holds_dir src/slopos; then
        echo "source src/slopos"
    else
        echo "$SELF: $IMAGE_PATH predates the seeded source tree; a new volume carries one" >&2
    fi
    if image_holds_dir "$GIT_FIXTURE_REL"; then
        echo "git $GIT_FIXTURE_REL"
    fi
    if image_holds_dir "$REGISTRY_REL"; then
        echo "registry $REGISTRY_REL"
    fi
    inventory lib "$STAGE/lib" 0
    if [ -n "${TOOLCHAIN_STAGE:-}" ]; then
        image_holds_dir "$TOOLCHAIN_REL" || stale "$TOOLCHAIN_REL"
        echo "toolchain $TOOLCHAIN_REL"
        inventory "$TOOLCHAIN_REL/bin" "$TOOLCHAIN_STAGE/bin" 1
        inventory "$TOOLCHAIN_REL/lib" "$TOOLCHAIN_STAGE/lib" 1
    fi
} >"$MARKER_FILE"

debugfs -w -R "rm /$MARKER" "$IMAGE_PATH" >/dev/null 2>&1 || true
debugfs -w -R "write $MARKER_FILE $MARKER" "$IMAGE_PATH" >/dev/null

echo "$SELF: $(grep -c '^file ' "$MARKER_FILE") files inventoried in /$MARKER"
