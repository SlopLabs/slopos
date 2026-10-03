#!/usr/bin/env bash
set -euo pipefail

# Run SlopOS in QEMU with mode-specific configuration.
#
# Usage: qemu_run.sh <mode> <iso> <fs_image>   (<iso> is ignored with BOOT_DISK_IMG)
#
#   mode: interactive - Full interactive boot (Ctrl+C to exit)
#         logged      - Headless boot with timeout, logs to file
#         test        - Test harness with exit-code interpretation
#
# Environment (all optional, sensible defaults provided):
#   QEMU_BIN, QEMU_SMP, QEMU_MEM, QEMU_ACCEL,
#   VIDEO, QEMU_DISPLAY,
#   QEMU_FB_WIDTH, QEMU_FB_HEIGHT, QEMU_FB_AUTO,
#   QEMU_FB_AUTO_POLICY, QEMU_FB_AUTO_OUTPUT,
#   QEMU_GTK_ZOOM_TO_FIT,
#   QEMU_ENABLE_ISA_EXIT, QEMU_PCI_DEVICES,
#   OVMF_DIR, OVMF_VARS_FILE,
#   BOOT_DISK_IMG, INSTALL_STICK, QEMU_ALLOW_REBOOT, QEMU_TEST_DISKS,
#   NET, NET_PORTS,
#   ECHO_PEER_ADDR, ECHO_PEER_PORT, ECHO_PEER_CMD,
#   GIT_PUSH_REPO, GIT_SSH_PEER,
#   BOOT_LOG_TIMEOUT, LOG_FILE

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/ext4.sh
. "$SCRIPT_DIR/lib/ext4.sh"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

MODE="${1:?Usage: qemu_run.sh <interactive|logged|test> <iso> <fs_image>}"
ISO="${2:?Usage: qemu_run.sh <mode> <iso> <fs_image>}"
FS_IMAGE="${3:?Usage: qemu_run.sh <mode> <iso> <fs_image>}"

# ── Configuration with defaults ──────────────────────────────────────────────
QEMU_BIN="${QEMU_BIN:-qemu-system-x86_64}"
QEMU_SMP="${QEMU_SMP:-4}"
# The suite needs more than an interactive boot: `bigprog_test` holds ~170 MiB
# resident to cross the threshold above which `fork`'s PTE snapshot used to ask
# the 1 MiB slab for one allocation and panic, and it runs beside a 74 MB
# kernel and a 39 MB initramfs.
if [ "$MODE" = "test" ]; then
    QEMU_MEM="${QEMU_MEM:-1G}"
else
    QEMU_MEM="${QEMU_MEM:-512M}"
fi

# Platform-aware acceleration and CPU model defaults
if [ "$(uname -s)" = "Darwin" ]; then
    QEMU_ACCEL="${QEMU_ACCEL:-hvf:tcg}"
    QEMU_DISPLAY="${QEMU_DISPLAY:-cocoa}"
    QEMU_CPU="${QEMU_CPU:-host}"
else
    QEMU_ACCEL="${QEMU_ACCEL:-kvm:tcg}"
    QEMU_DISPLAY="${QEMU_DISPLAY:-auto}"
    QEMU_CPU="${QEMU_CPU:-host}"
fi

# ── Auto-detect KVM and fix CPU model ────────────────────────────────────────
# -cpu host requires KVM (or HVF on macOS). When the hypervisor is not
# available QEMU falls back to TCG, but -cpu host is incompatible with TCG
# and causes an immediate exit — which the test harness misreads as "pass".
# Detect this and switch to -cpu max (TCG's full-feature model) instead.
needs_tcg_cpu=0
if [ "$QEMU_CPU" = "host" ]; then
    case "$QEMU_ACCEL" in
        tcg) needs_tcg_cpu=1 ;;  # explicit TCG-only — host won't work
    esac
    if [ "$needs_tcg_cpu" = "0" ]; then
        case "$(uname -s)" in
            Linux)
                if [ ! -c /dev/kvm ] || [ ! -r /dev/kvm ] || [ ! -w /dev/kvm ]; then
                    needs_tcg_cpu=1
                fi
                ;;
            Darwin)
                if ! "$QEMU_BIN" -accel help 2>/dev/null | grep -q hvf; then
                    needs_tcg_cpu=1
                fi
                ;;
        esac
    fi
    if [ "$needs_tcg_cpu" = "1" ]; then
        QEMU_CPU="max"
        QEMU_ACCEL="tcg"
        echo "No hardware acceleration — using TCG with -cpu max" >&2
    fi
fi

