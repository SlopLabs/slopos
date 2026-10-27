//! The apps, run as the session runs them: each program at `/usr/local/bin`
//! is dynamically linked against `libc.so`, and every app, the base's among
//! them, loads, finds its session's compositor and names its window with the
//! id and the name its dock entry has.
//!
//! The compositor here is this test, serving the session's socket through
//! the protocol's own server: under `tests=on` init starts none, and a
//! client's requests are what show the app came up.
//!
//! With `--trees`, as uid 0, the trees software is installed in: a second
//! person makes `/usr/local` the system's, whole, which the first person then
//! installs into for everyone through privd, allowing it by a click, but only
//! what everyone may read and what leads nowhere outside the tree, and into
//! their own `~/.local` holding nothing; the second person runs what
//! everyone has, its library found in `/usr/local/lib`, and finds nothing of
//! the first person's own.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use slopos_abi::fs::{OWNERS_RECORD, SESSION_HOME, SESSION_UID};
use slopos_abi::pixel::PixelFormat;
use slopos_abi::syscall::{O_CLOEXEC, POLLIN, UserPollFd};
use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};
use slopos_priv_core::consent::Refusal;
use slopos_protocol::{Event, PROTOCOL_VERSION, Request, Server};
use slopos_tree_core::MANIFEST_DIR;
use slopos_userland::init_control::{self, Command as InitCommand};
use slopos_userland::privd::WAITING;
use slopos_userland::root_layout::{self, search_path};
use slopos_userland::syscall::process::{self, Endowment};
use slopos_userland::syscall::{OwnedFd, fs as sys_fs, net};

#[path = "support/asking.rs"]
mod asking;
#[path = "support/sessions.rs"]
mod sessions;

const DEADLINE: Duration = Duration::from_secs(60);
const RETRY: Duration = Duration::from_millis(20);
const INTERP: &[u8] = b"/lib/ld-slopos.so.1\0";

const APPS: &[(&str, &str)] = &[
    ("/usr/local/bin/editor", "org.slopos.editor"),
    ("/usr/local/bin/file_manager", "org.slopos.files"),
    ("/usr/local/bin/image_viewer", "org.slopos.image-viewer"),
    ("/bin/sysmon", "org.slopos.sysmon"),
    ("/bin/terminal", "org.slopos.terminal"),
];

/// Where the apps' entries are: the base's for its own programs, else the
/// share they were installed with.
fn entry_name(program: &str, id: &str) -> Option<String> {
    let dir = match program.starts_with("/usr/local/") {
        true => "/usr/local/share",
        false => "/usr/share",
    };
    let share = slopos_userland::app_entry::Share {
        dir: dir.into(),
        only: None,
    };
    slopos_userland::app_entry::read(&[share], false)
        .into_iter()
        .find(|entry| entry.id == id)
        .map(|entry| entry.name)
}

/// The program's `PT_INTERP`, read from its ELF header and program headers.
fn interpreter(program: &str) -> Result<Vec<u8>, String> {
    let elf = std::fs::read(program).map_err(|e| format!("{program}: {e}"))?;
    let word = |at: usize, len: usize| -> Option<u64> {
        let bytes = elf.get(at..at + len)?;
        Some(bytes.iter().rev().fold(0, |v, b| v << 8 | u64::from(*b)))
    };
    let (phoff, phentsize, phnum) = (word(32, 8), word(54, 2), word(56, 2));
    let (Some(phoff), Some(phentsize), Some(phnum)) = (phoff, phentsize, phnum) else {
        return Err(format!("{program}: not an ELF file"));
    };
    for i in 0..phnum {
        let at = (phoff + i * phentsize) as usize;
        if word(at, 4) == Some(3) {
            let (offset, size) = (word(at + 8, 8), word(at + 32, 8));
            if let (Some(offset), Some(size)) = (offset, size) {
                return elf
                    .get(offset as usize..(offset + size) as usize)
                    .map(<[u8]>::to_vec)
                    .ok_or_else(|| format!("{program}: PT_INTERP past the file"));
            }
        }
    }
    Err(format!(
        "{program}: no PT_INTERP, so not linked against libc.so"
    ))
}

