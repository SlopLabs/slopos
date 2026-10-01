#!/usr/bin/env bash
set -euo pipefail

# Build an ext4 filesystem image populated with userland binaries, in the
# profile `ext4-core/profile` states (scripts/lib/ext4.sh).
#
# Usage: build_fs_image.sh <image_path> <build_dir> [bin1] [bin2] ...
#
# Each binary is placed in /bin/<name> except 'init' which goes to /sbin/init.
# With none, a volume still carries the base's data files (the C library when
# built, licence texts, fonts, the CA bundle, keymaps, the shell's completion
# rules), or under `FS_BASE=boot` only the base's sealed mount points.
#
# Environment:
#   FS_IMAGE_SIZE - image size (default: 32M)
#   FS_INODE_RATIO - bytes of volume per inode, passed to mke2fs -i. Unset
#                   uses the host mke2fs default (16384), i.e. ~65k inodes per
#                   GiB — enough for a checked-out tree plus a toolchain
#                   sysroot on a multi-GB volume.
#   FS_JOURNAL_SIZE - size of the jbd2 journal; default is 1/64 of the image,
#                   floored at 4M and capped at 64M. `0` builds no journal,
#                   which makes the kernel fall back to undo-scoped operations
#                   and refuse an unclean mount. A preserved image whose
#                   journal is another size gets a new one.
#   FS_POPULATE_DIR - a host directory whose tree becomes the volume's root, via
#                   `mke2fs -d`. Only honoured on a fresh mkfs: a preserved
#                   image holds whatever the guest wrote and is never
#                   repopulated. The directories and files this script installs
#                   afterwards are created over that tree.
#   FS_LABEL      - volume label (`mke2fs -L`); a preserved image is
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
#                   not block a refresh. An image short of the profile is
#                   converted in place, or the build refuses and names
#                   `just reset root`.
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
#   FS_BASE       - `image` (default) installs the binaries and assets; `boot`
#                   refuses binaries, COREUTILS_LINKS and EXTRA_SHARED_OBJECTS
#                   and leaves the base directories as sealed mount points,
#                   for a root the boot slot's base image is mounted over.

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
# mke2fs and tune2fs size a journal in whole MiB.
if ! journal_bytes="$(numfmt --from=iec "$FS_JOURNAL_SIZE" 2>/dev/null)" ||
   [ $(( journal_bytes % (1024 * 1024) )) -ne 0 ]; then
    echo "build_fs_image: FS_JOURNAL_SIZE must be a whole number of MiB, like 4M, got '$FS_JOURNAL_SIZE'" >&2
    exit 2
fi
VERITY="${VERITY:-on}"
case "$VERITY" in
    on|off|rw) ;;
    *) echo "build_fs_image: VERITY must be 'on', 'off' or 'rw', got '$VERITY'" >&2; exit 2 ;;
esac
FS_LABEL="${FS_LABEL:-}"
FS_HOST_TREES="${FS_HOST_TREES:-}"
FS_SEED_TREES="${FS_SEED_TREES:-}"
FS_FREE_FLOOR="${FS_FREE_FLOOR:-0}"
FS_BASE="${FS_BASE:-image}"
case "$FS_BASE" in
    image) ;;
    boot)
        if [ "${#BINS[@]}" -gt 0 ] || [ -n "${COREUTILS_LINKS:-}${EXTRA_SHARED_OBJECTS:-}" ]; then
            echo "build_fs_image: FS_BASE=boot installs no binaries; the boot slot's base carries them" >&2
            exit 2
        fi
        ;;
    *) echo "build_fs_image: FS_BASE must be 'image' or 'boot', got '$FS_BASE'" >&2; exit 2 ;;
esac

# These three mirror tools/initramfs's ROOT_DIRS and SLIBC_LICENSES and
# slopos_abi::fs::BASE_DIRS; a test in tools/initramfs holds them equal.
#
# ROOT_DIRS are created on every root rather than left to the first writer: the
# disk root does not auto-create parents the way ramfs does, and both roots
# must agree about whether a path is writable. `/media` is where a boot's
# `mount=` puts a volume.
ROOT_DIRS=(/etc /var /home /media)

# The on-disk carrier of the VFS seal; `lsattr` shows it as `i`.
IMMUTABLE_FL=0x10
SLIBC_LICENSES=(LICENSE-MIT LICENSE-APACHE NOTICE)
BASE_DIRS=(/bin /sbin /lib /usr/bin /usr/share /etc/ssl)

