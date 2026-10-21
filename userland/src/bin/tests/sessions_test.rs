//! The sessions init keeps, driven as people at the machine drive them: the
//! boot logs its one person straight into their desktop; a second person
//! logs in at the login screen; the screen, the keyboard and the clipboard
//! follow the desktop in front; a request from a session behind is refused
//! unasked; a locked desktop refuses a wrong password; a console login gets
//! a shell of its own, whose requests are asked at its terminal; and one
//! person's fork bomb leaves the other's session running.
//!
//! The runner hands this binary every capability as uid 0; it answers the
//! screens through `test_key`, which hands the input sink's holder a key.

#[path = "support/asking.rs"]
mod asking;
#[path = "support/sessions.rs"]
mod sessions;

use std::ffi::CString;
use std::io::{BufRead, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use slopos_abi::fs::{GREETER_UID, SESSION_UID};
use slopos_abi::input::keycode::{KEY_DELETE, KEY_ENTER, KEY_LEFTALT, KEY_LEFTCTRL, KEY_TAB};
use slopos_abi::syscall::{O_CLOEXEC, POLLIN, UserPollFd};
use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};
use slopos_priv_core::ENV_FD;
use slopos_userland::init_control::{self, Command as Init};
use slopos_userland::sessions::{self as listed, Class};
use slopos_userland::syscall::core::test_key;
use slopos_userland::syscall::process::{self, Endowment};
use slopos_userland::syscall::{OwnedFd, fs, input, net};

use asking::Asker;
use sessions::{SESSION_DEADLINE, entry, in_front, until};

const SELF: &str = "/bin/sessions_test";
const CAROL: &str = "carol";
const CAROL_PASSWORD: &str = "carol-pw";
/// `argv[1]` of a child that copies `argv[2]` to its session's clipboard.
const COPY: &str = "--copy";
/// `argv[1]` of a child that prints its session's clipboard.
const PASTE: &str = "--paste";
/// `argv[1]` of a child that forks lingering copies of itself until it is
/// refused, then copies that exit unreaped until it is refused again, says
/// how many of each it made, and lingers.
const BOMB: &str = "--bomb";
/// What the person asking with no session in front is refused with.
const UNREACHED: &str = "neither on the screen";
const CONFIGURE: [&str; 5] = ["org.slopos.net.configure", "link", "set", "lo", "up"];
/// Longer than the lock screen waits, after a wrong password, before it
/// takes keys again.
const AFTER_A_WRONG_PASSWORD: Duration = Duration::from_secs(6);

fn note(why: &str) {
    eprintln!("sessions_test: {why}");
}

fn users(args: &[&str], input: &str) -> Option<i32> {
    let mut child = Command::new("/bin/users")
        .args(args)
        .env_remove(ENV_FD)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.as_bytes());
    }
    child.wait().ok()?.code()
}

fn carol() -> Option<u32> {
    slopos_userland::accounts::uid_of(CAROL)
}

fn desktop_of(uid: u32) -> Option<u32> {
    listed::read()
        .into_iter()
        .find(|s| s.uid == uid && s.class == Class::User)
        .map(|s| s.tag)
}

fn ask_init(command: Init) -> bool {
    init_control::ask(command)
        .map_err(|why| note(&format!("{}: {why}", command.word())))
        .is_ok()
}

fn greeter_in_front() -> bool {
    until(SESSION_DEADLINE, || {
        listed::read()
            .iter()
            .any(|s| s.active && s.class == Class::Greeter)
    }) || {
        note(&format!(
            "no login screen came in front: {:?}",
            listed::read()
        ));
        false
    }
}

fn locked_within(tag: u32, locked: bool, within: Duration) -> bool {
    until(within, || entry(tag).is_some_and(|s| s.locked == locked))
}

/// A child of this binary as `uid` carrying `login`, its stdout in a pipe.
fn spawn_self(uid: u32, login: u32, args: &[&str]) -> Option<(u32, OwnedFd)> {
    let null = fs::open_cstr(c"/dev/null", slopos_abi::fs::O_RDWR | O_CLOEXEC as u32).ok()?;
    spawn_self_with(uid, login, args, null.raw())
}