VIDEO="${VIDEO:-0}"
QEMU_FB_WIDTH="${QEMU_FB_WIDTH:-1920}"
QEMU_FB_HEIGHT="${QEMU_FB_HEIGHT:-1080}"
QEMU_FB_AUTO="${QEMU_FB_AUTO:-1}"
QEMU_FB_AUTO_POLICY="${QEMU_FB_AUTO_POLICY:-primary}"
QEMU_FB_AUTO_OUTPUT="${QEMU_FB_AUTO_OUTPUT:-}"
QEMU_FB_DETECT_SCRIPT="${QEMU_FB_DETECT_SCRIPT:-${SCRIPT_DIR}/detect_qemu_resolution.sh}"
QEMU_GTK_ZOOM_TO_FIT="${QEMU_GTK_ZOOM_TO_FIT:-off}"
QEMU_ENABLE_ISA_EXIT="${QEMU_ENABLE_ISA_EXIT:-0}"
QEMU_PCI_DEVICES="${QEMU_PCI_DEVICES:-}"

NET="${NET:-0}"
NET_PORTS="${NET_PORTS:-7777,8080,8081}"

# Must stay inside 10.0.2.0/24 and be neither the gateway (.2) nor the DNS stub
# (.3) — `slirp_add_exec` rejects all three — and outside the DHCP range, which
# starts at .15.
ECHO_PEER_ADDR="${ECHO_PEER_ADDR:-10.0.2.100}"
ECHO_PEER_PORT="${ECHO_PEER_PORT:-9999}"
ECHO_PEER_CMD="${ECHO_PEER_CMD:-/bin/cat}"

# A seeded clone names git://10.0.2.4/ and git://10.0.2.4:9419/, so the git
# peer's address is fixed. GIT_SSH_PEER is the host's half of the ladder's ssh
# fixture (scripts/stage_ladder_fixtures.sh), served on the same address.
GIT_PUSH_REPO="${GIT_PUSH_REPO:-}"
GIT_SSH_PEER="${GIT_SSH_PEER:-}"
GIT_PEER_ADDR="10.0.2.4"

OVMF_DIR="${OVMF_DIR:-${REPO_ROOT}/third_party/ovmf}"
OVMF_CODE="${OVMF_DIR}/OVMF_CODE.fd"
OVMF_VARS="${OVMF_DIR}/OVMF_VARS.fd"

BOOT_LOG_TIMEOUT="${BOOT_LOG_TIMEOUT:-15}"
LOG_FILE="${LOG_FILE:-test_output.log}"
LOG_FILE_RAW="${LOG_FILE}.raw"

run_with_timeout() {
    local seconds="$1"
    shift

    if command -v timeout >/dev/null 2>&1; then
        timeout "${seconds}s" "$@"
        return $?
    fi
    if command -v gtimeout >/dev/null 2>&1; then
        gtimeout "${seconds}s" "$@"
        return $?
    fi

    local timeout_dir marker
    timeout_dir="$(mktemp -d)"
    marker="${timeout_dir}/timed_out"

    "$@" &
    local child_pid=$!
    (
        sleep "$seconds"
        if kill -0 "$child_pid" 2>/dev/null; then
            touch "$marker"
            kill -TERM "$child_pid" 2>/dev/null || true
            sleep 2
            kill -KILL "$child_pid" 2>/dev/null || true
        fi
    ) &
    local watchdog_pid=$!

    wait "$child_pid"
    local status=$?

    kill "$watchdog_pid" 2>/dev/null || true
    wait "$watchdog_pid" 2>/dev/null || true

    if [ -f "$marker" ]; then
        rm -rf "$timeout_dir"
        return 124
    fi

    rm -rf "$timeout_dir"
    return "$status"
}

# ── Validate SMP ─────────────────────────────────────────────────────────────
if [ "$QEMU_SMP" -lt 1 ]; then
    echo "QEMU_SMP must be >= 1" >&2
    exit 1
fi
if [ $(( QEMU_SMP & (QEMU_SMP - 1) )) -ne 0 ]; then
    echo "QEMU_SMP must be a power of 2 (got $QEMU_SMP)" >&2
    exit 1
fi

# INSTALL_STICK: the ISO on a USB stick, as the installer's medium is flashed,
# which the firmware reads and the kernel, without a USB driver, does not. The
# boot disk is then the disk it installs onto, booted after it.
INSTALL_STICK="${INSTALL_STICK:-}"
NV_FIRMWARE=0
if [ -n "${BOOT_DISK_IMG:-}" ] || [ -n "$INSTALL_STICK" ]; then
    NV_FIRMWARE=1
fi

# ── Ensure OVMF firmware ─────────────────────────────────────────────────────
if [ "$NV_FIRMWARE" = 1 ]; then
    "$SCRIPT_DIR/setup_ovmf.sh" nv
else
    "$SCRIPT_DIR/setup_ovmf.sh"
fi

# ── Check boot medium exists ─────────────────────────────────────────────────
if [ -n "$INSTALL_STICK" ] && [ ! -f "$INSTALL_STICK" ]; then
    echo "Install stick not found at $INSTALL_STICK" >&2
    exit 1
fi
if [ -n "${BOOT_DISK_IMG:-}" ]; then
    if [ ! -f "$BOOT_DISK_IMG" ]; then
        echo "Boot disk not found at $BOOT_DISK_IMG" >&2
        exit 1
    fi
