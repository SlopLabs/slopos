//! The people a machine knows. A root comes up with the one person, `user`,
//! as its first administrator; an account `/bin/users` makes has a home of
//! its own and the groups a process spawned as it holds; a password is set,
//! checked by `/sbin/authcheck` and refused when wrong, and changed by its
//! person only with the current one; only an administrator manages people
//! and the last one stays; and neither a `DacOverride` nor a `Mount` holder
//! reaches the database.
//!
//! The runner hands this test every capability as uid 0. `users` started
//! without its action's capabilities asks privd, which runs it for root; one
//! started holding them acts itself, for the requester privd would name.

use slopos_userland as _;

use std::ffi::CString;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Command, Stdio};

use slopos_abi::caps::{ALL, DAC_OVERRIDE, MOUNT, SEAL};
use slopos_abi::fs::inode_flags::FS_IMMUTABLE_FL;
use slopos_abi::fs::{ACCOUNTS_STORE, ADMIN_GID, PERSON_UIDS, SESSION_UID};
use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};
use slopos_priv_core::{ENV_ACTION, ENV_FD, ENV_UID};
use slopos_slibc::process::{_exit, WEXITSTATUS, WIFEXITED, fork, waitpid};
use slopos_slibc::test_harness::note;
use slopos_userland::accounts::Store;
use slopos_userland::syscall::core::caps_drop;
use slopos_userland::syscall::process::{self, Endowment};
use slopos_userland::syscall::{SyscallError, fs};

const USERS: &str = "/bin/users";
const AUTHCHECK: &str = "/sbin/authcheck";
const NAME: &str = "accounts-test";
const RENAMED: &str = "accounts-renamed";
const OWN: &str = "org.slopos.accounts.password";
const MANAGE: &str = "org.slopos.accounts.manage";
const GROUPS: &str = "--groups";
const EPERM: i32 = 1;
const EBUSY: i32 = 16;

fn fail(why: impl AsRef<str>) -> bool {
    note(why.as_ref());
    eprintln!("accounts_test: {}", why.as_ref());
    false
}

struct Ran {
    code: Option<i32>,
    said: String,
}

impl Ran {
    fn ok(&self) -> bool {
        self.code == Some(0)
    }

    fn shown(&self) -> String {
        format!("status {:?}: {}", self.code, self.said.trim())
    }
}

/// `users` started holding nothing, so it asks privd, with `input` typed.
fn asked(args: &[&str], input: &str) -> Ran {
    let child = Command::new(USERS)
        .args(args)
        .env_remove(ENV_FD)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let Ok(mut child) = child else {
        return Ran {
            code: None,
            said: "users did not start".into(),
        };
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.as_bytes());
    }
    match child.wait_with_output() {
        Ok(out) => Ran {
            code: out.status.code(),
            said: format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        },
        Err(e) => Ran {
            code: None,
            said: e.to_string(),
        },
    }
}

/// `users` holding what privd gives the accounts' mechanisms, started as
/// privd starts it for `for_uid` under `action`, with `input` typed.
fn mechanism(action: &str, for_uid: u32, args: &[&str], input: &str) -> Ran {
    let env = [
        format!("{ENV_ACTION}={action}"),
        format!("{ENV_UID}={for_uid}"),
        "PATH=/bin:/sbin".to_owned(),
    ];
    spawned(USERS, args, &env, input, SEAL | DAC_OVERRIDE, None)
}