fn exited(child: &mut Child) -> Option<String> {
    match child.try_wait() {
        Ok(Some(status)) => Some(format!("exited with {status}")),
        Ok(None) => None,
        Err(e) => Some(format!("cannot be waited for: {e}")),
    }
}

/// Serve `program` as its compositor until it names its window's app id:
/// that id, and the title it gave the window before it.
fn app_id_of(program: &str) -> Result<(String, String), String> {
    let path = slopos_protocol::socket_path(slopos_userland::syscall::process::geteuid());
    let shown = String::from_utf8_lossy(&path).into_owned();
    let _ = std::fs::remove_file(&shown);
    let mut server = Server::bind(&path).map_err(|e| format!("listening on {shown}: {e:?}"))?;
    let mut child = Command::new(program)
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("{program}: {e}"))?;
    let answer = serve(&mut server, &mut child);
    let _ = child.kill();
    let _ = child.wait();
    drop(server);
    let _ = std::fs::remove_file(&shown);
    answer.map_err(|e| format!("{program}: {e}"))
}

fn serve(server: &mut Server, child: &mut Child) -> Result<(String, String), String> {
    let mut title = String::new();
    let start = Instant::now();
    let client = loop {
        match server.accept() {
            Ok(Some(client)) => break client,
            Ok(None) => {}
            Err(e) => return Err(format!("accepting: {e:?}")),
        }
        if let Some(how) = exited(child) {
            return Err(format!("{how} before it connected"));
        }
        if start.elapsed() > DEADLINE {
            return Err(format!("did not connect within {DEADLINE:?}"));
        }
        std::thread::sleep(RETRY);
    };
    let hello = Event::Hello {
        version: PROTOCOL_VERSION,
        capabilities: 0,
    };
    server
        .queue_event(client, &hello)
        .map_err(|e| format!("greeting: {e:?}"))?;
    server.flush_clients();
    while start.elapsed() < DEADLINE {
        match server.recv_request(client) {
            Ok(Some(Request::Hello { .. })) => {
                let output = Event::OutputInfo {
                    width: 1024,
                    height: 768,
                    format: PixelFormat::Argb8888 as u32,
                    pitch: 1024 * 4,
                    scale: 1,
                };
                server
                    .queue_event(client, &output)
                    .map_err(|e| format!("describing the output: {e:?}"))?;
                server.flush_clients();
            }
            Ok(Some(Request::ToplevelSetTitle {
                title: said, len, ..
            })) => {
                title = String::from_utf8_lossy(&said[..len as usize]).into_owned();
            }
            Ok(Some(Request::ToplevelSetAppId { app_id, len, .. })) => {
                let id = String::from_utf8_lossy(&app_id[..len as usize]).into_owned();
                return Ok((id, title));
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                if let Some(how) = exited(child) {
                    return Err(format!("{how} before it named its window"));
                }
                std::thread::sleep(RETRY);
            }
            Err(e) => return Err(format!("reading its requests: {e:?}")),
        }
    }
    Err(format!("named no window within {DEADLINE:?}"))
}

fn run(program: &str, want: &str) -> bool {
    let loose = program.starts_with("/usr/local/");
    match loose.then(|| interpreter(program)) {
        None => {}
        Some(Ok(interp)) if interp == INTERP => {}
        Some(Ok(interp)) => {
            eprintln!(
                "apps_test: {program} names the interpreter {:?}",
                String::from_utf8_lossy(&interp)
            );
            return false;
        }
        Some(Err(e)) => {
            eprintln!("apps_test: {e}");
            return false;
        }
    }
    let Some(name) = entry_name(program, want) else {
        eprintln!("apps_test: no app entry {want}");
        return false;
    };
    let one_name = |title: &str| title == name || title.ends_with(&format!(" — {name}"));
    match app_id_of(program) {
        Ok((id, title)) if id == want && one_name(&title) => true,
        Ok((id, title)) => {
            eprintln!(
                "apps_test: {program} named its window {id:?}, titled {title:?}, not {want:?} and {name:?}"
            );
            false
        }
        Err(e) => {
            eprintln!("apps_test: {e}");
            false
        }
    }
}

fn the_editor_runs() -> bool {
    run(APPS[0].0, APPS[0].1)
}

fn the_file_manager_runs() -> bool {
    run(APPS[1].0, APPS[1].1)
}

