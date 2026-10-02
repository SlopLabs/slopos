set shell := ["bash", "-euo", "pipefail", "-c"]

cargo             := env("CARGO", "cargo")
rust_channel      := `sed -n 's/^channel[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' rust-toolchain.toml`
rust_target       := "targets/x86_64-slos.json"
userland_target   := "targets/x86_64-unknown-slopos.json"
kernel_rustflags  := env("KERNEL_RUSTFLAGS", "-C force-frame-pointers=yes")

build_dir        := env("BUILD_DIR", "builddir")
cargo_target_dir := build_dir / "target"
limine_dir       := "third_party/limine"
ovmf_dir         := "third_party/ovmf"
fs_image_dir     := "fs/assets"
fs_image         := fs_image_dir / "ext2.img"
fs_image_tests   := fs_image_dir / "ext2-tests.img"
fs_image_persist := fs_image_dir / "ext2-persist.img"
fs_image_size    := env("FS_IMAGE_SIZE", "32M")
# The tests image carries `bigprog_test`, a deliberately 24 MiB binary that is
# what proves `exec` no longer stages an image in kernel memory, and the C++
# runtime with the two fixtures that exercise it. Sized on its own so the
# shipped root stays 32M — and no larger than it has to be, because
# `persist_test`'s space filler must still meet the volume's reserve before it
# meets the 8192-block `DiskBlocks` quota. 64M left 809 free blocks against a
# 819-block reserve once `libc++.so` was on it, which refuses the filler before
# it has written anything.
fs_image_size_tests := env("FS_IMAGE_SIZE_TESTS", "80M")
# Sized on its own: this disk holds work, not the shipped appliance root. No
# longer capped at 1 GiB — the verity hash array is chunked, so what bounds it
# is the 4 bytes of resident hash per 4 KiB block the machine's RAM can hold,
# which the mount refuses past rather than discovering.
persist_image_size := env("PERSIST_IMAGE_SIZE", "512M")
# The capacity volume `just test-capacity` measures: 16 GiB, two orders of
# magnitude past the appliance root, which is what puts every mount-time
# allocation and every metadata cache decision past the size they were chosen
# at. 1 GiB is no longer the ceiling — the verity hash array is chunked — and
# this image carries no trailer at all.
fs_image_capacity     := fs_image_dir / "ext2-capacity.img"
capacity_image_size   := env("CAPACITY_IMAGE_SIZE", "16G")
capacity_inode_ratio  := env("CAPACITY_INODE_RATIO", "16384")
capacity_qemu_mem     := env("CAPACITY_QEMU_MEM", "2G")
# The tree the volume is populated from, under build_dir so it is not tracked
# and so `git clean` reclaims it.
capacity_stage        := build_dir / "capacity-stage"

# The toolchain a root carries at /usr/local: the one `just toolchain` built,
# unless TOOLCHAIN_STAGE names another. A root without one builds all the same.
toolchain_install     := env("TOOLCHAIN_STAGE", build_dir / "slopos-toolchain/install")
# Free space a root keeps for a checkout's builds: a clean dev and tests system
# build, kernels, userlands, bases and the C++ runtime, holds 3.8G of it. A root
# with less is grown.
root_free_floor       := env("ROOT_FREE_FLOOR", "6G")
# The log both roots carry: sized for the volume the floor grows a root into,
# not the size it starts at.
root_journal_size     := "64M"
# The self-hosting root the toolchain, self-hosting and guest-install checks
# boot: the persistent root's shape and a clone seeded with the vendored crates,
# under the tests base. Rebuilt every run, as the tests image is.
fs_image_selfhost     := fs_image_dir / "ext2-selfhost.img"
selfhost_stage        := build_dir / "selfhost-stage"
# Outside the root, so `just reset root` keeps what the guest pushed.
guest_push_repo       := fs_image_dir / "guest-push.git"
# A compiler session: the `core` compile peaks at 1.15 GiB anonymous, and the
# file map's per-process cap is usable memory / 8, which the debug tests
# kernel's link outgrew at 4G (refused at 126394 of its 124720 pages).
dev_qemu_mem          := env("DEV_QEMU_MEM", "6G")
# The toolchain's execs and forks hold interrupts masked for seconds under TCG,
# which runs that work far slower than the timer; at the default threshold each
# is an NMI report on the serial console of an hours-long build.
dev_watchdog          := "watchdog.miss_threshold=300"
initramfs        := build_dir / "initramfs.cpio"
initramfs_tests  := build_dir / "initramfs-tests.cpio"

# The installer's medium beside the kernel and base: Limine for the ESP, and
# with PAYLOAD=1 the toolchain at /usr/local and a clone of HEAD for /src.
payload      := env("PAYLOAD", "0")
payload_on   := if payload =~ '^(1|true|on|yes)$' { "1" } else { "0" }

# One artifact per variant: a shared path lets whichever build ran last answer
# for all three — to the gates, to gdb, to the ISO. An ISO with the payload
# installs a system that builds itself, ten times slower on a dev kernel.
kernel_release   := env("KERNEL_RELEASE", payload_on)
kernel_variant   := if kernel_release == "1" { "release" } else { "dev" }
kernel_variant_tests := if kernel_release == "1" { "release-tests" } else { "tests" }
kernel_elf       := build_dir / ("kernel-" + kernel_variant + ".elf")
kernel_elf_tests := build_dir / ("kernel-" + kernel_variant_tests + ".elf")
kernel_features_tests := "slopos-testing/qemu-exit kernel/tests"

iso          := build_dir / "slop.iso"
install_medium := build_dir / "install.cpio"
# `test-installer`'s medium: the optimized tests system, always with the payload
# unless INSTALLER_PAYLOAD=0, whose install then clones a slot rather than
# building one.
iso_installer   := build_dir / "slop-installer.iso"
installer_medium := build_dir / "install-tests.cpio"
installer_live_cmdline := "tests=on tests.shutdown=on tests.verbosity=summary boot.debug=off roulette=skip root=initramfs " + dev_watchdog + " tests.run=*ext2_aaa*,*installer*"
iso_tests    := build_dir / "slop-tests.iso"
# `test-elf`: a kernel built elsewhere, never the build's own ISO.
iso_elf_tests := build_dir / "slop-elf-tests.iso"
log_file     := env("LOG_FILE", "test_output.log")

# The UEFI boot disk, in the layout every SlopOS disk has: an ESP with Limine,
# a boot partition with A/B kernel slots, and a crash partition.
boot_disk    := build_dir / "boot-disk.img"

ports        := ""
net_env      := if ports != "" { "NET=1 NET_PORTS=" + ports } else { "" }

qemu_bin     := env("QEMU_BIN", "qemu-system-x86_64")
qemu_smp     := env("QEMU_SMP", "4")
qemu_mem     := env("QEMU_MEM", "512M")
qemu_accel   := if os() == "macos" { env("QEMU_ACCEL", "hvf:tcg") } else { env("QEMU_ACCEL", "kvm:tcg") }
qemu_display := if os() == "macos" { env("QEMU_DISPLAY", "cocoa") } else { env("QEMU_DISPLAY", "auto") }
qemu_cpu     := env("QEMU_CPU", "host")

qemu_fb_width       := env("QEMU_FB_WIDTH", "1920")
qemu_fb_height      := env("QEMU_FB_HEIGHT", "1080")
qemu_fb_auto        := env("QEMU_FB_AUTO", "1")
qemu_fb_auto_policy := env("QEMU_FB_AUTO_POLICY", "primary")
qemu_fb_auto_output := env("QEMU_FB_AUTO_OUTPUT", "")
qemu_gtk_zoom       := env("QEMU_GTK_ZOOM_TO_FIT", "off")
# Emulated display adapter: virtio-vga (default), virtio-gpu-pci, or vga.
gpu                 := env("GPU", "virtio-vga")

boot_log_timeout := env("BOOT_LOG_TIMEOUT", "15")
boot_cmdline     := env("BOOT_CMDLINE", "tests=off root=initramfs")
test_cmdline     := "tests=on tests.shutdown=on tests.verbosity=summary boot.debug=on roulette=skip root=auto mount=LABEL=slopos-media:/media"
# `TEST_CMDLINE=…` is how `builddir/run_tests` threads filter / verbosity flags
# into the ISO at build time.
test_cmdline_effective := env("TEST_CMDLINE", test_cmdline)
# The self-hosting root's boots run a compiler: debug klog there is a line per
# thread created and exited, over a serial port that costs a VM exit a byte.
dev_test_cmdline   := replace(test_cmdline, "boot.debug=on", "boot.debug=off")
# Appended to the self-hosting root's boots, e.g. `prof=on`.
test_cmdline_extra := env("TEST_CMDLINE_EXTRA", "")

debug         := env("DEBUG", "0")
debug_flag    := if debug =~ '^(1|true|on|yes)$' { "boot.debug=on" } else { "" }
roulette      := env("ROULETTE", "1")
roulette_flag := if roulette =~ '^(0|false|off|no|skip)$' { "roulette=skip" } else { "" }
boot_cmdline_effective := trim(replace(boot_cmdline + " " + debug_flag + " " + roulette_flag, "  ", " "))
dev_boot_cmdline := trim(replace("tests=off " + debug_flag + " " + roulette_flag, "  ", " "))

userland_bins       := `. scripts/lib/base.sh && printf %s "$BASE_PROGRAMS"`
coreutils_tools     := `. scripts/lib/base.sh && printf %s "$COREUTILS_TOOLS"`
test_userland_bins  := `. scripts/lib/base.sh && printf %s "$BASE_TEST_PROGRAMS"`
test_shared_objects := `. scripts/lib/base.sh && printf %s "$BASE_TEST_SHARED_OBJECTS"`
base_recipe_programs := `. scripts/lib/base.sh && printf %s "$BASE_RECIPE_PROGRAMS"`
base_recipes        := `. scripts/lib/base.sh && printf %s "$BASE_RECIPES"`
recipes_prefix      := build_dir / "slopos-recipes/prefix"
base_recipe_env     := "RECIPE_PREFIX=" + recipes_prefix + " RECIPE_PROGRAMS=\"" + base_recipe_programs + "\" RECIPE_LICENSES=\"" + base_recipes + "\""

[doc("Install Rust + Go toolchains, materialize the owned `slopos` sysroot, and verify workspace")]
setup:
    scripts/ensure_toolchain.sh
    scripts/ensure_go.sh
    mkdir -p {{build_dir}}
    CARGO_TARGET_DIR={{cargo_target_dir}} {{cargo}} +{{rust_channel}} metadata --locked --format-version 1 >/dev/null

# The host's half of a userland build; the guest runs scripts/build_userland.sh
# alone. `+slopos` is the owned sysroot ensure_toolchain.sh materialises, whose
# pinned std and libc forks `-Zbuild-std` resolves this target's std from.
_build-userland:
    scripts/ensure_toolchain.sh
    CARGO="{{cargo}} +slopos" USERLAND_TARGET={{userland_target}} \
        scripts/build_userland.sh "{{build_dir}}" "{{cargo_target_dir}}"

