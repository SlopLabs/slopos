#!/usr/bin/env bash
set -euo pipefail

# Build an ext2 filesystem image populated with userland binaries.
#
# Usage: build_fs_image.sh <image_path> <build_dir> [bin1] [bin2] ...
#
# Each binary is placed in /bin/<name> except 'init' which goes to /sbin/init.
# A run with no binaries builds an empty volume, which is what a scratch or
# capacity image wants.
#
# Environment:
#   FS_IMAGE_SIZE - image size (default: 32M)
#   FS_INODE_RATIO - bytes of volume per inode, passed to mkfs.ext2 -i. Unset
#                   uses the host mke2fs default (16384), i.e. ~65k inodes per
#                   GiB — enough for a checked-out tree plus a toolchain
#                   sysroot on a multi-GB volume.
#   FS_JOURNAL_SIZE - size of the metadata log at /.journal; default is 1/64 of
#                   the image, floored at 4M and capped at 64M. `0` builds no
#                   log, which makes the kernel fall back to undo-scoped
#                   operations and refuse an unclean mount.
#   FS_POPULATE_DIR - a host directory whose tree becomes the volume's root, via
#                   `mkfs.ext2 -d`. Only honoured on a fresh mkfs: a preserved
#                   image holds whatever the guest wrote and is never
#                   repopulated. The directories and files this script installs
#                   afterwards are created over that tree.
#   FS_LABEL      - ext2 volume label (`mke2fs -L`); a preserved image is
#                   relabelled with `tune2fs -L`. `mount=LABEL=<label>:<path>`
#                   on the kernel cmdline and `mount(2)` find a volume by it.
#   VERITY        - `on` (default) appends a v1 integrity trailer, which makes
#                   the kernel mount the image read-only; `rw` appends a v2
#                   trailer, which leaves the image writable (a write
#                   un-attests the blocks it touches); `off` leaves the image
#                   unverified, for images the test suite writes to.
#   PRESERVE_FS_IMAGE - `1` refreshes the binaries of an existing writable
#                   image in place instead of running mkfs, so a developer
#                   iterating on the kernel keeps whatever the guest wrote.
#                   Ignored when no image exists. When an image *does* exist
#                   and cannot be kept — damaged, left dirty by a killed boot,
#                   or carrying a write-protecting v1 trailer — this script
#                   REFUSES and names the fix rather than deleting it.
#                   FS_IMAGE_SIZE is a minimum: a larger one grows the image
#                   in place and a smaller one leaves it as it is. A v2
#                   trailer is recomputed at the end of the run, so it does
#                   not block a refresh.
#   FS_HOST_TREES - `<host dir>:<guest dir>` pairs, space-separated: trees the
#                   host owns, installed whenever the host directory exists
#                   and replaced when it changes (scripts/fs_tree.py). What
#                   each installed is recorded beside the image, in
#                   `<image>.host/trees/`, so a replacement removes exactly
#                   that.
#                   One that would overwrite what the guest put there is left
#                   as it was, with a warning, and retried on the next build.
#   FS_SEED_TREES - `<host dir>:<guest dir>` pairs: trees the guest owns once
#                   they are there, copied only onto an image without the
#                   guest directory.
#   FS_FREE_FLOOR - free space the image keeps (default 0); an image with less
#                   is grown to it on every build.

IMAGE_PATH="${1:?Usage: build_fs_image.sh <image_path> <build_dir> <bin1> [bin2] ...}"
BUILD_DIR="${2:?Usage: build_fs_image.sh <image_path> <build_dir> <bin1> [bin2] ...}"
shift 2
BINS=("$@")

FS_IMAGE_SIZE="${FS_IMAGE_SIZE:-32M}"
# Derived from the volume, not frozen: a log is sized by how much metadata one
# writeback window may hold, which scales with the filesystem it protects. 1/64
# of the image, floored at the 4M a 32M appliance root used and capped at the
# 64M past which the kernel's per-slot arrays stop paying for themselves.
default_journal_size() {
    local image_bytes want
    image_bytes="$(numfmt --from=iec "$FS_IMAGE_SIZE")"
    want=$(( image_bytes / 64 ))
    [ "$want" -ge $(( 4 * 1024 * 1024 )) ] || want=$(( 4 * 1024 * 1024 ))
    [ "$want" -le $(( 64 * 1024 * 1024 )) ] || want=$(( 64 * 1024 * 1024 ))
    echo "$(( want / 1024 / 1024 ))M"
}
FS_JOURNAL_SIZE="${FS_JOURNAL_SIZE:-$(default_journal_size)}"
VERITY="${VERITY:-on}"
case "$VERITY" in
    on|off|rw) ;;
    *) echo "build_fs_image: VERITY must be 'on', 'off' or 'rw', got '$VERITY'" >&2; exit 2 ;;