elif [ ! -f "$ISO" ]; then
    echo "ISO not found at $ISO" >&2
    exit 1
fi

# A boot disk selects its next boot through UEFI variables, which only the
# varstore-writing firmware keeps across a reset (see setup_ovmf.sh).
SECURE_PFLASH=1
if [ "$NV_FIRMWARE" = 1 ]; then
    OVMF_DIR="${OVMF_NV_DIR:-${REPO_ROOT}/third_party/ovmf-nv}"
    OVMF_CODE="${OVMF_DIR}/OVMF_CODE.fd"
    OVMF_VARS="${OVMF_DIR}/OVMF_VARS.fd"
    SECURE_PFLASH=0
fi

# ── Create runtime OVMF_VARS copy ────────────────────────────────────────────
# OVMF_VARS_FILE keeps the variables from one QEMU run to the next, as a
# machine's flash does across power cycles: made from the template when it is
# absent, and never removed here.
if [ -n "${OVMF_VARS_FILE:-}" ]; then
    [ -f "$OVMF_VARS_FILE" ] || cp "$OVMF_VARS" "$OVMF_VARS_FILE"
    OVMF_VARS_RUNTIME="$OVMF_VARS_FILE"
    cleanup() { :; }
else
    OVMF_VARS_RUNTIME="$(mktemp "${OVMF_DIR}/OVMF_VARS.runtime.XXXXXX")"
    cleanup() { rm -f "$OVMF_VARS_RUNTIME"; }
    cp "$OVMF_VARS" "$OVMF_VARS_RUNTIME"
fi
trap cleanup EXIT INT TERM

# ── Resolve framebuffer dimensions ───────────────────────────────────────────
fb_width="$QEMU_FB_WIDTH"
fb_height="$QEMU_FB_HEIGHT"
if [ "$QEMU_FB_AUTO" != "0" ] && [ "$VIDEO" != "0" ] && [ -x "$QEMU_FB_DETECT_SCRIPT" ]; then
    detected="$(QEMU_FB_WIDTH="$fb_width" QEMU_FB_HEIGHT="$fb_height" \
        QEMU_FB_AUTO_POLICY="$QEMU_FB_AUTO_POLICY" \
        QEMU_FB_AUTO_OUTPUT="$QEMU_FB_AUTO_OUTPUT" \
        "$QEMU_FB_DETECT_SCRIPT")" || true
    detected_w="${detected%% *}"
    detected_h="${detected##* }"
    if [ -n "${detected_w:-}" ] && [ -n "${detected_h:-}" ]; then
        fb_width="$detected_w"
        fb_height="$detected_h"
        echo "QEMU framebuffer auto-detected: ${fb_width} x ${fb_height}"
    fi
fi

# ── Detect available display backends ────────────────────────────────────────
HAS_SDL=0
HAS_COCOA=0
if $QEMU_BIN -display help 2>/dev/null | grep -q 'sdl'; then
    HAS_SDL=1
fi
if $QEMU_BIN -display help 2>/dev/null | grep -q 'cocoa'; then
    HAS_COCOA=1
fi

# ── Resolve display, serial, and extra args per mode ─────────────────────────
DISPLAY_ARGS=(-display none)
SERIAL_ARGS=(-serial stdio)
ADD_ISA_EXIT=0
ADD_NO_REBOOT=0
# Disks, as the guest names them. Every NVMe controller is probed ahead of
# every virtio one, in command-line order, so the root is disk0 (`root=auto`),
# and a namespace is named by its NSID, so an absent optional one renames
# nothing:
#   nvme0n1  the root filesystem image
#   nvme0n2  test mode: a blank scratch, recreated each run. Destructive block
#            tests target it, never the live root.
#   nvme0n3  CAPACITY_IMG, when it names an existing file: the large volume
#            the capacity check measures, opt-in and preserved.
#   nvme1n1  test mode: a labelled ext4 volume on 4096-byte logical blocks,
#            which the test command line mounts by label.
#   nvme1n2  test mode: a blank 4096-byte-block scratch.
#   nvme2n1  test mode: a controller of its own that a test shuts down.
#   nvme3n1  BOOT_DISK_IMG: the UEFI boot disk, in place of the ISO, on the
#            last controller: nvme1n1 outside test mode, and nvme0n1 with
#            neither a root disk nor QEMU_TEST_DISKS.
#   vda      test mode: the shipped verified image. The root is built
#            VERITY=off so the suite can write; without this no run would
#            exercise fs/src/verity.rs against a trailer a real device reports.
#   vdb      test mode: a blank virtio scratch, so virtio-blk stays graded.
ADD_SCRATCH_DISK=0
ADD_VERIFIED_DISK=0
ADD_CAPACITY_DISK=0
if [ -n "${CAPACITY_IMG:-}" ] && [ -f "$CAPACITY_IMG" ]; then
    ADD_CAPACITY_DISK=1