_build-userland-tests: _build-userland
    CARGO="{{cargo}} +slopos" USERLAND_TARGET={{userland_target}} \
        scripts/build_userland.sh "{{build_dir}}" "{{cargo_target_dir}}" --test

# `VERITY=off` for the tests image because the suite writes to it; the
# shipped image keeps its trailer and `test_verity_artifact_*` mounts it.
# `PRESERVE_FS_IMAGE=0` on the tests image is load-bearing: a persistent `/`
# makes every filesystem test a mutation of the image the next run boots
# from, so CI is order-independent only if each run starts from a fresh one.
_fs-image: _build-userland
    FS_IMAGE_SIZE={{fs_image_size}} VERITY=on PRESERVE_FS_IMAGE=0 COREUTILS_LINKS="{{coreutils_tools}}" \
        scripts/build_fs_image.sh "{{fs_image}}" "{{build_dir}}" {{userland_bins}}

_fs-image-tests: _build-userland-tests
    FS_IMAGE_SIZE={{fs_image_size_tests}} VERITY=off PRESERVE_FS_IMAGE=0 COREUTILS_LINKS="{{coreutils_tools}}" \
        EXTRA_SHARED_OBJECTS="{{test_shared_objects}}" \
        scripts/build_fs_image.sh "{{fs_image_tests}}" "{{build_dir}}" {{test_userland_bins}}

# `VERITY=rw`: writable, and attested wherever no boot rewrote a block. The
# system is the boot slot's base, mounted over the root's base directories; the
# toolchain at /usr/local is the host's, replaced when it changes; the clone at
# /src/slopos is the guest's once seeded, so it is staged only for a root
# without one.
_fs-image-persist:
    #!/usr/bin/env bash
    set -euo pipefail
    seed=""
    if [ ! -f "{{fs_image_persist}}" ] || ! python3 scripts/fs_tree.py exists "{{fs_image_persist}}" /src; then
        scripts/stage_workspace.sh "{{build_dir}}/workspace"
        seed="{{build_dir}}/workspace:/src"
    fi
    FS_IMAGE_SIZE={{persist_image_size}} FS_JOURNAL_SIZE={{root_journal_size}} VERITY=rw PRESERVE_FS_IMAGE=1 FS_BASE=boot \
        FS_HOST_TREES="{{toolchain_install}}:/usr/local" FS_SEED_TREES="$seed" FS_FREE_FLOOR={{root_free_floor}} \
        scripts/build_fs_image.sh "{{fs_image_persist}}" "{{build_dir}}"
    rm -rf "{{build_dir}}/workspace"

# The persistent root's shape, with the clone seeded `--vendored`, so a build
# in the guest reads no network, and the fixtures the toolchain ladder fetches
# from. The tests base comes from the boot module.
_fs-image-selfhost:
    #!/usr/bin/env bash
    set -euo pipefail
    rm -rf "{{selfhost_stage}}"
    scripts/stage_workspace.sh "{{selfhost_stage}}/src" --vendored
    scripts/stage_ladder_fixtures.sh "{{selfhost_stage}}/ladder"
    FS_IMAGE_SIZE=1G FS_JOURNAL_SIZE={{root_journal_size}} VERITY=rw PRESERVE_FS_IMAGE=0 FS_BASE=boot \
        FS_HOST_TREES="{{toolchain_install}}:/usr/local {{selfhost_stage}}/ladder:/srv/ladder" \
        FS_SEED_TREES="{{selfhost_stage}}/src:/src" FS_FREE_FLOOR={{root_free_floor}} \
        scripts/build_fs_image.sh "{{fs_image_selfhost}}" "{{build_dir}}"
    rm -rf "{{selfhost_stage}}"

# The capacity volume: a filesystem two orders of magnitude past the appliance
# root, which is the medium `just test-capacity` measures a mount, a search, a
# write and a tree walk against. Preserved and opt-in — an mkfs of this size
# writes ~256 MiB of inode tables, so building it on every run would dominate
# the suite.
#
# It comes up holding residency, not geometry alone: a checked-out copy of this
# repository plus the pinned toolchain sysroot, which is what "holds a
# checked-out copy of this repository plus a toolchain sysroot" asks for.
_fs-image-capacity:
    #!/usr/bin/env bash
    set -euo pipefail
    populate=""
    # Staged only for a fresh mkfs: with the image already there the build
    # below preserves it and never repopulates, so staging would copy a
    # gigabyte for nothing.
    if [ ! -f "{{fs_image_capacity}}" ]; then
        # Checked before any staging work: the repository alone is ~1500 files
        # and 17 MB, which the kernel's walk budgets accept and
        # `min-files`/`min-treebytes` in scripts/gates/fsperf/tests.txt do not.
        # An image built without the sysroot passes the test and fails the gate,
        # so the floor only describes a tree this recipe can actually build if a
        # missing sysroot stops it here.
        sysroot="${RUSTUP_HOME:-$HOME/.rustup}/toolchains/{{rust_channel}}-x86_64-unknown-linux-gnu"
        if [ ! -d "$sysroot" ]; then
            echo "capacity: no toolchain sysroot at $sysroot" >&2
            echo "  The capacity volume holds this repository plus the {{rust_channel}}" >&2
            echo "  sysroot, and the residency floors the gate grades are measured off" >&2
            echo "  both halves. Install it with: scripts/ensure_toolchain.sh" >&2
            echo "  (or set RUSTUP_HOME to the rustup that already has it)." >&2
            exit 1
        fi
        rm -rf "{{capacity_stage}}"
        mkdir -p "{{capacity_stage}}/repo" "{{capacity_stage}}/sysroot"
        # `git archive`, not a copy of the worktree: exactly what is committed,
        # with no builddir/ and no tens of GB of cargo output.
        git archive HEAD | tar -x -C "{{capacity_stage}}/repo"
        cp -a "$sysroot/." "{{capacity_stage}}/sysroot/"
        populate="{{capacity_stage}}"
    fi
    FS_IMAGE_SIZE={{capacity_image_size}} FS_INODE_RATIO={{capacity_inode_ratio}} FS_LABEL=slopos-capacity \
        VERITY=off PRESERVE_FS_IMAGE=1 FS_POPULATE_DIR="$populate" \
        scripts/build_fs_image.sh "{{fs_image_capacity}}" "{{build_dir}}"

[doc("Discard the persistent root so the next just boot builds it fresh, with a new clone of HEAD at /src/slopos: what the guest wrote and has not pushed goes with it")]
reset DISK:
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{DISK}}" in
        root) img="{{fs_image_persist}}" ;;
        *) echo "usage: just reset root" >&2; exit 2 ;;
    esac
    rm -rf "$img" "$img.stamp" "$img.host"
    echo "reset: discarded $img; the next just boot builds a fresh one"

# Parsed here: after the recipe name, just passes `NAME=value` through as a
# literal argument rather than setting anything.
[positional-arguments]
[doc("Copy one file off the persistent root, or IMAGE, once the guest has shut down: just export-file PATH=/src/slopos/builddir/kernel-release.elf OUT=builddir/guest.elf")]
export-file +ARGS:
    #!/usr/bin/env bash
    set -euo pipefail
    file="" out="" image="{{fs_image_persist}}"
    for arg in "$@"; do
        case "$arg" in
            PATH=*) file="${arg#PATH=}" ;;
            OUT=*) out="${arg#OUT=}" ;;
            IMAGE=*) image="${arg#IMAGE=}" ;;
            *) file="" out=""; break ;;
        esac
    done
    if [ -z "$file" ] || [ -z "$out" ]; then
        echo "usage: just export-file PATH=<path on the image> OUT=<host path> [IMAGE=<image>]" >&2
        exit 2
    fi
    scripts/export_fs_file.sh "$file" "$image" "$out"

# The recipes whose programs every base carries. Their cross build needs the
# tests userland's libraries.
_base-recipes: _build-userland-tests
    BUILD_DIR={{build_dir}} scripts/build_recipes.sh {{base_recipes}}

_initramfs: _build-userland _base-recipes
    COREUTILS_LINKS="{{coreutils_tools}}" {{base_recipe_env}} \
        scripts/build_initramfs.sh "{{initramfs}}" "{{build_dir}}" {{userland_bins}}

_initramfs-tests: _build-userland-tests _base-recipes
    COREUTILS_LINKS="{{coreutils_tools}}" EXTRA_SHARED_OBJECTS="{{test_shared_objects}}" {{base_recipe_env}} \
        scripts/build_initramfs.sh "{{initramfs_tests}}" "{{build_dir}}" {{test_userland_bins}}

# The host's half of a kernel build around scripts/build_kernel.sh, which the
# guest runs alone: the toolchain before, the ELF gates after. `+slopos`, the
# owned sysroot inside the checkout, so std's sources sit at the same
# workspace-relative path here and in the guest.
_kernel variant features='':
    scripts/ensure_toolchain.sh
    CARGO="{{cargo}} +slopos" RUST_TARGET={{rust_target}} KERNEL_RUSTFLAGS="{{kernel_rustflags}}" \
        scripts/build_kernel.sh "{{build_dir}}" "{{cargo_target_dir}}" "{{features}}"
    scripts/check_kernel_elf_gates.sh "{{build_dir}}" "{{variant}}"

[doc("Build the kernel (implies fs-image)")]
build: _fs-image (_kernel kernel_variant)

# No `_fs-image` dependency: building the userland binaries is most of the wall
# clock, and a gate-only job has no use for them.
[doc("Build the kernel ELF alone, skipping the fs image — for gate-only jobs")]
build-kernel-only: (_kernel kernel_variant)

_install-medium:
    #!/usr/bin/env bash
    set -euo pipefail
    toolchain=""
    if [ "{{payload_on}}" = 1 ]; then
        toolchain="{{toolchain_install}}"
    fi
    LIMINE_DIR={{limine_dir}} scripts/build_install_medium.sh "{{install_medium}}" ${toolchain:+"$toolchain"}

[doc("Build the live ISO (builddir/slop.iso): kernel + initramfs, runs from RAM with no disk, and installs from it with /bin/installer. PAYLOAD=1 adds the toolchain and a clone of HEAD, which the installer copies to /usr/local and /src, and the release kernel. Honors BOOT_CMDLINE")]
iso: _initramfs _install-medium (_kernel kernel_variant)
    KERNEL_ELF={{kernel_elf}} LIMINE_DIR={{limine_dir}} INITRAMFS_FILE={{initramfs}} INSTALL_ARCHIVE={{install_medium}} \
    QEMU_FB_WIDTH={{qemu_fb_width}} QEMU_FB_HEIGHT={{qemu_fb_height}} \
    QEMU_FB_AUTO={{qemu_fb_auto}} QEMU_FB_AUTO_POLICY={{qemu_fb_auto_policy}} \
    QEMU_FB_AUTO_OUTPUT="{{qemu_fb_auto_output}}" \
        scripts/build_iso.sh "{{iso}}" "{{build_dir}}" "{{boot_cmdline_effective}}"

