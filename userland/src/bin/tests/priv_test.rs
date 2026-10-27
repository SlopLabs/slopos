//! `priv` end to end: a command runs as whoever asked, holding the ceiling and
//! nothing of theirs but its directory; it is found only where only uid 0 can
//! change it; `priv -s` hands its power on at a terminal of its own and gives
//! the terminal back; an administrator's click allows it and is kept for the
//! command and directory alone; a standard user is answered by an
//! administrator's password; and what it runs edits a uid-0 file in `/etc`,
//! while the raw disk, the base, the user database and a sealed file refuse
//! it and a mechanism it runs still asks.
//!
//! The runner hands this binary every capability as uid 0, which privd's
//! `root` admits to `priv` unasked. A person's request needs a desktop to ask
//! them at, which a case asks init for; their answers are keys the kernel
//! delivers to the compositor as a keyboard's.

#[path = "support/asking.rs"]
mod asking;
#[path = "support/sessions.rs"]
mod sessions;

use std::fs::File;
use std::os::fd::FromRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use slopos_abi::caps::UNIVERSAL;
use slopos_abi::fs::{ADMIN_GID, DEFAULT_PATH};
use slopos_abi::syscall::{O_CLOEXEC, POLLIN, UserPollFd, UserWinsize};
use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};
use slopos_priv_core::REQUESTER_CEILING;
use slopos_priv_core::consent::WRONG_PASSWORD;
use slopos_priv_core::wire::{Exit, Reply, Request};
use slopos_slibc::process::ids::{getgroups, getresgid, getresuid};
use slopos_slibc::test_harness::note;
use slopos_userland::init_control::{self, Command as InitCommand};
use slopos_userland::privd::{Connection, STATUS_REFUSED, WAITING, request_id};
use slopos_userland::root_layout;
use slopos_userland::sealed::{is_sealed, set_sealed};
use slopos_userland::syscall::core::caps_drop;
use slopos_userland::syscall::process::{self, Endowment};
use slopos_userland::syscall::{OwnedFd, fs, net};

const SELF: &str = "/bin/priv_test";
const PRIV: &str = "/bin/priv";
const USER: u32 = 1000;
/// `argv[1]` of this binary run through `priv`, which says what it runs with.
const PROBE: &str = "--probe";
/// `argv[1]` of this binary run through `priv`, which says it runs and waits
/// to be interrupted.
const LINGER: &str = "--linger";
const LINGERING: &str = "lingering";
const ALICE: &str = "priv-alice";
const ALICE_PASSWORD: &str = "alice-pw";
const BOB: &str = "priv-bob";
const CONF: &str = "/etc/priv_test.conf";
const SEALED: &str = "/etc/priv_test.sealed";
const PLANTED: &str = "/bin/priv_test.planted";
const LOCAL_BIN: &str = "/home/user/.local/bin";
const SHADOW: &str = "priv-test-shadow";
const LINK: &str = "/opt/bin/priv-test-link";
/// Where only uid 0 writes, but the person's own.
const OWNED: &str = "/opt/bin/priv-test-owned";
/// Where only uid 0 writes, but writable by its group.
const WRITABLE: &str = "/opt/bin/priv-test-writable";
/// Where only uid 0 writes, but a script whose interpreter the person owns.
const INTERPRETED: &str = "/opt/bin/priv-test-interpreted";
/// Long enough for a session to start, or Argon2id to run, under TCG.
const DEADLINE: Duration = Duration::from_secs(120);
/// For a question to show or a program to say something: the runner gives up
/// on a boot that says nothing for two minutes.
const SAID_WITHIN: Duration = Duration::from_secs(60);
/// Past the five seconds a refusal holds back the person's next question.
const COOLED: Duration = Duration::from_secs(6);

fn fail(why: impl AsRef<str>) -> bool {
    note(why.as_ref());
    eprintln!("priv_test: {}", why.as_ref());
    false
}