/// `program` holding `caps`, as `ids` when given, with `input` typed; what
/// it wrote to its stdout and stderr.
fn spawned(
    program: &str,
    args: &[&str],
    env: &[String],
    input: &str,
    caps: u64,
    ids: Option<(u32, &[u32])>,
) -> Ran {
    let broken = |why: &str| Ran {
        code: None,
        said: why.to_owned(),
    };
    let (Ok((stdin, typed)), Ok((out, out_end))) = (fs::pipe(), fs::pipe()) else {
        return broken("no pipes");
    };
    let name = program.rsplit('/').next().unwrap_or(program);
    let argv: Vec<CString> = std::iter::once(name)
        .chain(args.iter().copied())
        .map(|arg| CString::new(arg).expect("no NUL in an argument"))
        .collect();
    let argv: Vec<*const u8> = argv.iter().map(|arg| arg.as_ptr().cast()).collect();
    let env: Vec<CString> = env
        .iter()
        .map(|var| CString::new(var.as_str()).expect("no NUL in a variable"))
        .collect();
    let env: Vec<*const u8> = env.iter().map(|var| var.as_ptr().cast()).collect();
    let actions = [
        process::clone_fd(stdin.raw(), 0),
        process::clone_fd(out_end.raw(), 1),
        process::clone_fd(out_end.raw(), 2),
    ];
    let pid = process::spawn_endowed(
        program.as_bytes(),
        &argv,
        &env,
        b"/",
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &actions,
        0,
        Endowment {
            caps,
            ids: ids.map(|(uid, _)| (uid, uid)),
            groups: ids.map(|(_, groups)| groups),
            login: None,
        },
    );
    drop((stdin, out_end));
    if pid <= 0 {
        return broken("it did not start");
    }
    let _ = fs::write_slice(typed.raw(), input.as_bytes());
    drop(typed);
    let mut said = String::new();
    // SAFETY: `out` is a pipe end this function owns.
    let _ = unsafe { std::fs::File::from_raw_fd(out.into_raw()) }.read_to_string(&mut said);
    let code = match process::wait_exit_code(pid as u32) {
        -1 => None,
        code => Some(code),
    };
    Ran { code, said }
}

/// What `/sbin/authcheck` answers for `name` and `secret`.
fn authcheck(name: &str, secret: &str) -> Option<i32> {
    spawned(AUTHCHECK, &[], &[], &format!("{name}\n{secret}\n"), 0, None).code
}

fn record() -> Option<slopos_accounts_core::Accounts> {
    Store::read(Path::new("/")).recorded().ok().flatten()
}

fn uid_of(name: &str) -> Option<u32> {
    let name = CString::new(name).ok()?;
    // SAFETY: `name` is a C string that outlives the call.
    let row = unsafe { slopos_slibc::pwd::getpwnam(name.as_ptr()) };
    // SAFETY: a non-null row is the library's, valid until its next call.
    (!row.is_null()).then(|| unsafe { (*row).pw_uid })
}

fn sealed(path: &str) -> bool {
    std::fs::File::open(path).is_ok_and(|file| {
        fs::inode_flags(file.as_raw_fd()).is_ok_and(|flags| flags & FS_IMMUTABLE_FL != 0)
    })
}

fn the_one_person_is_the_first_administrator() -> bool {
    let Some(accounts) = record() else {
        return fail("init wrote no record of the one person");
    };
    let one = accounts.by_uid(SESSION_UID);
    let user = one.is_some_and(|p| p.name == "user" && p.admin && p.password.is_none());
    if !user || accounts.autologin().map(|p| p.uid) != Some(SESSION_UID) {
        return fail(format!("the record is {}", accounts.render()));
    }
    for path in ["/etc/passwd", "/etc/group", "/etc/shadow"] {
        if !sealed(path) {
            return fail(format!("{path} is not sealed"));
        }
    }
    let modes = [
        ("/etc/shadow", 0o100_600),
        ("/etc/passwd", 0o100_644),
        (ACCOUNTS_STORE, 0o040_700),
    ];
    for (path, mode) in modes {
        match std::fs::metadata(path) {
            Ok(meta) if meta.mode() == mode && meta.uid() == 0 => {}
            other => {
                return fail(format!(
                    "{path} is {:?}",
                    other.map(|m| (m.uid(), m.mode()))
                ));
            }
        }
    }
    let listed = asked(&["list"], "");
    listed.ok() && listed.said.contains("admin") || fail(format!("users list: {}", listed.shown()))
}

/// The supplementary groups this process holds, printed for its parent.
fn print_groups() -> ! {
    let mut groups = [0u32; 32];
    // SAFETY: `groups` holds as many gids as it is said to.
    let count = unsafe { slopos_slibc::process::ids::getgroups(32, groups.as_mut_ptr()) };
    let shown: Vec<String> = groups[..count.max(0) as usize]
        .iter()
        .map(u32::to_string)
        .collect();
    println!("{}", shown.join(" "));
    std::process::exit(0);
}

