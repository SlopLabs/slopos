# shellcheck shell=bash
# Building Limine from the pinned source, shared by scripts/ensure_limine.sh
# (the loader SlopOS ships) and scripts/build_limine_diag.sh (a diagnostic
# build of the same loader). Source it after setting REPO_ROOT; every function
# dies through `die`, which the caller defines.

LIMINE_PATCH_DIR="$REPO_ROOT/toolchain/limine"
LIMINE_PIN="$LIMINE_PATCH_DIR/PIN"

limine_pin_value() {
    sed -n "s/^$1=\\(.*\\)$/\\1/p" "$LIMINE_PIN" | head -n 1
}

# LIMINE_VERSION, LIMINE_URL, LIMINE_SHA256 and the shipped patches, in order.
limine_load_pin() {
    [ -f "$LIMINE_PIN" ] || die "missing $LIMINE_PIN"
    LIMINE_VERSION="$(limine_pin_value limine_version)"
    LIMINE_URL="$(limine_pin_value limine_url)"
    LIMINE_SHA256="$(limine_pin_value limine_sha256)"
    [ -n "$LIMINE_VERSION" ] && [ -n "$LIMINE_URL" ] && [ -n "$LIMINE_SHA256" ] \
        || die "$LIMINE_PIN is missing a pinned value"
    LIMINE_PATCHES=()
    local p
    for p in "$LIMINE_PATCH_DIR"/*.patch; do
        [ -e "$p" ] && LIMINE_PATCHES+=("$p")
    done
}

# The target toolchain: CLANG and LD_LLD as scripts/cxx_host_tools.sh takes
# them, and the other three LLVM tools of the same version suffix (clang-18
# picks llvm-objcopy-18). Also checks make and nasm, and mtools when asked to.
limine_tools() {
    LIMINE_CC="${CLANG:-clang}"
    LIMINE_LD="${LD_LLD:-ld.lld}"
    local suffix="" t
    case "${LIMINE_CC##*/}" in
        clang-[0-9]*) suffix="-${LIMINE_CC##*clang-}" ;;
    esac
    for t in objcopy objdump readelf; do
        if command -v "llvm-$t$suffix" >/dev/null 2>&1; then
            printf -v "LIMINE_${t^^}" '%s' "llvm-$t$suffix"
        else
            printf -v "LIMINE_${t^^}" '%s' "llvm-$t"
        fi
    done
    local missing=() tools=(make nasm "$LIMINE_CC" "$LIMINE_LD" "$LIMINE_OBJCOPY" "$LIMINE_OBJDUMP" "$LIMINE_READELF")
    [ "${1:-}" = mtools ] && tools+=(mformat mcopy)
    for t in "${tools[@]}"; do
        command -v "$t" >/dev/null 2>&1 || missing+=("$t")
    done
    [ "${#missing[@]}" -eq 0 ] \
        || die "building Limine needs ${missing[*]} on PATH (packages: make, nasm, mtools, clang, lld, llvm)"
}

limine_sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$@" | cut -d' ' -f1
    else
        shasum -a 256 "$@" | cut -d' ' -f1
    fi
}

# The checked release tarball's path, fetched into third_party/ once.
# Offline, put it there or point LIMINE_SRC_URL at a copy.
limine_tarball() {
    local tarball="$REPO_ROOT/third_party/limine-${LIMINE_VERSION}.tar.xz" actual
    if [ ! -f "$tarball" ] || [ "$(limine_sha256_of "$tarball")" != "$LIMINE_SHA256" ]; then
        mkdir -p "$REPO_ROOT/third_party"
        echo "Fetching Limine v${LIMINE_VERSION} sources..." >&2
        curl -L --fail --progress-bar "${LIMINE_SRC_URL:-$LIMINE_URL}" -o "$tarball.part" >&2
        actual="$(limine_sha256_of "$tarball.part")"
        if [ "$actual" != "$LIMINE_SHA256" ]; then
            rm -f "$tarball.part"
            die "checksum mismatch for limine-${LIMINE_VERSION}.tar.xz: expected $LIMINE_SHA256, got $actual"
        fi
        mv "$tarball.part" "$tarball"
    fi
    printf '%s\n' "$tarball"
}

# A fresh tree in $1 with the shipped patches applied.
limine_patched_tree() {
    local dir="$1" tarball p
    tarball="$(limine_tarball)"
    rm -rf "$dir"
    mkdir -p "$dir"
    tar -xf "$tarball" -C "$dir" --strip-components=1
    for p in "${LIMINE_PATCHES[@]}"; do
        patch -d "$dir" -p1 -s --no-backup-if-mismatch < "$p" || die "$(basename "$p") does not apply"
    done
}

# Configure the tree in $1 with the target toolchain; further arguments are
# the ports to enable. The log goes to $2.
limine_configure() {
    local dir="$1" log="$2"
    shift 2
    (cd "$dir" && ./configure \
        CC_FOR_TARGET="$LIMINE_CC" \
        LD_FOR_TARGET="$LIMINE_LD" \
        OBJCOPY_FOR_TARGET="$LIMINE_OBJCOPY" \
        OBJDUMP_FOR_TARGET="$LIMINE_OBJDUMP" \
        READELF_FOR_TARGET="$LIMINE_READELF" \
        "$@" > "$log" 2>&1) || { tail -n 30 "$log" >&2; die "configure failed, see $log"; }
}

limine_pe_image_size() {
    local lfanew
    lfanew="$(od -An -tu4 -j 60 -N4 "$1" | tr -d ' ')"
    od -An -tu4 -j $((lfanew + 24 + 56)) -N4 "$1" | tr -d ' '
}