fn until(mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < DEADLINE {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    done()
}

/// Who runs `priv`: a uid holding `caps` beside the universal ones, carrying
/// `login`, or the test's own tag when `None`.
#[derive(Clone, Copy)]
struct Who {
    uid: u32,
    caps: u64,
    login: Option<u32>,
}

const ROOT: Who = Who {
    uid: 0,
    caps: 0,
    login: None,
};

fn person(uid: u32, login: Option<u32>) -> Who {
    Who {
        uid,
        caps: 0,
        login,
    }
}

/// `/bin/priv` with `stdin` to read, saying on one pipe what it and what it
/// runs write to stdout and stderr; killed once dropped.
struct Run {
    pid: u32,
    out: OwnedFd,
    seen: Vec<u8>,
    matched: usize,
}

impl Run {
    fn start(who: Who, cwd: &str, args: &[&str], env: &[&str], stdin: &str) -> Option<Run> {
        Run::program(PRIV, who, cwd, args, env, stdin)
    }

    fn program(
        path: &str,
        who: Who,
        cwd: &str,
        args: &[&str],
        env: &[&str],
        stdin: &str,
    ) -> Option<Run> {
        let (out, out_end) = fs::pipe2(O_CLOEXEC as u32).ok()?;
        let (input, input_end) = fs::pipe2(O_CLOEXEC as u32).ok()?;
        if fs::write_slice(input_end.raw(), stdin.as_bytes()).ok()? != stdin.len() {
            return None;
        }
        drop(input_end);
        let c = |s: &str| std::ffi::CString::new(s).expect("no NUL");
        let name = path.rsplit('/').next().unwrap_or(path);
        let argv: Vec<_> = std::iter::once(name)
            .chain(args.iter().copied())
            .map(c)
            .collect();
        let env: Vec<_> = env.iter().copied().map(c).collect();
        let argv: Vec<*const u8> = argv.iter().map(|a| a.as_ptr().cast()).collect();
        let env: Vec<*const u8> = env.iter().map(|v| v.as_ptr().cast()).collect();
        let pid = process::spawn_endowed(
            path.as_bytes(),
            &argv,
            &env,
            cwd.as_bytes(),
            TaskPriority::Normal,
            TASK_FLAG_USER_MODE,
            &[
                process::clone_fd(input.raw(), 0),
                process::clone_fd(out_end.raw(), 1),
                process::clone_fd(out_end.raw(), 2),
            ],
            0,
            Endowment {
                caps: who.caps,
                ids: Some((who.uid, who.uid)),
                groups: Some(&[]),
                login: who.login,
            },
        );
        if pid <= 0 {
            fail(format!(
                "{path} {args:?} as uid {} did not start: {pid}",
                who.uid
            ));
            return None;
        }
        let _ = net::set_nonblocking(out.raw());
        Some(Run {
            pid: pid as u32,
            out,
            seen: Vec::new(),
            matched: 0,
        })
    }

    /// Read what has come, waiting up to `ms`; `false` at end of output.
    fn read(&mut self, ms: i64) -> bool {
        let mut fds = [UserPollFd {
            fd: self.out.raw(),
            events: POLLIN,
            revents: 0,
        }];
        let _ = fs::poll(&mut fds, ms);
        let mut buf = [0u8; 1024];
        match fs::read_slice(self.out.raw(), &mut buf) {
            Ok(0) => false,
            Ok(n) => {
                self.seen.extend_from_slice(&buf[..n]);
                true
            }
            Err(_) => true,
        }
    }

    /// Whether it says `marker` within `within`, after what it said before
    /// the last marker found.
    fn says(&mut self, marker: &str, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        loop {
            let said = String::from_utf8_lossy(&self.seen[self.matched..]).into_owned();
            if let Some(at) = said.find(marker) {
                self.matched += at + marker.len();
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || !self.read(left.as_millis() as i64) {
                return false;
            }
        }
    }

    /// Everything it said once its output ends, and how it exited.
    fn end(mut self) -> (Option<i32>, String) {
        let deadline = Instant::now() + DEADLINE;
        while Instant::now() < deadline && self.read(250) {}
        let said = String::from_utf8_lossy(&self.seen).into_owned();
        let code = process::wait_exit_code_nohang(self.pid).or_else(|| {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                std::thread::sleep(Duration::from_millis(100));
                if let Some(code) = process::wait_exit_code_nohang(self.pid) {
                    break Some(code);
                }
                if Instant::now() >= deadline {
                    break None;
                }
            }
        });
        (code, said)
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        let _ = process::kill(self.pid, slopos_abi::signal::SIGKILL);
        let _ = process::wait_exit_code(self.pid);
    }
}

/// What the probe said, held to `want`: each line it must have said.
fn said_lines(said: &str, want: &[String]) -> Result<(), String> {
    for line in want {
        if !said.lines().any(|l| l == line) {
            return Err(format!("no {line:?} in {said:?}"));
        }
    }
    Ok(())
}

fn trusted(dir: &Path) -> bool {
    (!dir.starts_with(root_layout::LOCAL) || root_layout::settled())
        && dir.ancestors().all(|at| {
            std::fs::symlink_metadata(at)
                .is_ok_and(|m| m.is_dir() && m.uid() == 0 && m.mode() & 0o022 == 0)
        })
}

/// What a probe `priv` ran says of itself, run by `uid` in `groups` holding
/// the ceiling, carrying `login`, in `cwd`.
fn probed(uid: u32, groups: &[u32], login: u32, cwd: &str) -> Vec<String> {
    let path: Vec<&str> = DEFAULT_PATH
        .to_str()
        .unwrap_or_default()
        .split(':')
        .filter(|dir| trusted(Path::new(dir)))
        .collect();
    vec![
        format!("ids {uid} {uid} {uid} {uid} {uid} {uid}"),
        format!("groups {groups:?}"),
        format!("caps {:#x}", UNIVERSAL | REQUESTER_CEILING),
        format!("login {login}"),
        format!("cwd {cwd}"),
        "argv0 priv_test".into(),
        "env HOME LANG LOGNAME PATH SHELL SLOPOS_PRIV USER".into(),
        format!("PATH={}", path.join(":")),
        "tty false".into(),
    ]
}

fn probe() -> i32 {
    let (mut r, mut e, mut s) = (0, 0, 0);
    let (mut rg, mut eg, mut sg) = (0, 0, 0);
    let mut groups = [0u32; 32];
    // SAFETY: each pointer is a live local the call writes.
    let count = unsafe {
        getresuid(&mut r, &mut e, &mut s);
        getresgid(&mut rg, &mut eg, &mut sg);
        getgroups(32, groups.as_mut_ptr())
    };
    let groups = &groups[..usize::try_from(count).unwrap_or(0)];
    let mut names: Vec<String> = std::env::vars_os()
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .collect();
    names.sort();
    let cwd = std::env::current_dir().unwrap_or_default();
    let argv0 = std::env::args().next().unwrap_or_default();
    println!("ids {r} {e} {s} {rg} {eg} {sg}");
    println!("groups {groups:?}");
    println!("caps {:#x}", caps_drop(0));
    println!(
        "login {}",
        init_control::login_of(std::process::id()).unwrap_or(u32::MAX)
    );
    println!("cwd {}", cwd.display());
    println!("argv0 {argv0}");
    println!("env {}", names.join(" "));
    println!("PATH={}", std::env::var("PATH").unwrap_or_default());
    println!("tty {}", fs::isatty(0));
    0
}

/// The requester's own environment, none of which reaches what `priv` runs.
const REQUESTER_ENV: [&str; 3] = [
    "PATH=/home/user/.local/bin:/bin",
    "PRIV_TEST_LEAK=1",
    "LD_LIBRARY_PATH=/home/user/.local/lib",
];

/// root is run unasked, as uid 0 in its own group holding the ceiling, with
/// none of its environment.
fn root_runs_a_command_unasked_holding_the_ceiling() -> bool {
    let Some(run) = Run::start(ROOT, "/tmp", &[SELF, PROBE], &REQUESTER_ENV, "") else {
        return false;
    };
    let login = init_control::login_of(std::process::id()).unwrap_or(u32::MAX);
    let (code, said) = run.end();
    if code != Some(0) || said.contains(WAITING) {
        return fail(format!("priv {SELF} {PROBE} as root: {code:?} {said:?}"));
    }
    said_lines(&said, &probed(0, &[0], login, "/tmp")).map_or_else(fail, |()| true)
}

/// A requester that already holds the ceiling runs the command itself,
/// asking privd nothing.
fn a_requester_holding_the_ceiling_runs_it_itself() -> bool {
    let holding = Who {
        caps: REQUESTER_CEILING,
        ..person(USER, None)
    };
    let Some(run) = Run::start(holding, "/tmp", &[SELF, PROBE], &[], "") else {
        return false;
    };
    let login = init_control::login_of(std::process::id()).unwrap_or(u32::MAX);
    let (code, said) = run.end();
    if code != Some(0) || said.contains(WAITING) {
        return fail(format!("priv holding the ceiling: {code:?} {said:?}"));
    }
    said_lines(&said, &probed(USER, &[], login, "/tmp")).map_or_else(fail, |()| true)
}

fn plant(path: &str, text: &str) -> bool {
    std::fs::write(path, text).is_ok()
        && std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).is_ok()
        && std::os::unix::fs::chown(path, Some(USER), Some(USER)).is_ok()
}