/// [`spawn_self`] reading `input`.
fn spawn_self_with(
    uid: u32,
    login: u32,
    args: &[&str],
    input: slopos_userland::syscall::RawFd,
) -> Option<(u32, OwnedFd)> {
    let (out, out_end) = fs::pipe2(O_CLOEXEC as u32).ok()?;
    let owned: Vec<CString> = std::iter::once(SELF)
        .chain(args.iter().copied())
        .map(|arg| CString::new(arg).expect("no NUL in an argument"))
        .collect();
    let argv: Vec<*const u8> = owned.iter().map(|arg| arg.as_ptr().cast()).collect();
    let stdio = [
        process::clone_fd(input, 0),
        process::clone_fd(out_end.raw(), 1),
        process::clone_fd(2, 2),
    ];
    let pid = process::spawn_endowed(
        SELF.as_bytes(),
        &argv,
        &[],
        b"/",
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &stdio,
        0,
        Endowment {
            caps: 0,
            ids: Some((uid, uid)),
            groups: Some(&[]),
            login: Some(login),
        },
    );
    (pid > 0).then_some((pid as u32, out))
}

fn read_all(fd: &OwnedFd) -> String {
    let mut said = Vec::new();
    let mut buf = [0u8; 256];
    while let Ok(n) = fs::read_slice(fd.raw(), &mut buf) {
        if n == 0 {
            break;
        }
        said.extend_from_slice(&buf[..n]);
    }
    String::from_utf8_lossy(&said).trim().to_owned()
}

/// What a child of this binary printed and how it exited.
fn run_self(uid: u32, login: u32, args: &[&str]) -> Option<(i32, String)> {
    let (pid, out) = spawn_self(uid, login, args)?;
    let code = process::wait_exit_code(pid);
    Some((code, read_all(&out)))
}

fn press(keycode: u16, pressed: bool) {
    let rc = test_key(keycode, 0, pressed);
    if rc != 0 {
        note(&format!("test_key {keycode:#x}: {rc}"));
    }
    std::thread::sleep(Duration::from_millis(20));
}

/// Wait for the screen in front to take keys, then type at it.
fn typed_in_front(keys: impl FnOnce()) {
    std::thread::sleep(Duration::from_millis(1500));
    keys();
}

/// The boot's own session is its one person's desktop: no login screen.
fn one_person_boot_goes_straight_to_the_desktop() -> bool {
    if !ask_init(Init::RestartSession) {
        return false;
    }
    let Some(front) = in_front(SESSION_UID, SESSION_DEADLINE) else {
        note(&format!(
            "no desktop of the first person came in front: {:?}",
            listed::read()
        ));
        return false;
    };
    if listed::read().iter().any(|s| s.class == Class::Greeter) || front.locked {
        note(&format!(
            "the boot showed more than the desktop: {:?}",
            listed::read()
        ));
        return false;
    }
    true
}

/// A second person logs in at the login screen, which the first person's
/// desktop goes behind and locks for.
fn a_second_person_logs_in_at_the_login_screen() -> bool {
    let password = format!("{CAROL_PASSWORD}\n{CAROL_PASSWORD}\n");
    let added = [
        users(&["add", CAROL, "--admin", "--keep-autologin"], ""),
        users(&["passwd", CAROL], &password),
    ];
    let (Some(carol), Some(first)) = (carol(), desktop_of(SESSION_UID)) else {
        note(&format!(
            "{CAROL} was not added, or no desktop runs: {added:?}"
        ));
        return false;
    };
    if !ask_init(Init::Greeter) {
        return false;
    }
    if !greeter_in_front() || !locked_within(first, true, SESSION_DEADLINE) {
        note(&format!(
            "the login screen did not come in front over a locked desktop: {:?}",
            listed::read()
        ));
        return false;
    }
    typed_in_front(|| {
        asking::type_text(CAROL);
        asking::key(KEY_TAB, 0);
        asking::type_text(CAROL_PASSWORD);
        asking::key(KEY_ENTER, 0);
    });
    if in_front(carol, SESSION_DEADLINE).is_none() {
        note(&format!(
            "{CAROL}'s desktop did not come in front: {:?}",
            listed::read()
        ));
        return false;
    }
    let runtime = std::fs::metadata(runtime_dir(carol));
    let theirs = runtime.as_ref().is_ok_and(|m| {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        m.uid() == carol && m.permissions().mode() & 0o7777 == 0o700
    });
    if !theirs {
        note(&format!("{CAROL}'s runtime directory is {runtime:?}"));
        return false;
    }
    if !until(SESSION_DEADLINE, || {
        listed::read().iter().all(|s| s.class != Class::Greeter)
    }) {
        note("the login screen stayed once it had logged someone in");
        return false;
    }
    true
}

