//! Extensions' services as init runs them. The test extension's daemon runs
//! as its own user holding exactly its grant, takes its sockets from init,
//! logs, starts again after a crash with its port held, outlives a session
//! restart, is restarted by an update that changes it and not by one that
//! does not, is found again by an init that re-executed, and stops when its
//! extension does. Its helper starts once the daemon is ready, and a session
//! service runs with the session.
//!
//! The runner hands this binary every capability as uid 0, which privd's
//! `root` admits unasked.

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::time::{Duration, Instant};

use slopos_abi::caps::{SYS_INSPECT, UNIVERSAL};
use slopos_abi::fs::{S_IFREG, SESSION_UID, inode_flags::FS_IMMUTABLE_FL};
use slopos_abi::syscall::SYSCALL_CAPS_DROP;
use slopos_abi::task::MAX_TASKS;
use slopos_extension_core::image::Packer;
use slopos_extension_core::record::Record;
use slopos_extension_core::{RECORD, STORE, generation};
use slopos_priv_core::ENV_FD;
use slopos_slibc::test_harness::note;
use slopos_userland::init_control::{self, Command as Init, service_tag};
use slopos_userland::syscall::fs;

const NAME: &str = "svc";
const EXTCTL: &str = "/bin/extctl";
const DAEMON: &str = "/bin/ext_daemon";
const LIB: &str = "/lib/libexttest-fixture.so";
const DIR: &str = "/tmp/extension-service-test";
const DAEMON_PORT: u16 = 7411;
const HELPER_PORT: u16 = 7412;
/// What the logger lets a log grow to, and a write's worth past it.
const LOG_BOUND: u64 = 256 * 1024 + 64 * 1024;
/// Long enough for a service to stop and start again under TCG.
const DEADLINE: Duration = Duration::from_secs(120);
/// Long enough for a re-executed init to end or start what it would.
const SETTLE: Duration = Duration::from_secs(2);

fn fail(why: impl AsRef<str>) -> bool {
    note(why.as_ref());
    eprintln!("extension_service_test: {}", why.as_ref());
    false
}

fn until(mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < DEADLINE {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    done()
}

fn declaration() -> String {
    let serve = format!("serve {SYSCALL_CAPS_DROP}");
    format!(
        "user\nuser helper\nstate\nruntime\n\
         service daemon\n  program /opt/{NAME}/bin/daemon\n  args {serve}\n  caps SysInspect\n  \
         listen tcp {DAEMON_PORT}\n  listen unix ctl\n\
         service helper\n  program /opt/{NAME}/bin/helper\n  args {serve}\n  user helper\n  \
         after daemon\n  listen tcp {HELPER_PORT}\n\
         service desk\n  program /opt/{NAME}/bin/helper\n  args {serve}\n  session\n\
         service once\n  program /opt/{NAME}/bin/helper\n  args exit\n  restart never\n"
    )
}

/// Version `version`, its daemon's bytes `daemon`'s, with a note in `share`
/// when `note` names one.
fn image(version: u64, daemon: &[u8], note: Option<&str>) -> Option<String> {
    let read = |path: &str| {
        std::fs::read(path)
            .map_err(|e| fail(format!("{path}: {e}")))
            .ok()
    };
    let (helper, lib) = (read(DAEMON)?, read(LIB)?);
    let mut packer = Packer::default();
    packer.add("/bin/daemon", S_IFREG | 0o755, daemon.to_vec());
    packer.add("/bin/helper", S_IFREG | 0o755, helper);
    packer.add("/lib/libexttest.so", S_IFREG | 0o755, lib);
    if let Some(text) = note {
        packer.add("/share/note", S_IFREG | 0o644, text.as_bytes().to_vec());
    }
    let bytes = packer
        .finish(&format!("name {NAME}\nversion {version}\n"), &declaration())
        .map_err(|e| fail(format!("version {version} does not pack: {e}")))
        .ok()?;
    let path = format!("{DIR}/{NAME}-{version}.ext");
    std::fs::write(&path, bytes).ok()?;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));
    Some(path)
}

fn extctl(args: &[&str]) -> bool {
    match Command::new(EXTCTL).args(args).env_remove(ENV_FD).output() {
        Ok(out) if out.status.success() => true,
        Ok(out) => fail(format!(
            "extctl {}: {:?}: {}{}",
            args.join(" "),
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(e) => fail(format!("extctl: {e}")),
    }
}

fn install(version: u64, daemon: &[u8], note: Option<&str>) -> bool {
    image(version, daemon, note).is_some_and(|path| extctl(&["install", &path]))
}

fn daemon() -> Option<Vec<u8>> {
    std::fs::read(DAEMON)
        .map_err(|e| fail(format!("{DAEMON}: {e}")))
        .ok()
}

/// The record the store believes.
fn record() -> Option<Record> {
    let dir = format!("{STORE}/{NAME}");
    let newest = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| generation(e.ok()?.file_name().to_str()?, RECORD))
        .max()?;
    Record::parse(&std::fs::read_to_string(format!("{dir}/{RECORD}.{newest}")).ok()?)
}

fn ask_stream(mut stream: impl std::io::Read + Write, line: &str) -> Option<String> {
    stream.write_all(format!("{line}\n").as_bytes()).ok()?;
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer).ok()?;
    (!answer.is_empty()).then(|| answer.trim_end().to_owned())
}