fn the_image_viewer_runs() -> bool {
    run(APPS[2].0, APPS[2].1)
}

fn the_system_monitor_runs() -> bool {
    run(APPS[3].0, APPS[3].1)
}

fn the_terminal_runs() -> bool {
    run(APPS[4].0, APPS[4].1)
}

const CASES: &[(&str, fn() -> bool)] = &[
    ("the_editor_runs", the_editor_runs),
    ("the_file_manager_runs", the_file_manager_runs),
    ("the_image_viewer_runs", the_image_viewer_runs),
    ("the_system_monitor_runs", the_system_monitor_runs),
    ("the_terminal_runs", the_terminal_runs),
];

const TREES: &str = "--trees";
const WORK: &str = "/var/tmp/apps_test";
const BOB: &str = "apps-bob";
const LOCAL: &str = "/usr/local";
const EVERYONE: &str = "apps_test_everyone";
const OWN: &str = "apps_test_own";
const SECRET: &str = "apps_test_secret";
const OUTWARD: &str = "apps_test_outward";
const EVERYONE_COMMAND: &str = "apps-everyone";
const EVERYONE_ALIAS: &str = "apps-everyone-too";
const EVERYONE_LOADER: &str = "apps-everyone-loads";
const OWN_COMMAND: &str = "apps-own";
const LIBRARY: &str = "libdlsearch.so";

fn fail(why: impl std::fmt::Display) -> bool {
    eprintln!("apps_test: {why}");
    false
}

/// `/bin/users` asked of privd as root: its status.
fn users(args: &[&str]) -> Option<i32> {
    Command::new("/bin/users")
        .args(args)
        .env_remove(slopos_priv_core::ENV_FD)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .ok()?
        .code()
}

fn bob() -> Option<u32> {
    slopos_userland::accounts::uid_of(BOB)
}

fn bob_home() -> String {
    format!("/home/{BOB}")
}

/// What a person's session hands what it runs.
fn session_env(home: &str) -> Vec<String> {
    vec![
        format!("HOME={home}"),
        format!("PATH={}", search_path(home)),
    ]
}

/// A program run as `uid` holding nothing, carrying `login`, its stdout and
/// stderr on one pipe; killed once dropped.
struct Run {
    pid: u32,
    out: OwnedFd,
    seen: Vec<u8>,
}

impl Run {
    fn start(
        path: &str,
        uid: u32,
        login: Option<u32>,
        env: &[String],
        args: &[&str],
    ) -> Option<Run> {
        let (out, out_end) = sys_fs::pipe2(O_CLOEXEC as u32).ok()?;
        let (input, input_end) = sys_fs::pipe2(O_CLOEXEC as u32).ok()?;
        drop(input_end);
        let c = |s: &str| std::ffi::CString::new(s).expect("no NUL");
        let argv: Vec<_> = std::iter::once(path)
            .chain(args.iter().copied())
            .map(c)
            .collect();
        let env: Vec<_> = env.iter().map(|var| c(var)).collect();
        let argv: Vec<*const u8> = argv.iter().map(|a| a.as_ptr().cast()).collect();
        let env: Vec<*const u8> = env.iter().map(|v| v.as_ptr().cast()).collect();
        let pid = process::spawn_endowed(
            path.as_bytes(),
            &argv,
            &env,
            b"/",
            TaskPriority::Normal,
            TASK_FLAG_USER_MODE,
            &[
                process::clone_fd(input.raw(), 0),
                process::clone_fd(out_end.raw(), 1),
                process::clone_fd(out_end.raw(), 2),
            ],
            0,
            Endowment {
                caps: 0,
                ids: Some((uid, uid)),
                groups: Some(&[]),
                login,
            },
        );
        if pid <= 0 {
            fail(format!("{path} {args:?} as uid {uid} did not start: {pid}"));
            return None;
        }
        let _ = net::set_nonblocking(out.raw());
        Some(Run {
            pid: pid as u32,
            out,
            seen: Vec::new(),
        })
    }

