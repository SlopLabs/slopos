//! The disk half of `just test-usb`: the ext4 stick on qemu-xhci, `sda`,
//! mounted, written and pulled with writes in flight, then plugged back; and
//! a read-only drive the host plugs on nec-usb-xhci, which must report write
//! protection and mount read-only. The host plugs and pulls on the
//! `USB-TEST:` lines.

use slopos_userland as _;

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileTypeExt;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use slopos_slibc::test_harness::note;
use slopos_userland::syscall::fs as fs_syscall;
use slopos_userland::syscall::tty;

const HOST_WAIT: Duration = Duration::from_secs(300);
/// The engine waits minutes on a device that holds a request; one that is
/// gone must answer well inside this.
const AT_ONCE: Duration = Duration::from_secs(10);
/// The host's pull and the guest noticing it, then the writer's refusal: the
/// engine would wait out minutes instead.
const PULL_REFUSED: Duration = Duration::from_secs(60);
const EROFS: i32 = 30;

const DISK: &str = "/dev/sda";
const DISK_MOUNT: &str = "/tmp/usb-disk";
/// What the stick holds before the pull, fsynced.
const KEPT: &str = "kept";
const KEPT_TEXT: &[u8] = b"fsynced before the pull\n";
/// Written and fsynced in a loop until the pull ends it.
const BUSY: &str = "busy";
const CHUNK: usize = 64 << 10;
/// Fsynced chunks before the host is asked to pull.
const CHUNKS_BEFORE_PULL: u64 = 4;
/// Seeded by the host and never read before the pull, so reading it then
/// reaches the device.
const COLD: &str = "cold";

/// By the label the host gave it, whichever letter it took.
const READ_ONLY: &str = "/dev/disk/by-label/usb-ro";
const READ_ONLY_MOUNT: &str = "/tmp/usb-ro";
/// Seeded by the host on the read-only drive.
const SEEDED: &str = "seeded";
const SEEDED_TEXT: &[u8] = b"written by the host\n";

fn ask_host(what: &str) {
    tty::write(format!("USB-TEST: {what}\n").as_bytes());
}

fn is_disk(node: &str) -> bool {
    fs::metadata(node).is_ok_and(|m| m.file_type().is_block_device())
}