fn ask(port: u16, line: &str) -> Option<String> {
    let stream = TcpStream::connect(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    ask_stream(stream, line)
}

/// What `who` answers, by key.
struct Who(String);

impl Who {
    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .split(' ')
            .find_map(|pair| pair.strip_prefix(key)?.strip_prefix('='))
    }

    fn number(&self, key: &str) -> Option<i64> {
        self.get(key)?.parse().ok()
    }
}

fn who(port: u16) -> Option<Who> {
    ask(port, "who").map(Who)
}

/// `who` once it answers, within the deadline.
fn who_within(port: u16) -> Option<Who> {
    let mut found = None;
    until(|| {
        found = who(port);
        found.is_some()
    });
    found
}

fn pid_of(port: u16) -> Option<i64> {
    who_within(port)?.number("pid")
}

fn uid_of(user: &str) -> Option<u32> {
    record()?.uid(user)
}

/// Whether process `pid` has exited, within the deadline.
fn gone(pid: i64) -> bool {
    until(|| init_control::login_of(pid as u32).is_none())
}

/// What the daemon's process carries while it lives.
fn daemon_tag() -> Option<u32> {
    record()
        .and_then(|r| r.number())
        .and_then(|number| service_tag(number, 0))
}

fn bound_by_another(port: u16) -> bool {
    TcpListener::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)).is_err()
}

fn sealed(path: &str) -> bool {
    use std::os::fd::AsRawFd;
    std::fs::File::open(path).is_ok_and(|file| {
        fs::inode_flags(file.as_raw_fd()).is_ok_and(|flags| flags & FS_IMMUTABLE_FL != 0)
    })
}

/// Who `/etc/passwd`, through the C library, says `name` is.
fn passwd_uid(name: &str) -> Option<u32> {
    let name = std::ffi::CString::new(name).ok()?;
    // SAFETY: `name` is a C string that outlives the call.
    let row = unsafe { slopos_slibc::pwd::getpwnam(name.as_ptr()) };
    // SAFETY: a non-null row is the library's, valid until its next call.
    (!row.is_null()).then(|| unsafe { (*row).pw_uid })
}

fn a_service_runs_as_its_user_holding_its_grant() -> bool {
    let Some(bytes) = daemon() else {
        return false;
    };
    if !install(1, &bytes, None) {
        return false;
    }
    let (Some(daemon), Some(helper)) = (who_within(DAEMON_PORT), who_within(HELPER_PORT)) else {
        return fail("the services did not answer");
    };
    let (Some(own), Some(helpers)) = (uid_of(NAME), uid_of("svc-helper")) else {
        return fail("the record gives the users no uids");
    };
    let held = |who: &Who| who.number("caps").map(|caps| caps as u64);
    let checks = [
        (daemon.number("uid") == Some(own.into()), "the daemon's uid"),
        (daemon.number("gid") == Some(own.into()), "the daemon's gid"),
        (
            held(&daemon) == Some(SYS_INSPECT | UNIVERSAL),
            "the daemon's capabilities",
        ),
        (
            daemon.get("user") == Some(NAME),
            "the daemon's name for itself",
        ),
        (
            daemon.get("lib") == Some("its-own-lib"),
            "the daemon's own library",
        ),
        (
            helper.number("uid") == Some(helpers.into()),
            "the helper's uid",
        ),
        (
            held(&helper) == Some(UNIVERSAL),
            "the helper's capabilities",
        ),
        (
            helper.number("started") >= daemon.number("ready"),
            "the helper's start after the daemon's readiness",
        ),
        (
            passwd_uid("svc-helper") == Some(helpers),
            "/etc/passwd's svc-helper",
        ),
        (
            sealed("/etc/passwd") && sealed("/etc/group"),
            "the user database's seal",
        ),
    ];
    if let Some((_, what)) = checks.iter().find(|(ok, _)| !ok) {
        return fail(format!(
            "{what} is wrong: daemon {} helper {}",
            daemon.0, helper.0
        ));
    }
    let tag = daemon_tag();
    let pid = daemon.number("pid").unwrap_or(0) as u32;
    init_control::login_of(pid) == tag
        || fail(format!(
            "the daemon carries tag {:?}, not {tag:?}",
            init_control::login_of(pid)
        ))
}