fi
ADD_MEDIA_DISK=0
ADD_BOOT_DISK=0
BOOT_ORDER=d
if [ -n "${BOOT_DISK_IMG:-}" ]; then
    ADD_BOOT_DISK=1
    BOOT_ORDER=c
fi

case "$MODE" in
    test)
        # `-nographic` historically combined "no GUI" with implicit
        # `-serial mon:stdio`. Combined with our explicit `-serial stdio`,
        # QEMU's chardev layer silently mirrors every UART byte to BOTH
        # the explicit and the implicit stdio backend — every kernel
        # klog line shows up TWICE on the host pipe, corrupting the
        # KTAP wire stream the test harness emits. Use the modern
        # `-display none` instead so only the explicit `-serial stdio`
        # backend is wired to COM1.
        DISPLAY_ARGS=(-display none)
        ADD_ISA_EXIT=1
        ADD_NO_REBOOT=1
        # QEMU_TEST_DISKS=0: the suite's own disks stay off a machine whose
        # disks a test lays out itself.
        if [[ ! "${QEMU_TEST_DISKS:-1}" =~ ^(0|false|off|no)$ ]]; then
            SCRATCH_DIR="${SCRATCH_DIR:-${REPO_ROOT}/builddir}"
            mkdir -p "$SCRATCH_DIR"
            # Fresh, blank 8 MiB raw scratches each run (no filesystem; raw-block tests only).
            for scratch in scratch-nvme scratch-4kn scratch-virtio scratch-spare; do
                rm -f "$SCRATCH_DIR/$scratch.img"
                truncate -s 8M "$SCRATCH_DIR/$scratch.img"
            done
            ADD_SCRATCH_DISK=1
            MEDIA_IMG="${REPO_ROOT}/builddir/media-disk.img"
            media_stage="$(mktemp -d)"
            echo "slopos-media: the test harness's labelled volume" >"$media_stage/SLOPOS-MEDIA"
            rm -f "$MEDIA_IMG"
            truncate -s 16M "$MEDIA_IMG"
            ext4_mkfs_args
            mke2fs -F -q "${EXT4_MKFS_ARGS[@]}" -L slopos-media -d "$media_stage" "$MEDIA_IMG" ||
                { rm -rf "$media_stage"; exit 1; }
            rm -rf "$media_stage"
            ADD_MEDIA_DISK=1
            VERIFIED_IMG="${VERIFIED_IMG:-${REPO_ROOT}/fs/assets/ext2.img}"
            if [ -f "$VERIFIED_IMG" ]; then
                ADD_VERIFIED_DISK=1
            else
                echo "qemu_run: no verified image at $VERIFIED_IMG — the verity artifact test will report it absent" >&2
            fi
        fi
        ;;
    interactive|logged)
        if [ "$QEMU_ENABLE_ISA_EXIT" != "0" ]; then
            ADD_ISA_EXIT=1
        fi
        if [ "$VIDEO" != "0" ]; then
            if [ "$QEMU_DISPLAY" = "cocoa" ] && [ "$HAS_COCOA" = "1" ]; then
                DISPLAY_ARGS=(-display cocoa)
            elif [ "$QEMU_DISPLAY" = "sdl" ]; then
                DISPLAY_ARGS=(-display "sdl,grab-mod=lctrl-lalt")
            elif [ "$QEMU_DISPLAY" = "gtk" ]; then
                DISPLAY_ARGS=(-display "gtk,grab-on-hover=on,zoom-to-fit=$QEMU_GTK_ZOOM_TO_FIT")
            elif [ "$HAS_COCOA" = "1" ]; then
                DISPLAY_ARGS=(-display cocoa)
            elif [ "${XDG_SESSION_TYPE:-x11}" = "wayland" ] && [ "$HAS_SDL" = "1" ]; then
                DISPLAY_ARGS=(-display "sdl,grab-mod=lctrl-lalt")
            else
                DISPLAY_ARGS=(-display "gtk,grab-on-hover=on,zoom-to-fit=$QEMU_GTK_ZOOM_TO_FIT")
            fi
        fi
        if [ "$MODE" = "logged" ]; then
            SERIAL_ARGS=(-serial "file:${LOG_FILE_RAW}")
        fi
        ;;
    *)
        echo "Unknown mode: $MODE (expected: interactive, logged, test)" >&2
        exit 1
        ;;
esac

# A guest reboot then resets the machine inside this QEMU process, so the
# OVMF vars copy — and a `LoaderEntryOneShot` the guest set — survives it.
if [[ "${QEMU_ALLOW_REBOOT:-0}" =~ ^(1|true|on|yes)$ ]]; then
    ADD_NO_REBOOT=0
fi