# The live system `test-installer` boots: the tests system, so installer_test
# runs, and the medium with the payload unless INSTALLER_PAYLOAD=0.
_iso-installer: _initramfs-tests (_kernel kernel_variant_tests kernel_features_tests)
    #!/usr/bin/env bash
    set -euo pipefail
    toolchain=""
    if [[ ! "${INSTALLER_PAYLOAD:-1}" =~ ^(0|false|off|no)$ ]]; then
        toolchain="{{toolchain_install}}"
    fi
    LIMINE_DIR={{limine_dir}} scripts/build_install_medium.sh "{{installer_medium}}" ${toolchain:+"$toolchain"}
    KERNEL_ELF={{kernel_elf_tests}} LIMINE_DIR={{limine_dir}} INITRAMFS_FILE={{initramfs_tests}} \
    INSTALL_ARCHIVE={{installer_medium}} QEMU_FB_AUTO=0 \
        scripts/build_iso.sh "{{iso_installer}}" "{{build_dir}}" "{{installer_live_cmdline}}"

# `_fs-image`: the harness attaches the shipped image as a snapshot disk.
_iso-tests: _fs-image _fs-image-tests _initramfs-tests (_kernel kernel_variant_tests kernel_features_tests)
    KERNEL_ELF={{kernel_elf_tests}} LIMINE_DIR={{limine_dir}} INITRAMFS_FILE={{initramfs_tests}} \
    QEMU_FB_WIDTH={{qemu_fb_width}} QEMU_FB_HEIGHT={{qemu_fb_height}} \
    QEMU_FB_AUTO={{qemu_fb_auto}} QEMU_FB_AUTO_POLICY={{qemu_fb_auto_policy}} \
    QEMU_FB_AUTO_OUTPUT="{{qemu_fb_auto_output}}" \
        scripts/build_iso.sh "{{iso_tests}}" "{{build_dir}}" "{{test_cmdline_effective}}"

# `tests.run=__userland_only__` is a glob that deliberately matches no kernel
# test, leaving the userland phase as the only thing exercised.
_iso-tests-userland-only: _fs-image-tests _initramfs-tests (_kernel kernel_variant_tests kernel_features_tests)
    KERNEL_ELF={{kernel_elf_tests}} LIMINE_DIR={{limine_dir}} INITRAMFS_FILE={{initramfs_tests}} \
    QEMU_FB_WIDTH={{qemu_fb_width}} QEMU_FB_HEIGHT={{qemu_fb_height}} \
    QEMU_FB_AUTO={{qemu_fb_auto}} QEMU_FB_AUTO_POLICY={{qemu_fb_auto_policy}} \
    QEMU_FB_AUTO_OUTPUT="{{qemu_fb_auto_output}}" \
        scripts/build_iso.sh "{{iso_tests}}" "{{build_dir}}" \
            "{{test_cmdline_effective}} tests.run=__userland_only__"

# Rebuilt from scratch every run: both slots hold this build and slot a is the default.
_boot-disk: _fs-image-tests _initramfs-tests (_kernel kernel_variant_tests kernel_features_tests)
    LIMINE_DIR={{limine_dir}} \
    QEMU_FB_WIDTH={{qemu_fb_width}} QEMU_FB_HEIGHT={{qemu_fb_height}} \
    QEMU_FB_AUTO={{qemu_fb_auto}} QEMU_FB_AUTO_POLICY={{qemu_fb_auto_policy}} \
    QEMU_FB_AUTO_OUTPUT="{{qemu_fb_auto_output}}" \
        scripts/build_bootdisk.sh "{{boot_disk}}" "{{kernel_elf_tests}}" "{{initramfs_tests}}" "{{test_cmdline_effective}}"

_boot-disk-dev: _initramfs (_kernel kernel_variant)
    LIMINE_DIR={{limine_dir}} \
    QEMU_FB_WIDTH={{qemu_fb_width}} QEMU_FB_HEIGHT={{qemu_fb_height}} \
    QEMU_FB_AUTO={{qemu_fb_auto}} QEMU_FB_AUTO_POLICY={{qemu_fb_auto_policy}} \
    QEMU_FB_AUTO_OUTPUT="{{qemu_fb_auto_output}}" \
        scripts/build_bootdisk.sh "{{boot_disk}}" "{{kernel_elf}}" "{{initramfs}}" "{{dev_boot_cmdline}}"

_qemu-boot mode video iso fs_image *extra_env:
    QEMU_BIN={{qemu_bin}} QEMU_SMP={{qemu_smp}} QEMU_MEM={{qemu_mem}} \
    QEMU_ACCEL={{qemu_accel}} QEMU_CPU={{qemu_cpu}} QEMU_DISPLAY={{qemu_display}} \
    VIDEO={{video}} \
    QEMU_FB_WIDTH={{qemu_fb_width}} QEMU_FB_HEIGHT={{qemu_fb_height}} \
    QEMU_FB_AUTO={{qemu_fb_auto}} QEMU_FB_AUTO_POLICY={{qemu_fb_auto_policy}} \
    QEMU_FB_AUTO_OUTPUT="{{qemu_fb_auto_output}}" \
    QEMU_GTK_ZOOM_TO_FIT={{qemu_gtk_zoom}} \
    GPU={{gpu}} \
    OVMF_DIR={{ovmf_dir}} \
    {{extra_env}} \
        scripts/qemu_run.sh "{{mode}}" "{{iso}}" "{{fs_image}}"

# Optimized by default: a dev-profile kernel spends ten times as long in every
# syscall and page fault a compiler makes. A disk closed mid-write boots
# unrefreshed, because the host cannot write into an image whose journal the
# kernel has yet to replay.
[doc("Boot the development machine and spin the Wheel of Fate: a persistent / carrying the toolchain and a clone of HEAD, and an A/B boot disk the guest installs into. KERNEL_RELEASE=0 for a dev kernel, VIDEO=0 for serial only, ports=7777,8080 to forward")]
boot:
    #!/usr/bin/env bash
    set -euo pipefail
    export KERNEL_RELEASE="${KERNEL_RELEASE:-1}"
    refresh=_fs-image-persist
    . scripts/lib/ext4.sh
    if [ -f "{{fs_image_persist}}" ] && ext4_has_feature "{{fs_image_persist}}" has_journal &&
       [ -n "$(ext4_unrest "{{fs_image_persist}}")" ]; then
        echo "boot: {{fs_image_persist}} was closed mid-write; booting it unrefreshed so its journal replays" >&2
        refresh=""
    fi
    just _boot-disk-dev $refresh
    echo "boot: in the guest: cd /src/slopos; git pull; scripts/selfhost.sh install; bootctl reboot"
    echo "boot: the guest's git push lands in {{guest_push_repo}}; git fetch {{guest_push_repo}} <branch> takes it"
    QEMU_MEM="${QEMU_MEM:-{{dev_qemu_mem}}}" GIT_PUSH_REPO="$PWD/{{guest_push_repo}}" \
        just _qemu-boot "interactive" "${VIDEO:-1}" {{boot_disk}} {{fs_image_persist}} BOOT_DISK_IMG={{boot_disk}} {{net_env}}

[doc("just boot without the Wheel of Fate")]
boot-fast:
    ROULETTE=0 just boot

[doc("just boot-fast on the dev kernel with QEMU's GDB stub on :1234 and a monitor socket; attach with debug-gdb, debug-bt or debug-monitor")]
boot-debug:
    QEMU_DEBUG=1 KERNEL_RELEASE=0 ROULETTE=0 just boot

[doc("Boot the live ISO from RAM with no disk attached, as bare metal runs it; spins the Wheel of Fate unless ROULETTE=0")]
boot-live: iso
    just _qemu-boot "interactive" "${VIDEO:-1}" {{iso}} {{fs_image}} QEMU_NO_ROOT_DISK=1 {{net_env}}

[doc("Install check: on one disk in the bare-metal layout, register SlopOS's firmware entry, install a kernel into a boot slot, try it once, commit it, and roll back a slot that panics and one that aborts, each leaving its crash record, across the reboots of one QEMU, with the ESP untouched")]
test-install:
    #!/usr/bin/env bash
    set -euo pipefail
    TEST_CMDLINE="{{test_cmdline}} tests.run=*ext2_aaa*,*install*" BOOTDISK_PANIC_ENTRY=1 \
        BOOTDISK_ROOT_IMAGE={{fs_image_tests}} just _boot-disk
    . scripts/lib/bootdisk.sh
    bootdisk_layout
    esp="$(bootdisk_partition_sha256 {{boot_disk}} "$ESP_TYPE")"
    log="{{build_dir}}/install.log"
    rc=0
    # `bootctl clone` holds a slot's tests kernel and base in memory at once.
    # The root is the boot disk's own partition, as on bare metal.
    timeout "${INSTALL_TIMEOUT_SECS:-900}" \
        just _qemu-boot "test" "0" {{boot_disk}} {{fs_image_tests}} QEMU_ALLOW_REBOOT=1 BOOT_DISK_IMG={{boot_disk}} \
        QEMU_NO_ROOT_DISK=1 QEMU_MEM=1G >"$log" 2>&1 || rc=$?
    missing=0
    entry="$(grep -aoE 'INSTALL-FIRMWARE-ENTRY Boot[0-9A-F]{4}' "$log" | head -n1 | cut -d' ' -f2 || true)"
    for marker in "INSTALL-FIRMWARE-ENTRY ${entry:-Boot????} first" "INSTALL-STAGE 1: rebooting into slopos-b" \
        "INSTALL-THROUGH $entry at stage 1" "INSTALL-STAGE 2: rebooting into slopos-bad" "panic=reboot: resetting" \
        "INSTALL-THROUGH $entry at stage 2" "INSTALL-CRASH-RECORD /var/log/crash/" "INSTALL-STAGE 3: rebooting into slopos-abort" \
        "crash record: written" "INSTALL-THROUGH $entry at stage 3" "ok 1 - boot_slot_install_commit_rollback"; do
        grep -aqF "$marker" "$log" || { echo "FAIL: '$marker' not in $log" >&2; missing=1; }
    done
    grep -aqE "crash record: [0-9]+ written to nvme[0-9]+n[0-9]+p[0-9]+ slot [0-9]+" "$log" ||
        { echo "FAIL: the panic wrote no crash record; see $log" >&2; missing=1; }
    grep -aq "not ok" "$log" && { echo "FAIL: a test failed; see $log" >&2; missing=1; }
    [ "$(bootdisk_partition_sha256 {{boot_disk}} "$ESP_TYPE")" = "$esp" ] ||
        { echo "FAIL: the ESP changed; a commit must touch no limine.conf" >&2; missing=1; }
    window="$(bootdisk_partition {{boot_disk}} "$ROOT_TYPE")"
    read -r start size <<<"$window"
    root="{{build_dir}}/install-root.img"
    dd if={{boot_disk}} of="$root" bs=1M iflag=skip_bytes,count_bytes skip="$start" count="$size" conv=sparse status=none
    scripts/check_fs_image.sh "$root" || missing=1
    [ "$missing" = 0 ] || exit 1
    echo "test-install: registered, installed, tried, committed and rolled back with the ESP untouched (qemu rc=$rc); log in $log"