fn its_sockets_and_directories_are_init_s() -> bool {
    let fds = ask(DAEMON_PORT, "fds");
    let want =
        format!("count=2 names=tcp-{DAEMON_PORT}:ctl cwd=/var/lib/{NAME} home=/var/lib/{NAME}");
    if fds.as_deref() != Some(want.as_str()) {
        return fail(format!("the daemon was handed {fds:?}"));
    }
    let by_unix = UnixStream::connect(format!("/run/{NAME}/ctl"))
        .ok()
        .and_then(|stream| ask_stream(stream, "who"))
        .map(Who);
    if by_unix.and_then(|w| w.number("pid")) != pid_of(DAEMON_PORT) {
        return fail("the unix socket is not the daemon's");
    }
    let Some(own) = uid_of(NAME) else {
        return fail("no uid");
    };
    for dir in ["/var/lib", "/var/log", "/run"] {
        let path = format!("{dir}/{NAME}");
        if !std::fs::metadata(&path).is_ok_and(|meta| meta.is_dir() && meta.uid() == own) {
            return fail(format!("{path} is not the extension's"));
        }
    }
    bound_by_another(DAEMON_PORT) || fail("the daemon's port is free to bind")
}

fn it_logs() -> bool {
    let line = format!("hello-log-{}", std::process::id());
    if ask(DAEMON_PORT, &format!("say {line}")).as_deref() != Some("ok") {
        return fail("the daemon did not say it");
    }
    let log = format!("/var/log/{NAME}/daemon.log");
    let logged = || std::fs::read_to_string(&log).is_ok_and(|text| text.contains(&line));
    if !until(logged) {
        return fail(format!("{log} does not hold what the daemon said"));
    }
    if ask(DAEMON_PORT, "spew 400000").as_deref() != Some("ok") {
        return fail("the daemon did not spew");
    }
    let kept = format!("{log}.1");
    let rotated = || std::fs::metadata(&kept).is_ok_and(|meta| meta.len() > 0);
    if !until(rotated) {
        return fail(format!("{kept} never appeared"));
    }
    let size = |path: &str| std::fs::metadata(path).map_or(0, |meta| meta.len());
    if size(&log) > LOG_BOUND || size(&kept) > LOG_BOUND {
        return fail(format!(
            "the logs hold {} and {} bytes",
            size(&log),
            size(&kept)
        ));
    }
    std::path::Path::new(&format!("/var/log/{NAME}/helper.log")).exists()
        || fail("the helper keeps no log")
}

fn a_crash_restarts_it_with_its_port_held() -> bool {
    let Some(before) = pid_of(DAEMON_PORT) else {
        return fail("the daemon does not answer");
    };
    if ask(DAEMON_PORT, "crash").as_deref() != Some("bye") || !gone(before) {
        return fail("the daemon did not crash");
    }
    if !bound_by_another(DAEMON_PORT) {
        return fail("the port was free while the daemon was down");
    }
    let mut after = None;
    until(|| {
        after = who(DAEMON_PORT).and_then(|w| w.number("pid"));
        after.is_some_and(|pid| pid != before)
    }) || fail(format!("the daemon was {before} and is {after:?}"))
}

/// Every live process carrying `tag`, with its uid.
fn tagged(tag: u32) -> Vec<(u32, u32)> {
    let mut tasks = vec![slopos_abi::syscall::UserTaskEntry::default(); MAX_TASKS];
    let count = slopos_userland::syscall::core::process_list(&mut tasks).clamp(0, MAX_TASKS as i64);
    tasks.truncate(count as usize);
    let live = init_control::processes(tag);
    tasks
        .iter()
        .filter(|task| live.contains(&task.task_id))
        .map(|task| (task.task_id, task.uid))
        .collect()
}

fn a_session_restart_spares_it_and_runs_its_session_service() -> bool {
    let Some(before) = pid_of(DAEMON_PORT) else {
        return fail("the daemon does not answer");
    };
    let Some(desk) = record()
        .and_then(|r| r.number())
        .and_then(|n| service_tag(n, 2))
    else {
        return fail("no tag for the session service");
    };
    let was = !init_control::processes(init_control::DESKTOP).is_empty();
    if let Err(why) = init_control::ask(Init::RestartSession) {
        return fail(format!("restart-session: {why}"));
    }
    let runs = until(|| {
        let desk = tagged(desk);
        !desk.is_empty() && desk.iter().all(|&(_, uid)| uid == SESSION_UID)
    });
    let ended = init_control::ask(Init::EndSession);
    let gone = init_control::processes(desk).is_empty();
    if was && let Err(why) = init_control::ask(Init::RestartSession) {
        return fail(format!("restart-session: {why}"));
    }
    if !runs {
        return fail("the session service did not run as the person");
    }
    if let Err(why) = ended {
        return fail(format!("end-session: {why}"));
    }
    if !gone {
        return fail("the session service outlived the session");
    }
    pid_of(DAEMON_PORT) == Some(before) || fail("the daemon did not outlive the session")
}