/// A program the person's processes could change is never what runs: one
/// in `~/.local/bin`, by its name ahead of the system's on the requester's
/// `PATH` or by its path, one a link where uid 0 alone writes leads to, one
/// there the person owns or its group can write, a script there whose
/// interpreter the person owns, nor anything outside the search directories.
/// `priv` itself and a shell told to read its input are refused as usage.
fn a_program_is_found_only_where_only_uid_0_can_change_it() -> bool {
    let shadow = format!("{LOCAL_BIN}/{SHADOW}");
    let script = "#!/bin/sh\necho SHADOW-RAN\n";
    let made: Vec<&Path> = Path::new(LOCAL_BIN)
        .ancestors()
        .take_while(|dir| !dir.exists())
        .collect();
    let made_theirs = made.iter().rev().all(|dir| {
        std::fs::create_dir(dir).is_ok()
            && std::os::unix::fs::chown(dir, Some(USER), Some(USER)).is_ok()
    });
    let planted = made_theirs
        && plant(&shadow, script)
        && plant(&format!("{LOCAL_BIN}/whoami"), script)
        && plant(OWNED, script)
        && plant(WRITABLE, script)
        && plant(INTERPRETED, &format!("#!{shadow}\n"))
        && std::os::unix::fs::chown(INTERPRETED, Some(0), Some(0)).is_ok()
        && std::os::unix::fs::chown(WRITABLE, Some(0), Some(0)).is_ok()
        && std::fs::set_permissions(WRITABLE, std::fs::Permissions::from_mode(0o775)).is_ok()
        && std::os::unix::fs::symlink(&shadow, LINK).is_ok();
    let outcome = planted || fail("the shadowing programs were not planted");
    let outcome = outcome && {
        let env = [format!("PATH={LOCAL_BIN}:/bin:/opt/bin")];
        let env: Vec<&str> = env.iter().map(String::as_str).collect();
        let refused = |args: &[&str], why: &str| {
            let (code, said) = Run::start(ROOT, "/", args, &env, "")
                .map(Run::end)
                .unwrap_or_default();
            code == Some(127) && said.contains(why) && !said.contains("SHADOW-RAN")
                || fail(format!("priv {args:?}: {code:?} {said:?}"))
        };
        let untrusted = "priv runs only a program in";
        let up = format!("/bin/../{}/{SHADOW}", LOCAL_BIN.trim_start_matches('/'));
        let named = refused(&[SHADOW], "no program of that name")
            && refused(&[&shadow], untrusted)
            && refused(&[&up], untrusted)
            && refused(&["priv-test-link"], untrusted)
            && refused(&[OWNED], untrusted)
            && refused(&[WRITABLE], untrusted)
            && refused(&[INTERPRETED], untrusted)
            && refused(&["/lib/ld-slopos.so.1"], untrusted);
        let (code, said) = Run::start(ROOT, "/", &["whoami"], &env, "")
            .map(Run::end)
            .unwrap_or_default();
        named
            && (code == Some(0) && said.trim() == "root"
                || fail(format!(
                    "priv whoami with ~/.local/bin first: {code:?} {said:?}"
                )))
    };
    let usage = [
        &[][..],
        &["-x"],
        &["-s", "now"],
        &["priv", "-s"],
        &["sh", "-i"],
    ]
    .iter()
    .all(|args| {
        let (code, _) = Run::start(ROOT, "/", args, &[], "")
            .map(Run::end)
            .unwrap_or_default();
        code == Some(2) || fail(format!("priv {args:?}: {code:?}, not usage"))
    });
    for path in [
        shadow.as_str(),
        &format!("{LOCAL_BIN}/whoami"),
        LINK,
        OWNED,
        WRITABLE,
        INTERPRETED,
    ] {
        let _ = std::fs::remove_file(path);
    }
    for dir in made {
        let _ = std::fs::remove_dir(dir);
    }
    outcome && usage
}

