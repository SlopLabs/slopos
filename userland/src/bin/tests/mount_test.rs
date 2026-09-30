use slopos_userland as _;

use slopos_slibc::test_harness::note;
use slopos_userland::syscall::error::SyscallError;
use slopos_userland::syscall::fs as fs_syscall;
use std::ffi::c_char;
use std::fs::{self, File};
use std::io::{Read, Write};

/// Mounted under `/tmp`, which is a ramfs the boot step already put there, so
/// the test needs no writable disk to create its mount point.
const MOUNT_POINT: &str = "/tmp/mount_test_mp";
const MOUNT_POINT_C: &[u8] = b"/tmp/mount_test_mp\0";

fn umount_mount_point() {
    let _ = fs_syscall::umount2(MOUNT_POINT_C.as_ptr() as *const c_char, 0);
}

/// How many entries of `/tmp` carry the mount point's name. Exactly one: the
/// covered directory and the synthesised mount entry are the same name.
fn mount_point_appearances() -> usize {
    let Ok(rd) = fs::read_dir("/tmp") else {
        return usize::MAX;
    };
    rd.flatten()
        .filter(|e| e.file_name().to_str() == Some("mount_test_mp"))
        .count()
}

fn ramfs_mount_roundtrip() -> bool {
    let _ = fs::create_dir(MOUNT_POINT);
    if !fs::metadata(MOUNT_POINT)
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        println!("mount_test: could not create the mount point");
        return false;
    }

    if let Err(e) = fs_syscall::mount(b"", MOUNT_POINT.as_bytes(), b"ramfs", 0) {
        println!("mount_test: mount of a ramfs failed: {e}");
        let _ = fs::remove_dir(MOUNT_POINT);
        return false;
    }

    let mut ok = true;
    let probe = "/tmp/mount_test_mp/through-the-mount";
    let payload: &[u8] = b"mounted\n";

    match File::create(probe) {
        Ok(mut f) => {
            if f.write_all(payload).is_err() {
                println!("mount_test: write through the mount failed");
                ok = false;
            }
        }
        Err(e) => {
            println!("mount_test: create through the mount failed: {e}");
            ok = false;
        }
    }

    if ok {
        match File::open(probe) {
            Ok(mut f) => {
                let mut got = Vec::new();
                if f.read_to_end(&mut got).is_err() || got != payload {
                    println!(
                        "mount_test: read back {} bytes, want {}",
                        got.len(),
                        payload.len()
                    );
                    ok = false;
                }
            }
            Err(e) => {
                println!("mount_test: reopen through the mount failed: {e}");
                ok = false;
            }
        }
    }

    let seen = mount_point_appearances();
    if seen != 1 {
        println!("mount_test: the mount point appeared {seen} times in /tmp, want 1");
        ok = false;
    }

    // A live descriptor is `EBUSY`, so nothing may still hold the probe here.
    if let Err(e) = fs_syscall::umount2(MOUNT_POINT_C.as_ptr() as *const c_char, 0) {
        println!("mount_test: umount2 failed: {e}");
        umount_mount_point();
        let _ = fs::remove_dir(MOUNT_POINT);
        return false;
    }

    // The directory underneath was empty, so the file went with the mount.
    if File::open(probe).is_ok() {
        println!("mount_test: a file survived the unmount of its filesystem");
        ok = false;
    }
    if mount_point_appearances() != 1 {
        println!("mount_test: the plain directory vanished with the mount");
        ok = false;
    }

    let _ = fs::remove_dir(MOUNT_POINT);
    ok
}

/// The root mount is boot's: every open descriptor names the filesystem
/// underneath it.
fn umount_root_refused() -> bool {
    match fs_syscall::umount2(b"/\0".as_ptr() as *const c_char, 0) {
        Err(e) if e == SyscallError::EBUSY => true,
        Err(e) => {
            println!("mount_test: umount2(\"/\") gave {e}, want EBUSY");
            false
        }
        Ok(()) => {
            println!("mount_test: umount2(\"/\") succeeded");
            false
        }
    }
}

/// `mount(2)` cannot conjure a filesystem: the mountable set is closed.
fn unsupported_fstype_refused() -> bool {
    let _ = fs::create_dir(MOUNT_POINT);
    let result = fs_syscall::mount(b"", MOUNT_POINT.as_bytes(), b"nosuchfs", 0);
    let ok = match result {
        Err(e) if e == SyscallError::ENODEV => true,
        Err(e) => {
            println!("mount_test: an unsupported fstype gave {e}, want ENODEV");
            false
        }
        Ok(()) => {
            println!("mount_test: an unsupported fstype mounted");
            umount_mount_point();
            false
        }
    };
    let _ = fs::remove_dir(MOUNT_POINT);
    ok
}