esac
FS_LABEL="${FS_LABEL:-}"
FS_HOST_TREES="${FS_HOST_TREES:-}"
FS_SEED_TREES="${FS_SEED_TREES:-}"
FS_FREE_FLOOR="${FS_FREE_FLOOR:-0}"

# Created on every root rather than left to the first writer: the ext2 root
# does not auto-create parents the way ramfs does, and both roots must agree
# about whether a path is writable. `/media` is where a boot's `mount=` puts a
# volume. Mirrors gen_initramfs.py's EMPTY_DIRS.
ROOT_DIRS=(/etc /var /home /media)

# Mirrors gen_initramfs.py's SLIBC_LICENSES.
SLIBC_LICENSES=(LICENSE-MIT LICENSE-APACHE NOTICE)

# macOS: extend PATH to find e2fsprogs tools installed via Homebrew
if [ "$(uname -s)" = "Darwin" ]; then
    BREW_PREFIX="$(brew --prefix 2>/dev/null || echo /opt/homebrew)"
    export PATH="${BREW_PREFIX}/opt/e2fsprogs/sbin:${BREW_PREFIX}/opt/e2fsprogs/bin:${PATH}"
fi

if ! command -v mkfs.ext2 >/dev/null 2>&1; then
    echo "mkfs.ext2 is required to create $IMAGE_PATH" >&2
    exit 1
fi

if ! command -v debugfs >/dev/null 2>&1; then
    echo "debugfs is required to populate $IMAGE_PATH" >&2
    exit 1
fi

# debugfs prints its version banner on stderr on every run; drop only that line.
debugfs() {
    { command debugfs "$@" 2>&1 1>&3 3>&- | sed '/^debugfs [0-9][0-9.]* (/d' >&2; } 3>&1
}

IMAGE_DIR="$(dirname "$IMAGE_PATH")"
mkdir -p "$IMAGE_DIR"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
FS_TREE=(python3 "$SCRIPT_DIR/fs_tree.py")
# What the host keeps about an image, where its guest cannot write: the
# manifests of the trees it installed, and what its last seal attested.
HOST_STATE="${IMAGE_PATH}.host"
TREE_MANIFESTS="$HOST_STATE/trees"
VERITY_RECORD="$HOST_STATE/verity"

PRESERVE_FS_IMAGE="${PRESERVE_FS_IMAGE:-0}"
# Set when this run rewrote binaries into an image it kept, which obliges it to
# invalidate a log whose records describe the old ones.
REFRESHED_BINARIES=0
STAMP_PATH="${IMAGE_PATH}.stamp"
# The blocks a preserved v2 image's guest owns, measured before this build
# writes anything, so its seal attests none of them.
TAINT_PATH=""