/// A shell asked for as a command, which a kept answer would cover, is
/// refused by the copy privd starts: the shell is an action of its own.
fn a_shell_asked_for_as_a_command_is_refused() -> bool {
    let request = Request {
        id: request_id(),
        action: "org.slopos.priv".into(),
        args: vec![b"-s".to_vec()],
        cwd: true,

        terminal: false,
    };
    let replies = (|| {
        let mut privd = Connection::open().ok()?;
        let null = fs::open_cstr(c"/dev/null", slopos_abi::fs::O_RDWR).ok()?;
        let dir =
            fs::open_cstr(c"/", slopos_abi::fs::O_RDONLY | slopos_abi::fs::O_DIRECTORY).ok()?;
        privd
            .send(&request, &[null.raw(), null.raw(), null.raw(), dir.raw()])
            .ok()?;
        let mut replies = Vec::new();
        while let Ok(Some(reply)) = privd.reply(request.id) {
            let last = matches!(reply, Reply::Exited(_) | Reply::Denied { .. });
            replies.push(reply);
            if last {
                break;
            }
        }
        Some(replies)
    })();
    matches!(
        replies.as_deref(),
        Some([
            Reply::Started { .. },
            Reply::Exited(Exit::Code(STATUS_REFUSED))
        ])
    ) || fail(format!("org.slopos.priv asked for -s: {replies:?}"))
}

