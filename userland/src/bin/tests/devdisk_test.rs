use slopos_userland as _;

use slopos_abi::fs::MS_RDONLY;
use slopos_slibc::test_harness::note;
use slopos_userland::syscall::error::SyscallError;
use slopos_userland::syscall::fs as fs_syscall;
use std::ffi::c_char;
use std::fs;
use std::sync::LazyLock;

/// `qemu_run.sh` attaches the dev disk last, and the guest names a virtio
/// device by its position among the attached ones rather than by the id on
/// the QEMU command line: the suite's own three disks put it at `vdd`, and a
/// capacity run pushes it to `vde`. Both are probed.
const CANDIDATES: [&str; 2] = ["vdd", "vde"];

/// `/tmp` is the ramfs the boot step already put there, so the mount point
/// costs no writable disk of its own.
const MOUNT_POINT: &str = "/tmp/devdisk";
const MOUNT_POINT_C: &[u8] = b"/tmp/devdisk\0";

/// Written by `scripts/build_devdisk.sh` at the volume root. Line 1 is the
/// magic and a format version; `file <bytes> <path>` and `dir <path>` lines
/// are the inventory this test grades the volume against.
const MARKER: &str = "SLOPOS-DEVDISK";

fn mount_dev_disk(device: &str, flags: u32) -> Result<(), SyscallError> {
    let _ = fs::create_dir(MOUNT_POINT);
    fs_syscall::mount(device.as_bytes(), MOUNT_POINT.as_bytes(), b"ext2", flags)
}

fn umount_dev_disk() -> Result<(), SyscallError> {
    fs_syscall::umount2(MOUNT_POINT_C.as_ptr() as *const c_char, 0)
}

fn marker_path() -> String {
    format!("{MOUNT_POINT}/{MARKER}")
}

/// Which device carries the dev disk, probed `MS_RDONLY` so a candidate that
/// turns out to be somebody else's volume is never claimed for writing. The
/// error side is what was tried and what each candidate answered: a utest's
/// own stdout is init's console, not the serial line the run is read from, so
/// the note is the only channel that reaches the KTAP line.
static DEV_DISK: LazyLock<Result<&'static str, String>> = LazyLock::new(probe_dev_disk);

fn probe_dev_disk() -> Result<&'static str, String> {
    let mut tried = String::new();
    for device in CANDIDATES {
        match mount_dev_disk(device, MS_RDONLY) {
            Ok(()) => {
                let carries_marker = fs::read_to_string(marker_path())
                    .map(|text| text.starts_with(MARKER))
                    .unwrap_or(false);
                if let Err(e) = umount_dev_disk() {
                    tried.push_str(&format!("{device}: umount of the probe: {e}; "));
                    continue;
                }
                if carries_marker {
                    return Ok(device);
                }
                tried.push_str(&format!("{device}: mounted, no {MARKER}; "));
            }
            Err(e) => tried.push_str(&format!("{device}: {e}; ")),
        }
    }
    Err(tried)
}

/// No dev disk attached is a pass with a note, as the kernel-side capacity
/// report is: `DEV_DISK_IMG` is opt-in and an ordinary run sets none.
fn dev_disk_or_skip() -> Option<&'static str> {
    match &*DEV_DISK {
        Ok(device) => Some(device),
        Err(tried) => {
            note(&format!("no dev disk attached ({tried})"));
            None
        }
    }
}

/// Every `file` line names a path that is there and stats as the byte count
/// the host recorded when it staged the volume. A dev disk whose `lib/` never
/// arrived mounts, reads and passes every structural check; the inventory is
/// what turns that into a failure here rather than a link error in a guest
/// build hours later.
fn devdisk_inventory_reads_back() -> bool {
    let Some(device) = dev_disk_or_skip() else {
        return true;
    };
    if let Err(e) = mount_dev_disk(device, 0) {
        note(&format!("mount of /dev/{device} failed: {e}"));
        return false;
    }

    let outcome = match fs::read_to_string(marker_path()) {
        Ok(text) => grade_inventory(device, &text),
        Err(e) => {
            note(&format!("/dev/{device}: reading {MARKER} back: {e}"));
            false
        }
    };

    if let Err(e) = umount_dev_disk() {
        note(&format!("/dev/{device}: umount failed: {e}"));
        return false;
    }
    outcome
}

fn grade_inventory(device: &str, text: &str) -> bool {
    let mut files = 0usize;
    let mut dirs = 0usize;
    for line in text.lines() {
        let mut field = line.split(' ');
        match field.next() {
            Some("file") => {
                let (Some(want), Some(rel)) = (field.next(), field.next()) else {
                    note(&format!("/dev/{device}: malformed inventory line {line:?}"));
                    return false;
                };
                let Ok(want) = want.parse::<u64>() else {
                    note(&format!("/dev/{device}: unparsable size in {line:?}"));
                    return false;
                };
                let got = match fs::metadata(format!("{MOUNT_POINT}/{rel}")) {
                    Ok(meta) => meta.len(),
                    Err(e) => {
                        note(&format!("/dev/{device}: {rel} is not on the volume: {e}"));
                        return false;
                    }
                };
                if got != want {
                    note(&format!("/dev/{device}: {rel} is {got} bytes, want {want}"));
                    return false;
                }
                files += 1;
            }
            Some("dir") => {
                let Some(rel) = field.next() else {
                    note(&format!("/dev/{device}: malformed inventory line {line:?}"));
                    return false;
                };
                match fs::metadata(format!("{MOUNT_POINT}/{rel}")) {
                    Ok(meta) if meta.is_dir() => dirs += 1,
                    Ok(_) => {
                        note(&format!("/dev/{device}: {rel} is not a directory"));
                        return false;
                    }
                    Err(e) => {
                        note(&format!("/dev/{device}: {rel} is not on the volume: {e}"));
                        return false;
                    }
                }
            }
            _ => {}
        }
    }
    if files == 0 || dirs == 0 {
        note(&format!(
            "/dev/{device}: the marker lists {files} files and {dirs} directories"
        ));
        return false;
    }
    note(&format!(
        "/dev/{device}: {files} files and {dirs} directories verified"
    ));
    true
}

/// The unmount above gave the device's exclusive write claim back, so the
/// same disk mounts again. A leaked claim answers `AlreadyClaimed` forever,
/// which is a failure no amount of reading detects.
fn devdisk_remounts_after_umount() -> bool {
    let Some(device) = dev_disk_or_skip() else {
        return true;
    };
    if let Err(e) = mount_dev_disk(device, 0) {
        note(&format!("re-mount of /dev/{device} failed: {e}"));
        return false;
    }
    let readable = fs::metadata(marker_path()).is_ok();
    if let Err(e) = umount_dev_disk() {
        note(&format!("/dev/{device}: second umount failed: {e}"));
        return false;
    }
    if !readable {
        note(&format!("/dev/{device}: re-mounted without its {MARKER}"));
        return false;
    }
    true
}

fn main() {
    slopos_slibc::test_harness::run(&[
        ("devdisk_inventory_reads_back", devdisk_inventory_reads_back),
        (
            "devdisk_remounts_after_umount",
            devdisk_remounts_after_umount,
        ),
    ]);
}