# What the image's content is a function of: an equal stamp means a preserved
# image already carries these binaries and assets, so it needs no work.
build_stamp() {
    echo "size=$FS_IMAGE_SIZE verity=$VERITY journal=$FS_JOURNAL_SIZE links=${COREUTILS_LINKS:-}"
    echo "label=$FS_LABEL dirs=${ROOT_DIRS[*]} floor=$FS_FREE_FLOOR"
    local spec
    for spec in $FS_HOST_TREES; do
        printf 'tree %s ' "${spec#*:}"
        [ -d "${spec%%:*}" ] && "${FS_TREE[@]}" identity "${spec%%:*}" || echo absent
    done
    for bin in "${BINS[@]}"; do
        printf '%s ' "$bin"
        sha256sum "${BUILD_DIR}/${bin}.elf" 2>/dev/null | cut -d' ' -f1 || echo missing
    done
    # The shared objects are not in BINS and are named without the .elf
    # suffix, so a preserved image would otherwise keep last build's
    # interpreter while every static binary refreshed.
    for so in libc.so ${EXTRA_SHARED_OBJECTS:-}; do
        printf '%s ' "$so"
        sha256sum "${BUILD_DIR}/${so}" 2>/dev/null | cut -d' ' -f1 || echo missing
    done
    for asset in "${REPO_ROOT}/assets/fonts"/* "${REPO_ROOT}/assets/keymaps"/* \
                 "${REPO_ROOT}/assets/certs"/* "${REPO_ROOT}/assets/logo.png"; do
        [ -f "$asset" ] || continue
        sha256sum "$asset" | cut -d' ' -f1
    done
    for text in "${SLIBC_LICENSES[@]}"; do
        sha256sum "${REPO_ROOT}/slibc/$text" 2>/dev/null | cut -d' ' -f1 || echo missing
    done
}

# Read from the file rather than from $VERITY: the caller's intent for *this*
# build says nothing about what is already on disk.
# A trailer lies past the filesystem; the magic inside it is the guest's data.
image_carries_verity_trailer() {
    [ -s "$1" ] || return 1
    local magic
    [ "$(stat -c %s "$1")" -ge "$(( $(fs_extent_bytes "$1") + 32 ))" ] || return 1
    magic=$(tail -c 32 "$1" | head -c 4 | od -An -tx1 | tr -d ' \n')
    [ "$magic" = "54525653" ]
}

verity_trailer_version() {
    tail -c 32 "$1" | od -An -tu4 -j4 -N4 | tr -d ' \n'
}

# How big the *filesystem* is, which is what a size check asks: the file is
# larger by whatever trailer is appended, and the trailer starts where the
# filesystem ends.
fs_extent_bytes() {
    fs_blocks_bytes "$1" "Block count"
}

fs_free_bytes() {
    fs_blocks_bytes "$1" "Free blocks"
}

fs_blocks_bytes() {
    local hdr count bs
    hdr=$(dumpe2fs -h "$1" 2>/dev/null)
    count=$(echo "$hdr" | sed -n "s/^$2: *\([0-9]*\)/\1/p")
    bs=$(echo "$hdr" | sed -n 's/^Block size: *\([0-9]*\)/\1/p')
    echo $(( ${count:-0} * ${bs:-0} ))
}

# A preserved image is the developer's machine: nothing here deletes one, and
# every refusal names the command that would.
refuse() {
    echo "" >&2
    echo "preserve: $1" >&2
    echo "  $IMAGE_PATH holds whatever the guest wrote, so this build stops here." >&2
    echo "  Fix it:     $2" >&2
    echo "  Discard it: rm -f '$IMAGE_PATH' '$STAMP_PATH'" >&2
    exit 1
}

# Grow in place rather than refuse: raising FS_IMAGE_SIZE on a machine you are
# living in must not be a reason to throw it away.
grow_image() {
    local have="$1" want="$2" trailer="" now
    command -v resize2fs >/dev/null 2>&1 ||
        refuse "resize2fs is not installed, so the image cannot be grown to ${want}B" \
               "install e2fsprogs"
    # `resize2fs` refuses a filesystem whose last check predates its last
    # write, and this kernel never stamps `s_lastcheck` because it runs no
    # fsck. The image was proved sound and clean a moment ago; this pass is the
    # formality e2fsprogs insists on performing itself.
    e2fsck -fy "$IMAGE_PATH" >/dev/null 2>&1 ||
        refuse "e2fsck could not ready the image for a resize" "e2fsck -fy '$IMAGE_PATH'"
    # Kept aside until the resize lands, so a failure puts the image back
    # exactly as it was. The trailer itself is rebuilt at the end of this run.
    if image_carries_verity_trailer "$IMAGE_PATH"; then
        trailer="$(mktemp "${IMAGE_DIR}/trailer.XXXXXX")"
        tail -c "+$((have + 1))" "$IMAGE_PATH" > "$trailer"
    fi
    truncate -s "$have" "$IMAGE_PATH"
    truncate -s "$want" "$IMAGE_PATH"
    if ! resize2fs "$IMAGE_PATH" >/dev/null 2>&1; then
        truncate -s "$have" "$IMAGE_PATH"
        [ -z "$trailer" ] || cat "$trailer" >> "$IMAGE_PATH"
        rm -f "$trailer"
        refuse "resize2fs could not grow the image to ${want}B (it is unchanged)" \
               "e2fsck -fy '$IMAGE_PATH'"
    fi
    now="$(fs_extent_bytes "$IMAGE_PATH")"
    if [ "$now" -le "$have" ]; then
        # resize2fs drops a last group too small to hold its own metadata, and
        # a grow that adds nothing must not cost the image its trailer.
        truncate -s "$have" "$IMAGE_PATH"
        [ -z "$trailer" ] || cat "$trailer" >> "$IMAGE_PATH"
        rm -f "$trailer"
        return 1
    fi
    rm -f "$trailer"
    echo "grew $IMAGE_PATH from ${have}B to ${now}B, keeping its contents"
}

# Grown before a log or a tree lands, never discovered full halfway through.
# Again until it fits: every group a grow adds spends some of itself on inodes.
ensure_room() {
    local need="$1" free have step=$((256 * 1024 * 1024))
    while free="$(fs_free_bytes "$IMAGE_PATH")" && [ "$free" -lt "$need" ]; do
        have="$(fs_extent_bytes "$IMAGE_PATH")"
        grow_image "$have" "$(( (have + need - free + step - 1) / step * step ))" ||
            refuse "resize2fs added nothing to an image ${free}B free, short of ${need}B" \
                   "e2fsck -fy '$IMAGE_PATH'"
    done
}

tree_bytes() {
    echo $(( $(du -s --block-size=4096 "$1" | cut -f1) * 4096 ))
}

# Held to the same oracle CI holds a boot's output to: sound *and* clean.
# `e2fsck -fn` alone exits 0 on a dirty superblock, which is what a boot killed
# mid-write leaves. Clean also means the log is empty, which is what makes
# install_journal's invalidation below safe.
preserve_or_refuse() {
    local want have
    want=$(numfmt --from=iec "$FS_IMAGE_SIZE")
    have=$(fs_extent_bytes "$IMAGE_PATH")
    if [ "$have" = "0" ]; then
        refuse "there is no ext2 superblock in the image" "e2fsck -fy '$IMAGE_PATH'"
    fi
    # A v1 trailer's hashes cover the bytes a refresh would rewrite; a v2
    # trailer is recomputed at the end of this run.
    if image_carries_verity_trailer "$IMAGE_PATH" &&
       { [ "$VERITY" != "rw" ] || [ "$(verity_trailer_version "$IMAGE_PATH")" != "2" ]; }; then
        refuse "the image carries a write-protecting v1 trailer, whose hashes cover the bytes a refresh rewrites" \
               "build this image with VERITY=rw"
    fi
    "${SCRIPT_DIR}/check_fs_image.sh" "$IMAGE_PATH" ||
        refuse "the image is damaged, or a boot left it dirty (see above)" \
               "e2fsck -fy '$IMAGE_PATH'"
    if [ "$VERITY" = "rw" ]; then
        TAINT_PATH="$(mktemp "${IMAGE_DIR}/taint.XXXXXX")"
        trap 'rm -f "$TAINT_PATH"' EXIT
        python3 "${SCRIPT_DIR}/gen_verity.py" --taint-out "$TAINT_PATH" --record "$VERITY_RECORD" "$IMAGE_PATH" ||
            refuse "gen_verity.py could not measure what the guest wrote" "e2fsck -fy '$IMAGE_PATH'"
    fi
    if [ "$want" -gt "$have" ]; then
        grow_image "$have" "$want" || true
    fi
}

# `write` refuses an existing name, so a refresh unlinks first. debugfs writes
# the raw structures, so EXT2_IMMUTABLE_FL does not stop it.
install_binary() {
    local src="$1" dst="$2"
    debugfs -w -R "rm $dst" "$IMAGE_PATH" >/dev/null 2>&1 || true
    debugfs -w -R "write $src $dst" "$IMAGE_PATH" >/dev/null
    debugfs -w -R "set_inode_field $dst mode 0100755" "$IMAGE_PATH" >/dev/null
    # EXT2_IMMUTABLE_FL: the on-disk carrier of the VFS seal. Program-identity
    # privilege is keyed on a binary's path, so a shipped binary that is not
    # sealed is one any task holding a write descriptor can replace and then
    # spawn into the grant. `lsattr` shows this as `i`.
    debugfs -w -R "set_inode_field $dst flags 0x10" "$IMAGE_PATH" >/dev/null
}

install_file() {
    local src="$1" dst="$2"
    debugfs -w -R "rm $dst" "$IMAGE_PATH" >/dev/null 2>&1 || true
    debugfs -w -R "write $src $dst" "$IMAGE_PATH" >/dev/null
}

# Asks first: `debugfs mkdir` on a name that exists allocates the inode, fails
# at the link, and leaves the leak `e2fsck` reports as an unconnected inode.
mkdir_p() {
    if debugfs -R "stat $1" "$IMAGE_PATH" 2>/dev/null | grep -q '^Inode:'; then
        return 0
    fi
    debugfs -w -R "mkdir $1" "$IMAGE_PATH" >/dev/null 2>&1 || true
}

tree_manifest() {
    printf '%s' "${1#/}" | tr '/' '_'
}

tree_pending() {
    local host="$1" guest="$2" want
    [ -d "$host" ] || return 1
    want="$("${FS_TREE[@]}" identity "$host")" || exit 1
    [ "$("${FS_TREE[@]}" installed "$TREE_MANIFESTS" "$(tree_manifest "$guest")")" != "$want" ]
}

seed_pending() {
    [ -d "$1" ] && ! "${FS_TREE[@]}" exists "$IMAGE_PATH" "$2"
}

# What a matching stamp cannot vouch for: a tree an earlier build left
# uninstalled, a seed the guest removed, and room the guest has used.
image_owes_work() {
    local spec
    for spec in $FS_HOST_TREES; do
        tree_pending "${spec%%:*}" "${spec#*:}" && return 0
    done
    for spec in $FS_SEED_TREES; do
        seed_pending "${spec%%:*}" "${spec#*:}" && return 0
    done
    [ "$(fs_free_bytes "$IMAGE_PATH")" -lt "$(numfmt --from=iec "$FS_FREE_FLOOR")" ]
}

if [ "$PRESERVE_FS_IMAGE" = "1" ] && [ -f "$IMAGE_PATH" ]; then
    preserve_or_refuse
    if [ -f "$STAMP_PATH" ] && [ "$(cat "$STAMP_PATH")" = "$(build_stamp)" ] && ! image_owes_work; then
        rm -f "$TAINT_PATH"
        echo "preserve: $IMAGE_PATH is current — leaving it and its contents alone"
        exit 0
    fi
    echo "preserve: refreshing binaries in $IMAGE_PATH, keeping everything else"
    # A refresh that dies midway must not leave the old stamp, or the next run
    # reports an image missing a binary as current.
    rm -f "$STAMP_PATH"
    REFRESHED_BINARIES=1
    if [ -n "$FS_LABEL" ]; then
        tune2fs -L "$FS_LABEL" "$IMAGE_PATH" >/dev/null
    fi
else
    echo "Rebuilding ext2 image at $IMAGE_PATH ($FS_IMAGE_SIZE)"
    rm -rf "$IMAGE_PATH" "$STAMP_PATH" "$HOST_STATE"
    truncate -s "$FS_IMAGE_SIZE" "$IMAGE_PATH"
    MKFS_ARGS=(-F -b 4096)
    [ -z "$FS_LABEL" ] || MKFS_ARGS+=(-L "$FS_LABEL")
    [ -z "${FS_INODE_RATIO:-}" ] || MKFS_ARGS+=(-i "$FS_INODE_RATIO")
    if [ -n "${FS_POPULATE_DIR:-}" ]; then
        if [ ! -d "$FS_POPULATE_DIR" ]; then
            echo "build_fs_image: FS_POPULATE_DIR='$FS_POPULATE_DIR' is not a directory" >&2
            exit 2
        fi
        echo "Populating the root from $FS_POPULATE_DIR ($(du -sh "$FS_POPULATE_DIR" | cut -f1))"
        MKFS_ARGS+=(-d "$FS_POPULATE_DIR")
    fi
    mkfs.ext2 "${MKFS_ARGS[@]}" "$IMAGE_PATH" >/dev/null
fi

mkdir_p /bin
mkdir_p /sbin
for dir in "${ROOT_DIRS[@]}"; do
    mkdir_p "$dir"
done

# The metadata log (fs/src/ext2/journal.rs). A plain preallocated file, so
# `e2fsck` sees a file and the format carries no feature bit; the seal is what
# refuses every write, rename and unlink of it from userland.
#
# Filled with non-zero bytes: `debugfs write` leaves a hole where its source
# block is all zeros, and the kernel refuses a sparse log because writing into
# a hole would have to allocate mid-commit.
journal_blocks() {
    debugfs -R "stat /.journal" "$IMAGE_PATH" 2>/dev/null |
        sed -n 's/.*Blockcount: \([0-9]*\).*/\1/p'
}