/// A signal the requester gets is relayed to what `priv` runs, which ends of
/// it, and the requester exits as a shell reports that.
fn a_signal_the_requester_relays_reaches_the_command() -> bool {
    let Some(mut run) = Run::start(ROOT, "/", &[SELF, LINGER], &[], "") else {
        return false;
    };
    if !run.says(LINGERING, SAID_WITHIN) {
        return fail("the command never ran");
    }
    let _ = process::kill(run.pid, slopos_abi::signal::SIGINT);
    let (code, said) = run.end();
    code == Some(130) || fail(format!("interrupted: {code:?} {said:?}"))
}

/// `priv -s` typed at a terminal runs a shell on a terminal of its own that
/// says what it holds, hands it to what it runs, follows an interrupt typed
/// to the program it runs and not to itself, and leaves the terminal as it
/// was once it exits.
fn priv_s_at_a_terminal_hands_its_power_on() -> bool {
    match at_a_terminal() {
        Ok(()) => true,
        Err(why) => fail(why),
    }
}

fn at_a_terminal() -> Result<(), String> {
    let (master, _) = process::openpty().map_err(|e| format!("no pty: {e}"))?;
    let slave = fs::ioctl_tiocgptpeer(master.raw()).map_err(|e| format!("no pty slave: {e}"))?;
    let size = UserWinsize {
        ws_row: 40,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    fs::tiocswinsz(master.raw(), &size).map_err(|e| format!("sizing the pty: {e}"))?;
    let tty = || {
        fs::dup(slave.raw())
            // SAFETY: `dup` just opened this descriptor for us.
            .map(|dup| Stdio::from(unsafe { File::from_raw_fd(dup.into_raw()) }))
            .map_err(|e| format!("dup: {e}"))
    };
    let mut cmd = Command::new("/bin/shell");
    cmd.args([
        "-c",
        "echo before-$(stty -g)-; priv -s && echo after-$(stty -g)-",
    ])
    .current_dir("/tmp")
    .stdin(tty()?)
    .stdout(tty()?)
    .stderr(tty()?);
    // SAFETY: the closure makes two system calls and allocates nothing.
    unsafe {
        cmd.pre_exec(|| {
            if process::setsid() < 0 || fs::tiocsctty(0).is_err() {
                return Err(std::io::Error::other("no controlling terminal"));
            }
            Ok(())
        });
    }
    let mut shell = cmd.spawn().map_err(|e| format!("priv -s: {e}"))?;
    drop(slave);
    let _ = net::set_nonblocking(master.raw());
    let mut screen = Screen {
        master,
        seen: Vec::new(),
        from: 0,
    };
    let held: Vec<&str> = slopos_priv_core::capability_names(REQUESTER_CEILING).collect();
    screen.until(&format!(
        "priv: what runs here holds {} as root.",
        held.join(" ")
    ))?;
    screen.until("priv\x1b[0m ")?;
    screen.typed(format!("{SELF} {PROBE}\r").as_bytes())?;
    screen.until(&format!("caps {:#x}", UNIVERSAL | REQUESTER_CEILING))?;
    screen.until("tty true")?;
    screen.typed(b"stty size\r")?;
    screen.until("40 120")?;
    screen.typed(format!("{SELF} {LINGER}\r").as_bytes())?;
    screen.until(LINGERING)?;
    screen.typed(b"\x03")?;
    screen.typed(b"echo status-$?\r")?;
    screen.until("status-130")?;
    screen.typed(b"exit\r")?;
    screen.until("after-")?;
    let deadline = Instant::now() + DEADLINE;
    loop {
        match shell.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            other => {
                let _ = shell.kill();
                return Err(format!("the shell around priv -s ended as {other:?}"));
            }
        }
    }
    let shown = screen.rest();
    let settings = |label: &str| {
        let from = shown.find(label)? + label.len();
        shown[from..].split('-').next()
    };
    if settings("before-").is_none() || settings("before-") != settings("after-") {
        return Err(format!(
            "the terminal was not given back as it was: {shown:?}"
        ));
    }
    Ok(())
}

/// What a terminal's master has shown.
struct Screen {
    master: OwnedFd,
    seen: Vec<u8>,
    from: usize,
}

impl Screen {
    /// Read until `marker` shows after the last marker found.
    fn until(&mut self, marker: &str) -> Result<(), String> {
        let deadline = Instant::now() + SAID_WITHIN;
        let mut buf = [0u8; 512];
        while Instant::now() < deadline {
            match fs::read_slice(self.master.raw(), &mut buf) {
                Ok(n) if n > 0 => self.seen.extend_from_slice(&buf[..n]),
                _ => std::thread::sleep(Duration::from_millis(50)),
            }
            let shown = String::from_utf8_lossy(&self.seen[self.from..]).into_owned();
            if let Some(at) = shown.find(marker) {
                self.from += at + marker.len();
                return Ok(());
            }
        }
        let shown = String::from_utf8_lossy(&self.seen);
        let from = shown.char_indices().rev().nth(119).map_or(0, |(at, _)| at);
        let tail = &shown[from..];
        Err(format!("no {marker:?} from priv -s, which ended {tail:?}"))
    }