# macOS: extend PATH to find e2fsprogs tools installed via Homebrew
if [ "$(uname -s)" = "Darwin" ]; then
    BREW_PREFIX="$(brew --prefix 2>/dev/null || echo /opt/homebrew)"
    export PATH="${BREW_PREFIX}/opt/e2fsprogs/sbin:${BREW_PREFIX}/opt/e2fsprogs/bin:${PATH}"
fi

if ! command -v mke2fs >/dev/null 2>&1; then
    echo "mke2fs is required to create $IMAGE_PATH" >&2
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
# shellcheck source=lib/ext4.sh
. "$SCRIPT_DIR/lib/ext4.sh"
FS_TREE=(python3 "$SCRIPT_DIR/fs_tree.py")
# What the host keeps about an image, where its guest cannot write: the
# manifests of the trees it installed, and what its last seal attested.
HOST_STATE="${IMAGE_PATH}.host"
TREE_MANIFESTS="$HOST_STATE/trees"
VERITY_RECORD="$HOST_STATE/verity"

PRESERVE_FS_IMAGE="${PRESERVE_FS_IMAGE:-0}"
STAMP_PATH="${IMAGE_PATH}.stamp"
# The blocks a preserved v2 image's guest owns, which its seal must not attest.
TAINT_PATH=""
CLEANUP=()
trap 'rm -f -- "${CLEANUP[@]:-}"' EXIT

# What the image's content is a function of: an equal stamp means a preserved
# image already carries these binaries and assets, so it needs no work.
build_stamp() {
    echo "size=$FS_IMAGE_SIZE verity=$VERITY journal=$FS_JOURNAL_SIZE links=${COREUTILS_LINKS:-}"
    sha256sum "$EXT4_PROFILE" | cut -d' ' -f1
    echo "label=$FS_LABEL dirs=${ROOT_DIRS[*]} floor=$FS_FREE_FLOOR base=$FS_BASE"
    echo "base_dirs=${BASE_DIRS[*]}"
    local spec
    for spec in $FS_HOST_TREES; do
        printf 'tree %s ' "${spec#*:}"
        [ -d "${spec%%:*}" ] && "${FS_TREE[@]}" identity "${spec%%:*}" || echo absent
    done
    [ "$FS_BASE" = image ] || return 0
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
                 "${REPO_ROOT}/assets/completions"/* \
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
    [ -z "$2" ] || echo "  Fix it:     $2" >&2
    echo "  Discard it: rm -f '$IMAGE_PATH' '$STAMP_PATH'" >&2
    exit 1
}

GREW=0

# Grow in place rather than refuse: raising FS_IMAGE_SIZE on a machine you are
# living in must not be a reason to throw it away.
grow_image() {
    local have="$1" want="$2" trailer="" now
    GREW=0
    command -v resize2fs >/dev/null 2>&1 ||
        refuse "resize2fs is not installed, so the image cannot be grown to ${want}B" \
               "install e2fsprogs"
    # `resize2fs` refuses a filesystem whose last check predates its last
    # write, and this kernel never stamps `s_lastcheck` because it runs no
    # fsck. The image was proved sound and clean a moment ago; this pass is the
    # formality e2fsprogs insists on performing itself.
    e2fsck -fy "$IMAGE_PATH" >/dev/null 2>&1 ||
        refuse "e2fsck could not ready the image for a resize" "e2fsck -fy '$IMAGE_PATH'"
    # resize2fs moves blocks onto ones that were free, so once the guest's
    # blocks are measured, those it allocates join them.
    local allocated_before=""
    if [ -n "$TAINT_PATH" ]; then
        allocated_before="$(mktemp "${IMAGE_DIR}/allocated.XXXXXX")"
        CLEANUP+=("$allocated_before")
        python3 "${SCRIPT_DIR}/gen_verity.py" --allocated-out "$allocated_before" "$IMAGE_PATH" ||
            refuse "gen_verity.py could not read what the image allocates" "e2fsck -fy '$IMAGE_PATH'"
    fi
    # Kept aside until the resize lands, so a failure puts the image back
    # exactly as it was. The trailer itself is rebuilt at the end of this run.
    if image_carries_verity_trailer "$IMAGE_PATH"; then
        trailer="$(mktemp "${IMAGE_DIR}/trailer.XXXXXX")"
        CLEANUP+=("$trailer")
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
        return 0
    fi
    GREW=1
    rm -f "$trailer"
    # The grow cut off the trailer; dropping the stamp makes the run reseal.
    rm -f "$STAMP_PATH"
    [ -z "$allocated_before" ] || taint_what_moved "$allocated_before"
    echo "grew $IMAGE_PATH from ${have}B to ${now}B, keeping its contents"
}

# Grown before a log or a tree lands, never discovered full halfway through.
# Again until it fits: every group a grow adds spends some of itself on inodes.
ensure_room() {
    local need="$1" free have step=$((256 * 1024 * 1024))
    while free="$(fs_free_bytes "$IMAGE_PATH")" && [ "$free" -lt "$need" ]; do
        have="$(fs_extent_bytes "$IMAGE_PATH")"
        grow_image "$have" "$(( (have + need - free + step - 1) / step * step ))"
        [ "$GREW" = 1 ] ||
            refuse "resize2fs added nothing to an image ${free}B free, short of ${need}B" \
                   "e2fsck -fy '$IMAGE_PATH'"
    done
}

tree_bytes() {
    echo $(( $(du -s --block-size=4096 "$1" | cut -f1) * 4096 ))
}

journal_mib() {
    echo $(( $(numfmt --from=iec "$FS_JOURNAL_SIZE") / 1024 / 1024 ))
}

CONVERT_LOG=""

convert_run() {
    echo "\$ $*" >>"$CONVERT_LOG"
    "$@" >>"$CONVERT_LOG" 2>&1
}

# e2fsck exits 1 or 2 after a repair; only 4 and up mean it could not.
convert_fsck() {
    local rc=0
    convert_run e2fsck "$@" || rc=$?
    [ "$rc" -lt 4 ]
}

legacy_log_in() {
    local stat flags
    stat="$(debugfs -R "stat /.journal" "$1" 2>/dev/null)"
    echo "$stat" | grep -q 'Type: regular' || return 1
    flags="$(echo "$stat" | sed -n 's/.*Flags: \(0x[0-9a-f]*\).*/\1/p' | head -n 1)"
    [ $(( ${flags:-0} & IMMUTABLE_FL )) -ne 0 ]
}