journal_bytes() {
    debugfs -R "stat /.journal" "$IMAGE_PATH" 2>/dev/null |
        sed -n 's/.* Size: \([0-9]*\)$/\1/p' | head -n1
}

# Byte offset of the log's own superblock, i.e. of its first block.
journal_first_byte() {
    local first bs
    first=$(debugfs -R "blocks /.journal" "$IMAGE_PATH" 2>/dev/null | awk '{print $1}')
    bs=$(dumpe2fs -h "$IMAGE_PATH" 2>/dev/null | sed -n 's/^Block size:  *\([0-9]*\)/\1/p')
    echo $((first * bs))
}

install_journal() {
    [ "$FS_JOURNAL_SIZE" != "0" ] || return 0
    local have bytes
    have="$(journal_blocks)"
    bytes="$(numfmt --from=iec "$FS_JOURNAL_SIZE")"
    if [ -n "$have" ] && [ "$have" != "0" ] && [ "$(journal_bytes)" = "$bytes" ]; then
        # Its superblock is zeroed after a binary refresh, because debugfs
        # rewrote inodes the log knows nothing about and replaying stale
        # copies of them would lose the fresh ones; and a log of another size
        # is rebuilt. Both are safe only because a preserved image is clean,
        # and a clean image's log is empty.
        if [ "$REFRESHED_BINARIES" = "1" ]; then
            dd if=/dev/zero of="$IMAGE_PATH" bs=1 count=4 conv=notrunc status=none \
                seek="$(journal_first_byte)" 2>/dev/null || true
            echo "journal: invalidated /.journal — its records predate this refresh"
        else
            echo "journal: /.journal already present — leaving it alone"
        fi
        return 0
    fi


    if [ -n "$have" ]; then
        debugfs -w -R "rm /.journal" "$IMAGE_PATH" >/dev/null 2>&1 || true
    fi
    local filled
    # Under the build directory, not $TMPDIR: debugfs word-splits the request
    # string, so the path must be one this repo controls.
    filled="$(mktemp "${IMAGE_DIR}/journal.XXXXXX")"
    trap 'rm -f "$filled"' RETURN
    # Read the length rather than piping through a filter: a pipeline whose
    # head exits early takes SIGPIPE under `set -o pipefail`.
    head -c "$bytes" /dev/urandom > "$filled"
    if ! debugfs -w -R "write $filled /.journal" "$IMAGE_PATH" >/dev/null 2>&1; then
        echo "journal: no room for a ${FS_JOURNAL_SIZE} log in $IMAGE_PATH" >&2
        exit 1
    fi
    debugfs -w -R "set_inode_field /.journal mode 0100600" "$IMAGE_PATH" >/dev/null
    debugfs -w -R "set_inode_field /.journal flags 0x10" "$IMAGE_PATH" >/dev/null
    have="$(journal_blocks)"
    if [ -z "$have" ] || [ "$have" = "0" ]; then
        echo "journal: /.journal came out sparse — the kernel would refuse it" >&2
        exit 1
    fi
    echo "journal: installed /.journal ($FS_JOURNAL_SIZE, $have sectors)"
}
ensure_room "$(numfmt --from=iec "$FS_FREE_FLOOR")"
install_journal