    fn typed(&self, text: &[u8]) -> Result<(), String> {
        fs::write_slice(self.master.raw(), text)
            .map(drop)
            .map_err(|e| format!("typing at priv -s: {e}"))
    }

    /// Everything shown, once what is left has been read.
    fn rest(mut self) -> String {
        let mut buf = [0u8; 512];
        while let Ok(n) = fs::read_slice(self.master.raw(), &mut buf)
            && n > 0
        {
            self.seen.extend_from_slice(&buf[..n]);
        }
        String::from_utf8_lossy(&self.seen).into_owned()
    }
}

/// `/bin/users` with `input` typed, asked of privd as root: its status.
fn users(args: &[&str], input: &str) -> Option<i32> {
    use std::io::Write;
    let mut child = Command::new("/bin/users")
        .args(args)
        .env_remove(slopos_priv_core::ENV_FD)
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

fn uid_of(name: &str) -> Option<u32> {
    slopos_userland::accounts::uid_of(name)
}

/// Beside the first person, an administrator and a standard user, each with
/// a password.
fn the_people_are_added() -> bool {
    let added = [
        users(&["add", ALICE, "--admin", "--keep-autologin"], ""),
        users(&["add", BOB, "--keep-autologin"], ""),
        users(
            &["passwd", ALICE],
            &format!("{ALICE_PASSWORD}\n{ALICE_PASSWORD}\n"),
        ),
        users(&["passwd", BOB], "bob-pw\nbob-pw\n"),
    ];
    added == [Some(0); 4] && uid_of(ALICE).is_some() && uid_of(BOB).is_some()
        || fail(format!("{ALICE} and {BOB} were not added: {added:?}"))
}

/// The login tag of the desktop in front, whose processes the people are
/// asked from.
fn desktop() -> u32 {
    sessions::front_tag().unwrap_or(0)
}

/// A desktop to ask the people at.
fn a_desktop_asks_the_first_person() -> bool {
    if let Err(why) = init_control::ask(InitCommand::RestartSession) {
        return fail(format!("restart-session: {why}"));
    }
    until(|| {
        Run::start(person(USER, Some(desktop())), "/", &[SELF, PROBE], &[], "")
            .is_some_and(|mut run| run.says(WAITING, Duration::from_secs(30)))
    }) || fail("the desktop never asked the person")
}

/// The first person, an administrator with no password, allows a command by
/// a click; it runs as them in their groups and session, and runs unasked
/// again from the same directory and parent, and is asked again from another
/// directory or a shell it did not start from.
fn the_first_person_s_click_runs_it_as_them_and_is_kept() -> bool {
    let me = person(USER, Some(desktop()));
    let Some(mut run) = Run::start(me, "/tmp", &[SELF, PROBE], &[], "") else {
        return false;
    };
    if !run.says(WAITING, SAID_WITHIN) {
        return fail("the first person was not asked");
    }
    asking::tab_and_enter();
    let (code, said) = run.end();
    if code != Some(0) {
        return fail(format!("allowed by a click: {code:?} {said:?}"));
    }
    if let Err(why) = said_lines(&said, &probed(USER, &[USER, ADMIN_GID], desktop(), "/tmp")) {
        return fail(why);
    }
    let (code, said) = Run::start(me, "/tmp", &[SELF, PROBE], &[], "")
        .map(Run::end)
        .unwrap_or_default();
    if code != Some(0) || said.contains(WAITING) {
        return fail(format!("the same command again: {code:?} {said:?}"));
    }
    let line = format!("{PRIV} {SELF} {PROBE}; exit $?");
    let refused = |run: Option<Run>, from: &str| {
        let Some(mut run) = run else {
            return false;
        };
        if !run.says(WAITING, SAID_WITHIN) {
            return fail(format!("{from}, the person was not asked again"));
        }
        asking::escape();
        let (code, said) = run.end();
        std::thread::sleep(COOLED);
        code == Some(126) || fail(format!("{from}, refused by Escape: {code:?} {said:?}"))
    };
    refused(
        Run::start(me, "/", &[SELF, PROBE], &[], ""),
        "from another directory",
    ) && refused(
        Run::program("/bin/shell", me, "/tmp", &["-c", &line], &[], ""),
        "from a shell",
    )
}

/// The shell named with nothing after it is `priv -s`, whose answer is never
/// kept: allowed once, it is asked for again.
fn a_shell_is_asked_for_every_time() -> bool {
    let me = person(USER, Some(desktop()));
    let Some(mut first) = Run::start(me, "/tmp", &["sh"], &[], "echo SHELL-RAN\n") else {
        return false;
    };
    if !first.says(WAITING, SAID_WITHIN) {
        return fail("priv sh was not asked for");
    }
    asking::tab_and_enter();
    let (code, said) = first.end();
    if code != Some(0) || !said.contains("SHELL-RAN") {
        return fail(format!("priv sh, allowed: {code:?} {said:?}"));
    }
    let Some(mut again) = Run::start(me, "/tmp", &["sh"], &[], "echo SHELL-RAN\n") else {
        return false;
    };
    if !again.says(WAITING, SAID_WITHIN) {
        return fail("priv sh was not asked for again");
    }
    asking::escape();
    let (code, said) = again.end();
    std::thread::sleep(COOLED);
    code == Some(126) && !said.contains("SHELL-RAN")
        || fail(format!("priv sh, refused: {code:?} {said:?}"))
}

/// A disk the suite attaches.
fn a_disk() -> Option<&'static str> {
    ["/dev/nvme0n1", "/dev/vda", "/dev/sda"]
        .into_iter()
        .find(|path| Path::new(path).exists())
}

