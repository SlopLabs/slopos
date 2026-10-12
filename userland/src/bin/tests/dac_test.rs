//! Permission bits hold against a process that has become uid 1000 and given
//! up `DacOverride`, and every refusal has a positive control beside it: what
//! the same process may do one bit away.
//!
//! The runner hands this test every capability as uid 0, so the parent lays
//! out root-owned fixtures and each case runs in a forked child that changes
//! its ids and drops its overrides first.

use slopos_userland as _;

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

use slopos_abi::caps::{DAC_OVERRIDE, SET_ID};
use slopos_slibc::process::ids::{getresuid, setgroups, setresgid, setresuid};
use slopos_slibc::process::{_exit, WEXITSTATUS, WIFEXITED, fork, waitpid};
use slopos_slibc::test_harness::note;
use slopos_userland::syscall::core::caps_drop;

const USER: u32 = 1000;
const EPERM: i32 = 1;
const EACCES: i32 = 13;

const ETC_FILE: &str = "/etc/dac_test.conf";
const ETC_NEW: &str = "/etc/dac_test.new";
const PRIVATE_DIR: &str = "/var/dac_test";
const SECRET: &str = "/var/dac_test/secret";
const PUBLIC: &str = "/var/dac_test/public";
const THEIRS: &str = "/tmp/dac_test.theirs";
const MINE: &str = "/tmp/dac_test.mine";
const ROOTS_OWN: &str = "/tmp/dac_test.roots";

/// The errno an operation failed with, or 0 for success.
fn errno_of<T>(result: std::io::Result<T>) -> i32 {
    match result {
        Ok(_) => 0,
        Err(e) => e.raw_os_error().unwrap_or(-1),
    }
}

fn expect(label: &str, got: i32, want: i32) -> bool {
    if got == want {
        return true;
    }
    note(&format!("{label}: errno {got}, want {want}"));
    false
}

/// Become uid and gid 1000 with no supplementary group, then give up the
/// capabilities that would let the bits be ignored.
fn become_user() -> bool {
    let switched = unsafe {
        setgroups(0, core::ptr::null()) == 0
            && setresgid(USER, USER, USER) == 0
            && setresuid(USER, USER, USER) == 0
    };
    caps_drop(DAC_OVERRIDE | SET_ID);
    switched
}

/// Run `body` as the person in a child process, so what it gives up this
/// one keeps.
fn as_user(label: &str, body: fn() -> bool) -> bool {
    let pid = unsafe { fork() };
    if pid == 0 {
        let ok = become_user() && body();
        unsafe { _exit(if ok { 0 } else { 1 }) };
    }
    reaped_cleanly(label, pid)
}

fn reaped_cleanly(label: &str, pid: i32) -> bool {
    if pid < 0 {
        note(&format!("{label}: fork failed"));
        return false;
    }
    let mut status = 0;
    let reaped = unsafe { waitpid(pid, &mut status, 0) };
    reaped == pid && WIFEXITED(status) && WEXITSTATUS(status) == 0
}