for bin in "${BINS[@]}"; do
    src="${BUILD_DIR}/${bin}.elf"
    if [ ! -f "$src" ]; then
        echo "Missing userland binary: $src" >&2
        exit 1
    fi

    dst="/bin/${bin}"
    if [ "$bin" = "init" ]; then
        dst="/sbin/init"
    fi

    install_binary "$src" "$dst"
done

# A symlink, so the exec grant keyed on `/bin/shell` follows `/bin/sh`.
for bin in "${BINS[@]}"; do
    if [ "$bin" = "shell" ]; then
        debugfs -w -R "rm /bin/sh" "$IMAGE_PATH" >/dev/null 2>&1 || true
        debugfs -w -R "symlink /bin/sh shell" "$IMAGE_PATH" >/dev/null
        echo "Installed /bin/sh -> shell"
    fi
done

# The multicall binary's names. A symlink, not a copy: fifty-odd copies of std
# would be ~8 MiB of a 32 MiB root, and `argv[0]` selects the tool anyway.
# `debugfs symlink` writes a fast symlink, so a name costs an inode and no block.
if [ -n "${COREUTILS_LINKS:-}" ]; then
    if [ ! -f "${BUILD_DIR}/coreutils.elf" ]; then
        echo "COREUTILS_LINKS is set but ${BUILD_DIR}/coreutils.elf is missing" >&2
        exit 1
    fi
    # Word splitting is wanted here; pathname expansion is not, and a name
    # holding `*` would otherwise glob against the build directory.
    set -f
    for tool in $COREUTILS_LINKS; do
        # This runs after the binaries are installed, so a name in both lists
        # would replace a program -- and inherit its grant.
        for bin in "${BINS[@]}"; do
            if [ "$tool" = "$bin" ]; then
                echo "COREUTILS_LINKS name '$tool' collides with an installed binary" >&2
                exit 1
            fi
        done
        debugfs -w -R "rm /bin/${tool}" "$IMAGE_PATH" >/dev/null 2>&1 || true
        debugfs -w -R "symlink /bin/${tool} coreutils" "$IMAGE_PATH" >/dev/null
    done
    set +f
    echo "Installed $(set -f; set -- $COREUTILS_LINKS; echo $#) utility names in /bin -> coreutils"
