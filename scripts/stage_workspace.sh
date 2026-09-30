#!/usr/bin/env bash
set -euo pipefail

# Stage the guest's workspace: a clone of this checkout for a root to seed.
#
# Usage: stage_workspace.sh <dir> [--vendored]
#
# `<dir>/slopos` is `git clone --no-local` of HEAD's branch, so its history is
# what the branches and tags reach and nothing else. Its `origin` is the
# checkout `qemu_run.sh` serves the guest and its `host` remote, the push
# default, the repository it pushes into; it takes this checkout's user.name
# and user.email.
#
# `--vendored` adds the vendored crates, in the clone's ignored
# `third_party/vendor`; `<dir>/.cargo/config.toml`, which points cargo
# anywhere below `<dir>` at them with no registry and leaves the clone's own
# `.cargo/config.toml` as committed; and the llvm-project tarball the C++
# runtime is built from. A root the tests build on then reads no network.
# Without it the clone resolves its crates from crates.io and fetches the
# tarball, as this checkout does.

SELF="stage_workspace"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
USAGE="usage: stage_workspace.sh <dir> [--vendored]"

DIR="${1:?$USAGE}"
VENDORED=0
case "${2:-}" in
    "") ;;
    --vendored) VENDORED=1 ;;
    *) echo "$USAGE" >&2; exit 2 ;;
esac

die() {
    echo "$SELF: $*" >&2
    exit 1
}

# `qemu_run.sh`'s git peer: this checkout on the default port, the push
# repository on 9419.
GIT_PEER="git://10.0.2.4"

command -v git >/dev/null 2>&1 || die "git is required to stage the workspace"
git -C "$REPO_ROOT" rev-parse --verify -q HEAD >/dev/null ||
    die "the workspace clones this checkout, which has no HEAD"
[ -z "$(git -C "$REPO_ROOT" status --porcelain --untracked-files=no)" ] ||
    echo "$SELF: the working tree has uncommitted changes; the workspace is cloned at HEAD without them" >&2

rm -rf "$DIR"
mkdir -p "$DIR"
src="$DIR/slopos"
git clone -q --no-local "$REPO_ROOT" "$src" || die "could not clone $REPO_ROOT into $src"
git -C "$src" remote set-url origin "$GIT_PEER/slopos"
git -C "$src" remote add host "$GIT_PEER:9419/slopos"
git -C "$src" config remote.pushDefault host
git -C "$src" config push.default current
for key in user.name user.email; do
    value="$(git -C "$REPO_ROOT" config "$key" || true)"
    [ -z "$value" ] || git -C "$src" config "$key" "$value"
done

[ "$VENDORED" -eq 1 ] || exit 0
. "$SCRIPT_DIR/lib/toolchain_pin.sh"
git -C "$REPO_ROOT" diff --quiet HEAD -- Cargo.lock .cargo rust-toolchain.toml toolchain/PIN ||
    die "Cargo.lock, .cargo/ or the toolchain pin differ from HEAD; commit or stash them, since the vendored crates must be the ones the clone names"
VENDOR_REL="$(tp_vendor_rel "$REPO_ROOT")" || die ".cargo/vendor.toml names no vendored-sources directory"
"$SCRIPT_DIR/make_vendor.sh"
mkdir -p "$src/$(dirname "$VENDOR_REL")" "$DIR/.cargo"
cp -a "$REPO_ROOT/$VENDOR_REL" "$src/$VENDOR_REL"
config="$DIR/.cargo/config.toml"
# Relative to `<dir>`, the directory holding this config's `.cargo`.
sed -e '/^#/d' -e "s|^directory = \"$VENDOR_REL\"\$|directory = \"slopos/$VENDOR_REL\"|" \
    "$REPO_ROOT/.cargo/vendor.toml" >"$config"
grep -qxF "directory = \"slopos/$VENDOR_REL\"" "$config" || die "could not point $config at slopos/$VENDOR_REL"

tarball="$("$SCRIPT_DIR/make_slopos_cxx.sh" --fetch-source)" ||
    die "could not provide the llvm-project tarball toolchain/cxx/PIN names"
mkdir -p "$src/third_party"
cp "$tarball" "$src/third_party/"