fn write_mode(path: &str, mode: u32) -> std::io::Result<()> {
    let _ = fs::remove_file(path);
    fs::write(path, b"fixture\n")?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

fn lay_out_fixtures() -> bool {
    let _ = fs::remove_file(ETC_NEW);
    let _ = fs::remove_file(MINE);
    let _ = fs::create_dir(PRIVATE_DIR);
    let laid = write_mode(ETC_FILE, 0o644).is_ok()
        && fs::set_permissions(PRIVATE_DIR, fs::Permissions::from_mode(0o755)).is_ok()
        && write_mode(SECRET, 0o600).is_ok()
        && write_mode(PUBLIC, 0o644).is_ok()
        && write_mode(THEIRS, 0o666).is_ok();
    if !laid {
        note("could not lay out the root-owned fixtures");
    }
    laid
}

fn clean_up() -> bool {
    for path in [ETC_FILE, ETC_NEW, SECRET, PUBLIC, THEIRS, MINE, ROOTS_OWN] {
        let _ = fs::remove_file(path);
    }
    fs::remove_dir(PRIVATE_DIR).is_ok()
}

fn tmp_is_shared_and_sticky() -> bool {
    let tmp = fs::metadata("/tmp").map(|m| (m.uid(), m.mode() & 0o7777));
    let theirs = fs::metadata(THEIRS).map(|m| (m.uid(), m.mode() & 0o7777));
    if tmp.as_ref().ok() != Some(&(0, 0o1777)) || theirs.as_ref().ok() != Some(&(0, 0o666)) {
        note(&format!("/tmp is {tmp:?} and the fixture {theirs:?}"));
        return false;
    }
    true
}

fn the_switch_takes_every_id() -> bool {
    as_user("ids", || {
        let (mut r, mut e, mut s) = (0, 0, 0);
        let read = unsafe { getresuid(&mut r, &mut e, &mut s) };
        let back = unsafe { setresuid(0, 0, 0) };
        let made = File::create(MINE).and_then(|f| f.metadata());
        let owned = made
            .as_ref()
            .is_ok_and(|m| m.uid() == USER && m.gid() == USER);
        let _ = fs::remove_file(MINE);
        if read != 0 || (r, e, s) != (USER, USER, USER) {
            note(&format!("getresuid answered {read}: ({r}, {e}, {s})"));
            return false;
        }
        if back != -1 {
            note("uid 1000 without SetId became uid 0 again");
            return false;
        }
        if !owned {
            note(&format!("a new file is not uid 1000's: {made:?}"));
            return false;
        }
        true
    })
}

fn etc_refuses_a_new_name_and_a_writer() -> bool {
    as_user("etc", || {
        let created = OpenOptions::new()
            .write(true)
            .create(true)
            .mode(0o644)
            .open(ETC_NEW);
        let written = OpenOptions::new().write(true).open(ETC_FILE);
        let read = File::open(ETC_FILE);
        expect("create in /etc", errno_of(created), EACCES)
            && expect("write an /etc file", errno_of(written), EACCES)
            && expect("read an /etc file", errno_of(read), 0)
    })
}

fn a_root_file_closed_to_others_cannot_be_read() -> bool {
    as_user("secret", || {
        let secret = fs::read(SECRET);
        let public = fs::read(PUBLIC);
        let chmod = fs::set_permissions(SECRET, fs::Permissions::from_mode(0o644));
        expect("read a 0600 root file", errno_of(secret), EACCES)
            && expect("read a 0644 root file", errno_of(public), 0)
            && expect("chmod a root file", errno_of(chmod), EPERM)
    })
}

fn sticky_tmp_keeps_a_name_to_its_owner() -> bool {
    as_user("sticky", || {
        let opened = OpenOptions::new().write(true).open(THEIRS);
        let removed = fs::remove_file(THEIRS);
        let replaced = File::create(MINE).and_then(|_| fs::rename(MINE, THEIRS));
        let own_removed = fs::remove_file(MINE);
        expect("open another's 0666 file", errno_of(opened), 0)
            && expect("unlink another's file from /tmp", errno_of(removed), EPERM)
            && expect(
                "rename over another's file in /tmp",
                errno_of(replaced),
                EPERM,
            )
            && expect("unlink its own file from /tmp", errno_of(own_removed), 0)
    })
}

fn a_root_file_stays_root_owned() -> bool {
    as_user("chown", || {
        let taken = std::os::unix::fs::chown(PUBLIC, Some(USER), None);
        let regrouped = std::os::unix::fs::chown(PUBLIC, None, Some(USER));
        let own = File::create(MINE).and_then(|_| std::os::unix::fs::chown(MINE, None, Some(USER)));
        let _ = fs::remove_file(MINE);
        expect("take a root file", errno_of(taken), EPERM)
            && expect(
                "move a root file to the user's group",
                errno_of(regrouped),
                EPERM,
            )
            && expect("move its own file to its own group", errno_of(own), 0)
    })
}

/// uid 0 without `DacOverride` is held to the owner's bits like anyone.
fn uid_zero_holds_no_override() -> bool {
    let pid = unsafe { fork() };
    if pid == 0 {
        caps_drop(DAC_OVERRIDE);
        let ok = write_mode(ROOTS_OWN, 0o444).is_ok()
            && expect(
                "write its own 0444 file",
                errno_of(OpenOptions::new().write(true).open(ROOTS_OWN)),
                EACCES,
            )
            && expect(
                "chmod its own file",
                errno_of(fs::set_permissions(
                    ROOTS_OWN,
                    fs::Permissions::from_mode(0o644),
                )),
                0,
            )
            && expect(
                "write its own 0644 file",
                errno_of(OpenOptions::new().write(true).open(ROOTS_OWN)),
                0,
            );
        let _ = fs::remove_file(ROOTS_OWN);
        unsafe { _exit(if ok { 0 } else { 1 }) };
    }
    reaped_cleanly("uid 0", pid)
}

fn main() {
    slopos_slibc::test_harness::run(&[
        ("lay_out_fixtures", lay_out_fixtures),
        ("tmp_is_shared_and_sticky", tmp_is_shared_and_sticky),
        ("the_switch_takes_every_id", the_switch_takes_every_id),
        (
            "etc_refuses_a_new_name_and_a_writer",
            etc_refuses_a_new_name_and_a_writer,
        ),
        (
            "a_root_file_closed_to_others_cannot_be_read",
            a_root_file_closed_to_others_cannot_be_read,
        ),
        (
            "sticky_tmp_keeps_a_name_to_its_owner",
            sticky_tmp_keeps_a_name_to_its_owner,
        ),
        ("a_root_file_stays_root_owned", a_root_file_stays_root_owned),
        ("uid_zero_holds_no_override", uid_zero_holds_no_override),
        ("clean_up", clean_up),
    ]);
}