# Ordered by e2fsprogs' preconditions: `tune2fs -I` refuses a flex_bg volume,
# and 64-bit descriptors precede the checksums over them. `ext_attr` stays, as
# tune2fs cannot clear it; the journal is fit_journal's.
convert_steps() {
    local img="$1" add=() want inode_size size have
    if ext4_ext2_era "$img" && legacy_log_in "$img"; then
        convert_run debugfs -w -R "sif /.journal flags 0" "$img" &&
            convert_run debugfs -w -R "rm /.journal" "$img" || return 1
    fi
    # Every tune2fs step below refuses a volume mounted since its last check.
    convert_fsck -fy "$img" || return 1
    inode_size="$(ext4_profile_value inode_size)"
    size="$(dumpe2fs -h "$img" 2>/dev/null | sed -n 's/^Inode size:[[:space:]]*//p')"
    if [ "$size" -gt "$inode_size" ]; then
        echo "its ${size}-byte inodes are larger than the profile's ${inode_size}, which tune2fs cannot shrink" >>"$CONVERT_LOG"
        return 1
    fi
    if [ "$size" -lt "$inode_size" ]; then
        convert_run tune2fs -f -I "$inode_size" "$img" || return 1
    fi
    have=" $(dumpe2fs -h "$img" 2>/dev/null | sed -n 's/^Filesystem features:[[:space:]]*//p') "
    for want in $(ext4_profile_value features | tr ',' ' '); do
        case "$want" in 64bit|metadata_csum|has_journal) continue ;; esac
        case "$have" in *" $want "*) ;; *) add+=("$want") ;; esac
    done
    if [ "${#add[@]}" -gt 0 ]; then
        convert_run tune2fs -O "$(IFS=,; echo "${add[*]}")" "$img" || return 1
    fi
    convert_fsck -fy "$img" || return 1
    case "$have" in *" 64bit "*) ;; *) convert_run resize2fs -b "$img" || return 1 ;; esac
    case "$have" in
        *" metadata_csum "*) ;;
        *) convert_run tune2fs -O metadata_csum "$img" || return 1 ;;
    esac
    convert_fsck -fyD "$img" || return 1
    for want in dir_index resize_inode; do
        case ",$(ext4_profile_value features)," in *",$want,"*) continue ;; esac
        case "$have" in *" $want "*) convert_run tune2fs -O "^$want" "$img" || return 1 ;; esac
    done
    convert_fsck -fyD "$img" || return 1
    convert_run e2fsck -fn "$img" || return 1
    [ -z "$(ext4_unrest "$img")" ] || { echo "not at rest: $(ext4_unrest "$img")" >>"$CONVERT_LOG"; return 1; }
    ext4_meets_profile "$img" has_journal ||
        { echo "the result still falls short of ext4-core/profile" >>"$CONVERT_LOG"; return 1; }
}

