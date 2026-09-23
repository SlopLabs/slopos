use slopos_userland as _;

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
        ("label_source_resolution", label_source_resolution),
        ("dev_shm_is_writable", dev_shm_is_writable),
    ]);
}