fn an_account_has_a_private_home_and_its_groups() -> bool {
    let added = asked(&["add", NAME, "--admin", "--keep-autologin"], "");
    let Some(uid) = uid_of(NAME) else {
        return fail(format!("users add: {}", added.shown()));
    };
    if !added.ok() || !PERSON_UIDS.contains(&uid) || uid == SESSION_UID {
        return fail(format!("{NAME} is uid {uid}: {}", added.shown()));
    }
    let home = format!("/home/{NAME}");
    match std::fs::symlink_metadata(&home) {
        Ok(meta)
            if meta.is_dir()
                && (meta.uid(), meta.gid(), meta.mode() & 0o7777) == (uid, uid, 0o700) => {}
        other => {
            return fail(format!(
                "{home} is {:?}",
                other.map(|m| (m.uid(), m.mode()))
            ));
        }
    }
    let name = CString::new(NAME).expect("no NUL");
    let mut groups = [0u32; 8];
    let mut room = groups.len() as i32;
    // SAFETY: `groups` holds `room` gids.
    let found = unsafe {
        slopos_slibc::pwd::getgrouplist(name.as_ptr(), uid, groups.as_mut_ptr(), &mut room)
    };
    if found != 2 || groups[..2] != [uid, ADMIN_GID] {
        return fail(format!("{NAME}'s groups are {found}: {groups:?}"));
    }
    let me = std::env::current_exe().map(|p| p.display().to_string());
    let me = me.unwrap_or_else(|_| "/bin/accounts_test".into());
    let seen = spawned(&me, &[GROUPS], &[], "", 0, Some((uid, &groups[..2])));
    let want = format!("{uid} {ADMIN_GID}");
    seen.said.trim() == want
        || fail(format!(
            "a process spawned as {NAME} holds {}",
            seen.shown()
        ))
}

fn a_password_is_set_checked_and_refused_when_wrong() -> bool {
    let set = asked(&["passwd", NAME], "first secret\nfirst secret\n");
    if !set.ok() {
        return fail(format!("users passwd {NAME}: {}", set.shown()));
    }
    let shadow = std::fs::read_to_string("/etc/shadow").unwrap_or_default();
    let line = shadow.lines().find(|l| l.starts_with(&format!("{NAME}:")));
    if !line.is_some_and(|l| l.contains(":$argon2id$v=19$m=65536,t=3,p=4$")) {
        return fail(format!("/etc/shadow holds {line:?}"));
    }
    let answers = [
        authcheck(NAME, "first secret"),
        authcheck(NAME, "first secreT"),
        authcheck(NAME, ""),
        authcheck("user", ""),
        authcheck("user", "first secret"),
        authcheck("nobody-here", "first secret"),
    ];
    if answers != [Some(0), Some(1), Some(1), Some(1), Some(1), Some(1)] {
        return fail(format!("authcheck answered {answers:?}"));
    }
    let differing = asked(&["passwd", NAME], "second\nsecond?\n");
    !differing.ok() && authcheck(NAME, "first secret") == Some(0)
        || fail(format!("two different passwords: {}", differing.shown()))
}

fn ones_own_password_takes_the_current_one() -> bool {
    let Some(uid) = uid_of(NAME) else {
        return fail(format!("{NAME} is gone"));
    };
    let wrong = mechanism(OWN, uid, &["passwd"], "not it\nsecond\nsecond\n");
    if wrong.ok() || authcheck(NAME, "first secret") != Some(0) {
        return fail(format!("a wrong current password: {}", wrong.shown()));
    }
    let right = mechanism(OWN, uid, &["passwd"], "first secret\nsecond\nsecond\n");
    if !right.ok() {
        return fail(format!("the current password: {}", right.shown()));
    }
    let answers = [authcheck(NAME, "second"), authcheck(NAME, "first secret")];
    if answers != [Some(0), Some(1)] {
        return fail(format!("after the change authcheck answered {answers:?}"));
    }
    let mismatched = mechanism(MANAGE, uid, &["passwd"], "");
    !mismatched.ok() || fail("the manage action changed an own password")
}