[doc("Guest install check: the guest takes HEAD over git, builds its kernel and base on the self-hosting root, installs them into slot b and boots them; that system pushes a commit the host fetches; then it commits the slot and rolls back a slot that panics")]
test-install-guest:
    #!/usr/bin/env bash
    set -euo pipefail
    [ -d "{{toolchain_install}}" ] || { echo "FAIL: no toolchain at {{toolchain_install}} — run just toolchain" >&2; exit 1; }
    git diff --quiet HEAD || { echo "FAIL: slot a is built from the working tree and the guest builds HEAD; commit or stash first" >&2; exit 1; }
    head="$(git rev-parse HEAD)"
    just _fs-image-selfhost
    # Slot a is the optimized tests kernel, as for test-selfhost: it is the
    # machine that runs the build.
    KERNEL_RELEASE=1 TEST_CMDLINE="{{dev_test_cmdline}} {{dev_watchdog}} tests.run=*ext2_aaa*,*install*" \
        BOOTDISK_PANIC_ENTRY=1 just _boot-disk
    log="{{build_dir}}/install-guest.log"
    push="$PWD/{{build_dir}}/install-guest-push.git"
    rm -rf "$push"
    rc=0
    # The self-hosting budget: under TCG the guest's build alone takes hours.
    timeout "${INSTALL_TIMEOUT_SECS:-28800}" \
        just _qemu-boot "test" "0" {{boot_disk}} {{fs_image_selfhost}} QEMU_ALLOW_REBOOT=1 BOOT_DISK_IMG={{boot_disk}} \
        GIT_PUSH_REPO="$push" QEMU_MEM="${QEMU_MEM:-{{dev_qemu_mem}}}" \
        >"$log" 2>&1 || rc=$?
    missing=0
    for marker in "INSTALL-COMMIT $head" "INSTALL-BUILT guest-" "INSTALL-STAGE 1: rebooting into slopos-b" "INSTALL-BOOTED " \
        "INSTALL-BASE guest-" "INSTALL-PUSHED " "INSTALL-STAGE 2: rebooting into slopos-bad" "panic=reboot: resetting" \
        "INSTALL-CRASH-RECORD /var/log/crash/" "INSTALL-STAGE 3: rebooting into slopos-abort" "crash record: written" \
        "ok 1 - boot_slot_install_commit_rollback"; do
        grep -aqF "$marker" "$log" || { echo "FAIL: '$marker' not in $log" >&2; missing=1; }
    done
    grep -aqE "crash record: [0-9]+ written to nvme[0-9]+n[0-9]+p[0-9]+ slot [0-9]+" "$log" ||
        { echo "FAIL: the panic wrote no crash record; see $log" >&2; missing=1; }
    grep -aq "not ok" "$log" && { echo "FAIL: a test failed; see $log" >&2; missing=1; }
    [ "$rc" != 124 ] || { echo "FAIL: timed out after ${INSTALL_TIMEOUT_SECS:-28800} s (INSTALL_TIMEOUT_SECS)" >&2; exit 1; }
    [ "$missing" = 0 ] || exit 1
    tag="$(grep -aoE 'INSTALL-BUILT guest-[0-9]+' "$log" | head -n1 | cut -d' ' -f2)"
    booted="$(grep -aE "BOOT: kernel .*/boot/b/kernel.elf \([0-9]+ bytes\), build tag $tag\b" "$log" | head -n1 || true)"
    [ -n "$booted" ] || { echo "FAIL: no boot of /boot/b/kernel.elf reports build tag $tag" >&2; exit 1; }
    # The ref comes from what the guest printed, so only the shape install_test
    # gives it reaches `git fetch` in this checkout.
    read -r _ pushed ref < <(grep -aoE 'INSTALL-PUSHED [0-9a-f]{40} [^[:space:]]+' "$log" | head -n1)
    [ "$ref" = "install-test/$tag" ] || { echo "FAIL: the guest pushed '$ref', not install-test/$tag" >&2; exit 1; }
    git fetch -q "$push" "refs/heads/$ref"
    [ "$(git rev-parse FETCH_HEAD)" = "$pushed" ] && [ "$(git rev-parse FETCH_HEAD^)" = "$head" ] ||
        { echo "FAIL: $ref in $push is not the guest's commit $pushed on $head" >&2; exit 1; }
    guest="{{build_dir}}/guest"
    mkdir -p "$guest"
    scripts/export_fs_file.sh src/slopos/builddir/kernel-tests.elf "{{fs_image_selfhost}}" "$guest/installed.elf"
    scripts/export_fs_file.sh src/slopos/builddir/initramfs-tests.cpio "{{fs_image_selfhost}}" "$guest/installed.cpio"
    . scripts/lib/bootdisk.sh
    bootdisk_layout
    window="$(bootdisk_partition {{boot_disk}} "$BOOT_TYPE")"
    read -r boot_at _ <<<"$window"
    mcopy -o -i "{{boot_disk}}@@$boot_at" "::$SLOTS_DIR/b/$KERNEL_FILE" "$guest/slot-b.elf"
    mcopy -o -i "{{boot_disk}}@@$boot_at" "::$SLOTS_DIR/b/$BASE_FILE" "$guest/slot-b.cpio"
    cmp "$guest/installed.elf" "$guest/slot-b.elf" ||
        { echo "FAIL: slot b does not hold the kernel the guest built" >&2; exit 1; }
    cmp "$guest/installed.cpio" "$guest/slot-b.cpio" ||
        { echo "FAIL: slot b does not hold the base the guest built" >&2; exit 1; }
    size="$(stat -c %s "$guest/installed.elf")"
    grep -qF "($size bytes)" <<<"$booted" ||
        { echo "FAIL: the booted kernel's size is not the guest build's $size bytes: $booted" >&2; exit 1; }
    base_size="$(stat -c %s "$guest/installed.cpio")"
    grep -aqE "BOOT: base .*/boot/b/base.img \($base_size bytes\)" "$log" ||
        { echo "FAIL: no boot of /boot/b/base.img reports the guest build's $base_size bytes" >&2; exit 1; }
    echo "test-install-guest: the guest built $head as $tag (kernel $size bytes, base $base_size bytes), booted it from slot b, pushed $pushed, committed the slot and rolled back a panicking slot (qemu rc=$rc); log in $log"