fn sealed(path: &str) -> bool {
    File::open(path).is_ok_and(|file| is_sealed(&file))
}

/// What `priv -s` runs edits a uid-0 file in `/etc` itself and through a
/// program it runs, is refused the raw disk, the base, the user database and
/// a sealed file, runs `priv` again holding what it holds, and a mechanism it
/// runs is still asked for.
fn priv_s_edits_etc_and_is_refused_what_the_ceiling_leaves_out() -> bool {
    let Some(disk) = a_disk() else {
        return fail("no disk to read");
    };
    let _cleaned = Cleaned;
    let prepared = std::fs::write(CONF, "before\n").is_ok()
        && std::fs::set_permissions(CONF, std::fs::Permissions::from_mode(0o644)).is_ok()
        && std::fs::write(SEALED, "sealed\n").is_ok()
        && File::open(SEALED).is_ok_and(|file| set_sealed(&file, true).is_ok());
    let database = std::fs::read("/etc/passwd").unwrap_or_default();
    if !prepared || !sealed(SEALED) {
        return fail("the files priv edits and is refused were not made");
    }
    let script = format!(
        "echo edited >> {CONF} && echo EDITED\n\
         echo piped | tee -a {CONF} > /dev/null && echo TEED\n\
         echo x >> /etc/passwd || echo DATABASE-REFUSED\n\
         echo x >> {SEALED} || echo SEALED-REFUSED\n\
         head -c 512 {disk} > /dev/null || echo DISK-REFUSED\n\
         echo x > {PLANTED} || echo BASE-REFUSED\n\
         priv whoami && echo NESTED\n\
         bootctl base install /tmp/priv_test.base || echo MECHANISM-REFUSED\n\
         echo DONE\n"
    );
    let Some(mut run) = Run::start(person(USER, Some(desktop())), "/tmp", &["-s"], &[], &script)
    else {
        return false;
    };
    if !run.says(&format!("priv: {WAITING}"), SAID_WITHIN) {
        return fail("priv -s was not asked for");
    }
    asking::tab_and_enter();
    if !run.says(&format!("bootctl: {WAITING}"), SAID_WITHIN) {
        return fail(format!(
            "bootctl from priv -s was not asked for: {:?}",
            String::from_utf8_lossy(&run.seen)
        ));
    }
    asking::escape();
    let (code, said) = run.end();
    std::thread::sleep(COOLED);
    let want: Vec<String> = [
        "EDITED",
        "TEED",
        "DATABASE-REFUSED",
        "SEALED-REFUSED",
        "DISK-REFUSED",
        "BASE-REFUSED",
        "user",
        "NESTED",
        "MECHANISM-REFUSED",
        "DONE",
    ]
    .map(String::from)
    .to_vec();
    if code != Some(0) {
        return fail(format!("priv -s exited {code:?}: {said:?}"));
    }
    if let Err(why) = said_lines(&said, &want) {
        return fail(why);
    }
    let conf = std::fs::read_to_string(CONF).unwrap_or_default();
    let owner = std::fs::metadata(CONF).map(|m| m.uid()).ok();
    let outcome = (conf == "before\nedited\npiped\n" && owner == Some(0)
        || fail(format!("{CONF} holds {conf:?}, owned by {owner:?}")))
        && (std::fs::read("/etc/passwd").unwrap_or_default() == database
            || fail("/etc/passwd changed"))
        && (std::fs::read_to_string(SEALED).is_ok_and(|s| s == "sealed\n") && sealed(SEALED)
            || fail(format!("{SEALED} changed")))
        && (!Path::new(PLANTED).exists() || fail(format!("{PLANTED} was planted")));
    outcome
}