fn an_update_restarts_what_it_changes_alone() -> bool {
    let (Some(daemon), Some(helper)) = (pid_of(DAEMON_PORT), pid_of(HELPER_PORT)) else {
        return fail("the services do not answer");
    };
    let Some(mut bytes) = self::daemon() else {
        return false;
    };
    if !install(2, &bytes, Some("a note neither service reads\n")) {
        return false;
    }
    let now = (pid_of(DAEMON_PORT), pid_of(HELPER_PORT));
    if now != (Some(daemon), Some(helper)) {
        return fail(format!(
            "an update changing neither restarted them: {now:?}"
        ));
    }
    bytes.extend_from_slice(b"\0another build");
    if !install(3, &bytes, None) {
        return false;
    }
    let mut now = (None, None);
    let restarted = until(|| {
        now = (pid_of(DAEMON_PORT), pid_of(HELPER_PORT));
        now.0.is_some_and(|pid| pid != daemon)
    });
    (restarted && now.1 == Some(helper))
        || fail(format!(
            "an update changing the daemon left it {daemon} and the helper {helper}: {now:?}"
        ))
}

/// How often the service that runs once has started.
fn runs_of_once() -> usize {
    std::fs::read_to_string(format!("/var/log/{NAME}/once.log"))
        .map_or(0, |log| log.matches("ext_daemon: pid").count())
}

fn a_reexecuted_init_keeps_it() -> bool {
    let Some(before) = pid_of(DAEMON_PORT) else {
        return fail("the daemon does not answer");
    };
    let once = runs_of_once();
    if !until(|| runs_of_once() > 0) {
        return fail("the service that runs once never ran");
    }
    if let Err(why) = init_control::ask(Init::Reexec) {
        return fail(format!("reexec: {why}"));
    }
    std::thread::sleep(SETTLE);
    let tag = daemon_tag();
    if pid_of(DAEMON_PORT) != Some(before)
        || tag.is_none()
        || init_control::login_of(before as u32) != tag
    {
        return fail("the re-execution restarted the daemon");
    }
    if ask(DAEMON_PORT, "crash").as_deref() != Some("bye") || !gone(before) {
        return fail("the daemon did not crash");
    }
    if !bound_by_another(DAEMON_PORT) {
        return fail("the re-executed init does not hold the daemon's port");
    }
    if runs_of_once() != once.max(1) {
        return fail("the re-execution ran the service that runs once again");
    }
    let mut after = None;
    until(|| {
        after = who(DAEMON_PORT).and_then(|w| w.number("pid"));
        after.is_some_and(|pid| pid != before)
    }) || fail(format!(
        "the re-executed init did not start it again: {after:?}"
    ))
}

fn it_stops_with_its_extension() -> bool {
    let stopped = |port| until(|| who(port).is_none() && !bound_by_another(port));
    if !extctl(&["disable", NAME]) || !stopped(DAEMON_PORT) {
        return fail("a disabled extension's daemon runs on");
    }
    if !extctl(&["enable", NAME]) || pid_of(DAEMON_PORT).is_none() {
        return fail("an enabled extension's daemon does not run");
    }
    let own = uid_of(NAME);
    if !extctl(&["remove", NAME]) || !stopped(DAEMON_PORT) || !stopped(HELPER_PORT) {
        return fail("a removed extension's services run on");
    }
    if passwd_uid(NAME).is_some() {
        return fail("/etc/passwd lists a removed extension's user");
    }
    (own.is_some() && uid_of(NAME) == own) || fail("the record lost its uid")
}

const CASES: &[(&str, fn() -> bool)] = &[
    (
        "a_service_runs_as_its_user_holding_its_grant",
        a_service_runs_as_its_user_holding_its_grant,
    ),
    (
        "its_sockets_and_directories_are_init_s",
        its_sockets_and_directories_are_init_s,
    ),
    ("it_logs", it_logs),
    (
        "a_crash_restarts_it_with_its_port_held",
        a_crash_restarts_it_with_its_port_held,
    ),
    (
        "a_session_restart_spares_it_and_runs_its_session_service",
        a_session_restart_spares_it_and_runs_its_session_service,
    ),
    (
        "an_update_restarts_what_it_changes_alone",
        an_update_restarts_what_it_changes_alone,
    ),
    ("a_reexecuted_init_keeps_it", a_reexecuted_init_keeps_it),
    ("it_stops_with_its_extension", it_stops_with_its_extension),
];

fn main() {
    let _ = std::fs::create_dir_all(DIR);
    let _ = std::fs::set_permissions(DIR, std::fs::Permissions::from_mode(0o755));
    slopos_slibc::test_harness::run(CASES);
}