# Each disk: a QEMU on the NV-varstore OVMF with the medium on a USB stick
# installs, then one with the stick gone boots the disk, builds and installs
# the system and commits it; the varstore is kept between them, as a
# machine's flash is. The blank disk then takes a reinstall that keeps its
# root, and a boot after it.
[doc("Installer check: from the ISO on a USB stick, install SlopOS onto a blank disk (erase), beside another system (free space) and over an existing partition (reuse), then boot each disk with the stick gone, build and install the system there and commit it, and reinstall over the blank disk keeping its root; the host holds every table to sfdisk, every root to e2fsck, every FAT volume to fsck.fat and the other system's partitions, entries and files to their bytes. INSTALLER_PAYLOAD=0 installs without the toolchain and clones a slot instead of building one; names a subset: just test-installer foreign")]
test-installer *DISKS:
    #!/usr/bin/env bash
    set -euo pipefail
    payload=1
    [[ ! "${INSTALLER_PAYLOAD:-1}" =~ ^(0|false|off|no)$ ]] || payload=0
    if [ "$payload" = 1 ] && [ ! -d "{{toolchain_install}}" ]; then
        echo "FAIL: no toolchain at {{toolchain_install}} — run just toolchain, or INSTALLER_PAYLOAD=0" >&2
        exit 1
    fi
    # The optimized tests kernel: the installed system is the machine that
    # runs the build.
    KERNEL_RELEASE=1 just _iso-installer
    . scripts/lib/bootdisk.sh
    bootdisk_layout
    # Each check says what failed and answers non-zero, so one disk's failure
    # leaves the others to run.
    # One QEMU on the disk, from the stick when `$3` names one, which must
    # print each of the markers after it and fail no test. A guest whose
    # serial log stops growing is ended rather than waited on for its whole
    # budget: a build prints every few seconds, and a blocked one never again.
    boot_once() {
        local log="$1" budget="$2" stick="$3" ok=0 marker runner size=-1 still=0
        local stall="${INSTALLER_STALL_SECS:-1800}"
        shift 3
        local how=(QEMU_ALLOW_REBOOT=1)
        [ -z "$stick" ] || how=(INSTALL_STICK="$stick")
        setsid timeout "$budget" \
            just _qemu-boot "test" "0" {{iso_installer}} "$disk" "${how[@]}" BOOT_DISK_IMG="$disk" \
            OVMF_VARS_FILE="$PWD/$vars" QEMU_NO_ROOT_DISK=1 QEMU_TEST_DISKS=0 \
            QEMU_MEM="${QEMU_MEM:-{{dev_qemu_mem}}}" >"$log" 2>&1 &
        runner=$!
        while kill -0 "$runner" 2>/dev/null; do
            sleep 30
            if [ "$(stat -c %s "$log")" = "$size" ]; then
                still=$((still + 30))
            else
                still=0
                size="$(stat -c %s "$log")"
            fi
            if [ "$still" -ge "$stall" ]; then
                echo "FAIL: $log has not grown in ${still}s; the guest is ended" >&2
                kill -TERM -- "-$runner" 2>/dev/null || true
                ok=1
                break
            fi
        done
        wait "$runner" || true
        for marker in "$@"; do
            grep -aqF "$marker" "$log" || { echo "FAIL: '$marker' not in $log" >&2; ok=1; }
        done
        ! grep -aq "not ok" "$log" || { echo "FAIL: a test failed; see $log" >&2; ok=1; }
        return "$ok"
    }
    # The table whole, every FAT volume SlopOS wrote clean, and the root
    # clean, at rest and with the base's mount points sealed.
    check_disk() {
        local disk="$1" kind="$2" window start size image type dir flags said ok=0
        said="$(sfdisk --verify "$disk" 2>&1)" && grep -qF "No errors detected" <<<"$said" ||
            { echo "FAIL: sfdisk finds $disk's table wrong: $said" >&2; ok=1; }
        image="{{build_dir}}/installer-$kind-part.img"
        for type in "$ESP_TYPE" "$BOOT_TYPE" "$ROOT_TYPE"; do
            window="$(bootdisk_partition "$disk" "$type")" ||
                { echo "FAIL: $disk holds no partition of type $type" >&2; ok=1; continue; }
            read -r start size <<<"$window"
            dd if="$disk" of="$image" bs=1M iflag=skip_bytes,count_bytes skip="$start" count="$size" \
                conv=sparse status=none || { echo "FAIL: reading $type of $disk" >&2; ok=1; continue; }
            if [ "$type" != "$ROOT_TYPE" ]; then
                fsck.fat -n "$image" >/dev/null 2>&1 ||
                    { echo "FAIL: fsck.fat finds the $type volume on $disk wrong" >&2; ok=1; }
                continue
            fi
            scripts/check_fs_image.sh "$image" || ok=1
            for dir in /bin /sbin /lib /usr/bin /usr/share /etc/ssl; do
                flags="$(debugfs -R "stat $dir" "$image" 2>/dev/null |
                    sed -n 's/.*Flags: \(0x[0-9a-f]*\).*/\1/p' | head -n 1)"
                (( ${flags:-0} & 0x10 )) || { echo "FAIL: $dir on the root is not sealed" >&2; ok=1; }
            done
        done
        rm -f "$image"
        return "$ok"
    }
    # What the disk held before the install, against what it holds after.
    check_kept() {
        local disk="$1" kind="$2" keep="$1.foreign" window start size file old new ok=0
        new="$(sfdisk --dump "$disk")"
        case "$kind" in
            foreign)
                for name in "EFI system partition" "foreign data"; do
                    old="$(grep -F "name=\"$name\"" "$disk.before" || true)"
                    [ -n "$old" ] && [ "$old" = "$(grep -F "name=\"$name\"" <<<"$new")" ] ||
                        { echo "FAIL: the table's entry for \"$name\" changed or is gone" >&2; ok=1; }
                done
                window="$(sed -n 's/.*start= *\([0-9]*\), size= *\([0-9]*\),.*name="foreign data".*/\1 \2/p' <<<"$new")"
                read -r start size <<<"$window"
                [ "$(dd if="$disk" bs=512 skip="$start" count="$size" status=none | sha256sum | cut -d' ' -f1)" = \
                    "$(cat "$keep/data.sha256")" ] ||
                    { echo "FAIL: the installer changed the other system's partition" >&2; ok=1; }
                window="$(bootdisk_partition "$disk" "$ESP_TYPE")" ||
                    { echo "FAIL: $disk holds no ESP" >&2; return 1; }
                read -r start _ <<<"$window"
                for file in BOOTX64.EFI grub.cfg; do
                    MTOOLS_SKIP_CHECK=1 mcopy -n -i "$disk@@$start" "::/EFI/other/$file" "$keep/$file.after" &&
                        cmp -s "$keep/$file" "$keep/$file.after" ||
                        { echo "FAIL: the installer changed /EFI/other/$file" >&2; ok=1; }
                done
                ! MTOOLS_SKIP_CHECK=1 mdir -i "$disk@@$start" ::/EFI/BOOT >/dev/null 2>&1 ||
                    { echo "FAIL: the installer wrote the removable-media path on a shared ESP" >&2; ok=1; }
                ;;
            reuse)
                read -r start old <<<"$(sed -n 's/.*start= *\([0-9]*\),.*uuid=\([0-9A-F-]*\),.*name="installer-test-root".*/\1 \2/p' "$disk.before")"
                new="$(grep -E "start= *$start," <<<"$new" | sed -n 's/.*uuid=\([0-9A-F-]*\),.*/\1/p')"
                [ -n "$old" ] && [ -n "$new" ] && [ "$old" != "$new" ] ||
                    { echo "FAIL: the reused root kept the PARTUUID $old the other system knew it by" >&2; ok=1; }
                ;;
        esac
        return "$ok"
    }
    failed=0
    for kind in {{ if DISKS == "" { "blank foreign reuse" } else { DISKS } }}; do
        echo "── $kind ──"
        disk="{{build_dir}}/installer-$kind.img"
        vars="{{build_dir}}/installer-$kind.vars"
        logs="{{build_dir}}/installer-$kind"
        scripts/make_installer_disk.sh "$kind" "$disk" ||
            { echo "FAIL: $kind: no disk to install onto" >&2; failed=1; continue; }
        sfdisk --dump "$disk" >"$disk.before" 2>/dev/null || : >"$disk.before"
        rm -f "$vars"
        missing=0
        boot_once "$logs-install.log" "${INSTALLER_TIMEOUT_SECS:-1800}" {{iso_installer}} \
            "INSTALLER-INSTALLED" "ok 1 - installed_built_and_committed" || missing=1
        if [ "$missing" = 0 ]; then
            # The build's budget: under TCG the guest's build alone takes hours.
            budget=1800
            [ "$payload" = 0 ] || budget=28800
            markers=("INSTALLER-BOOTED slopos-a" "INSTALLER-STAGE 2: rebooting into slopos-b"
                "INSTALLER-COMMITTED slopos-b" "ok 1 - installed_built_and_committed")
            [ "$payload" = 0 ] || markers+=("INSTALLER-BUILT guest-" "INSTALLER-RUNS ")
            [ "$kind" != foreign ] || markers+=("INSTALLER-FOREIGN-KEPT")
            boot_once "$logs-boot.log" "${INSTALLER_BOOT_TIMEOUT_SECS:-$budget}" "" "${markers[@]}" || missing=1
            check_disk "$disk" "$kind" || missing=1
        fi
        check_kept "$disk" "$kind" || missing=1
        if [ "$missing" = 0 ] && [ "$kind" = blank ]; then
            # The firmware boots SlopOS first now; a person picks the stick
            # from its boot menu, which a fresh varstore stands in for.
            rm -f "$vars"
            markers=("INSTALLER-INSTALLED Reinstall" "ok 1 - installed_built_and_committed")
            [ "$payload" = 0 ] ||
                markers+=("/usr/local already holds this medium's toolchain" "the root has a /src already")
            boot_once "$logs-reinstall.log" "${INSTALLER_TIMEOUT_SECS:-1800}" {{iso_installer}} \
                "${markers[@]}" || missing=1
            [ "$missing" = 1 ] ||
                boot_once "$logs-reinstalled.log" "${INSTALLER_TIMEOUT_SECS:-1800}" "" \
                    "INSTALLER-KEPT" "ok 1 - installed_built_and_committed" || missing=1
            check_disk "$disk" "$kind" || missing=1
        fi
        if [ "$missing" = 0 ]; then
            loop="built and installed the system"
            [ "$payload" = 1 ] || loop="cloned slot a"
            [ "$kind" != blank ] || loop="$loop, then reinstalled keeping the root"
            echo "test-installer: $kind: installed from the stick, booted from the disk, $loop"
            rm -rf "$disk" "$disk.foreign" "$disk.before" "$vars"
        else
            echo "FAIL: $kind; logs in $logs-*.log, the disk at $disk" >&2
            failed=1
        fi
    done
    exit "$failed"

[doc("Boot the live ISO headless for BOOT_LOG_TIMEOUT seconds, serial log in test_output.log; fails unless /sbin/init launched")]
boot-log: iso
    #!/usr/bin/env bash
    set -euo pipefail
    just _qemu-boot "logged" "0" {{iso}} {{fs_image}} "BOOT_LOG_TIMEOUT={{boot_log_timeout}} LOG_FILE={{log_file}} QEMU_NO_ROOT_DISK=1"
    grep -q "USERLAND: launched /sbin/init" "{{log_file}}" ||
        { echo "boot-log: /sbin/init did not launch; the log is {{log_file}}" >&2; exit 1; }

# The `debug-*` recipes attach to a QEMU already running under `boot-debug`;
# they never rebuild.

[doc("Capture all-CPU backtraces from the running kernel (writes builddir/freeze-gdb.log)")]
debug-bt:
    @test -f {{kernel_elf}} || { echo "missing {{kernel_elf}} — run 'just boot-debug' first" >&2; exit 1; }
    @echo "Attaching to QEMU GDB stub on :1234 — kernel must be running with 'just boot-debug'…"
    gdb -q {{kernel_elf}} \
        -ex 'set pagination off' \
        -ex 'target remote :1234' \
        -ex 'info threads' \
        -ex 'thread apply all bt 30' \
        -ex 'detach' \
        -ex 'quit' 2>&1 | tee {{build_dir}}/freeze-gdb.log
    @echo "Wrote {{build_dir}}/freeze-gdb.log"

[doc("Interactive GDB attached to the running kernel (Ctrl-D to exit)")]
debug-gdb:
    @test -f {{kernel_elf}} || { echo "missing {{kernel_elf}} — run 'just boot-debug' first" >&2; exit 1; }
    gdb -q {{kernel_elf}} \
        -ex 'set pagination off' \
        -ex 'target remote :1234'

[doc("Connect to the QEMU monitor socket — info cpus, cpu N, info registers, …")]
debug-monitor:
    @test -S /tmp/slopos-monitor.sock || { echo "no monitor socket at /tmp/slopos-monitor.sock — boot with 'just boot-debug' first" >&2; exit 1; }
    @echo "Connecting to QEMU monitor — type 'quit' or Ctrl-D to detach…"
    socat - UNIX-CONNECT:/tmp/slopos-monitor.sock

# Record/replay needs TCG + smp=1 for icount, so it cannot capture SMP-only races.

# These compare one emulated clock against another, which icount decouples;
# skipping them lets the "tests" boot step pass so recording reaches the
# userland phase. `test_hpet_delay_accuracy` came off this list once it started
# measuring the HPET against itself over a minimum of several rounds — an
# exemption that stops describing a real failure is a dead exemption.
rr_skip := "slopos_core::syscall::tests::test_kill_process_group_semantics,slopos_drivers::tests::apic_timer_tests::test_lapic_timer_tick_rate_reasonable"

[doc("Record a deterministic test run to builddir/replay.bin (TCG, smp=1)")]
rr-record:
    TEST_CMDLINE="{{test_cmdline}} tests.skip={{rr_skip}}" just _iso-tests
    scripts/qemu_rr.sh record "{{iso_tests}}" "{{fs_image_tests}}"

[doc("Replay builddir/replay.bin under interactive GDB (gdbstub halted on :1234)")]
rr-replay:
    @test -f {{build_dir}}/replay.bin || { echo "no recording — run 'just rr-record' first" >&2; exit 1; }
    scripts/qemu_rr.sh replay "{{iso_tests}}" "{{fs_image_tests}}" &
    sleep 2
    -gdb -q -x scripts/gdb/slopos.gdb
    -pkill -f 'rr=replay'

[doc("Batch reverse-debug: run to fault; pass WATCH=<VA> to reverse-find its writer")]
rr-gdb WATCH='0':
    @test -f {{build_dir}}/replay.bin || { echo "no recording — run 'just rr-record' first" >&2; exit 1; }
    scripts/qemu_rr.sh replay "{{iso_tests}}" "{{fs_image_tests}}" &
    sleep 2
    -gdb -q -batch -ex 'set $watch_va = {{WATCH}}' -x scripts/gdb/find_corruptor.gdb 2>&1 | tee {{build_dir}}/rr-session.log
    -pkill -f 'rr=replay'

# Idempotent — Go's build cache makes warm rebuilds ~50ms — so every `test*`
# recipe below can depend on it unconditionally.
_build-run-tests:
    mkdir -p {{build_dir}}
    cd tools/run_tests && go build -o ../../{{build_dir}}/run_tests .

[doc("Run the SlopOS test harness — live progress bar, per-failure detail; pass a 'glob' filter as the positional argument")]
test FILTER='': _build-run-tests
    {{build_dir}}/run_tests --filter "{{FILTER}}"