fn only_an_administrator_manages_and_the_last_stays() -> bool {
    let Some(uid) = uid_of(NAME) else {
        return fail(format!("{NAME} is gone"));
    };
    let demoted = asked(&["demote", NAME], "");
    if !demoted.ok() || record().and_then(|r| r.person(NAME).map(|p| p.admin)) != Some(false) {
        return fail(format!("users demote {NAME}: {}", demoted.shown()));
    }
    let by_them = mechanism(MANAGE, uid, &["promote", NAME], "");
    if by_them.ok() || !by_them.said.contains("does not administer") {
        return fail(format!("{NAME} promoted themselves: {}", by_them.shown()));
    }
    for args in [&["demote", "user"][..], &["remove", "user"]] {
        let refused = asked(args, "");
        if refused.ok() {
            return fail(format!("users {}: {}", args.join(" "), refused.shown()));
        }
    }
    let admins = record().map(|r| r.admins().map(|p| p.name.clone()).collect::<Vec<_>>());
    admins == Some(vec!["user".to_owned()]) || fail(format!("the administrators are {admins:?}"))
}

/// Run `body` in a child that holds only `caps` of the gated capabilities.
fn holding(caps: u64, body: fn() -> bool) -> bool {
    // SAFETY: the child runs `body` and exits; nothing it shares is locked.
    let pid = unsafe { fork() };
    if pid == 0 {
        caps_drop(ALL & !caps);
        // SAFETY: ends the child.
        unsafe { _exit(i32::from(!body())) };
    }
    if pid < 0 {
        return fail("fork failed");
    }
    let mut status = 0;
    // SAFETY: `status` outlives the call.
    unsafe { waitpid(pid, &mut status, 0) };
    WIFEXITED(status) && WEXITSTATUS(status) == 0
}

fn errno_of<T>(result: std::io::Result<T>) -> i32 {
    result.map_or_else(|e| e.raw_os_error().unwrap_or(-1), |_| 0)
}

fn expect(what: &str, got: i32, want: i32) -> bool {
    got == want || fail(format!("{what}: errno {got}, want {want}"))
}

fn dac_override_reaches_no_database() -> bool {
    let unseal = std::fs::File::open("/etc/group")
        .map(|file| fs::set_inode_flags(file.as_raw_fd(), 0).map_err(|e| e.errno()));
    let staged = std::fs::write("/etc/accounts_test.group", b"admin:x:10:mallory\n");
    let swapped = std::fs::rename("/etc/accounts_test.group", "/etc/group");
    let _ = std::fs::remove_file("/etc/accounts_test.group");
    expect(
        "writing /etc/group",
        errno_of(std::fs::OpenOptions::new().write(true).open("/etc/group")),
        EPERM,
    ) && expect(
        "removing /etc/group",
        errno_of(std::fs::remove_file("/etc/group")),
        EPERM,
    ) && (staged.is_ok() || fail("/etc took no new file"))
        && expect("renaming over /etc/group", errno_of(swapped), EPERM)
        && expect(
            "unsealing /etc/group",
            unseal.ok().and_then(Result::err).unwrap_or(0),
            EPERM,
        )
        && expect(
            "writing in the store",
            errno_of(std::fs::write(format!("{ACCOUNTS_STORE}/record.99"), b"x")),
            EPERM,
        )
        && expect(
            "moving the store",
            errno_of(std::fs::rename(ACCOUNTS_STORE, "/var/lib/slopos/moved")),
            EBUSY,
        )
        && expect(
            "moving /etc",
            errno_of(std::fs::rename("/etc", "/etc.moved")),
            EBUSY,
        )
}

fn the_database_holds_against_dac_override() -> bool {
    holding(DAC_OVERRIDE, dac_override_reaches_no_database)
}

const MOUNTED_HERE: &str = "/tmp/accounts_test.mnt";
const UNDER_ETC: &str = "/etc/accounts_test.d";