taint_what_moved() {
    python3 "${SCRIPT_DIR}/gen_verity.py" --taint-moved "$TAINT_PATH" "$1" "$IMAGE_PATH" ||
        refuse "gen_verity.py could not read what the image allocates" "e2fsck -fy '$IMAGE_PATH'"
    rm -f "$1"
}

# On a copy, so a step e2fsprogs refuses leaves the image as it was; the copy
# stops at the filesystem, whose trailer is rebuilt at the end of the run.
convert_to_profile() {
    local target work
    command -v tune2fs >/dev/null 2>&1 && command -v resize2fs >/dev/null 2>&1 ||
        refuse "converting the image to the ext4 profile needs tune2fs and resize2fs" \
               "install e2fsprogs"
    target="$(realpath -- "$IMAGE_PATH")"
    work="$(mktemp "$(dirname "$target")/convert.XXXXXX")"
    CONVERT_LOG="$(mktemp "$(dirname "$target")/convert-log.XXXXXX")"
    CLEANUP+=("$work" "$CONVERT_LOG")
    cp --reflink=auto --sparse=always "$target" "$work"
    chmod --reference="$target" "$work"
    truncate -s "$(fs_extent_bytes "$target")" "$work"
    if ! convert_steps "$work"; then
        rm -f "$work"
        echo "preserve: the last steps of the conversion:" >&2
        tail -n 15 "$CONVERT_LOG" | sed 's/^/  /' >&2
        refuse "the image could not be converted to the ext4 profile (it is unchanged)" \
               "what the last step says above; for the persistent root, \`just reset root\` discards it"
    fi
    mv "$work" "$target"
    rm -f "$STAMP_PATH"
    echo "preserve: converted $IMAGE_PATH to the ext4 profile, keeping its contents"
}

# Remade when its size is not FS_JOURNAL_SIZE: tune2fs adds a journal only to
# a volume without one, and an image at rest holds nothing in its journal.
fit_journal() {
    local have=0 want
    want="$(numfmt --from=iec "$FS_JOURNAL_SIZE")"
    if dumpe2fs -h "$IMAGE_PATH" 2>/dev/null | grep -q '^Filesystem features:.*\bhas_journal\b'; then
        have="$(debugfs -R "stat <$(dumpe2fs -h "$IMAGE_PATH" 2>/dev/null | sed -n 's/^Journal inode:[[:space:]]*//p')>" \
            "$IMAGE_PATH" 2>/dev/null | sed -n 's/.* Size: \([0-9]*\)$/\1/p' | head -n 1)"
    fi
    [ "${have:-0}" != "$want" ] || return 0
    if [ "${have:-0}" != 0 ]; then
        tune2fs -O ^has_journal "$IMAGE_PATH" >/dev/null 2>&1 ||
            refuse "tune2fs could not remove the ${have}-byte journal" "e2fsck -fy '$IMAGE_PATH'"
    fi
    [ "$want" != 0 ] || return 0
    # tune2fs refuses a journal larger than half the free space.
    ensure_room $(( 2 * want ))
    tune2fs -O has_journal -J "size=$(journal_mib)" "$IMAGE_PATH" >/dev/null 2>&1 ||
        refuse "tune2fs could not make a ${FS_JOURNAL_SIZE} journal" "e2fsck -fy '$IMAGE_PATH'"
    rm -f "$STAMP_PATH"
    echo "preserve: made a ${FS_JOURNAL_SIZE} journal in $IMAGE_PATH"
}