/// Each desktop's processes reach its own clipboard alone.
fn each_session_has_its_own_clipboard() -> bool {
    let (Some(carol), Some(theirs), Some(first)) = (
        carol(),
        carol().and_then(desktop_of),
        desktop_of(SESSION_UID),
    ) else {
        return false;
    };
    let copied = [
        run_self(SESSION_UID, first, &[COPY, "the first person's"]),
        run_self(carol, theirs, &[COPY, "carol's"]),
    ];
    let pasted = [
        run_self(SESSION_UID, first, &[PASTE]),
        run_self(carol, theirs, &[PASTE]),
    ];
    let wanted = [
        Some((0, "the first person's".to_owned())),
        Some((0, "carol's".to_owned())),
    ];
    if pasted != wanted {
        note(&format!("copied {copied:?}, pasted {pasted:?}"));
        return false;
    }
    true
}

/// A request from the desktop behind is refused unasked; one from the
/// desktop in front is put to its person.
fn a_request_from_the_session_behind_is_refused() -> bool {
    let (Some(carol), Some(theirs), Some(first)) = (
        carol(),
        carol().and_then(desktop_of),
        desktop_of(SESSION_UID),
    ) else {
        return false;
    };
    let asker = |uid, login| Asker {
        path: SELF,
        uid,
        login: Some(login),
        cwd: "/",
        with_cwd: false,
    };
    let behind = asker(SESSION_UID, first).ask(&CONFIGURE, None);
    if !behind.contains(UNREACHED) {
        note(&format!("the request from the desktop behind: {behind}"));
        return false;
    }
    let front = asker(carol, theirs).ask(&CONFIGURE, Some(asking::escape));
    if !front.starts_with("denied by the person") {
        note(&format!("the request from the desktop in front: {front}"));
        return false;
    }
    true
}

/// Brought in front again, the first person's locked desktop takes the
/// keyboard: Enter unlocks it for a person with no password. The second
/// person's goes behind and locks.
fn the_keyboard_follows_the_desktop_in_front() -> bool {
    let (Some(theirs), Some(first)) = (carol().and_then(desktop_of), desktop_of(SESSION_UID))
    else {
        return false;
    };
    if !ask_init(Init::Switch(first)) || in_front(SESSION_UID, SESSION_DEADLINE).is_none() {
        note("the first person's desktop did not come in front again");
        return false;
    }
    typed_in_front(|| asking::key(KEY_ENTER, 0));
    if !locked_within(first, false, SESSION_DEADLINE)
        || !locked_within(theirs, true, SESSION_DEADLINE)
    {
        note(&format!(
            "the desktops did not swap their locks: {:?}",
            listed::read()
        ));
        return false;
    }
    true
}

/// The second person's locked desktop, in front again, refuses a wrong
/// password and their requests, takes the right password, and locks again
/// at Ctrl+Alt+Delete.
fn a_locked_session_refuses_a_wrong_password() -> bool {
    let (Some(carol), Some(theirs)) = (carol(), carol().and_then(desktop_of)) else {
        return false;
    };
    if !ask_init(Init::Switch(theirs)) || in_front(carol, SESSION_DEADLINE).is_none() {
        note(&format!("{CAROL}'s desktop did not come in front again"));
        return false;
    }
    let asked = Asker {
        path: SELF,
        uid: carol,
        login: Some(theirs),
        cwd: "/",
        with_cwd: false,
    }
    .ask(&CONFIGURE, None);
    if !asked.contains(UNREACHED) {
        note(&format!("the request from a locked desktop: {asked}"));
        return false;
    }
    let failures = format!("/run/slopos-auth/{CAROL}");
    let _ = std::fs::remove_file(&failures);
    typed_in_front(|| {
        asking::type_text("not-carols");
        asking::key(KEY_ENTER, 0);
    });
    let counted = until(SESSION_DEADLINE, || {
        std::path::Path::new(&failures).exists()
    });
    std::thread::sleep(AFTER_A_WRONG_PASSWORD);
    if !counted || !entry(theirs).is_some_and(|s| s.locked) {
        note("a wrong password unlocked the desktop");
        return false;
    }
    asking::type_text(CAROL_PASSWORD);
    asking::key(KEY_ENTER, 0);
    if !locked_within(theirs, false, SESSION_DEADLINE) {
        note("the right password did not unlock the desktop");
        return false;
    }
    for key in [KEY_LEFTCTRL, KEY_LEFTALT] {
        press(key, true);
    }
    press(KEY_DELETE, true);
    press(KEY_DELETE, false);
    for key in [KEY_LEFTALT, KEY_LEFTCTRL] {
        press(key, false);
    }
    if !locked_within(theirs, true, SESSION_DEADLINE) {
        note("Ctrl+Alt+Delete did not lock the desktop");
        return false;
    }
    typed_in_front(|| {
        asking::type_text(CAROL_PASSWORD);
        asking::key(KEY_ENTER, 0);
    });
    locked_within(theirs, false, SESSION_DEADLINE)
}