# ── Display device selection ─────────────────────────────────────────────────
# GPU selects the emulated display adapter SlopOS drives:
#   virtio-vga (default) — virtio-gpu plus a VGA-compatible linear framebuffer.
#                          The firmware framebuffer is present from power-on, so
#                          the boot console (splash + ESC log) is live through
#                          early boot; the kernel driver then upgrades scanout
#                          to virtio-gpu (the image is copied across).
#   virtio-gpu-pci       — pure virtio-gpu; NO early framebuffer (OVMF's GOP for
#                          it dies at ExitBootServices), so nothing renders on
#                          screen until the driver probes at PCI init.
#   vga                  — plain stdvga (passive framebuffer only). Pair with
#                          BOOT_CMDLINE='video=framebuffer'.
# A device the kernel can't instantiate falls back to stdvga (still an early
# framebuffer). Applies to every mode, including headless test.
GPU="${GPU:-virtio-vga}"

# Plain stdvga (passive framebuffer): VRAM sized to the resolution (2x
# headroom) and rounded up to a power of two as QEMU's stdvga requires.
stdvga_args() {
    local fb_vram_bytes vgamem_mb p
    fb_vram_bytes=$((fb_width * fb_height * 4 * 2))
    vgamem_mb=$(( (fb_vram_bytes + 1048575) / 1048576 ))
    if [ "$vgamem_mb" -lt 16 ]; then vgamem_mb=16; fi
    p=1; while [ "$p" -lt "$vgamem_mb" ]; do p=$((p * 2)); done
    vgamem_mb=$p
    VIDEO_ARGS=(-vga none -device "VGA,edid=on,xres=${fb_width},yres=${fb_height},vgamem_mb=${vgamem_mb}")
}

# True only if the named display device both EXISTS and INITIALIZES without
# crashing. A grep of `-device help` is not enough: a virtio-gpu PCI module
# installed without its base `virtio-gpu-device` module is listed but segfaults
# at instance_init (the child type is unregistered). Spin up a paused throwaway
# VM (`-S` halts before guest code, after device realize) and check it survives.
gpu_device_usable() {
    local dev="$1"
    "$QEMU_BIN" -device help 2>/dev/null | grep -q "\"$dev\"" || return 1
    "$QEMU_BIN" -machine q35 -nodefaults -no-user-config -display none \
        -serial null -S -device "$dev" >/dev/null 2>&1 &
    local pid=$!
    sleep 0.5
    if kill -0 "$pid" 2>/dev/null; then
        kill "$pid" 2>/dev/null
        wait "$pid" 2>/dev/null
        return 0
    fi
    wait "$pid" 2>/dev/null
    return 1
}

# Build VIDEO_ARGS for a virtio display device, falling back to stdvga (the
# kernel then sees no virtio-gpu PCI device and stays on the passive
# framebuffer) when this QEMU lacks or can't instantiate it.
virtio_display_args() {
    local dev="$1"
    if gpu_device_usable "$dev"; then
        VIDEO_ARGS=(-vga none -device "${dev},edid=on,xres=${fb_width},yres=${fb_height}")
    else
        echo "warning: this QEMU can't instantiate '${dev}' (missing or broken module);" >&2
        echo "         falling back to stdvga. SlopOS runs on the passive framebuffer." >&2
        echo "         On Arch/CachyOS: pacman -S qemu-hw-display-virtio-gpu qemu-hw-display-virtio-gpu-pci" >&2
        stdvga_args
    fi
}

case "$GPU" in
    vga|std|stdvga)
        stdvga_args
        ;;
    virtio-vga)
        virtio_display_args "virtio-vga"
        ;;
    virtio-gpu-pci|virtio|virtio-gpu)
        virtio_display_args "virtio-gpu-pci"
        ;;
    *)
        echo "Unknown GPU=$GPU (expected: virtio-gpu-pci, virtio-vga, vga)" >&2
        exit 1
        ;;
esac

# Handle optional PCI devices
HAVE_PCI_ARGS=0
if [ -n "$QEMU_PCI_DEVICES" ]; then
    # Split space-separated PCI device strings into array elements
    HAVE_PCI_ARGS=1
    read -ra PCI_ARGS <<< "$QEMU_PCI_DEVICES"
fi

# ── Network port forwarding ────────────────────────────────────────────────
NET_HOSTFWD=""
if [[ "$NET" =~ ^(1|true|on|yes)$ ]]; then
    if [ -z "$NET_PORTS" ]; then
        echo "NET_PORTS must not be empty when NET is enabled" >&2
        exit 1
    fi
    IFS=',' read -ra NET_PORT_ARRAY <<< "$NET_PORTS"
    for _p in "${NET_PORT_ARRAY[@]}"; do
        if [[ "$_p" == *:* ]]; then
            # host:guest format
            _host="${_p%%:*}"
            _guest="${_p#*:}"
        else
            # single port: same on both sides
            _host="$_p"
            _guest="$_p"
        fi
        NET_HOSTFWD+=",hostfwd=tcp::${_host}-:${_guest}"
    done
    echo "Network port forwarding enabled: ${NET_PORTS}"
fi