# Sound *and* at rest: `e2fsck -fn` passes a journal still holding a
# transaction, whose later replay would land over what the host writes here.
preserve_or_refuse() {
    local want have
    want=$(numfmt --from=iec "$FS_IMAGE_SIZE")
    have=$(fs_extent_bytes "$IMAGE_PATH")
    if [ "$have" = "0" ]; then
        refuse "there is no superblock in the image" "e2fsck -fy '$IMAGE_PATH'"
    fi
    # A v1 trailer's hashes cover the bytes a refresh would rewrite; a v2
    # trailer is recomputed at the end of this run.
    if image_carries_verity_trailer "$IMAGE_PATH" &&
       { [ "$VERITY" != "rw" ] || [ "$(verity_trailer_version "$IMAGE_PATH")" != "2" ]; }; then
        refuse "the image carries a write-protecting v1 trailer, whose hashes cover the bytes a refresh rewrites" \
               "build this image with VERITY=rw"
    fi
    if ! "${SCRIPT_DIR}/check_fs_image.sh" "$IMAGE_PATH"; then
        if ext4_ext2_era "$IMAGE_PATH" && [ -n "$(ext4_unrest "$IMAGE_PATH")" ]; then
            refuse "a root from before ext4 was left mid-write, and only the kernel before ext4 replays its log" \
                   "boot it once on that kernel; e2fsck -fy '$IMAGE_PATH' instead discards what the log held"
        fi
        refuse "the image is damaged, or a boot left it dirty (see above)" \
               "e2fsck -fy '$IMAGE_PATH'"
    fi
    # Grown before the guest's blocks are measured, since a grow moves blocks,
    # and before a conversion, whose steps need room.
    if [ "$want" -gt "$have" ]; then
        grow_image "$have" "$want"
    fi
    ensure_room "$(numfmt --from=iec "$FS_FREE_FLOOR")"
    meets_profile || convert_to_profile
    fit_journal
    ensure_room "$(numfmt --from=iec "$FS_FREE_FLOOR")"
    if [ "$VERITY" = "rw" ]; then
        TAINT_PATH="$(mktemp "${IMAGE_DIR}/taint.XXXXXX")"
        CLEANUP+=("$TAINT_PATH")
        python3 "${SCRIPT_DIR}/gen_verity.py" --taint-out "$TAINT_PATH" --record "$VERITY_RECORD" "$IMAGE_PATH" ||
            refuse "gen_verity.py could not measure what the guest wrote" "e2fsck -fy '$IMAGE_PATH'"
    fi
}

meets_profile() {
    ext4_meets_profile "$IMAGE_PATH" has_journal
}

# OR'd into what the inode has: an extent-mapped file without EXTENTS_FL
# loses its map.
seal() {
    local flags
    flags="$(debugfs -R "stat $1" "$IMAGE_PATH" 2>/dev/null | sed -n 's/.*Flags: \(0x[0-9a-f]*\).*/\1/p' | head -n 1)"
    debugfs -w -R "set_inode_field $1 flags $(printf '0x%x' $(( ${flags:-0} | IMMUTABLE_FL )))" "$IMAGE_PATH" >/dev/null
}

# `write` refuses an existing name, so a refresh unlinks first. debugfs writes
# the raw structures, so IMMUTABLE_FL does not stop it.
install_binary() {
    local src="$1" dst="$2"
    debugfs -w -R "rm $dst" "$IMAGE_PATH" >/dev/null 2>&1 || true
    debugfs -w -R "write $src $dst" "$IMAGE_PATH" >/dev/null
    debugfs -w -R "set_inode_field $dst mode 0100755" "$IMAGE_PATH" >/dev/null
    # Program-identity privilege is keyed on a binary's path, so a shipped
    # binary that is not sealed is one any task holding a write descriptor can
    # replace and then spawn into the grant.
    seal "$dst"
}

install_file() {
    local src="$1" dst="$2"
    debugfs -w -R "rm $dst" "$IMAGE_PATH" >/dev/null 2>&1 || true
    debugfs -w -R "write $src $dst" "$IMAGE_PATH" >/dev/null
}

# Asks first: `debugfs mkdir` on a name that exists allocates the inode, fails
# at the link, and leaves the leak `e2fsck` reports as an unconnected inode.
mkdir_p() {
    local out
    out="$(debugfs -R "stat $1" "$IMAGE_PATH" 2>&1)"
    case "$out" in
        *'Inode:'*) return 0 ;;
        *'File not found by ext2_lookup'*) ;;
        *) echo "build_fs_image: debugfs could not tell whether $1 exists" >&2; exit 1 ;;
    esac
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
    if [ -n "$FS_LABEL" ]; then
        tune2fs -L "$FS_LABEL" "$IMAGE_PATH" >/dev/null
    fi