fi

# /lib: the shared C library, which is also the program interpreter every
# dynamically linked binary names in its PT_INTERP. Sealed and in a sealed
# directory for the same reason /bin is: the interpreter runs before the
# program does, so replacing it is replacing every dynamic program at once.
mkdir_p /lib
if [ -f "${BUILD_DIR}/libc.so" ]; then
    install_binary "${BUILD_DIR}/libc.so" /lib/libc.so
    debugfs -w -R "rm /lib/ld-slopos.so.1" "$IMAGE_PATH" >/dev/null 2>&1 || true
    debugfs -w -R "symlink /lib/ld-slopos.so.1 libc.so" "$IMAGE_PATH" >/dev/null
    echo "Installed /lib/libc.so and /lib/ld-slopos.so.1"
fi
# Only the -tests recipes set this. Installing by file presence would put a
# dlopen fixture into the shipped, attested root out of a stale builddir.
for so in ${EXTRA_SHARED_OBJECTS:-}; do
    install_binary "${BUILD_DIR}/${so}" "/lib/${so}"
done

# The directories too, on a root userland can write: a sealed binary cannot be
# overwritten, but until now its *directory* could be renamed aside and a
# fresh /bin/halt planted under the path the grant is keyed on. debugfs is not
# subject to the flag, so a preserved image still refreshes in place.
seal_dir() {
    debugfs -w -R "set_inode_field $1 flags 0x10" "$IMAGE_PATH" >/dev/null
}
seal_dir /bin
seal_dir /sbin
seal_dir /lib