[doc("Re-run only the tests that failed on the previous `just test` invocation.")]
test-rerun-failed: _build-run-tests
    {{build_dir}}/run_tests --rerun-failed

[doc("Same as `just test` but dump captured klog of every test (not only failures).")]
test-verbose FILTER='': _build-run-tests
    {{build_dir}}/run_tests --verbose --filter "{{FILTER}}"

[doc("Suppress per-test output; render only failures + summary.")]
test-quiet FILTER='': _build-run-tests
    {{build_dir}}/run_tests --quiet --filter "{{FILTER}}"

[doc("Passthrough QEMU stdout verbatim — KTAP and klog interleaved. Last-resort debugging.")]
test-raw: _build-run-tests
    {{build_dir}}/run_tests --raw

[doc("Append one JSON event per line to PATH (machine-consumable).")]
test-json PATH: _build-run-tests
    {{build_dir}}/run_tests --json "{{PATH}}"

[doc("Skip the kernel-side test phase; run only the Phase 3 userland tests.")]
test-userland-only: _iso-tests-userland-only _build-run-tests
    {{build_dir}}/run_tests --no-build --iso "{{iso_tests}}" --fs-image "{{fs_image_tests}}"

# Parsed here: after the recipe name, just passes `NAME=value` through as a
# literal argument rather than setting anything.
[positional-arguments]
[doc("Run the suite on a tests kernel built elsewhere, e.g. by the guest, without building one: just test-elf [ELF=]builddir/guest-tests.elf [BASE=<tests base.cpio>] ['glob']")]
test-elf +ARGS: _fs-image _fs-image-tests _initramfs-tests _build-run-tests
    #!/usr/bin/env bash
    set -euo pipefail
    elf="" base="{{initramfs_tests}}" filter=""
    for arg in "$@"; do
        case "$arg" in
            ELF=*) elf="${arg#ELF=}" ;;
            BASE=*) base="${arg#BASE=}" ;;
            *) if [ -z "$elf" ] && [ -f "$arg" ]; then elf="$arg"; else filter="$arg"; fi ;;
        esac
    done
    [ -n "$elf" ] || { echo "usage: just test-elf ELF=<kernel> [BASE=<tests base.cpio>] ['glob']" >&2; exit 2; }
    KERNEL_ELF="$elf" LIMINE_DIR={{limine_dir}} INITRAMFS_FILE="$base" \
    QEMU_FB_WIDTH={{qemu_fb_width}} QEMU_FB_HEIGHT={{qemu_fb_height}} \
    QEMU_FB_AUTO={{qemu_fb_auto}} QEMU_FB_AUTO_POLICY={{qemu_fb_auto_policy}} \
    QEMU_FB_AUTO_OUTPUT="{{qemu_fb_auto_output}}" \
        scripts/build_iso.sh "{{iso_elf_tests}}" "{{build_dir}}" "{{test_cmdline_effective}}${filter:+ tests.run=$filter}"
    {{build_dir}}/run_tests --no-build --iso "{{iso_elf_tests}}" --fs-image "{{fs_image_tests}}"

# In the body, not a dependency: `_iso-tests` rebuilds the fs image, which must not happen between the boots.
[doc("Two-boot persistence check: write+fsync, power down, boot the same image, read the payload back")]
test-persist: _build-run-tests
    #!/usr/bin/env bash
    set -euo pipefail
    # `*ext2_aaa*` is the ext2 root mount every file-creating filtered run needs.
    TEST_CMDLINE="{{test_cmdline}} tests.run=*ext2_aaa*,*persist*" just _iso-tests
    echo "── boot 1: seed ──"
    {{build_dir}}/run_tests --no-build --iso "{{iso_tests}}" --fs-image "{{fs_image_tests}}"
    scripts/check_fs_image.sh "{{fs_image_tests}}"
    echo "── boot 2: verify (same image, no rebuild) ──"
    # `set -e` must not abort: a failing boot 2 is what this recipe reports.
    rc=0
    {{build_dir}}/run_tests --no-build --iso "{{iso_tests}}" --fs-image "{{fs_image_tests}}" \
        --raw --no-color > {{build_dir}}/persist-boot2.log 2>&1 || rc=$?
    tail -n 40 {{build_dir}}/persist-boot2.log
    if [ "$rc" -eq 0 ] && grep -q "PERSIST: verified" {{build_dir}}/persist-boot2.log; then
        echo "PASS: the payload survived the reboot"
    else
        echo "FAIL: the payload did not survive the reboot (boot 2 exit $rc) — full log in {{build_dir}}/persist-boot2.log" >&2
        exit 1
    fi

# The ISO is built in the body, not as a dependency, so it carries this
# cmdline: the test runs only when named exactly, since it ends the machine.
[doc("Rude-exit check: a boot fsyncs a file into the root's journal and dies holding it; the host's e2fsck must replay it")]
test-rude-exit: _build-run-tests
    #!/usr/bin/env bash
    set -euo pipefail
    TEST_CMDLINE="{{test_cmdline}} tests.run=slopos_fs::tests::rude_exit::test_ext4_rude_exit" just _iso-tests
    # `set -e` must not abort: the boot ends mid-run on purpose.
    {{build_dir}}/run_tests --no-build --iso "{{iso_tests}}" --fs-image "{{fs_image_tests}}" \
        --raw --no-color > {{build_dir}}/rude-exit.log 2>&1 || true
    if ! grep -q "RUDE_EXIT: committed" {{build_dir}}/rude-exit.log; then
        tail -n 40 {{build_dir}}/rude-exit.log
        echo "FAIL: the boot never committed its file — full log in {{build_dir}}/rude-exit.log" >&2
        exit 1
    fi
    printf 'slopos-rude-exit-v1\n' > {{build_dir}}/rude-exit.payload
    scripts/check_fs_replay.sh "{{fs_image_tests}}" /rude-exit {{build_dir}}/rude-exit.payload

# The capacity check: one boot with a 16 GiB volume attached as nvme0n3,
# which the suite mounts, measures and grades. Separate from `just test`
# because the image takes minutes to build once and is then preserved; the
# per-run ratchet that CI does grade lives in `check_fs_throughput.sh`.
[doc("Capacity check: mount and write a 16 GiB volume, then grade the mount and write cost")]
test-capacity: _build-run-tests _fs-image-capacity
    #!/usr/bin/env bash
    set -euo pipefail
    TEST_CMDLINE="{{test_cmdline}} tests.run=*ext2_aaa*,*capacity*" just _iso-tests
    # The silence guard is the wrong instrument here: the capacity fill is one
    # test that legitimately runs for minutes emitting nothing, and a host
    # still flushing 1.35 GB of freshly written image makes the guest's
    # fsync-per-create workload queue behind it. Silence is not a hang.
    rc=0
    CAPACITY_IMG="$PWD/{{fs_image_capacity}}" QEMU_MEM="${QEMU_MEM:-{{capacity_qemu_mem}}}" \
        {{build_dir}}/run_tests --no-build --iso "{{iso_tests}}" --fs-image "{{fs_image_tests}}" \
        --silence-secs 900 --timeout-secs 1800 \
        --raw --no-color > {{build_dir}}/capacity.log 2>&1 || rc=$?
    tail -n 30 {{build_dir}}/capacity.log
    [ "$rc" -eq 0 ] || { echo "FAIL: the capacity boot exited $rc — full log in {{build_dir}}/capacity.log" >&2; exit 1; }
    scripts/check_fs_throughput.sh --log {{build_dir}}/capacity.log --require-capacity
    scripts/check_fs_image.sh "{{fs_image_capacity}}"

# Two boots of one image, as test-persist: the first climbs the ladder and
# leaves a clone on /, the second reads it back and climbs the ladder again.
# Separate from `just test`, which runs the same utests on a root with no
# toolchain and no clone, and they pass by saying so.
[doc("Toolchain check at 6G on the self-hosting root: hold the toolchain to its manifest and the clone to its vendored crates, climb the ladder (rustc, rustc+cc, cargo with a build script and a proc macro, cargo fetching a git dependency through libgit2 and a crate over HTTPS from a loopback sparse registry, clang, git reading the clone and reaching the host, git cloning GitHub, cargo resolving the lockfiles from crates.io over HTTPS, a bash script, a Ninja graph and a CMake project), and find a clone made on / intact after a power-off")]
test-toolchain: _build-run-tests
    #!/usr/bin/env bash
    set -euo pipefail
    just _fs-image-selfhost
    free="$(dumpe2fs -h "{{fs_image_selfhost}}" 2>/dev/null | awk -F: '/^Free blocks:/ {f=$2} /^Block size:/ {b=$2} END {print f * b}')"
    [ "$free" -ge "$(numfmt --from=iec {{root_free_floor}})" ] ||
        { echo "FAIL: the self-hosting root has ${free}B free, under the {{root_free_floor}} floor" >&2; exit 1; }
    TEST_CMDLINE="{{dev_test_cmdline}} {{dev_watchdog}} {{test_cmdline_extra}} tests.run=*ext2_aaa*,*toolchain*,*reboot_clone*" just _iso-tests
    push="$PWD/{{build_dir}}/toolchain-push.git"
    for boot in 1 2; do
        echo "── boot $boot ──"
        log="{{build_dir}}/toolchain-boot$boot.log"
        rm -rf "$push"
        rc=0
        GIT_PUSH_REPO="$push" QEMU_MEM="${QEMU_MEM:-{{dev_qemu_mem}}}" \
            {{build_dir}}/run_tests --no-build --iso "{{iso_tests}}" --fs-image "{{fs_image_selfhost}}" \
            --timeout-secs 3600 --silence-secs 1800 --raw --no-color >"$log" 2>&1 || rc=$?
        tail -n 30 "$log"
        [ "$rc" -eq 0 ] || { echo "FAIL: boot $boot exited $rc — full log in $log" >&2; exit 1; }
        scripts/check_fs_image.sh "{{fs_image_selfhost}}"
        # git rides the toolchain: without one there is no clone to grade.
        [ -d "{{toolchain_install}}" ] || exit 0
        grep -aq 'git_reads_the_clone_and_reaches_the_host # git status: clean' "$log" ||
            { echo "FAIL: the seeded clone is not clean in the guest — see $log" >&2; exit 1; }
    done
    written="$(grep -aoE 'a_clone_survives_a_reboot # CLONE-WRITTEN [0-9a-f]{40}' "{{build_dir}}/toolchain-boot1.log" | awk '{print $NF}')"
    survived="$(grep -aoE 'a_clone_survives_a_reboot # CLONE-SURVIVED [0-9a-f]{40}' "{{build_dir}}/toolchain-boot2.log" | awk '{print $NF}')"
    [ -n "$written" ] && [ "$written" = "$survived" ] ||
        { echo "FAIL: boot 1 committed '${written:-nothing}' in /home/clone and boot 2 found '${survived:-nothing}'" >&2; exit 1; }
    echo "test-toolchain: the ladder held on both boots, and the clone's commit $written survived the power-off"