    /// Read what has come, waiting up to `ms`; `false` at end of output.
    fn read(&mut self, ms: i64) -> bool {
        let mut fds = [UserPollFd {
            fd: self.out.raw(),
            events: POLLIN,
            revents: 0,
        }];
        let _ = sys_fs::poll(&mut fds, ms);
        let mut buf = [0u8; 1024];
        match sys_fs::read_slice(self.out.raw(), &mut buf) {
            Ok(0) => false,
            Ok(n) => {
                self.seen.extend_from_slice(&buf[..n]);
                true
            }
            Err(_) => true,
        }
    }

    fn says(&mut self, marker: &str, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        loop {
            if String::from_utf8_lossy(&self.seen).contains(marker) {
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
        let deadline = Instant::now() + asking::DEADLINE;
        while Instant::now() < deadline && self.read(250) {}
        let said = String::from_utf8_lossy(&self.seen).into_owned();
        let deadline = Instant::now() + Duration::from_secs(10);
        let code = loop {
            if let Some(code) = process::wait_exit_code_nohang(self.pid) {
                break Some(code);
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        (code, said)
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        let _ = process::kill(self.pid, slopos_abi::signal::SIGKILL);
        let _ = process::wait_exit_code(self.pid);
    }
}

/// `command` run by a shell of `uid`'s whose home is `home`, as their
/// session runs it: its status and what it said.
fn shell_as(uid: u32, home: &str, command: &str) -> (Option<i32>, String) {
    Run::start(
        "/bin/shell",
        uid,
        None,
        &session_env(home),
        &["-c", command],
    )
    .map(Run::end)
    .unwrap_or_default()
}

/// A tree to install, laid out under [`WORK`] by uid 0 for anyone to read.
fn stage(name: &str, files: &[(&str, &[u8], u32)]) -> Option<PathBuf> {
    let tree = Path::new(WORK).join(name);
    let _ = fs::remove_dir_all(&tree);
    for (rel, bytes, mode) in files {
        let path = tree.join(rel);
        let staged = path
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| fs::write(&path, bytes))
            .and_then(|()| fs::set_permissions(&path, fs::Permissions::from_mode(*mode)));
        if let Err(e) = staged {
            fail(format!("{}: {e}", path.display()));
            return None;
        }
    }
    Some(tree)
}

fn script(says: &str) -> Vec<u8> {
    format!("#!/bin/shell\necho {says}\n").into_bytes()
}

fn entries(top: &Path) -> Vec<(PathBuf, fs::Metadata)> {
    let mut out = Vec::new();
    let mut dirs = vec![top.to_path_buf()];
    if let Ok(meta) = fs::symlink_metadata(top) {
        out.push((top.to_path_buf(), meta));
    }
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if meta.is_dir() {
                dirs.push(entry.path());
            }
            out.push((entry.path(), meta));
        }
    }
    out
}

/// A second person makes `/usr/local` the system's to its last entry, so the
/// first person writes there no more. The apps' manifest stays theirs, which
/// uid 0 then installs over without believing.
fn a_second_person_makes_usr_local_the_system_s() -> bool {
    if users(&["add", BOB, "--keep-autologin"]) != Some(0) || bob().is_none() {
        return fail(format!("{BOB} was not added"));
    }
    let record = fs::read_to_string(OWNERS_RECORD).unwrap_or_default();
    if record
        .lines()
        .any(|line| line.ends_with(&format!(" {LOCAL}")))
    {
        return fail(format!("{OWNERS_RECORD} still names {LOCAL}: {record:?}"));
    }
    let manifest = Path::new(MANIFEST_DIR).join(slopos_tree_core::APPS);
    let tree = entries(Path::new(LOCAL));
    if tree.len() < 2 {
        return fail(format!("{LOCAL} holds no apps"));
    }
    let others = |meta: &fs::Metadata| {
        meta.uid() != 0 || !meta.file_type().is_symlink() && meta.mode() & 0o022 != 0
    };
    if let Some((path, meta)) = tree.iter().find(|(_, meta)| others(meta)) {
        return fail(format!(
            "{} is uid {}'s, mode {:o}",
            path.display(),
            meta.uid(),
            meta.mode()
        ));
    }
    if fs::symlink_metadata(&manifest).map(|meta| meta.uid()).ok() != Some(SESSION_UID) {
        return fail(format!("{} is not the first person's", manifest.display()));
    }
    if !Path::new(LOCAL).join("bin").is_dir() {
        return fail(format!("{LOCAL}/bin is not a directory"));
    }
    let (code, said) = shell_as(
        SESSION_UID,
        SESSION_HOME,
        "echo started; echo x > /usr/local/bin/apps-test-mine",
    );
    said.starts_with("started\n") && matches!(code, Some(code) if code != 0)
        || fail(format!(
            "the first person writing {LOCAL}: {code:?} {said:?}"
        ))
}

/// The first person installs a tree for everyone: `tree-install` asks privd,
/// they allow it by a click at their desktop, and it is installed as uid
/// 0's, writable by uid 0 alone whatever the tree's modes say.
fn the_first_person_installs_for_everyone_through_privd() -> bool {
    let (Ok(loader), Ok(library)) = (
        fs::read("/bin/dl_search_rpath"),
        fs::read("/lib/libdlsearch-fixture.so"),
    ) else {
        return fail("the loader's fixtures are not in the base");
    };
    let everyone = script("everyone");
    let Some(tree) = stage(
        EVERYONE,
        &[
            (&format!("bin/{EVERYONE_COMMAND}"), &everyone, 0o775),
            (&format!("bin/{EVERYONE_LOADER}"), &loader, 0o755),
            (&format!("lib/{LIBRARY}"), &library, 0o644),
        ],
    ) else {
        return false;
    };
    let alias = tree.join("bin").join(EVERYONE_ALIAS);
    let aliased = format!("../bin/{EVERYONE_COMMAND}");
    if let Err(e) = std::os::unix::fs::symlink(&aliased, &alias) {
        return fail(format!("{}: {e}", alias.display()));
    }
    let (code, said) = install_for_everyone(EVERYONE, &tree);
    if code != Some(0) {
        return fail(format!("allowed by a click: {code:?} {said:?}"));
    }
    let bin = Path::new(LOCAL).join("bin");
    let installed = fs::symlink_metadata(bin.join(EVERYONE_COMMAND));
    let link = fs::read_link(bin.join(EVERYONE_ALIAS));
    let manifest = fs::symlink_metadata(Path::new(MANIFEST_DIR).join(EVERYONE));
    match (&installed, &link, &manifest) {
        (Ok(file), Ok(target), Ok(kept))
            if file.uid() == 0
                && file.mode() & 0o7777 == 0o755
                && target == Path::new(&aliased)
                && kept.uid() == 0 =>
        {
            true
        }
        _ => fail(format!(
            "what was installed: {installed:?}, {link:?}, {manifest:?}"
        )),
    }
}

/// `tree-install` of `tree` as `name` at `/usr/local`, asked by the first
/// person and allowed by their click, asked again while their desktop cannot
/// yet show the question: its status and what it said.
fn install_for_everyone(name: &str, tree: &Path) -> (Option<i32>, String) {
    let Some(desktop) = sessions::front_desktop() else {
        return (None, "no desktop came in front to ask".into());
    };
    let tree = tree.to_string_lossy();
    let deadline = Instant::now() + asking::DEADLINE;
    loop {
        let Some(mut run) = Run::start(
            "/bin/tree-install",
            SESSION_UID,
            Some(desktop),
            &session_env(SESSION_HOME),
            &[name, &tree, LOCAL],
        ) else {
            return (None, "tree-install did not start".into());
        };
        if run.says(WAITING, asking::DEADLINE) {
            asking::tab_and_enter();
            return run.end();
        }
        let (_, said) = run.end();
        if !said.contains(&Refusal::NoAgent.to_string()) || Instant::now() >= deadline {
            return (None, format!("the first person was not asked: {said}"));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// What not everyone may read goes into `/usr/local` for nobody, whatever
/// privd's mechanism, uid 0, may read: the install is refused.
fn a_tree_holding_what_not_everyone_may_read_is_refused() -> bool {
    let Some(tree) = stage(
        SECRET,
        &[
            ("bin/apps-secret", &script("secret"), 0o755),
            ("secret", b"uid 0's alone\n", 0o600),
        ],
    ) else {
        return false;
    };
    let (code, said) = install_for_everyone(SECRET, &tree);
    let landed = [
        format!("{LOCAL}/secret"),
        format!("{LOCAL}/bin/apps-secret"),
        format!("{MANIFEST_DIR}/{SECRET}"),
    ]
    .into_iter()
    .filter(|path| fs::symlink_metadata(path).is_ok())
    .collect::<Vec<_>>();
    matches!(code, Some(code) if code != 0)
        && said.contains("not everyone may read")
        && landed.is_empty()
        || fail(format!("{code:?} {said:?}, and {landed:?} installed"))
}

/// A link leading out of a tree, which would have everyone's command run
/// what its requester may change, refuses the install.
fn a_tree_whose_link_leads_out_of_it_is_refused() -> bool {
    let Some(tree) = stage(OUTWARD, &[("bin/apps-outward", &script("outward"), 0o755)]) else {
        return false;
    };
    let out = tree.join("bin").join("apps-out");
    let theirs = format!("../../../{SESSION_HOME}/.local/bin/{OWN_COMMAND}");
    if let Err(e) = std::os::unix::fs::symlink(&theirs, &out) {
        return fail(format!("{}: {e}", out.display()));
    }
    let (code, said) = install_for_everyone(OUTWARD, &tree);
    let landed = [
        format!("{LOCAL}/bin/apps-outward"),
        format!("{LOCAL}/bin/apps-out"),
        format!("{MANIFEST_DIR}/{OUTWARD}"),
    ]
    .into_iter()
    .filter(|path| fs::symlink_metadata(path).is_ok())
    .collect::<Vec<_>>();
    matches!(code, Some(code) if code != 0)
        && said.contains("outside the tree")
        && landed.is_empty()
        || fail(format!("{code:?} {said:?}, and {landed:?} installed"))
}

/// The first person installs a tree of their own into `~/.local` holding
/// nothing and asking nobody, its manifest kept in their home, and runs it
/// by its name.
fn the_first_person_installs_their_own_holding_nothing() -> bool {
    let Some(tree) = stage(
        OWN,
        &[(&format!("bin/{OWN_COMMAND}"), &script("own"), 0o755)],
    ) else {
        return false;
    };
    let dest = format!("{SESSION_HOME}/.local");
    let (code, said) = Run::start(
        "/bin/tree-install",
        SESSION_UID,
        None,
        &session_env(SESSION_HOME),
        &[OWN, &tree.to_string_lossy(), &dest],
    )
    .map(Run::end)
    .unwrap_or_default();
    if code != Some(0) || said.contains(WAITING) {
        return fail(format!("installing into {dest}: {code:?} {said:?}"));
    }
    let manifest = format!("{dest}/share/slopos/trees/{OWN}");
    if fs::symlink_metadata(&manifest).map(|meta| meta.uid()).ok() != Some(SESSION_UID) {
        return fail(format!("{manifest} is not the first person's"));
    }
    let (code, said) = shell_as(SESSION_UID, SESSION_HOME, OWN_COMMAND);
    code == Some(0) && said == "own\n"
        || fail(format!("{OWN_COMMAND} for its person: {code:?} {said:?}"))
}

/// The second person runs what everyone has, by its name and its link, a
/// program of it loading its library from `/usr/local/lib`, and finds
/// nothing of the first person's own: not by its name, and not by its path
/// through a home the root made for its person alone.
fn the_second_person_runs_everyone_s_and_not_the_first_s_own() -> bool {
    let Some(bob) = bob() else {
        return fail(format!("{BOB} has no uid"));
    };
    let first_home =
        fs::symlink_metadata(SESSION_HOME).map(|meta| (meta.uid(), meta.mode() & 0o7777));
    if first_home.as_ref().ok() != Some(&(SESSION_UID, 0o700)) {
        return fail(format!("{SESSION_HOME} is {first_home:?}"));
    }
    let home = bob_home();
    let loads = format!("{EVERYONE_LOADER} open {LIBRARY} {LOCAL}/lib/{LIBRARY}");
    let theirs = format!("{SESSION_HOME}/.local/bin/{OWN_COMMAND}");
    let ran = [
        shell_as(bob, &home, EVERYONE_COMMAND),
        shell_as(bob, &home, EVERYONE_ALIAS),
        shell_as(bob, &home, &loads),
        shell_as(bob, &home, OWN_COMMAND),
        shell_as(bob, &home, &theirs),
    ];
    match &ran {
        [
            (Some(0), everyone),
            (Some(0), too),
            (Some(0), _),
            (Some(127), _),
            (Some(code), _),
        ] if everyone == "everyone\n" && too == everyone && *code != 0 => true,
        _ => fail(format!("{BOB} ran: {ran:?}")),
    }
}

/// `priv` runs what the system installed only on a boot that began with
/// `/usr/local` the system's: here for root, which holds the ceiling and runs
/// it itself, as the same rule finds it for anyone.
fn priv_runs_what_the_system_installed() -> bool {
    let run = || {
        Command::new("/bin/priv")
            .arg(EVERYONE_COMMAND)
            .env_remove(slopos_priv_core::ENV_FD)
            .stdin(Stdio::null())
            .output()
    };
    let was = root_layout::settled();
    let _ = fs::remove_file(root_layout::SETTLED);
    let unsettled = run();
    let settled = root_layout::settle();
    let ran = run();
    if !was {
        let _ = fs::remove_file(root_layout::SETTLED);
    }
    match (unsettled, settled, ran) {
        (Ok(early), Ok(()), Ok(out))
            if !early.status.success() && out.status.success() && out.stdout == b"everyone\n" =>
        {
            true
        }
        other => fail(format!("priv {EVERYONE_COMMAND}: {other:?}")),
    }
}

fn the_trees_and_the_second_person_go() -> bool {
    let everyone = [
        format!("{LOCAL}/bin/{EVERYONE_COMMAND}"),
        format!("{LOCAL}/bin/{EVERYONE_ALIAS}"),
        format!("{LOCAL}/bin/{EVERYONE_LOADER}"),
        format!("{LOCAL}/lib/{LIBRARY}"),
        format!("{MANIFEST_DIR}/{EVERYONE}"),
        format!("{SESSION_HOME}/.local/bin/{OWN_COMMAND}"),
        format!("{SESSION_HOME}/.local/share/slopos/trees/{OWN}"),
    ];
    let mut kept: Vec<String> = everyone
        .iter()
        .filter(|path| {
            fs::remove_file(path).is_err_and(|e| e.kind() != std::io::ErrorKind::NotFound)
        })
        .cloned()
        .collect();
    let _ = fs::remove_dir(format!("{LOCAL}/lib"));
    for dir in ["bin", "share/slopos/trees", "share/slopos", "share", ""] {
        let _ = fs::remove_dir(format!("{SESSION_HOME}/.local/{dir}"));
    }
    let _ = fs::remove_dir_all(WORK);
    let ended = init_control::ask(InitCommand::EndSession);
    if users(&["remove", BOB, "--home"]) != Some(0) || bob().is_some() {
        kept.push(BOB.to_owned());
    }
    (kept.is_empty() || fail(format!("these stayed: {kept:?}")))
        && (ended.is_ok() || fail(format!("end-session: {ended:?}")))
}

const TREE_CASES: &[(&str, fn() -> bool)] = &[
    (
        "a_second_person_makes_usr_local_the_system_s",
        a_second_person_makes_usr_local_the_system_s,
    ),
    (
        "the_first_person_installs_for_everyone_through_privd",
        the_first_person_installs_for_everyone_through_privd,
    ),
    (
        "a_tree_holding_what_not_everyone_may_read_is_refused",
        a_tree_holding_what_not_everyone_may_read_is_refused,
    ),
    (
        "a_tree_whose_link_leads_out_of_it_is_refused",
        a_tree_whose_link_leads_out_of_it_is_refused,
    ),
    (
        "the_first_person_installs_their_own_holding_nothing",
        the_first_person_installs_their_own_holding_nothing,
    ),
    (
        "the_second_person_runs_everyone_s_and_not_the_first_s_own",
        the_second_person_runs_everyone_s_and_not_the_first_s_own,
    ),
    (
        "priv_runs_what_the_system_installed",
        priv_runs_what_the_system_installed,
    ),
    (
        "the_trees_and_the_second_person_go",
        the_trees_and_the_second_person_go,
    ),
];

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some(TREES) => slopos_slibc::test_harness::run(TREE_CASES),
        _ => slopos_slibc::test_harness::run(CASES),
    }
}