/// A terminal this test types at as a person would.
struct Console {
    master: OwnedFd,
    seen: String,
}

impl Console {
    fn open() -> Option<(Console, OwnedFd)> {
        let (master, _) = process::openpty().ok()?;
        let slave = fs::ioctl_tiocgptpeer(master.raw()).ok()?;
        let _ = net::set_nonblocking(master.raw());
        let console = Console {
            master,
            seen: String::new(),
        };
        Some((console, slave))
    }

    fn type_line(&self, line: &str) {
        let _ = fs::write_slice(self.master.raw(), format!("{line}\n").as_bytes());
    }

    /// Wait for `text` to appear, keeping what follows it.
    fn wait_for(&mut self, text: &str, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        loop {
            if let Some(at) = self.seen.find(text) {
                self.seen.drain(..at + text.len());
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                note(&format!(
                    "waited for {text:?}; the terminal said {:?}",
                    self.seen
                ));
                return false;
            }
            let mut fds = [UserPollFd {
                fd: self.master.raw(),
                events: POLLIN,
                revents: 0,
            }];
            let _ = fs::poll(&mut fds, left.as_millis().min(500) as i64);
            let mut buf = [0u8; 512];
            if let Ok(n) = fs::read_slice(self.master.raw(), &mut buf) {
                self.seen.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        }
    }
}

/// The console's prompt, as init runs it on its own terminal, logs the
/// second person in to a shell of its own session; a request from it that
/// needs their password asks at that terminal; and the session ends with
/// the shell.
fn a_console_login_starts_a_shell_of_its_own() -> bool {
    let Some(carol) = carol() else {
        return false;
    };
    let Some((mut console, slave)) = Console::open() else {
        note("no terminal to type at");
        return false;
    };
    let stdio = [
        process::clone_fd(slave.raw(), 0),
        process::clone_fd(slave.raw(), 1),
        process::clone_fd(slave.raw(), 2),
    ];
    let login = process::spawn_endowed(
        b"/bin/login",
        &[c"login".as_ptr().cast()],
        &[],
        b"/",
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &stdio,
        0,
        Endowment {
            caps: 0,
            ids: Some((GREETER_UID, GREETER_UID)),
            groups: Some(&[]),
            login: None,
        },
    );
    drop(slave);
    if login <= 0 {
        note(&format!("/bin/login did not start: {login}"));
        return false;
    }
    let login = login as u32;
    let outcome = (|| {
        if !console.wait_for("login: ", SESSION_DEADLINE) {
            return false;
        }
        console.type_line(CAROL);
        if !console.wait_for("Password: ", SESSION_DEADLINE) {
            return false;
        }
        console.type_line(CAROL_PASSWORD);
        if !console.wait_for("/home/carol", SESSION_DEADLINE) {
            return false;
        }
        let session = listed::read()
            .into_iter()
            .find(|s| s.uid == carol && s.class == Class::Console);
        if !session.as_ref().is_some_and(|s| s.tty.is_some()) {
            note(&format!(
                "no console session of {CAROL}: {:?}",
                listed::read()
            ));
            return false;
        }
        console.type_line("whoami | tr a-z A-Z");
        if !console.wait_for("CAROL", SESSION_DEADLINE) {
            return false;
        }
        console.type_line("priv /bin/true; echo status=$?");
        if !console.wait_for("password: ", SESSION_DEADLINE) {
            return false;
        }
        console.type_line(CAROL_PASSWORD);
        if !console.wait_for("status=0", SESSION_DEADLINE) {
            return false;
        }
        console.type_line("exit");
        let tag = session.map(|s| s.tag).unwrap_or_default();
        if !until(SESSION_DEADLINE, || entry(tag).is_none()) {
            note("the console session outlived its shell");
            return false;
        }
        console.wait_for("login: ", SESSION_DEADLINE)
    })();
    let _ = process::kill(login, slopos_abi::signal::SIGKILL);
    let _ = process::waitpid(login);
    outcome
}

/// One person's processes share a ceiling no other person's spend: the
/// second person's children left unreaped stop at their share of tasks,
/// which the rest of their session holds a little of, and their fork bomb
/// at their share of processes, while the first person's session still
/// starts what it asks; ending the session ends the bomb.
fn one_person_s_fork_bomb_leaves_the_other_s_session_running() -> bool {
    let (Some(carol), Some(theirs), Some(first)) = (
        carol(),
        carol().and_then(desktop_of),
        desktop_of(SESSION_UID),
    ) else {
        return false;
    };
    let Ok((go, go_end)) = fs::pipe2(O_CLOEXEC as u32) else {
        return false;
    };
    let Some((bomb, out)) = spawn_self_with(carol, theirs, &[BOMB], go.raw()) else {
        note("the bomb did not start");
        return false;
    };
    drop(go);
    let first_runs = || {
        run_self(SESSION_UID, first, &[PASTE]).is_some_and(|(code, _)| code == 0)
            && entry(first).and_then(|s| s.leader).is_some_and(alive)
    };
    let unreaped = said_count(&out);
    let beside_zombies = first_runs();
    let _ = fs::write_slice(go_end.raw(), b"\n");
    let live = said_count(&out);
    let beside_the_bomb = first_runs();
    let ended = ask_init(Init::End(theirs));
    let gone = until(SESSION_DEADLINE, || {
        init_control::processes(theirs).is_empty()
            && !std::path::Path::new(&runtime_dir(carol)).exists()
    });
    let _ = process::waitpid(bomb);
    let bounded = unreaped
        .is_some_and(|n| n > PERSON_TASKS - PERSON_PROCESSES && n <= PERSON_TASKS)
        && live.is_some_and(|n| n > 0 && n <= PERSON_PROCESSES);
    if !bounded || !beside_zombies || !beside_the_bomb || !ended || !gone {
        note(&format!(
            "the bomb left {unreaped:?} unreaped and forked {live:?}; the first person's \
             session ran beside them: {beside_zombies}, {beside_the_bomb}; ended {ended}, \
             gone {gone}"
        ));
        return false;
    }
    true
}

/// The next count the bomb says, within ten minutes.
fn said_count(out: &OwnedFd) -> Option<u32> {
    let mut fds = [UserPollFd {
        fd: out.raw(),
        events: POLLIN,
        revents: 0,
    }];
    if !matches!(fs::poll(&mut fds, 600_000), Ok(n) if n > 0) {
        return None;
    }
    let mut buf = [0u8; 32];
    let n = fs::read_slice(out.raw(), &mut buf).ok()?;
    std::str::from_utf8(&buf[..n]).ok()?.trim().parse().ok()
}

fn runtime_dir(uid: u32) -> String {
    format!("/run/user/{uid}")
}

fn alive(pid: u32) -> bool {
    process::kill(pid, 0) == 0
}

/// With nobody's desktop in front, the login screen is; the first person
/// logs in by their name alone, having no password, to the desktop they
/// left, which it unlocks.
fn the_login_screen_takes_the_place_of_a_session_logged_out() -> bool {
    let Some(first) = desktop_of(SESSION_UID) else {
        return false;
    };
    if !greeter_in_front() {
        return false;
    }
    typed_in_front(|| {
        asking::type_text("user");
        asking::key(KEY_TAB, 0);
        asking::key(KEY_ENTER, 0);
    });
    let back = in_front(SESSION_UID, SESSION_DEADLINE);
    if back.as_ref().is_none_or(|s| s.tag != first)
        || !locked_within(first, false, SESSION_DEADLINE)
    {
        note(&format!(
            "the first person's own desktop did not come back: {:?}",
            listed::read()
        ));
        return false;
    }
    true
}

fn the_sessions_end_and_the_person_added_goes() -> bool {
    let ended = ask_init(Init::EndSession);
    let removed = users(&["remove", CAROL, "--home"], "");
    if !ended || carol().is_some() || !listed::read().is_empty() {
        note(&format!(
            "ended {ended}, {CAROL} removed {removed:?}: {:?}",
            listed::read()
        ));
        return false;
    }
    true
}

const CASES: &[(&str, fn() -> bool)] = &[
    (
        "one_person_boot_goes_straight_to_the_desktop",
        one_person_boot_goes_straight_to_the_desktop,
    ),
    (
        "a_second_person_logs_in_at_the_login_screen",
        a_second_person_logs_in_at_the_login_screen,
    ),
    (
        "each_session_has_its_own_clipboard",
        each_session_has_its_own_clipboard,
    ),
    (
        "a_request_from_the_session_behind_is_refused",
        a_request_from_the_session_behind_is_refused,
    ),
    (
        "the_keyboard_follows_the_desktop_in_front",
        the_keyboard_follows_the_desktop_in_front,
    ),
    (
        "a_locked_session_refuses_a_wrong_password",
        a_locked_session_refuses_a_wrong_password,
    ),
    (
        "a_console_login_starts_a_shell_of_its_own",
        a_console_login_starts_a_shell_of_its_own,
    ),
    (
        "one_person_s_fork_bomb_leaves_the_other_s_session_running",
        one_person_s_fork_bomb_leaves_the_other_s_session_running,
    ),
    (
        "the_login_screen_takes_the_place_of_a_session_logged_out",
        the_login_screen_takes_the_place_of_a_session_logged_out,
    ),
    (
        "the_sessions_end_and_the_person_added_goes",
        the_sessions_end_and_the_person_added_goes,
    ),
];

/// A person's share of the process and task tables.
const PERSON_PROCESSES: u32 = (slopos_abi::task::MAX_PROCESSES / 4) as u32;
const PERSON_TASKS: u32 = (slopos_abi::task::MAX_TASKS / 4) as u32;

/// Fork until refused for a second on end, each child running `child`, so
/// a refusal that lasts, not one for a child still on its way out, stops
/// it, or past the person's share; answer how many forked.
fn fork_until_refused(child: fn() -> !) -> u32 {
    const REFUSALS: u32 = 20;
    let (mut forked, mut refused) = (0, 0);
    while refused < REFUSALS && forked <= PERSON_TASKS {
        match process::fork() {
            0 => child(),
            pid if pid < 0 => {
                refused += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => {
                (forked, refused) = (forked + 1, 0);
                if forked % 256 == 0 {
                    eprintln!("sessions_test: the bomb has forked {forked}");
                }
            }
        }
    }
    forked
}

/// Fill this person's share with children left unreaped and say how many;
/// once told on standard input, reap them, fill it with children that
/// linger, say how many, and linger.
fn bomb() -> i32 {
    let say = |n: u32| {
        println!("{n}");
        let _ = std::io::stdout().flush();
    };
    say(fork_until_refused(|| {
        slopos_userland::syscall::core::exit_with_code(0)
    }));
    let _ = std::io::stdin().lock().read_line(&mut String::new());
    while process::wait_with(-1, 0).is_some() {}
    say(fork_until_refused(|| {
        loop {
            slopos_userland::syscall::core::sleep_ms(60_000);
        }
    }));
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some(asking::REQUEST) => std::process::exit(asking::request_child(&args[2..])),
        Some(COPY) => {
            let text = args.get(2).map_or(&b""[..], |t| t.as_bytes());
            std::process::exit(i32::from(input::clipboard_copy(text) != text.len()));
        }
        Some(PASTE) => {
            let mut buf = [0u8; 256];
            let n = input::clipboard_paste(&mut buf);
            print!("{}", String::from_utf8_lossy(&buf[..n]));
        }
        Some(BOMB) => std::process::exit(bomb()),
        _ => slopos_slibc::test_harness::run(CASES),
    }
}
