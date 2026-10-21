//! Permission bits hold against a process that has become uid 1000 and given
//! up `DacOverride`, and every refusal has a positive control beside it: what
//! the same process may do one bit away.
//!
//! The runner hands this test every capability as uid 0, so the parent lays
//! out root-owned fixtures and each case runs in a forked child that changes
//! its ids and drops its overrides first. Under `--session` it runs as the
//! session does, declared uid 1000 with nothing gated, and holds the root's
//! ownership map to what that process may write.

use slopos_userland as _;

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::caps::{DAC_OVERRIDE, SET_ID};
use slopos_abi::net::{AF_UNIX, SOCK_STREAM};
use slopos_slibc::SyscallResult;
use slopos_slibc::process::ids::{getresgid, getresuid, setgroups, setresgid, setresuid};
use slopos_slibc::process::{_exit, WEXITSTATUS, WIFEXITED, fork, waitpid};
use slopos_slibc::test_harness::note;
use slopos_userland::root_layout;
use slopos_userland::syscall::core::caps_drop;
use slopos_userland::syscall::{OwnedFd, net, process};

const USER: u32 = 1000;
const EPERM: i32 = 1;
const EACCES: i32 = 13;
const SESSION: &str = "--session";

const ETC_FILE: &str = "/etc/dac_test.conf";
const ETC_NEW: &str = "/etc/dac_test.new";
const PRIVATE_DIR: &str = "/var/dac_test";
const SECRET: &str = "/var/dac_test/secret";
const PUBLIC: &str = "/var/dac_test/public";
const THEIRS: &str = "/tmp/dac_test.theirs";
const MINE: &str = "/tmp/dac_test.mine";
const ROOTS_OWN: &str = "/tmp/dac_test.roots";
const THEIR_LINK: &str = "/tmp/dac_test.link";
const ROOTS_LINK: &str = "/tmp/dac_test.roots_link";
const ROOTS_SOCKET: &str = "/var/dac_test/socket";
const REFUSED_SOCKET: &str = "/var/dac_test/theirs.sock";
const SHARED_SOCKET: &str = "/tmp/dac_test.sock";
const OLD_ROOT: &str = "/var/tmp/dac_test.old_root";

static ROOTS_PTS: AtomicU32 = AtomicU32::new(u32::MAX);
static SAID: Mutex<String> = Mutex::new(String::new());

/// The errno an operation failed with, or 0 for success.
fn errno_of<T>(result: std::io::Result<T>) -> i32 {
    match result {
        Ok(_) => 0,
        Err(e) => e.raw_os_error().unwrap_or(-1),
    }
}

fn sys_errno<T>(result: SyscallResult<T>) -> i32 {
    match result {
        Ok(_) => 0,
        Err(e) => e.errno(),
    }
}

/// `note`, kept where a forked case can hand it to its parent.
fn say(msg: &str) {
    note(msg);
    *SAID.lock().unwrap_or_else(|e| e.into_inner()) = msg.to_owned();
}

fn expect(label: &str, got: i32, want: i32) -> bool {
    if got == want {
        return true;
    }
    say(&format!("{label}: errno {got}, want {want}"));
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
    in_child(label, become_user, body)
}

/// `body` in a child that has run `setup` first; what the child said on
/// failing becomes this case's note.
fn in_child(label: &str, setup: fn() -> bool, body: fn() -> bool) -> bool {
    let Ok((mut reader, mut writer)) = std::io::pipe() else {
        say(&format!("{label}: pipe failed"));
        return false;
    };
    let pid = unsafe { fork() };
    if pid == 0 {
        let ok = setup() && body();
        if !ok {
            let said = SAID.lock().map(|said| said.clone()).unwrap_or_default();
            let _ = writer.write_all(said.as_bytes());
        }
        unsafe { _exit(if ok { 0 } else { 1 }) };
    }
    drop(writer);
    if pid < 0 {
        say(&format!("{label}: fork failed"));
        return false;
    }
    let mut status = 0;
    let reaped = unsafe { waitpid(pid, &mut status, 0) };
    if reaped == pid && WIFEXITED(status) && WEXITSTATUS(status) == 0 {
        return true;
    }
    let mut said = String::new();
    let _ = reader.read_to_string(&mut said);
    say(&format!("{label}: {said}"));
    false
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
        say("could not lay out the root-owned fixtures");
    }
    laid
}