fn mount_covers_no_database() -> bool {
    let refused = |target: &str| {
        let got = fs::mount(b"", target.as_bytes(), b"ramfs", 0).map_or_else(|e| e.errno(), |()| 0);
        expect(
            &format!("mounting at {target}"),
            got,
            SyscallError::EPERM.errno(),
        )
    };
    let control = fs::mount(b"", MOUNTED_HERE.as_bytes(), b"ramfs", 0);
    let unmounted = CString::new(MOUNTED_HERE).map(|path| fs::umount2(path.as_ptr(), 0));
    ["/etc", UNDER_ETC, "/var/lib/slopos", ACCOUNTS_STORE]
        .into_iter()
        .all(refused)
        && (control.is_ok() && matches!(unmounted, Ok(Ok(())))
            || fail(format!("the control mount at {MOUNTED_HERE}: {control:?}")))
}

fn the_database_holds_against_mount() -> bool {
    for dir in [MOUNTED_HERE, UNDER_ETC] {
        if std::fs::create_dir_all(dir).is_err() {
            return fail(format!("{dir} could not be made"));
        }
    }
    let held = holding(MOUNT, mount_covers_no_database);
    let _ = std::fs::remove_dir(UNDER_ETC);
    let _ = std::fs::remove_dir(MOUNTED_HERE);
    held
}

fn a_renamed_or_removed_account_keeps_its_uid_given() -> bool {
    let Some(uid) = uid_of(NAME) else {
        return fail(format!("{NAME} is gone"));
    };
    let renamed = asked(&["rename", NAME, RENAMED], "");
    let home = format!("/home/{NAME}");
    if !renamed.ok() || uid_of(RENAMED) != Some(uid) || uid_of(NAME).is_some() {
        return fail(format!("users rename: {}", renamed.shown()));
    }
    let removed = asked(&["remove", RENAMED, "--home"], "");
    if !removed.ok() || uid_of(RENAMED).is_some() || Path::new(&home).exists() {
        return fail(format!("users remove --home: {}", removed.shown()));
    }
    let again = asked(&["add", NAME, "--keep-autologin"], "");
    let given = uid_of(NAME);
    let kept = asked(&["remove", NAME], "");
    let left = std::fs::symlink_metadata(&home).is_ok_and(|m| m.is_dir());
    let _ = std::fs::remove_dir(&home);
    if given.is_none_or(|given| given <= uid) || !again.ok() {
        return fail(format!(
            "added again as {given:?}, once {uid}: {}",
            again.shown()
        ));
    }
    kept.ok() && left || fail(format!("users remove kept no home: {}", kept.shown()))
}

fn clean_up() -> bool {
    for name in [NAME, RENAMED] {
        if uid_of(name).is_some() {
            let _ = asked(&["promote", "user"], "");
            let _ = asked(&["remove", name, "--home"], "");
        }
    }
    let one = record().is_some_and(|r| r.people().len() == 1 && r.admins().count() == 1);
    let _ = std::fs::remove_dir_all(format!("/home/{NAME}"));
    one || fail("the record does not hold the one person alone again")
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some(GROUPS) {
        print_groups();
    }
    slopos_slibc::test_harness::run(&[
        (
            "the_one_person_is_the_first_administrator",
            the_one_person_is_the_first_administrator,
        ),
        (
            "an_account_has_a_private_home_and_its_groups",
            an_account_has_a_private_home_and_its_groups,
        ),
        (
            "a_password_is_set_checked_and_refused_when_wrong",
            a_password_is_set_checked_and_refused_when_wrong,
        ),
        (
            "ones_own_password_takes_the_current_one",
            ones_own_password_takes_the_current_one,
        ),
        (
            "only_an_administrator_manages_and_the_last_stays",
            only_an_administrator_manages_and_the_last_stays,
        ),
        (
            "the_database_holds_against_dac_override",
            the_database_holds_against_dac_override,
        ),
        (
            "the_database_holds_against_mount",
            the_database_holds_against_mount,
        ),
        (
            "a_renamed_or_removed_account_keeps_its_uid_given",
            a_renamed_or_removed_account_keeps_its_uid_given,
        ),
        ("clean_up", clean_up),
    ]);
}