[doc("Self-hosting check: the guest takes HEAD over git and builds the dev and tests systems — kernel, userland and base — on the self-hosting root; the host holds the root to e2fsck, runs the ELF gates on both kernels and the suite on the guest's tests kernel and base")]
test-selfhost: _build-run-tests
    #!/usr/bin/env bash
    set -euo pipefail
    [ -d "{{toolchain_install}}" ] || { echo "FAIL: no toolchain at {{toolchain_install}} — run just toolchain" >&2; exit 1; }
    git diff --quiet HEAD || { echo "FAIL: the guest builds HEAD and the host grades it with the working tree's gates and tests; commit or stash first" >&2; exit 1; }
    head="$(git rev-parse HEAD)"
    just _fs-image-selfhost
    # The machine running the build boots the optimized tests kernel: a
    # dev-profile one spends ten times as long in every syscall and fault.
    KERNEL_RELEASE=1 TEST_CMDLINE="{{dev_test_cmdline}} {{dev_watchdog}} {{test_cmdline_extra}} tests.run=*ext2_aaa*,*selfhost*" just _iso-tests
    rc=0
    push="$PWD/{{build_dir}}/selfhost-push.git"
    rm -rf "$push"
    GIT_PUSH_REPO="$push" QEMU_MEM="${QEMU_MEM:-{{dev_qemu_mem}}}" \
        {{build_dir}}/run_tests --no-build --iso "{{iso_tests}}" --fs-image "{{fs_image_selfhost}}" \
        --timeout-secs "${SELFHOST_TIMEOUT_SECS:-28800}" --silence-secs 0 --raw --no-color > {{build_dir}}/selfhost.log 2>&1 || rc=$?
    tail -n 30 {{build_dir}}/selfhost.log
    [ "$rc" -eq 0 ] || { echo "FAIL: the self-hosting boot exited $rc — full log in {{build_dir}}/selfhost.log" >&2; exit 1; }
    grep -aqF "guest_takes_the_host_head # SELFHOST-COMMIT $head" {{build_dir}}/selfhost.log ||
        { echo "FAIL: the guest did not build $head — see {{build_dir}}/selfhost.log" >&2; exit 1; }
    scripts/check_fs_image.sh "{{fs_image_selfhost}}"
    guest="{{build_dir}}/guest"
    mkdir -p "$guest"
    for variant in dev tests; do
        scripts/export_fs_file.sh "src/slopos/builddir/kernel-$variant.elf" \
            "{{fs_image_selfhost}}" "$guest/kernel-$variant.elf"
        scripts/check_kernel_elf_gates.sh "$guest" "$variant"
    done
    scripts/export_fs_file.sh src/slopos/builddir/initramfs-tests.cpio "{{fs_image_selfhost}}" "$guest/initramfs-tests.cpio"
    just test-elf "ELF=$guest/kernel-tests.elf" "BASE=$guest/initramfs-tests.cpio"

# The self-hosting build as a benchmark: the guest half of test-selfhost, with
# prof=on. No clean-tree check, so it runs on an uncommitted kernel; the guest
# builds HEAD. The libc.so the boot ran is kept beside the log: user ticks
# symbolize against the objects that took them.
[doc("Benchmark the guest's kernel build: boot the optimized tests kernel on the self-hosting root with prof=on, build the dev and tests kernels from clean, and summarize where the time went (builddir/bench-selfhost.log)")]
bench-selfhost: _build-run-tests
    #!/usr/bin/env bash
    set -euo pipefail
    [ -d "{{toolchain_install}}" ] || { echo "FAIL: no toolchain at {{toolchain_install}} — run just toolchain" >&2; exit 1; }
    just _fs-image-selfhost
    KERNEL_RELEASE=1 TEST_CMDLINE="{{dev_test_cmdline}} {{dev_watchdog}} ${BENCH_PROF-prof=on} {{test_cmdline_extra}} tests.run=*ext2_aaa*,*selfhost*" just _iso-tests
    cp {{build_dir}}/libc.so {{build_dir}}/bench-libc.so
    rc=0
    push="$PWD/{{build_dir}}/bench-push.git"
    rm -rf "$push"
    GIT_PUSH_REPO="$push" QEMU_MEM="${QEMU_MEM:-{{dev_qemu_mem}}}" \
        {{build_dir}}/run_tests --no-build --iso "{{iso_tests}}" --fs-image "{{fs_image_selfhost}}" \
        --timeout-secs "${SELFHOST_TIMEOUT_SECS:-28800}" --silence-secs 0 --raw --no-color > {{build_dir}}/bench-selfhost.log 2>&1 || rc=$?
    [ "$rc" -eq 0 ] || { tail -n 30 {{build_dir}}/bench-selfhost.log; echo "FAIL: the benchmark boot exited $rc — full log in {{build_dir}}/bench-selfhost.log" >&2; exit 1; }
    python3 scripts/prof_report.py {{build_dir}}/bench-selfhost.log --libc {{build_dir}}/bench-libc.so --lib-dir {{toolchain_install}}/lib

[doc("Run host-side unit tests: abi, gfx, font, keymap-core, terminal-core, shell-core, editor-core, net-core, nvme-core, ext4-core, http-core, fat-core, boot-core, tree-core, tls-core, chrome-core, slibc-core, kallsyms, initramfs, plus the slopos-ostd suite natively (same tests KernMiri interprets, seconds instead of minutes — catches assertion drift early; UB detection still needs `just check-miri`)")]
test-host:
    {{cargo}} +{{rust_channel}} test -p slopos-abi -p slopos-gfx -p slopos-font -p slopos-keymap-core -p slopos-terminal-core -p slopos-shell-core -p slopos-editor-core -p slopos-net-core -p slopos-nvme-core -p slopos-ext4-core -p slopos-http-core -p slopos-fat-core -p slopos-boot-core -p slopos-tree-core -p slopos-tls-core -p slopos-chrome-core -p slopos-slibc-core -p slopos-ostd -p slopos-kallsyms -p slopos-initramfs

[doc("Run the Go-based wrapper's own unit tests (host-side, no QEMU)")]
check-tests-host:
    cd tools/run_tests && go test ./...

[doc("Count-regression guard: assert `just test` plans at least TEST_COUNT_BASELINE tests")]
check-test-count: _build-run-tests
    scripts/check_test_count.sh

[doc("Hold an image a boot wrote to e2fsck -fn and to being at rest: clean, no journal to replay")]
check-fs-image *ARGS:
    scripts/check_fs_image.sh {{ARGS}}

[doc("SMP gate: assert every online CPU is eligible for task placement")]
check-sched-spread *ARGS:
    scripts/check_sched_spread.sh {{ARGS}}

[doc("Lockdep ratchet: assert the validator boots ACTIVE and no pool nears its ceiling")]
check-lockdep-headroom: _build-run-tests
    scripts/check_lockdep_headroom.sh

[doc("Quota ratchet: assert every account's peak stays under its measured cap and nothing was denied")]
check-quota-headroom: _build-run-tests
    scripts/check_quota_headroom.sh

[doc("Filesystem cost ratchet: assert a write still costs a bounded number of transactions and device requests per MiB")]
check-fs-throughput: _build-run-tests
    scripts/check_fs_throughput.sh

# Two passes: `TaskOwnCell::get_ptr` hands out `*mut T` rather than `&mut T` so
# two witnesses for one task may hold live pointers into the same field, and
# whether that is legal is a raw-pointer retagging question — exactly where
# Stacked and Tree Borrows differ.
#
# Four processes, because Miri interprets every thread of a process on one core:
# libtest's thread pool buys nothing here, so two invocations back to back leave
# the machine idle. cargo serialises the four builds on the target-directory
# lock and releases it before running the tests, so they share one target dir.
# `cargo miri nextest run` shards per *test* instead, which is measurably worse:
# a Miri process costs about a second to start and there are 643 of them.
#
# Naming targets excludes the doctests. OSTD's are `ignore` or `compile_fail` —
# claims about the compiler, not about the machine — and `just test-host` runs
# them.
[doc("Run slopos-ostd unit + integration tests under Miri to detect UB in the OSTD critical path, under both Stacked and Tree Borrows. See tools/kernmiri/README.md.")]
check-miri:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p {{build_dir}}
    rustup component list --installed --toolchain {{rust_channel}} 2>/dev/null | grep -q '^miri' \
        || rustup component add miri --toolchain {{rust_channel}}
    {{cargo}} +{{rust_channel}} miri setup
    pids=(); tags=()
    for model in stacked tree; do
        flags="-Zmiri-ignore-leaks"
        if [ "$model" = tree ]; then
            flags="$flags -Zmiri-tree-borrows"
        fi
        for shard in lib tests; do
            if [ "$shard" = lib ]; then
                sel=(--lib)
            else
                sel=(--test '*')
            fi
            tag="$model-$shard"
            MIRIFLAGS="$flags" {{cargo}} +{{rust_channel}} miri test -p slopos-ostd \
                "${sel[@]}" --no-fail-fast \
                >"{{build_dir}}/kernmiri-$tag.log" 2>&1 &
            pids+=($!); tags+=("$tag")
        done
    done
    rc=0
    for i in "${!pids[@]}"; do
        if wait "${pids[$i]}"; then
            echo "── KernMiri ${tags[$i]}: ok ──"
        else
            echo "── KernMiri ${tags[$i]}: FAILED ({{build_dir}}/kernmiri-${tags[$i]}.log) ──" >&2
            sed -n '/^failures:/,$p' "{{build_dir}}/kernmiri-${tags[$i]}.log" >&2
            rc=1
        fi
    done
    exit "$rc"

[doc("Print TCB ratio: unsafe lines in slopos-ostd / total kernel Rust LoC (target Phase 1 <= 1.5%, Phase 2 <= 1.0%)")]
tcb-ratio:
    scripts/tcb_ratio.sh

[doc("Download + pin the Verus toolchain (verification/verus.toml) under third_party/verus")]
ensure-verus:
    scripts/ensure_verus.sh >/dev/null

[doc("Materialise third_party/vendor: every crates.io package the workspace and -Zbuild-std compile, pinned by the two lockfiles")]
vendor:
    scripts/make_vendor.sh

[doc("Hold the tree to building with no registry: the vendored directory against both lockfiles, then a kernel and userland check from an empty CARGO_HOME, offline")]
check-offline-build: vendor
    scripts/check_offline_build.sh --require

[doc("Materialise the pinned rustc source tree with the SlopOS target patch under third_party/slopos-rustc-src (fetches 265 MB)")]
rustc-src:
    scripts/make_rustc_src.sh

[doc("Cross-build the Rust toolchain that runs on SlopOS. Hours of CPU; `just check-bootstrap-config` is the affordable half. `--pgo` builds it as a Rust release is (ThinLTO, one codegen unit, PGO with the profiles `just toolchain-profile` gathers, made first when missing or stale).")]
toolchain *ARGS:
    scripts/bootstrap_slopos_toolchain.sh {{ARGS}}