fn clean_up() -> bool {
    for path in [
        ETC_FILE,
        ETC_NEW,
        SECRET,
        PUBLIC,
        THEIRS,
        MINE,
        ROOTS_OWN,
        THEIR_LINK,
        ROOTS_LINK,
        ROOTS_SOCKET,
        REFUSED_SOCKET,
        SHARED_SOCKET,
    ] {
        let _ = fs::remove_file(path);
    }
    fs::remove_dir(PRIVATE_DIR).is_ok()
}

fn tmp_is_shared_and_sticky() -> bool {
    let tmp = fs::metadata("/tmp").map(|m| (m.uid(), m.mode() & 0o7777));
    let theirs = fs::metadata(THEIRS).map(|m| (m.uid(), m.mode() & 0o7777));
    if tmp.as_ref().ok() != Some(&(0, 0o1777)) || theirs.as_ref().ok() != Some(&(0, 0o666)) {
        say(&format!("/tmp is {tmp:?} and the fixture {theirs:?}"));
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
            say(&format!("getresuid answered {read}: ({r}, {e}, {s})"));
            return false;
        }
        if back != -1 {
            say("uid 1000 without SetId became uid 0 again");
            return false;
        }
        if !owned {
            say(&format!("a new file is not uid 1000's: {made:?}"));
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
    in_child(
        "uid 0",
        || {
            caps_drop(DAC_OVERRIDE);
            true
        },
        || {
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
            ok
        },
    )
}

fn pts(index: u32) -> String {
    format!("/dev/pts/{index}")
}

fn stat_of(path: &str) -> Option<(u32, u32, u32)> {
    let meta = fs::metadata(path).ok()?;
    Some((meta.uid(), meta.gid(), meta.mode() & 0o7777))
}

fn read_write(path: &str) -> std::io::Result<File> {
    OpenOptions::new().read(true).write(true).open(path)
}

/// A slave belongs to whoever opened its master, so another uid cannot open
/// it; the same uid's own slave opens.
fn a_pty_slave_answers_its_opener() -> bool {
    let Ok((_master, index)) = process::openpty() else {
        say("root could not open a pty");
        return false;
    };
    ROOTS_PTS.store(index, Ordering::Relaxed);
    let roots = stat_of(&pts(index));
    if roots != Some((0, 0, 0o620)) {
        say(&format!("root's slave is {roots:?}"));
        return false;
    }
    as_user("pty", || {
        let theirs = read_write(&pts(ROOTS_PTS.load(Ordering::Relaxed)));
        let Ok((_master, own)) = process::openpty() else {
            say("the user could not open a pty");
            return false;
        };
        let owned = stat_of(&pts(own));
        if owned != Some((USER, USER, 0o620)) {
            say(&format!("the user's slave is {owned:?}"));
            return false;
        }
        expect("open root's slave", errno_of(theirs), EACCES)
            && expect("open its own slave", errno_of(read_write(&pts(own))), 0)
    })
}

fn unix_listener(path: &str) -> SyscallResult<OwnedFd> {
    let fd = net::socket(AF_UNIX, SOCK_STREAM, 0)?;
    net::bind_unix(fd.raw(), path.as_bytes())?;
    net::listen(fd.raw(), 4)?;
    Ok(fd)
}

fn unix_connect(path: &str) -> i32 {
    sys_errno(
        net::socket(AF_UNIX, SOCK_STREAM, 0)
            .and_then(|fd| net::connect_unix(fd.raw(), path.as_bytes())),
    )
}

/// A socket's name is a file: binding one takes write on its directory, and
/// connecting takes write on the name.
fn a_socket_name_is_held_to_its_bits() -> bool {
    let _ = fs::remove_file(ROOTS_SOCKET);
    let Ok(_listener) = unix_listener(ROOTS_SOCKET) else {
        say("root could not listen in its own directory");
        return false;
    };
    let shut = fs::set_permissions(ROOTS_SOCKET, fs::Permissions::from_mode(0o755)).is_ok()
        && as_user("socket closed", || {
            let _ = fs::remove_file(SHARED_SOCKET);
            let refused = unix_listener(REFUSED_SOCKET);
            let shared = unix_listener(SHARED_SOCKET);
            let owner = stat_of(SHARED_SOCKET).map(|(uid, ..)| uid);
            let _ = fs::remove_file(SHARED_SOCKET);
            expect(
                "connect to root's 0755 socket",
                unix_connect(ROOTS_SOCKET),
                EACCES,
            ) && expect("bind in root's directory", sys_errno(refused), EACCES)
                && expect("bind in /tmp", sys_errno(shared), 0)
                && expect(
                    "own its name",
                    owner.map_or(-1, |uid| uid as i32),
                    USER as i32,
                )
        });
    shut && fs::set_permissions(ROOTS_SOCKET, fs::Permissions::from_mode(0o666)).is_ok()
        && as_user("socket open", || {
            expect(
                "connect to root's 0666 socket",
                unix_connect(ROOTS_SOCKET),
                0,
            )
        })
}

/// In a sticky directory anyone may write, a link is followed only by its
/// owner or the directory's, so a planted name cannot steer a root process.
fn a_link_in_tmp_is_followed_by_its_owners() -> bool {
    let _ = fs::remove_file(ROOTS_LINK);
    let planted = as_user("plant", || {
        let _ = fs::remove_file(THEIR_LINK);
        let made = symlink(PUBLIC, THEIR_LINK);
        expect("plant a link in /tmp", errno_of(made), 0)
            && expect("follow its own link", errno_of(fs::read(THEIR_LINK)), 0)
    });
    let followed = fs::read(THEIR_LINK);
    let looked_at = fs::symlink_metadata(THEIR_LINK).map(|m| m.uid());
    let own = symlink(PUBLIC, ROOTS_LINK).and_then(|()| fs::read(ROOTS_LINK));
    let removed = fs::remove_file(THEIR_LINK);
    planted
        && expect("follow a link the user planted", errno_of(followed), EACCES)
        && expect("look at the link itself", errno_of(looked_at), 0)
        && expect("follow its own link", errno_of(own), 0)
        && expect("remove the planted link", errno_of(removed), 0)
}

fn old_root(rel: &str) -> String {
    format!("{OLD_ROOT}/{rel}")
}

fn owner_in_old_root(rel: &str) -> Option<(u32, u32)> {
    let meta = fs::symlink_metadata(old_root(rel)).ok()?;
    Some((meta.uid(), meta.gid()))
}

fn stage_old_root() -> bool {
    let _ = fs::remove_dir_all(OLD_ROOT);
    let files = [
        "home/user/notes",
        "src/slopos/Cargo.toml",
        "usr/local/bin/tool",
        "etc/keymap",
    ];
    files.iter().all(|rel| {
        let path = old_root(rel);
        Path::new(&path)
            .parent()
            .is_some_and(|dir| fs::create_dir_all(dir).is_ok())
            && fs::write(&path, b"written as uid 0\n").is_ok()
    }) && fs::hard_link(old_root("etc/keymap"), old_root("home/user/keymap")).is_ok()
        && symlink(old_root("etc/keymap"), old_root("home/user/link")).is_ok()
        && fs::set_permissions(old_root("etc"), fs::Permissions::from_mode(0o777)).is_ok()
}

/// A root every process wrote as uid 0 is migrated once: the first person's
/// trees become their own but for a file with another name, the map's
/// directories take their owner and mode, and the record keeps a later boot
/// from doing it again.
fn a_preserved_root_is_handed_to_the_session() -> bool {
    if !stage_old_root() {
        say("could not stage a root written as uid 0");
        return false;
    }
    root_layout::lay_out(Path::new(OLD_ROOT));
    let session = Some((USER, USER));
    let system = Some((0, 0));
    for (rel, want) in [
        ("home/user", session),
        ("home/user/notes", session),
        ("home/user/link", session),
        ("src", session),
        ("src/slopos/Cargo.toml", session),
        ("usr/local", session),
        ("usr/local/bin/tool", session),
        ("home/user/keymap", system),
        ("etc/keymap", system),
        ("home", system),
        ("root", system),
    ] {
        let held = owner_in_old_root(rel);
        if held != want {
            say(&format!("{rel} is {held:?}, not {want:?}"));
            return false;
        }
    }
    for (rel, mode) in [("etc", 0o755), ("root", 0o700), ("var/tmp", 0o1777)] {
        let held = stat_of(&old_root(rel)).map_or(0, |(.., held)| held);
        if held != mode {
            say(&format!("{rel} has mode {held:o}, not {mode:o}"));
            return false;
        }
    }
    let record = fs::read_to_string(old_root("var/lib/slopos/owners")).unwrap_or_default();
    if record != TODAYS_RECORD {
        say(&format!("the record reads {record:?}"));
        return false;
    }
    let later = fs::write(old_root("home/user/later"), b"after the migration\n").is_ok();
    root_layout::lay_out(Path::new(OLD_ROOT));
    let kept = owner_in_old_root("home/user/later");
    let _ = fs::remove_dir_all(OLD_ROOT);
    if !later || kept != system {
        say(&format!(
            "a file made after the migration is {kept:?} after a second boot"
        ));
        return false;
    }
    true
}

/// The record a root made for one person carries.
const TODAYS_RECORD: &str = "1000:1000 /home/user\n1000:1000 /src\n1000:1000 /usr/local\n";

/// A record from before the map named the first person's home `/home` is
/// rewritten as today's, and nothing on the root moves.
fn an_older_record_is_rewritten_and_nothing_moves() -> bool {
    if !stage_old_root() {
        say("could not stage a root written as uid 0");
        return false;
    }
    let record = old_root("var/lib/slopos/owners");
    let older = "1000:1000 /home\n1000:1000 /src\n1000:1000 /usr/local\n";
    let staged = fs::create_dir_all(old_root("var/lib/slopos"))
        .and_then(|()| fs::write(&record, older))
        .is_ok();
    root_layout::lay_out(Path::new(OLD_ROOT));
    let rewritten = fs::read_to_string(&record).unwrap_or_default();
    let notes = owner_in_old_root("home/user/notes");
    let _ = fs::remove_dir_all(OLD_ROOT);
    staged
        && (rewritten == TODAYS_RECORD || {
            say(&format!("the older record reads {rewritten:?}"));
            false
        })
        && (notes == Some((0, 0)) || {
            say(&format!("a file of a recorded root moved to {notes:?}"));
            false
        })
}

/// A root whose `/usr/local` is the first person's, with what a person may
/// have left there: their file group-writable, a directory anyone writes, a
/// name for another uid's file, links in and out and one through a link of
/// theirs, and the apps' manifest.
fn stage_shared_root() -> bool {
    let _ = fs::remove_dir_all(OLD_ROOT);
    let made = [
        "usr/local/bin",
        "usr/local/open",
        "home/user",
        "var/tmp",
        "var/lib/slopos/trees",
    ]
    .iter()
    .all(|rel| fs::create_dir_all(old_root(rel)).is_ok());
    let manifest = |at: &str| format!("identity {}\nat {at}\n", "0".repeat(64));
    let written = [
        ("usr/local/bin/tool", "#!/bin/shell\n".to_owned()),
        ("home/user/build", "#!/bin/shell\n".to_owned()),
        ("var/tmp/theirs", "another's\n".to_owned()),
        ("var/lib/slopos/trees/apps", manifest("/usr/local")),
        ("var/lib/slopos/owners", TODAYS_RECORD.to_owned()),
    ]
    .iter()
    .all(|(rel, text)| fs::write(old_root(rel), text).is_ok());
    let theirs = |rel: &str, uid: u32| {
        std::os::unix::fs::lchown(old_root(rel), Some(uid), Some(uid)).is_ok()
    };
    made && written
        && symlink("tool", old_root("usr/local/bin/kept")).is_ok()
        && symlink("/home/user/build", old_root("usr/local/bin/away")).is_ok()
        && symlink("/usr/local/bin/tool", old_root("usr/local/bin/fixed")).is_ok()
        && symlink("/home", old_root("usr/local/homes")).is_ok()
        && symlink("../../../home/user/relay", old_root("usr/local/bin/chain")).is_ok()
        && symlink("../../usr/local/bin/tool", old_root("home/user/relay")).is_ok()
        && fs::hard_link(
            old_root("var/tmp/theirs"),
            old_root("usr/local/open/theirs"),
        )
        .is_ok()
        && [
            ("usr/local", USER),
            ("usr/local/bin", USER),
            ("usr/local/bin/tool", USER),
            ("usr/local/bin/kept", USER),
            ("usr/local/bin/away", USER),
            ("usr/local/bin/chain", USER),
            ("usr/local/bin/fixed", USER),
            ("usr/local/homes", USER),
            ("usr/local/open", USER),
            ("home/user", USER),
            ("home/user/build", USER),
            ("home/user/relay", USER),
            ("var/tmp/theirs", USER + 1),
            ("var/lib/slopos/trees/apps", USER),
        ]
        .iter()
        .all(|(rel, uid)| theirs(rel, *uid))
        && fs::set_permissions(
            old_root("usr/local/bin/tool"),
            fs::Permissions::from_mode(0o775),
        )
        .is_ok()
        && fs::set_permissions(
            old_root("usr/local/open"),
            fs::Permissions::from_mode(0o777),
        )
        .is_ok()
}

/// A root shared with a second person has `/usr/local` uid 0's and writable
/// by uid 0 alone to its last entry; another uid's file loses only its name
/// there, a link that leads through what another uid may change goes, as
/// does one to a directory outside it, and one within it stays, named from
/// the root or from its directory. The first person's files stay theirs,
/// their home closed
/// to others, and so does the apps' manifest they wrote, which uid 0 does not
/// believe.
fn usr_local_is_handed_to_the_system_whole() -> bool {
    if !stage_shared_root() {
        say("could not stage a root whose /usr/local is the first person's");
        return false;
    }
    let handed = root_layout::share(Path::new(OLD_ROOT));
    let held = |rel: &str| {
        fs::symlink_metadata(old_root(rel))
            .ok()
            .map(|meta| (meta.uid(), meta.mode() & 0o7777))
    };
    let record = fs::read_to_string(old_root("var/lib/slopos/owners")).unwrap_or_default();
    let checks = [
        (handed.is_ok(), format!("the hand-over: {handed:?}")),
        (held("usr/local") == Some((0, 0o755)), "/usr/local".into()),
        (
            held("usr/local/bin/tool") == Some((0, 0o755)),
            "a group-writable file".into(),
        ),
        (
            held("usr/local/open") == Some((0, 0o755)),
            "a directory anyone wrote".into(),
        ),
        (
            held("usr/local/open/theirs").is_none(),
            "a name for another uid's file".into(),
        ),
        (
            held("var/tmp/theirs").map(|h| h.0) == Some(USER + 1),
            "another uid's file".into(),
        ),
        (
            held("usr/local/bin/kept").map(|h| h.0) == Some(0),
            "a link within it".into(),
        ),
        (
            held("usr/local/bin/away").is_none(),
            "a link to a person's file".into(),
        ),
        (
            held("usr/local/bin/chain").is_none(),
            "a link through a person's link".into(),
        ),
        (
            held("usr/local/bin/fixed").map(|h| h.0) == Some(0),
            "a link within it named from the root".into(),
        ),
        (
            held("usr/local/homes").is_none(),
            "a link to a directory outside it".into(),
        ),
        (held("home/user") == Some((USER, 0o700)), "the home".into()),
        (
            held("home/user/build").map(|h| h.0) == Some(USER),
            "the person's own file".into(),
        ),
        (
            held("var/lib/slopos/trees/apps").map(|h| h.0) == Some(USER),
            "the apps' manifest".into(),
        ),
        (
            record == "1000:1000 /home/user\n1000:1000 /src\n",
            format!("the record reads {record:?}"),
        ),
    ];
    let _ = fs::remove_dir_all(OLD_ROOT);
    match checks.iter().find(|(ok, _)| !ok) {
        Some((_, what)) => {
            say(&format!("handed to the system: {what}"));
            false
        }
        None => true,
    }
}

/// The accounts store `stage_shared_root` gives two people, taken away with
/// the root once dropped, unsealed first.
struct TwoPeople;

impl TwoPeople {
    fn record() -> Option<TwoPeople> {
        let mut accounts = slopos_accounts_core::Accounts::one_person();
        accounts.add("dac-second", false).ok()?;
        let store = slopos_userland::accounts::Store::write(Path::new(OLD_ROOT)).ok()?;
        store.record(&accounts).ok()?;
        Some(TwoPeople)
    }
}

impl Drop for TwoPeople {
    fn drop(&mut self) {
        use slopos_userland::sealed;
        let store = old_root("var/lib/slopos/accounts");
        if let Ok(dir) = sealed::open_dir(Path::new(&store)) {
            let _ = sealed::set_sealed(&dir, false);
        }
        for entry in fs::read_dir(&store).into_iter().flatten().flatten() {
            let _ = sealed::remove_entry(&entry.path());
        }
        let _ = fs::remove_dir_all(OLD_ROOT);
    }
}

/// A boot finishes the hand-over an add cut short: on a root whose record
/// still names `/usr/local` and whose accounts name two people, `/usr/local`
/// becomes the system's and the record says so.
fn a_hand_over_cut_short_is_finished_at_boot() -> bool {
    if !stage_shared_root() {
        say("could not stage a root whose /usr/local is the first person's");
        return false;
    }
    let Some(_people) = TwoPeople::record() else {
        say("could not record two people on the staged root");
        return false;
    };
    root_layout::lay_out(Path::new(OLD_ROOT));
    let record = fs::read_to_string(old_root("var/lib/slopos/owners")).unwrap_or_default();
    let local = owner_in_old_root("usr/local/bin/tool");
    record == "1000:1000 /home/user\n1000:1000 /src\n" && local == Some((0, 0)) || {
        say(&format!(
            "after a boot: the record {record:?}, a file of it {local:?}"
        ));
        false
    }
}

/// A hand-over cut short before the add recorded anyone leaves a root of one
/// person and its mark; a boot finishes it.
fn a_hand_over_begun_alone_is_finished_at_boot() -> bool {
    let begun = old_root(root_layout::SHARING.trim_start_matches('/'));
    if !stage_shared_root()
        || fs::write(&begun, b"").is_err()
        || Path::new(&old_root("var/lib/slopos/accounts")).exists()
    {
        say("could not stage a root of one person whose hand-over began");
        return false;
    }
    root_layout::lay_out(Path::new(OLD_ROOT));
    let record = fs::read_to_string(old_root("var/lib/slopos/owners")).unwrap_or_default();
    let local = owner_in_old_root("usr/local/bin/tool");
    let home = stat_of(&old_root("home/user")).map(|(.., mode)| mode);
    let marked = Path::new(&begun).exists();
    let _ = fs::remove_dir_all(OLD_ROOT);
    record == "1000:1000 /home/user\n1000:1000 /src\n"
        && local == Some((0, 0))
        && home == Some(0o700)
        && !marked
        || {
            say(&format!(
                "after a boot: the record {record:?}, a file of /usr/local {local:?}, \
                 the home's mode {home:?}, the mark left {marked}"
            ));
            false
        }
}

/// A root without a record whose accounts name two people is laid out with
/// `/usr/local` the system's and the first person's home closed, as a
/// hand-over leaves it.
fn a_recordless_root_of_two_is_shared() -> bool {
    if !stage_old_root() || fs::create_dir_all(old_root("var/lib/slopos")).is_err() {
        say("could not stage a root written as uid 0");
        return false;
    }
    let Some(_people) = TwoPeople::record() else {
        say("could not record two people on the staged root");
        return false;
    };
    root_layout::lay_out(Path::new(OLD_ROOT));
    let record = fs::read_to_string(old_root("var/lib/slopos/owners")).unwrap_or_default();
    let local = owner_in_old_root("usr/local/bin/tool");
    let notes = owner_in_old_root("home/user/notes");
    let home = stat_of(&old_root("home/user")).map(|(.., mode)| mode);
    record == "1000:1000 /home/user\n1000:1000 /src\n"
        && local == Some((0, 0))
        && notes == Some((USER, USER))
        && home == Some(0o700)
        || {
            say(&format!(
                "laid out: the record {record:?}, a file of /usr/local {local:?}, \
                 one of the home {notes:?}, the home's mode {home:?}"
            ));
            false
        }
}

fn session_is_the_person() -> bool {
    let (mut uids, mut gids) = ([0u32; 3], [0u32; 3]);
    let read = unsafe {
        getresuid(&mut uids[0], &mut uids[1], &mut uids[2]) == 0
            && getresgid(&mut gids[0], &mut gids[1], &mut gids[2]) == 0
    };
    if !read || uids != [USER; 3] || gids != [USER; 3] {
        say(&format!("the session runs as uids {uids:?}, gids {gids:?}"));
        return false;
    }
    true
}

fn create_errno(dir: &str) -> i32 {
    let path = format!("{dir}/dac_test.session");
    let made = File::create(&path);
    if made.is_ok() {
        let _ = fs::remove_file(&path);
    }
    errno_of(made)
}

fn session_writes_only_its_own() -> bool {
    let mut ok = true;
    for dir in ["/", "/etc", "/var/lib/slopos", "/usr"] {
        ok &= expect(&format!("create in {dir}"), create_errno(dir), EACCES);
    }
    for dir in [slopos_abi::fs::SESSION_HOME, "/tmp", "/var/tmp"] {
        ok &= expect(&format!("create in {dir}"), create_errno(dir), 0);
    }
    ok & expect("list /root", errno_of(fs::read_dir("/root")), EACCES)
}

fn session_has_its_runtime_dir() -> bool {
    let dir = format!("/run/user/{USER}");
    let held = stat_of(&dir);
    if held != Some((USER, USER, 0o700)) {
        say(&format!("{dir} is {held:?}"));
        return false;
    }
    true
}

fn session_owns_its_pty() -> bool {
    let Ok((_master, index)) = process::openpty() else {
        say("the session could not open a pty");
        return false;
    };
    let held = stat_of(&pts(index));
    if held != Some((USER, USER, 0o620)) {
        say(&format!("its slave is {held:?}"));
        return false;
    }
    expect("open its slave", errno_of(read_write(&pts(index))), 0)
}

fn session_binds_where_it_writes() -> bool {
    let _ = fs::remove_file(SHARED_SOCKET);
    let refused = unix_listener("/etc/dac_test.sock");
    let shared = unix_listener(SHARED_SOCKET);
    let _ = fs::remove_file(SHARED_SOCKET);
    expect("bind in /etc", sys_errno(refused), EACCES)
        && expect("bind in /tmp", sys_errno(shared), 0)
}

const SESSION_CASES: &[(&str, fn() -> bool)] = &[
    ("session_is_the_person", session_is_the_person),
    ("session_writes_only_its_own", session_writes_only_its_own),
    ("session_has_its_runtime_dir", session_has_its_runtime_dir),
    ("session_owns_its_pty", session_owns_its_pty),
    (
        "session_binds_where_it_writes",
        session_binds_where_it_writes,
    ),
];

fn main() {
    if std::env::args().nth(1).as_deref() == Some(SESSION) {
        slopos_slibc::test_harness::run(SESSION_CASES);
    }
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
        (
            "a_pty_slave_answers_its_opener",
            a_pty_slave_answers_its_opener,
        ),
        (
            "a_socket_name_is_held_to_its_bits",
            a_socket_name_is_held_to_its_bits,
        ),
        (
            "a_link_in_tmp_is_followed_by_its_owners",
            a_link_in_tmp_is_followed_by_its_owners,
        ),
        (
            "a_preserved_root_is_handed_to_the_session",
            a_preserved_root_is_handed_to_the_session,
        ),
        (
            "an_older_record_is_rewritten_and_nothing_moves",
            an_older_record_is_rewritten_and_nothing_moves,
        ),
        (
            "usr_local_is_handed_to_the_system_whole",
            usr_local_is_handed_to_the_system_whole,
        ),
        (
            "a_hand_over_cut_short_is_finished_at_boot",
            a_hand_over_cut_short_is_finished_at_boot,
        ),
        (
            "a_hand_over_begun_alone_is_finished_at_boot",
            a_hand_over_begun_alone_is_finished_at_boot,
        ),
        (
            "a_recordless_root_of_two_is_shared",
            a_recordless_root_of_two_is_shared,
        ),
        ("clean_up", clean_up),
    ]);
}