else
    echo "Rebuilding ext4 image at $IMAGE_PATH ($FS_IMAGE_SIZE)"
    rm -rf "$IMAGE_PATH" "$STAMP_PATH" "$HOST_STATE"
    truncate -s "$FS_IMAGE_SIZE" "$IMAGE_PATH"
    ext4_mkfs_args
    MKFS_ARGS=(-F -q "${EXT4_MKFS_ARGS[@]}")
    if [ "$FS_JOURNAL_SIZE" = 0 ]; then
        MKFS_ARGS+=(-O ^has_journal)
    else
        MKFS_ARGS+=(-J "size=$(journal_mib)")
    fi
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
    mke2fs "${MKFS_ARGS[@]}" "$IMAGE_PATH" >/dev/null
fi

mkdir_p /bin
mkdir_p /sbin
for dir in "${ROOT_DIRS[@]}"; do
    mkdir_p "$dir"
done

ensure_room "$(numfmt --from=iec "$FS_FREE_FLOOR")"

install_base() {
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

    # A symlink, not a copy: fifty-odd copies of std would be ~8 MiB of a
    # 32 MiB root, and `argv[0]` selects the tool anyway. `debugfs symlink`
    # writes a fast symlink, so a name costs an inode and no block.
    if [ -n "${COREUTILS_LINKS:-}" ]; then
        if [ ! -f "${BUILD_DIR}/coreutils.elf" ]; then
            echo "COREUTILS_LINKS is set but ${BUILD_DIR}/coreutils.elf is missing" >&2
            exit 1
        fi
        # Word splitting is wanted here; pathname expansion is not, and a name
        # holding `*` would otherwise glob against the build directory.
        set -f
        for tool in $COREUTILS_LINKS; do
            # This runs after the binaries are installed, so a name in both
            # lists would replace a program -- and inherit its grant.
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
        # Where every `#!/usr/bin/env` script looks for it.
        case " $COREUTILS_LINKS " in
            *" env "*)
                mkdir_p /usr
                mkdir_p /usr/bin
                debugfs -w -R "rm /usr/bin/env" "$IMAGE_PATH" >/dev/null 2>&1 || true
                debugfs -w -R "symlink /usr/bin/env /bin/env" "$IMAGE_PATH" >/dev/null
                ;;
        esac
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

    # A sealed directory cannot be renamed aside to plant a fresh /bin/halt
    # under the path its grant is keyed on.
    seal /bin
    seal /sbin
    seal /lib

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

    # The C++ runtime's license texts, beside the library they cover, for the
    # same reason the fonts below carry theirs, and only where that library
    # ships.
    CXX_LICENSES="${BUILD_DIR}/libc++-licenses"
    if [ -d "$CXX_LICENSES" ] && [[ " ${EXTRA_SHARED_OBJECTS:-} " == *" libc++.so "* ]]; then
        mkdir_p /usr/share/licenses
        mkdir_p /usr/share/licenses/libc++
        for text in "$CXX_LICENSES"/*; do
            [ -f "$text" ] || continue
            fname="$(basename "$text")"
            install_file "$text" "/usr/share/licenses/libc++/$fname"
            echo "Installed license: /usr/share/licenses/libc++/$fname"
        done
    fi

    FONTS_DIR="${REPO_ROOT}/assets/fonts"
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

    COMPLETIONS_DIR="${REPO_ROOT}/assets/completions"
    if [ -d "$COMPLETIONS_DIR" ]; then
        mkdir_p /usr/share/shell
        mkdir_p /usr/share/shell/completions
        for rules in "$COMPLETIONS_DIR"/*; do
            [ -f "$rules" ] || continue
            cname=$(basename "$rules")
            install_file "$rules" "/usr/share/shell/completions/$cname"
            echo "Installed completion rules: /usr/share/shell/completions/$cname"
        done
    fi
}

install_mount_points() {
    local dir
    for dir in "${BASE_DIRS[@]}"; do
        mkdir_p "$(dirname "$dir")"
        mkdir_p "$dir"
        seal "$dir"
    done
}

if [ "$FS_BASE" = boot ]; then
    install_mount_points
else
    install_base
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