/// The VFS caps a created name at `fs::MAX_NAME_LEN` (255) whatever filesystem
/// is underneath; ext2's own ceiling on the tests image is the same 255.
fn long_name_refused_on_the_root() -> bool {
    let dir = "/var/mount_test_names";
    let too_long = format!("{dir}/{}", "n".repeat(256));
    let longest = format!("{dir}/{}", "n".repeat(255));

    // Tolerates its own leavings: the disk root persists, so a boot that was
    // cut short must not make the next one fail on a directory that exists.
    let _ = fs::remove_dir(&longest);
    let _ = fs::create_dir(dir);
    if !fs::metadata(dir).map(|m| m.is_dir()).unwrap_or(false) {
        println!("mount_test: could not create the name fixture directory");
        return false;
    }

    let mut ok = true;
    if fs::create_dir(&too_long).is_ok() {
        println!("mount_test: a 256-byte name was accepted");
        ok = false;
    }
    if let Err(e) = fs::create_dir(&longest) {
        println!("mount_test: the 255-byte limit itself was refused: {e}");
        ok = false;
    }

    let _ = fs::remove_dir(&longest);
    let _ = fs::remove_dir(dir);
    ok
}

/// Refused even though this binary holds the mount capability: every
/// program-identity grant is keyed on a path under `/bin`, so covering it with
/// a caller-written filesystem would hand the caller those privileges.
fn mount_over_bin_refused() -> bool {
    match fs_syscall::mount(b"", b"/bin", b"ramfs", 0) {
        Err(e) if e == SyscallError::EPERM => true,
        Err(e) => {
            println!("mount_test: mount over /bin gave {e}, want EPERM");
            false
        }
        Ok(()) => {
            println!("mount_test: /bin was covered by a caller-supplied ramfs");
            let _ = fs_syscall::umount2(b"/bin\0".as_ptr() as *const c_char, 0);
            false
        }
    }
}

/// Nothing replaces, covers, renames aside or unmounts the base a disk root's
/// slot booted, and no base directory is the disk's.
fn the_base_is_sealed_over_a_disk_root() -> bool {
    let kind =
        |path: &[u8]| fs_syscall::statfs_path(path.as_ptr() as *const c_char).map(|s| s.f_type);
    if kind(b"/\0") != Ok(slopos_abi::fs::EXT2_SUPER_MAGIC) {
        note("the root is not a disk; the base is the root itself");
        return true;
    }
    let mut failures = Vec::new();
    for dir in slopos_abi::fs::BASE_DIRS {
        let path = format!("{dir}\0");
        if kind(path.as_bytes()) == Ok(slopos_abi::fs::EXT2_SUPER_MAGIC) {
            failures.push(format!("{dir} is the disk's, not the base's"));
        }
        let unmounted = fs_syscall::umount2(path.as_ptr() as *const c_char, 0);
        if unmounted != Err(SyscallError::EPERM) {
            failures.push(format!("umount {dir} gave {unmounted:?}, want EPERM"));
        }
    }
    let covered = fs_syscall::mount(b"", b"/usr/share/fonts", b"ramfs", 0);
    if covered != Err(SyscallError::EPERM) {
        failures.push(format!(
            "a mount beneath /usr/share gave {covered:?}, want EPERM"
        ));
    }
    if File::create("/usr/share/mount_test_probe").is_ok() {
        let _ = fs::remove_file("/usr/share/mount_test_probe");
        failures.push("created a file in the base".into());
    }
    for (dir, aside) in [
        ("/lib", "/lib.mount_test"),
        ("/usr", "/usr.mount_test"),
        ("/etc", "/etc.mount_test"),
    ] {
        if fs::rename(dir, aside).is_ok() {
            let _ = fs::rename(aside, dir);
            failures.push(format!("renamed {dir}, which leads to the base, aside"));
        }
    }
    let _ = fs::create_dir(MOUNT_POINT);
    match fs_syscall::mount(b"", MOUNT_POINT.as_bytes(), b"ext2", 0) {
        Ok(()) => {
            for dir in ["usr", "etc"] {
                let from = format!("{MOUNT_POINT}/{dir}");
                let aside = format!("{from}.mount_test");
                if fs::rename(&from, &aside).is_ok() {
                    let _ = fs::rename(&aside, &from);
                    failures.push(format!("renamed /{dir} aside through a second mount"));
                }
            }
            umount_mount_point();
        }
        Err(e) => failures.push(format!("a second mount of the root gave {e}")),
    }
    let _ = fs::remove_dir(MOUNT_POINT);
    let env = std::process::Command::new("/usr/bin/env")
        .arg("true")
        .status();
    if !env.as_ref().is_ok_and(|status| status.success()) {
        failures.push(format!("/usr/bin/env true: {env:?}"));
    }
    for failure in &failures {
        println!("mount_test: {failure}");
    }
    failures.is_empty()
}

/// `LABEL=` resolves against the volume labels of the attached devices; one
/// no device carries is `ENOENT`, as an absent device name is, and an empty
/// label names nothing at all.
fn label_source_resolution() -> bool {
    let _ = fs::create_dir(MOUNT_POINT);
    let mut ok = true;
    for (source, want) in [
        (&b"LABEL=no-such-volume"[..], SyscallError::ENOENT),
        (&b"LABEL="[..], SyscallError::EINVAL),
    ] {
        match fs_syscall::mount(source, MOUNT_POINT.as_bytes(), b"ext2", 0) {
            Err(e) if e == want => {}
            Err(e) => {
                println!(
                    "mount_test: {} gave {e}, want {want}",
                    String::from_utf8_lossy(source)
                );
                ok = false;
            }
            Ok(()) => {
                println!("mount_test: {} mounted", String::from_utf8_lossy(source));
                umount_mount_point();
                ok = false;
            }
        }
    }
    let _ = fs::remove_dir(MOUNT_POINT);
    ok
}