[doc("Gather the PGO profiles for `just toolchain` from an instrumented Linux-hosted build of the same compiler running the kernel build (builddir/slopos-pgo). `--optimized-host` also builds that compiler with them, for timing a host kernel build; `--force` regenerates.")]
toolchain-profile *ARGS:
    scripts/make_toolchain_profile.sh {{ARGS}}

[doc("Build the recipes under toolchain/recipes/ (zlib, nghttp2, Mbed TLS, OpenSSL, curl, libssh2, libgit2, git) for SlopOS from their pinned tarballs into builddir/slopos-recipes/prefix; names build only those and what they depend on")]
recipes *NAMES: _build-userland-tests
    BUILD_DIR={{build_dir}} scripts/build_recipes.sh {{NAMES}}

[doc("Hold the cross-build plan and the compiler wrapper to the toolchain they claim to produce. Needs `just rustc-src` and a tests userland build first.")]
check-bootstrap-config:
    scripts/check_bootstrap_config.sh --require

[doc("Hold the built-in x86_64-unknown-slopos target to targets/x86_64-unknown-slopos.json and to rustc's own consistency test. Needs `just rustc-src` first.")]
check-rustc-target:
    scripts/check_rustc_target.sh --require

[doc("Materialise the pinned llvm-project sources with the SlopOS port applied under third_party/llvm-project-<version>.src (1.5 GB on disk)")]
llvm-src:
    scripts/make_slopos_llvm_src.sh

[doc("Compile LLVM's Support library for x86_64-unknown-slopos. Needs `just llvm-src` and a tests userland build first.")]
check-llvm-port:
    scripts/check_llvm_port.sh --require

[doc("Hold the port's clang driver to the link line build_userland.sh writes by hand. Needs `just llvm-src` first.")]
check-clang-driver:
    scripts/check_clang_driver.sh --require

[doc("Machine-check the OSTD critical-path proofs under verification/proofs/ on the pinned Verus toolchain. Pass a proof stem to verify one file.")]
verify FILTER='':
    scripts/verify.sh "{{FILTER}}"

[doc("Fail the build on any `async fn` in a kernel crate (AD-8/AD-9/R13). OSTD + all kernel services stay sync; async lives in userspace.")]
check-no-kernel-async:
    scripts/check_no_kernel_async.sh

# Single source of truth for the gate list: CI calls this recipe rather than
# duplicating it inline, because `check-framekernel` below also runs KernMiri
# and Verus, which are separate CI jobs.
[doc("Run the framekernel gate scripts only — no fmt, KernMiri, or Verus (requires a prior `just build`)")]
check-framekernel-gates:
    # Self-tests first: a gate whose patterns have rotted produces output nobody
    # can trust.
    scripts/check_unsafe_outside_ostd.sh --self-test
    scripts/check_alloc_dep.sh --self-test
    scripts/check_no_kernel_async.sh --self-test
    scripts/check_drop_panic_free.sh --self-test
    scripts/check_wait_predicate_purity.sh --self-test
    scripts/check_wait_result_handling.sh --self-test
    scripts/check_kernel_pml4_writer.sh --self-test
    scripts/check_task_ownership.sh --self-test
    scripts/check_process_designator.sh --self-test
    scripts/check_frame_ownership.sh --self-test
    scripts/check_stack_sizes.sh --self-test
    scripts/check_kernel_softfloat.sh --self-test
    scripts/check_registry_sections.sh --self-test
    scripts/check_bootstrap_stack_rewind.sh --self-test
    scripts/check_authority_reachability.sh --self-test
    scripts/check_lockdep_headroom.sh --self-test
    scripts/check_safe_contract_surface.sh --self-test
    scripts/check_charge_linearity.sh --self-test
    scripts/check_quota_headroom.sh --self-test
    scripts/check_sched_spread.sh --self-test
    scripts/check_fs_image.sh --self-test
    scripts/check_fs_replay.sh --self-test
    python3 scripts/fs_tree.py --self-test
    python3 scripts/gen_verity.py --self-test
    scripts/check_fs_throughput.sh --self-test
    scripts/check_syscall_abi.sh --self-test
    scripts/check_toolchain_pin.sh --self-test
    scripts/check_rustc_target.sh --self-test
    scripts/check_cxx_pin.sh --self-test
    scripts/check_llvm_port.sh --self-test
    scripts/check_clang_driver.sh --self-test
    scripts/check_bootstrap_config.sh --self-test
    scripts/check_recipes.sh --self-test
    scripts/check_libc_license.sh --self-test
    scripts/check_codegen_backend.sh --self-test
    scripts/check_linker_script.sh --self-test
    scripts/check_offline_build.sh --self-test
    scripts/check_vendor_pin.sh
    scripts/check_offline_build.sh --pins-only
    scripts/check_toolchain_pin.sh
    scripts/check_rustc_target.sh
    scripts/check_cxx_pin.sh
    scripts/check_llvm_port.sh
    scripts/check_clang_driver.sh
    scripts/check_bootstrap_config.sh
    scripts/check_recipes.sh
    scripts/check_libc_license.sh
    scripts/check_unsafe_outside_ostd.sh
    scripts/check_unsafe_expansion.sh
    scripts/check_no_kernel_async.sh
    scripts/check_alloc_dep.sh
    scripts/check_drop_panic_free.sh
    scripts/check_stack_sizes.sh --variant dev {{build_dir}}/kernel-dev.elf
    scripts/check_kernel_softfloat.sh --variant dev {{build_dir}}/kernel-dev.elf
    scripts/check_registry_sections.sh {{build_dir}}/kernel-dev.elf
    scripts/check_bootstrap_stack_rewind.sh --variant dev {{build_dir}}/kernel-dev.elf
    scripts/check_authority_reachability.sh --variant dev {{build_dir}}/kernel-dev.elf
    scripts/check_wait_predicate_purity.sh
    scripts/check_wait_result_handling.sh
    scripts/check_task_ownership.sh
    scripts/check_process_designator.sh
    scripts/check_frame_ownership.sh
    scripts/check_safe_contract_surface.sh
    scripts/check_charge_linearity.sh
    scripts/check_syscall_abi.sh
    just check-toolchain-coverage
    scripts/tcb_ratio.sh --max 1.0

[doc("Hold every codegen backend and linker to what targets/x86_64-slos.json and link.ld need. `llvm`/`lld` are the shipping pair; `cranelift`/`wild` skip when not installed.")]
check-toolchain-coverage:
    scripts/check_codegen_backend.sh --backend llvm
    scripts/check_codegen_backend.sh --backend cranelift
    scripts/check_linker_script.sh --linker lld
    scripts/check_linker_script.sh --linker wild

# TODO(tech-debt): no `cargo clippy -- -D warnings` gate here — there is no
# clippy config in tree and the custom `no_std` target needs plumbing first.
[doc("Run every framekernel-discipline gate: vendor pin / toolchain pin / unsafe source + expansion / async / alloc / Drop / stack / registry sections / task ownership / TCB ratio / fmt / KernMiri / Verus (requires a prior `just build`)")]
check-framekernel: check-framekernel-gates
    {{cargo}} +{{rust_channel}} fmt --all -- --check
    just check-miri
    just verify

[doc("Show detected QEMU framebuffer resolution")]
show-qemu-resolution:
    #!/usr/bin/env bash
    set -euo pipefail
    detected="$(QEMU_FB_WIDTH={{qemu_fb_width}} QEMU_FB_HEIGHT={{qemu_fb_height}} \
        QEMU_FB_AUTO_POLICY={{qemu_fb_auto_policy}} \
        QEMU_FB_AUTO_OUTPUT="{{qemu_fb_auto_output}}" \
        scripts/detect_qemu_resolution.sh)"
    w="${detected%% *}"
    h="${detected##* }"
    echo "Configured framebuffer mode: ${w} x ${h}"
    if [ "{{qemu_fb_auto}}" = "0" ]; then
        echo "Auto-detection disabled (QEMU_FB_AUTO=0)."
    fi

[doc("Check formatting")]
fmt:
    {{cargo}} +{{rust_channel}} fmt --all -- --check

[doc("Enforce kernel allocation + stack-frame invariants against the dev kernel ELF")]
check:
    scripts/check_alloc_dep.sh
    scripts/check_stack_sizes.sh --variant dev {{build_dir}}/kernel-dev.elf

[doc("Heuristic audit: kernel `pub fn` returning large by-value types — slow, not part of `check`")]
check-return-types:
    scripts/check_return_types.sh

[doc("Audit kernel ELF for functions whose stack frame exceeds the 32 KiB task-stack budget")]
stack-audit:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ ! -f "{{kernel_elf}}" ]; then
        echo "{{kernel_elf}} missing — run \`just build\` first" >&2
        exit 1
    fi
    # Anything above 8 KiB eats into the call-depth budget on a 32 KiB task stack.
    THRESHOLD="${THRESHOLD:-8192}"
    echo "Kernel functions with frame > ${THRESHOLD} bytes:"
    OBJDUMP="$(scripts/llvm_tool.sh llvm-objdump)"
    "$OBJDUMP" -d --no-show-raw-insn --x86-asm-syntax=intel \
        "{{kernel_elf}}" \
      | awk -v t="${THRESHOLD}" '
          /^ffffffff[0-9a-f]+ <.*>:/ { fn=$0 }
          /sub[[:space:]]+rsp,0x/ {
              m=$0; sub(/.*sub[[:space:]]+rsp,0x/,"",m); v=strtonum("0x" m);
              if (v>t) printf "%8d  %s\n", v, fn;
          }' \
      | sort -rn

[doc("Clean build artifacts")]
clean:
    #!/usr/bin/env bash
    set -euo pipefail
    # cargo refuses an explicit --target-dir with no CACHEDIR.TAG, and writes one
    # only for a directory it created itself.
    tag="{{cargo_target_dir}}/CACHEDIR.TAG"
    if [ -d "{{cargo_target_dir}}" ]; then
        rm -f "$tag"
        printf '%s\n' \
            'Signature: 8a477f597d28d172789f06886806bc55' \
            '# This file is a cache directory tag created by cargo.' \
            '# For information about cache directory tags see https://bford.info/cachedir/' \
            > "$tag"
    fi
    {{cargo}} +{{rust_channel}} clean --target-dir {{cargo_target_dir}}
    rm -f {{build_dir}}/kernel-*.elf
    rm -rf {{build_dir}}/gates/codegen-probe {{build_dir}}/gates/rustc-target-probe-* {{build_dir}}/gates/rustc-target-test {{build_dir}}/gates/llvm-port {{build_dir}}/gates/clang-driver {{build_dir}}/gates/bootstrap-config {{build_dir}}/gates/offline-build

[doc("Full clean including ISOs, images, and logs")]
distclean: clean
    rm -rf {{build_dir}} {{iso}} {{iso_tests}} {{log_file}}
    rm -f {{fs_image}} {{fs_image_tests}} {{initramfs}} {{initramfs_tests}}
    rm -rf third_party/llvm-project-*.src third_party/slopos-rustc-src third_party/vendor