fn wait_for(what: &str, until: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + HOST_WAIT;
    while Instant::now() < deadline {
        if until() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    note(&format!("waited {}s for {what}", HOST_WAIT.as_secs()));
    false
}

fn mount(node: &str, at: &str) -> bool {
    let _ = fs::create_dir(at);
    match fs_syscall::mount(node.as_bytes(), at.as_bytes(), b"ext4", 0) {
        Ok(()) => true,
        Err(e) => {
            note(&format!("mounting {node} at {at}: {e}"));
            false
        }
    }
}

fn umount(at: &str) -> bool {
    let path = CString::new(at).expect("no NUL in a mount point");
    match fs_syscall::umount2(path.as_ptr(), 0) {
        Ok(()) => true,
        Err(e) => {
            note(&format!("umount of {at}: {e}"));
            false
        }
    }
}

fn erofs(what: &str, result: std::io::Result<impl Sized>) -> bool {
    match result {
        Err(e) if e.raw_os_error() == Some(EROFS) => true,
        Err(e) => {
            note(&format!("{what} gave {e}, want EROFS"));
            false
        }
        Ok(_) => {
            note(&format!("{what} succeeded on a read-only mount"));
            false
        }
    }
}

fn path(mount: &str, name: &str) -> String {
    format!("{mount}/{name}")
}

/// Writes and fsyncs `BUSY` until the device stops answering, reporting each
/// durable length and then the refusal.
fn write_until_refused(report: mpsc::Sender<std::io::Result<u64>>) {
    let refused = File::create(path(DISK_MOUNT, BUSY)).and_then(|mut busy| {
        let chunk = vec![0x5au8; CHUNK];
        let mut written = 0u64;
        loop {
            busy.write_all(&chunk).and_then(|()| busy.sync_data())?;
            written += CHUNK as u64;
            let _ = report.send(Ok(written));
        }
    });
    let _ = report.send(refused);
}

/// Mounted, written, then pulled while a writer is fsyncing: the mount turns
/// read-only, a read of what was never cached fails at once, and `umount`
/// releases the stick. Plugged back, it is `sda` again and holds what was
/// fsynced.
fn an_ext4_stick_survives_a_pull_mid_write() -> bool {
    if !wait_for("sda", || is_disk(DISK)) || !mount(DISK, DISK_MOUNT) {
        return false;
    }
    let kept = File::create(path(DISK_MOUNT, KEPT))
        .and_then(|mut f| f.write_all(KEPT_TEXT).and_then(|()| f.sync_all()));
    if let Err(e) = kept {
        note(&format!("writing {KEPT}: {e}"));
        return false;
    }
    let (report, reported) = mpsc::channel();
    std::thread::spawn(move || write_until_refused(report));
    let mut fsynced = 0;
    while fsynced < CHUNKS_BEFORE_PULL * CHUNK as u64 {
        match reported.recv_timeout(HOST_WAIT) {
            Ok(Ok(length)) => fsynced = length,
            stopped => {
                note(&format!("the writer stopped before the pull: {stopped:?}"));
                return false;
            }
        }
    }
    ask_host("pull disk");
    let asked = Instant::now();
    let mut durable = fsynced;
    loop {
        match reported.recv_timeout(PULL_REFUSED.saturating_sub(asked.elapsed())) {
            Ok(Ok(length)) => durable = length,
            Ok(Err(refused)) => {
                note(&format!(
                    "the writer stopped at {durable} bytes, {} ms after the pull was asked for: {refused}",
                    asked.elapsed().as_millis()
                ));
                break;
            }
            Err(_) => {
                note(&format!(
                    "the writer was not refused within {}s of the pull",
                    PULL_REFUSED.as_secs()
                ));
                return false;
            }
        }
    }
    if !wait_for("sda to leave", || !is_disk(DISK)) {
        return false;
    }

    let started = Instant::now();
    let cold = File::open(path(DISK_MOUNT, COLD)).and_then(|mut f| f.read(&mut [0u8; 4096]));
    if cold.is_ok() {
        note("a read of a block never cached succeeded with the stick gone");
        return false;
    }
    if started.elapsed() > AT_ONCE {
        note(&format!(
            "a read with the stick gone took {} ms",
            started.elapsed().as_millis()
        ));
        return false;
    }
    let mutations = erofs("a create", File::create(path(DISK_MOUNT, "after-the-pull")))
        && erofs("a mkdir", fs::create_dir(path(DISK_MOUNT, "dir")))
        && erofs(
            "a write",
            OpenOptions::new()
                .append(true)
                .open(path(DISK_MOUNT, KEPT))
                .and_then(|mut f| f.write_all(b"more")),
        );
    if !mutations || !umount(DISK_MOUNT) {
        return false;
    }

    ask_host("plug disk");
    if !wait_for("sda to return", || is_disk(DISK)) || !mount(DISK, DISK_MOUNT) {
        return false;
    }
    let kept = fs::read(path(DISK_MOUNT, KEPT));
    let busy = fs::metadata(path(DISK_MOUNT, BUSY)).map(|m| m.len());
    let unmounted = umount(DISK_MOUNT);
    match (kept, busy) {
        (Ok(text), Ok(length)) if text == KEPT_TEXT && length >= durable => unmounted,
        (kept, busy) => {
            note(&format!(
                "after the pull {KEPT} reads {kept:?} and {BUSY} is {busy:?} bytes, \
                 {durable} of which were fsynced"
            ));
            false
        }
    }
}

/// A drive the host attaches read-only reports write protection, and an
/// ext4 mount of it that asked for nothing comes up read-only.
fn a_write_protected_drive_mounts_read_only() -> bool {
    ask_host("plug ro");
    if !wait_for("the read-only drive", || is_disk(READ_ONLY)) {
        return false;
    }
    let protected = File::open(READ_ONLY)
        .map_err(|e| e.to_string())
        .and_then(|f| fs_syscall::read_only(f.as_raw_fd()).map_err(|e| format!("{e:?}")));
    if protected != Ok(true) {
        note(&format!("BLKROGET on {READ_ONLY} answers {protected:?}"));
        return false;
    }
    if !mount(READ_ONLY, READ_ONLY_MOUNT) {
        return false;
    }
    let seeded = fs::read(path(READ_ONLY_MOUNT, SEEDED));
    let refused = erofs(
        "a create on the read-only drive",
        File::create(path(READ_ONLY_MOUNT, "new")),
    );
    let unmounted = umount(READ_ONLY_MOUNT);
    if !matches!(&seeded, Ok(text) if text == SEEDED_TEXT) {
        note(&format!("{SEEDED} reads {seeded:?}"));
        return false;
    }
    ask_host("pull ro");
    refused && unmounted && wait_for("the read-only drive to leave", || !is_disk(READ_ONLY))
}

fn main() {
    slopos_slibc::test_harness::run(&[
        (
            "an_ext4_stick_survives_a_pull_mid_write",
            an_ext4_stick_survives_a_pull_mid_write,
        ),
        (
            "a_write_protected_drive_mounts_read_only",
            a_write_protected_drive_mounts_read_only,
        ),
    ]);
}