# ── In-network TCP echo peer ────────────────────────────────────────────────
# SLIRP answers TCP on this address from inside the guest network, forking
# ECHO_PEER_CMD per connection with the socket on stdin/stdout, and ARPs for it
# like any host on the segment (libslirp's arp_input replies for every
# exec_list address). So the guest reaches it over eth0 through the ordinary
# route, neighbour and source-selection paths, with no egress off the host.
#
# Absence is fatal rather than silently dropped: a missing peer would present
# as the connection failures these tests exist to detect.
if [ ! -x "$ECHO_PEER_CMD" ]; then
    echo "qemu_run.sh: echo peer command '$ECHO_PEER_CMD' is not executable." >&2
    echo "  The userland network tests dial ${ECHO_PEER_ADDR}:${ECHO_PEER_PORT} and need it." >&2
    echo "  Override with ECHO_PEER_CMD=/path/to/responder." >&2
    exit 1
fi
NET_GUESTFWD=",guestfwd=tcp:${ECHO_PEER_ADDR}:${ECHO_PEER_PORT}-cmd:${ECHO_PEER_CMD}"

# ── Git peer ─────────────────────────────────────────────────────────────────
# The echo peer's mechanism: SLIRP runs one `git daemon --inetd` per
# connection, so nothing listens on the host. The guest fetches this checkout
# and pushes into GIT_PUSH_REPO, a bare repository made if absent. Each daemon
# serves one git directory: the path template covers a request that names a
# host, and the strict allowlist one that does not, which the template leaves
# alone. `--log-destination=none` keeps `--inetd` from logging every
# connection to the host's syslog.
NET_GITFWD=""
if [ -n "$GIT_PUSH_REPO" ]; then
    command -v git >/dev/null 2>&1 || { echo "qemu_run.sh: GIT_PUSH_REPO needs git on PATH" >&2; exit 1; }
    case "$GIT_PUSH_REPO" in
        /*) ;;
        *) echo "qemu_run.sh: GIT_PUSH_REPO=$GIT_PUSH_REPO is not an absolute path" >&2; exit 1 ;;
    esac
    [ -d "$GIT_PUSH_REPO" ] || git init -q --bare "$GIT_PUSH_REPO"
    checkout_dir="$(git -C "$REPO_ROOT" rev-parse --absolute-git-dir)" ||
        { echo "qemu_run.sh: the git peer serves $REPO_ROOT, which is not a git checkout" >&2; exit 1; }
    # A directory inside a repository resolves to that repository's.
    push_dir="$(cd "$GIT_PUSH_REPO" && pwd -P)"
    [ "$(git -C "$push_dir" rev-parse --is-bare-repository 2>/dev/null)" = true ] &&
        [ "$(git -C "$push_dir" rev-parse --absolute-git-dir)" = "$push_dir" ] ||
        { echo "qemu_run.sh: GIT_PUSH_REPO=$GIT_PUSH_REPO is not a bare repository of its own" >&2; exit 1; }
    # QEMU splits options on commas, libslirp parses the command as a shell
    # line, and the daemon expands `%` in its path template.
    for path in "$checkout_dir" "$push_dir"; do
        case "$path" in
            *[,%\'\"\\[:space:]]*) echo "qemu_run.sh: the git peer cannot serve the path $path" >&2; exit 1 ;;
        esac
    done
    git_daemon="git daemon --inetd --strict-paths --export-all --log-destination=none --init-timeout=30 --timeout=600"
    NET_GITFWD=",guestfwd=tcp:${GIT_PEER_ADDR}:9418-cmd:${git_daemon} --forbid-override=receive-pack --interpolated-path=${checkout_dir} ${checkout_dir}"
    NET_GITFWD+=",guestfwd=tcp:${GIT_PEER_ADDR}:9419-cmd:${git_daemon} --enable=receive-pack --interpolated-path=${push_dir} ${push_dir}"
    # libslirp hands the command the connection as stdin, stdout and stderr,
    # so `-E` keeps sshd's log out of the stream.
    if [ -n "$GIT_SSH_PEER" ]; then
        [ -f "$GIT_SSH_PEER/sshd_config" ] ||
            { echo "qemu_run.sh: GIT_SSH_PEER=$GIT_SSH_PEER holds no sshd_config" >&2; exit 1; }
        sshd_bin="$(command -v sshd || echo /usr/sbin/sshd)"
        [ -x "$sshd_bin" ] || { echo "qemu_run.sh: GIT_SSH_PEER needs sshd on PATH or at /usr/sbin/sshd" >&2; exit 1; }
        case "$GIT_SSH_PEER" in
            *[,%\'\"\\[:space:]]*) echo "qemu_run.sh: the ssh peer cannot run from $GIT_SSH_PEER" >&2; exit 1 ;;
        esac
        NET_GITFWD+=",guestfwd=tcp:${GIT_PEER_ADDR}:22-cmd:${sshd_bin} -i -E ${GIT_SSH_PEER}/sshd.log -f ${GIT_SSH_PEER}/sshd_config"
        NET_GITFWD+=" -o 'SetEnv=LADDER_CHECKOUT=${checkout_dir} LADDER_PUSH=${push_dir}'"
    fi
fi

# ── Debug-mode plumbing ─────────────────────────────────────────────────────
# Set QEMU_DEBUG=1 to enable the QEMU monitor on a Unix socket plus the GDB
# stub on TCP :1234. The monitor lets you run `info cpus`, `info registers`,
# `cpu N`, etc. when the system freezes; GDB gives backtraces per CPU.
#
#   Monitor:  socat - UNIX-CONNECT:/tmp/slopos-monitor.sock
#   GDB:      gdb builddir/kernel.elf -ex "target remote :1234"
if [ "${QEMU_DEBUG:-0}" != "0" ]; then
    rm -f /tmp/slopos-monitor.sock
    DEBUG_ARGS=(
        -monitor "unix:/tmp/slopos-monitor.sock,server,nowait"
        -s
    )
else
    DEBUG_ARGS=(-monitor none)
fi

# The root disk (nvme0n1). Omitted when QEMU_NO_ROOT_DISK=1: the live ISO
# boots purely from the Limine initramfs, and a boot disk carrying its own root
# partition needs no other.
if [[ ! "${QEMU_NO_ROOT_DISK:-0}" =~ ^(1|true|on|yes)$ ]]; then
    ADD_ROOT_DISK=1
else
    echo "QEMU_NO_ROOT_DISK=1 → no root disk at nvme0n1"
    ADD_ROOT_DISK=0
fi

# ── Assemble common QEMU arguments ──────────────────────────────────────────
QEMU_ARGS=(
    -machine "q35,accel=$QEMU_ACCEL"
    -cpu "$QEMU_CPU"
    -smp "$QEMU_SMP"
    -m "$QEMU_MEM"
    -drive "if=pflash,format=raw,unit=0,readonly=on,file=$OVMF_CODE"
    -drive "if=pflash,format=raw,unit=1,file=$OVMF_VARS_RUNTIME"
    -device "ich9-ahci,id=ahci0,bus=pcie.0,addr=0x3"
)
# The pinned nightly faults before any console unless the varstore pflash is
# secure: no serial, no display, indistinguishable from a dead kernel.
if [ "$SECURE_PFLASH" = "1" ]; then
    QEMU_ARGS+=(-global "driver=cfi.pflash01,property=secure,value=on")
fi
BOOT_DISK_INDEX=0
if [ -n "$INSTALL_STICK" ]; then
    QEMU_ARGS+=(
        -device "qemu-xhci,id=xhci"
        -drive "if=none,id=stick,format=raw,readonly=on,file=$INSTALL_STICK"
        -device "usb-storage,bus=xhci.0,drive=stick,removable=on,bootindex=0"
    )
    BOOT_DISK_INDEX=1
elif [ "$ADD_BOOT_DISK" = "0" ]; then
    QEMU_ARGS+=(
        -drive "if=none,id=cdrom,media=cdrom,readonly=on,file=$ISO"
        -device "ide-cd,bus=ahci0.0,drive=cdrom,bootindex=0"
    )
fi
if [ "$ADD_ROOT_DISK" = "1" ] || [ "$ADD_SCRATCH_DISK" = "1" ] || [ "$ADD_CAPACITY_DISK" = "1" ]; then
    QEMU_ARGS+=(-device "nvme,id=nvme0,serial=slopos-root")
fi
if [ "$ADD_ROOT_DISK" = "1" ]; then
    QEMU_ARGS+=(
        -drive "file=$FS_IMAGE,if=none,id=root-disk,format=raw"
        -device "nvme-ns,bus=nvme0,drive=root-disk,nsid=1"
    )
fi
if [ "$ADD_SCRATCH_DISK" = "1" ]; then
    QEMU_ARGS+=(
        -drive "file=$SCRATCH_DIR/scratch-nvme.img,if=none,id=scratch-disk,format=raw"
        -device "nvme-ns,bus=nvme0,drive=scratch-disk,nsid=2"
    )
fi
if [ "$ADD_CAPACITY_DISK" = "1" ]; then
    QEMU_ARGS+=(
        -drive "file=$CAPACITY_IMG,if=none,id=capacity-disk,format=raw,cache=writeback"
        -device "nvme-ns,bus=nvme0,drive=capacity-disk,nsid=3"
    )
fi
if [ "$ADD_MEDIA_DISK" = "1" ]; then
    QEMU_ARGS+=(
        -device "nvme,id=nvme1,serial=slopos-4kn"
        -drive "file=$MEDIA_IMG,if=none,id=media-disk,format=raw"
        -device "nvme-ns,bus=nvme1,drive=media-disk,nsid=1,logical_block_size=4096,physical_block_size=4096"
        -drive "file=$SCRATCH_DIR/scratch-4kn.img,if=none,id=scratch-4kn,format=raw"
        -device "nvme-ns,bus=nvme1,drive=scratch-4kn,nsid=2,logical_block_size=4096,physical_block_size=4096"
    )
fi
if [ "$ADD_SCRATCH_DISK" = "1" ]; then
    QEMU_ARGS+=(
        -device "nvme,id=nvme-spare,serial=slopos-spare"
        -drive "file=$SCRATCH_DIR/scratch-spare.img,if=none,id=spare-disk,format=raw"
        -device "nvme-ns,bus=nvme-spare,drive=spare-disk,nsid=1"
    )
fi
if [ "$ADD_BOOT_DISK" = "1" ]; then
    QEMU_ARGS+=(
        -device "nvme,id=nvme-boot,serial=slopos-boot"
        -drive "file=$BOOT_DISK_IMG,if=none,id=boot-disk,format=raw"
        -device "nvme-ns,bus=nvme-boot,drive=boot-disk,nsid=1,bootindex=$BOOT_DISK_INDEX"
    )
fi
if [ "$ADD_VERIFIED_DISK" = "1" ]; then
    # snapshot=on: a bug is exactly when "the guest never writes it" is not to
    # be trusted.
    QEMU_ARGS+=(
        -drive "file=$VERIFIED_IMG,if=none,id=verified-disk,format=raw,snapshot=on"
        -device "virtio-blk-pci,drive=verified-disk,disable-legacy=on"
    )
fi
if [ "$ADD_SCRATCH_DISK" = "1" ]; then
    QEMU_ARGS+=(
        -drive "file=$SCRATCH_DIR/scratch-virtio.img,if=none,id=scratch-virtio,format=raw"
        -device "virtio-blk-pci,drive=scratch-virtio,disable-legacy=on"
    )
fi
QEMU_ARGS+=(
    # No `dns=`: it sets the guest-visible address of SLIRP's own stub, not an
    # upstream to forward to, so naming a public resolver moves the stub
    # somewhere nothing replies from and every lookup times out.
    -netdev "user,id=slopnet0${NET_HOSTFWD}${NET_GUESTFWD}${NET_GITFWD}"
    -device "virtio-net-pci,netdev=slopnet0,disable-legacy=on"
    -boot "order=${BOOT_ORDER},menu=off"
    "${SERIAL_ARGS[@]}"
    "${DEBUG_ARGS[@]}"
    "${DISPLAY_ARGS[@]}"
    "${VIDEO_ARGS[@]}"
)
if [ "$ADD_ISA_EXIT" = "1" ]; then
    QEMU_ARGS+=(-device "isa-debug-exit,iobase=0xf4,iosize=0x01")
fi
if [ "$ADD_NO_REBOOT" = "1" ]; then
    QEMU_ARGS+=(-no-reboot)
fi
if [ "$HAVE_PCI_ARGS" = "1" ]; then
    QEMU_ARGS+=("${PCI_ARGS[@]}")
fi

# ── Launch QEMU ──────────────────────────────────────────────────────────────
case "$MODE" in
    interactive)
        echo "Starting QEMU in interactive mode (Ctrl+C to exit)..."
        "$QEMU_BIN" "${QEMU_ARGS[@]}"
        ;;

    logged)
        echo "Starting QEMU with ${BOOT_LOG_TIMEOUT}s timeout (logging to ${LOG_FILE})..."
        : > "$LOG_FILE_RAW"
        tail -n +1 -F "$LOG_FILE_RAW" 2>/dev/null &
        tail_pid=$!
        trap 'kill "$tail_pid" 2>/dev/null; wait "$tail_pid" 2>/dev/null || true; cleanup; rm -f "$LOG_FILE_RAW"' EXIT INT TERM

        set +e
        run_with_timeout "$BOOT_LOG_TIMEOUT" "$QEMU_BIN" "${QEMU_ARGS[@]}"
        status=$?
        set -e

        sleep 0.2
        kill "$tail_pid" 2>/dev/null
        wait "$tail_pid" 2>/dev/null || true
        trap - EXIT INT TERM
        cleanup

        sed 's/\x1b\[[^a-zA-Z]*[a-zA-Z]//g' "$LOG_FILE_RAW" > "$LOG_FILE"
        rm -f "$LOG_FILE_RAW"
        if [ $status -eq 124 ]; then
            echo "QEMU terminated after ${BOOT_LOG_TIMEOUT}s timeout" | tee -a "$LOG_FILE"
            exit 0
        fi
        exit $status
        ;;

    test)
        echo "Starting QEMU for test harness..."
        set +e
        "$QEMU_BIN" "${QEMU_ARGS[@]}"
        status=$?
        set -e
        trap - EXIT INT TERM
        cleanup
        if [ $status -eq 1 ]; then
            echo "Tests passed."
        elif [ $status -eq 3 ]; then
            echo "Tests reported failures." >&2
            exit 1
        else
            echo "Unexpected QEMU exit status $status" >&2
            exit $status
        fi
        ;;
esac