/// Takes away what the `/etc` case made, however it ends.
struct Cleaned;

impl Drop for Cleaned {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(CONF);
        if let Ok(file) = File::open(SEALED) {
            let _ = set_sealed(&file, false);
        }
        let _ = std::fs::remove_file(SEALED);
    }
}

/// A standard user's command from the desktop in front is allowed by an
/// administrator typing their own password, and runs as the standard user,
/// without the administrators' group; a wrong password refuses it. The two
/// run in other directories, so nothing is kept between them.
fn a_standard_user_s_command_is_answered_by_an_administrator() -> bool {
    let Some(bob) = uid_of(BOB) else {
        return fail(format!("{BOB} is gone"));
    };
    let answered = |cwd: &str, answer: fn()| {
        let mut run = Run::start(person(bob, Some(desktop())), cwd, &[SELF, PROBE], &[], "")?;
        if !run.says(WAITING, SAID_WITHIN) {
            return None;
        }
        answer();
        Some(run.end())
    };
    let (code, said) = answered("/tmp", || {
        asking::armed();
        asking::type_text(ALICE_PASSWORD);
        asking::key(slopos_abi::input::keycode::KEY_ENTER, 0);
    })
    .unwrap_or_default();
    if code != Some(0) {
        return fail(format!(
            "{BOB}'s command, {ALICE}'s password: {code:?} {said:?}"
        ));
    }
    if let Err(why) = said_lines(&said, &probed(bob, &[bob], desktop(), "/tmp")) {
        return fail(why);
    }
    let (code, said) = answered("/", || {
        asking::armed();
        asking::type_text("not-hers");
        asking::key(slopos_abi::input::keycode::KEY_ENTER, 0);
    })
    .unwrap_or_default();
    code == Some(126) && said.contains(WRONG_PASSWORD)
        || fail(format!("a wrong password: {code:?} {said:?}"))
}

fn the_desktop_ends_and_the_people_go() -> bool {
    let ended = init_control::ask(InitCommand::EndSession);
    let removed = [BOB, ALICE].map(|name| users(&["remove", name, "--home"], ""));
    (ended.is_ok() || fail(format!("end-session: {ended:?}")))
        && (uid_of(ALICE).is_none() && uid_of(BOB).is_none()
            || fail(format!("{ALICE} or {BOB} stayed: {removed:?}")))
}

const CASES: &[(&str, fn() -> bool)] = &[
    (
        "root_runs_a_command_unasked_holding_the_ceiling",
        root_runs_a_command_unasked_holding_the_ceiling,
    ),
    (
        "a_requester_holding_the_ceiling_runs_it_itself",
        a_requester_holding_the_ceiling_runs_it_itself,
    ),
    (
        "a_program_is_found_only_where_only_uid_0_can_change_it",
        a_program_is_found_only_where_only_uid_0_can_change_it,
    ),
    (
        "a_shell_asked_for_as_a_command_is_refused",
        a_shell_asked_for_as_a_command_is_refused,
    ),
    (
        "a_signal_the_requester_relays_reaches_the_command",
        a_signal_the_requester_relays_reaches_the_command,
    ),
    (
        "priv_s_at_a_terminal_hands_its_power_on",
        priv_s_at_a_terminal_hands_its_power_on,
    ),
    ("the_people_are_added", the_people_are_added),
    (
        "a_desktop_asks_the_first_person",
        a_desktop_asks_the_first_person,
    ),
    (
        "the_first_person_s_click_runs_it_as_them_and_is_kept",
        the_first_person_s_click_runs_it_as_them_and_is_kept,
    ),
    (
        "a_shell_is_asked_for_every_time",
        a_shell_is_asked_for_every_time,
    ),
    (
        "priv_s_edits_etc_and_is_refused_what_the_ceiling_leaves_out",
        priv_s_edits_etc_and_is_refused_what_the_ceiling_leaves_out,
    ),
    (
        "a_standard_user_s_command_is_answered_by_an_administrator",
        a_standard_user_s_command_is_answered_by_an_administrator,
    ),
    (
        "the_desktop_ends_and_the_people_go",
        the_desktop_ends_and_the_people_go,
    ),
];

/// Ends on its own should the case that interrupts it fail first.
fn linger() -> i32 {
    println!("{LINGERING}");
    std::thread::sleep(DEADLINE);
    0
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some(PROBE) => std::process::exit(probe()),
        Some(LINGER) => std::process::exit(linger()),
        _ => slopos_slibc::test_harness::run(CASES),
    }
}