/// The harness's labelled volume, which the test command line's
/// `mount=LABEL=slopos-media:/media` mounts at boot.
const MEDIA: &str = "/media";
const MEDIA_C: &[u8] = b"/media\0";
const MEDIA_LABEL: &[u8] = b"LABEL=slopos-media";
const MEDIA_MARKER: &str = "/media/SLOPOS-MEDIA";

fn media_mounted() -> bool {
    fs::read_to_string(MEDIA_MARKER).is_ok_and(|text| text.starts_with("slopos-media"))
}

/// The boot found the volume by its label and mounted it writable.
fn the_boot_mounts_a_volume_by_label() -> bool {
    if !media_mounted() {
        note(&format!(
            "{MEDIA_MARKER} is not there: the boot's mount= did not mount the volume"
        ));
        return false;
    }
    let probe = format!("{MEDIA}/mount_test_probe");
    let wrote =
        fs::write(&probe, b"probe").is_ok() && fs::read(&probe).is_ok_and(|b| b == b"probe");
    let _ = fs::remove_file(&probe);
    if !wrote {
        note(&format!("{MEDIA} took no write"));
    }
    wrote
}

/// The boot's mount holds the device's exclusive write claim, so a second
/// writable mount is refused, and `umount2` gives it back, so the volume
/// mounts again by label: a leaked claim answers `EBUSY` forever. Leaves
/// `/media` mounted.
fn a_volume_remounts_by_label_after_umount() -> bool {
    let _ = fs::create_dir(MOUNT_POINT);
    let second = fs_syscall::mount(MEDIA_LABEL, MOUNT_POINT.as_bytes(), b"ext2", 0);
    let _ = fs::remove_dir(MOUNT_POINT);
    match second {
        Err(e) if e == SyscallError::EBUSY => {}
        Err(e) => {
            note(&format!("a second writable mount gave {e}, want EBUSY"));
            return false;
        }
        Ok(()) => {
            umount_mount_point();
            note("the volume mounted writable twice: the boot's mount holds no claim");
            return false;
        }
    }
    if let Err(e) = fs_syscall::umount2(MEDIA_C.as_ptr() as *const c_char, 0) {
        note(&format!("umount of {MEDIA} failed: {e}"));
        return false;
    }
    if media_mounted() {
        note(&format!("{MEDIA} still shows the volume after its umount"));
        return false;
    }
    if let Err(e) = fs_syscall::mount(MEDIA_LABEL, MEDIA.as_bytes(), b"ext2", 0) {
        note(&format!("re-mount by label failed: {e}"));
        return false;
    }
    if !media_mounted() {
        note(&format!("re-mounted without {MEDIA_MARKER}"));
        return false;
    }
    true
}

/// `/dev/shm` is the ramfs boot mounts for `shm_open`: a file there can be
/// created, sized, read back and unlinked.
fn dev_shm_is_writable() -> bool {
    const PATH: &str = "/dev/shm/mount_test_shm";
    let outcome = (|| -> std::io::Result<()> {
        let mut file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(PATH)?;
        file.set_len(8192)?;
        file.write_all(b"shm")?;
        if fs::metadata(PATH)?.len() != 8192 {
            return Err(std::io::Error::other("ftruncate did not size the object"));
        }
        let mut back = [0u8; 3];
        File::open(PATH)?.read_exact(&mut back)?;
        if &back != b"shm" {
            return Err(std::io::Error::other("the object read back wrong"));
        }
        fs::remove_file(PATH)?;
        if fs::metadata(PATH).is_ok() {
            return Err(std::io::Error::other("the object outlived its unlink"));
        }
        Ok(())
    })();
    if let Err(e) = outcome {
        println!("mount_test: /dev/shm: {e}");
        let _ = fs::remove_file(PATH);
        return false;
    }
    true
}

fn main() {
    slopos_slibc::test_harness::run(&[
        ("ramfs_mount_roundtrip", ramfs_mount_roundtrip),
        ("umount_root_refused", umount_root_refused),
        ("unsupported_fstype_refused", unsupported_fstype_refused),
        (
            "long_name_refused_on_the_root",
            long_name_refused_on_the_root,
        ),
        ("mount_over_bin_refused", mount_over_bin_refused),
        (
            "the_base_is_sealed_over_a_disk_root",
            the_base_is_sealed_over_a_disk_root,
        ),
        ("label_source_resolution", label_source_resolution),
        (
            "the_boot_mounts_a_volume_by_label",
            the_boot_mounts_a_volume_by_label,
        ),
        (
            "a_volume_remounts_by_label_after_umount",
            a_volume_remounts_by_label_after_umount,
        ),
        ("dev_shm_is_writable", dev_shm_is_writable),
    ]);
}