# Install font files into /usr/share/fonts/ if assets/fonts/ exists
FONTS_DIR="${REPO_ROOT}/assets/fonts"

mkdir_p /usr
mkdir_p /usr/share

if [ -f "${BUILD_DIR}/libc.so" ]; then
    mkdir_p /usr/share/licenses
    mkdir_p /usr/share/licenses/slibc
    for text in "${SLIBC_LICENSES[@]}"; do
        [ -f "${REPO_ROOT}/slibc/$text" ] || { echo "build_fs_image: slibc/$text is missing" >&2; exit 1; }
        install_file "${REPO_ROOT}/slibc/$text" "/usr/share/licenses/slibc/$text"
        echo "Installed license: /usr/share/licenses/slibc/$text"
    done
fi

# The C++ runtime's license texts, beside the library they cover, for the same
# reason the fonts below carry theirs. Only the -tests recipes stage them,
# because only the tests image carries `libc++.so`.
CXX_LICENSES="${BUILD_DIR}/libc++-licenses"
if [ -d "$CXX_LICENSES" ]; then
    mkdir_p /usr/share/licenses
    mkdir_p /usr/share/licenses/libc++
    for text in "$CXX_LICENSES"/*; do
        [ -f "$text" ] || continue
        fname="$(basename "$text")"
        install_file "$text" "/usr/share/licenses/libc++/$fname"
        echo "Installed license: /usr/share/licenses/libc++/$fname"
    done
fi

if [ -d "$FONTS_DIR" ]; then
    mkdir_p /usr/share/fonts

    # The OFL license texts ship beside the fonts they cover: the license
    # requires each copy of the font to carry its notice.
    for font in "$FONTS_DIR"/*.ttf "$FONTS_DIR"/*-OFL.txt; do
        [ -f "$font" ] || continue
        fname="$(basename "$font")"
        install_file "$font" "/usr/share/fonts/$fname"
        echo "Installed font asset: /usr/share/fonts/$fname"
    done
fi

mkdir_p /usr/share/slopos
mkdir_p /usr/share/slopos/doc
mkdir_p /usr/share/slopos/wallpapers

# Documentation the shipped programs open from their own Help menus.
DOCS_DIR="${REPO_ROOT}/assets/docs"
if [ -d "$DOCS_DIR" ]; then
    for doc in "$DOCS_DIR"/*.md; do
        [ -f "$doc" ] || continue
        install_file "$doc" "/usr/share/slopos/doc/$(basename "$doc")"
        echo "Installed doc: /usr/share/slopos/doc/$(basename "$doc")"
    done
fi

if [ -f "${REPO_ROOT}/assets/logo.png" ]; then
    install_file "${REPO_ROOT}/assets/logo.png" /usr/share/slopos/wallpapers/default.png
    echo "Installed wallpaper: /usr/share/slopos/wallpapers/default.png"
fi

CERTS_DIR="${REPO_ROOT}/assets/certs"
if [ -f "$CERTS_DIR/ca-certificates.crt" ]; then
    mkdir_p /etc/ssl
    mkdir_p /etc/ssl/certs
    install_file "$CERTS_DIR/ca-certificates.crt" /etc/ssl/certs/ca-certificates.crt
    # OpenSSL's default CA file.
    debugfs -w -R "rm /etc/ssl/cert.pem" "$IMAGE_PATH" >/dev/null 2>&1 || true
    debugfs -w -R "symlink /etc/ssl/cert.pem certs/ca-certificates.crt" "$IMAGE_PATH" >/dev/null
    mkdir_p /usr/share/licenses
    mkdir_p /usr/share/licenses/ca-certificates
    install_file "$CERTS_DIR/MPL-2.0.txt" /usr/share/licenses/ca-certificates/MPL-2.0.txt
    echo "Installed CA bundle: /etc/ssl/certs/ca-certificates.crt"
fi

# Install keyboard layout files into /usr/share/keymaps/
KEYMAPS_DIR="${REPO_ROOT}/assets/keymaps"
if [ -d "$KEYMAPS_DIR" ]; then
    mkdir_p /usr/share/keymaps
    for layout in "$KEYMAPS_DIR"/*.layout; do
        [ -e "$layout" ] || continue
        lname=$(basename "$layout")
        install_file "$layout" "/usr/share/keymaps/$lname"
        echo "Installed keymap: /usr/share/keymaps/$lname"
    done
fi

install_trees() {
    local spec host guest pending=0 hosts=() seeds=() rc
    for spec in $FS_HOST_TREES; do
        tree_pending "${spec%%:*}" "${spec#*:}" || continue
        pending=$(( pending + $(tree_bytes "${spec%%:*}") ))
        hosts+=("$spec")
    done
    for spec in $FS_SEED_TREES; do
        seed_pending "${spec%%:*}" "${spec#*:}" || continue
        pending=$(( pending + $(tree_bytes "${spec%%:*}") ))
        seeds+=("$spec")
    done
    [ "${#hosts[@]}" -gt 0 ] || [ "${#seeds[@]}" -gt 0 ] || return 0
    ensure_room $(( pending + $(numfmt --from=iec "$FS_FREE_FLOOR") ))
    for spec in "${hosts[@]}"; do
        host="${spec%%:*}"
        guest="${spec#*:}"
        rc=0
        "${FS_TREE[@]}" install "$IMAGE_PATH" "$host" "$guest" \
            --manifests "$TREE_MANIFESTS" --name "$(tree_manifest "$guest")" || rc=$?
        case "$rc" in
            0) echo "Installed $host at $guest" ;;
            3) echo "build_fs_image: left $guest as it was; the next build tries again" >&2 ;;
            *) exit 1 ;;
        esac
    done
    for spec in "${seeds[@]}"; do
        rc=0
        "${FS_TREE[@]}" install "$IMAGE_PATH" "${spec%%:*}" "${spec#*:}" || rc=$?
        case "$rc" in
            0) echo "Seeded ${spec#*:} from ${spec%%:*}" ;;
            3) echo "build_fs_image: did not seed ${spec#*:}; the next build tries again" >&2 ;;
            *) exit 1 ;;
        esac
    done
}
install_trees
ensure_room "$(numfmt --from=iec "$FS_FREE_FLOOR")"

# Append a block-integrity (verity) trailer so the kernel detects on-disk
# corruption at read time (fs/src/verity.rs). Must be the LAST step — it
# hashes the finished image.
if [ "$VERITY" = "off" ]; then
    echo "verity: VERITY=off — $IMAGE_PATH will mount unverified and writable"
elif command -v python3 >/dev/null 2>&1; then
    if [ "$VERITY" = "rw" ]; then
        mkdir -p "$HOST_STATE"
        python3 "${SCRIPT_DIR}/gen_verity.py" --version 2 ${TAINT_PATH:+--taint "$TAINT_PATH"} \
            --record "$VERITY_RECORD" "$IMAGE_PATH"
        rm -f "$TAINT_PATH"
    else
        python3 "${SCRIPT_DIR}/gen_verity.py" --version 1 "$IMAGE_PATH"
    fi
else
    # Not a warning: a build that silently produced a writable image where a
    # verified one was asked for is the fail-open this trailer exists to end.
    echo "gen_verity: python3 is required to build a verified image (or pass VERITY=off)" >&2
    exit 1
fi

# Last: a stamp for an unfinished build would make the next run skip the work
# that failed.
build_stamp > "$STAMP_PATH"
