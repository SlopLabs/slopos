//! Extensions' actions, commands, apps and configuration end to end. The
//! test extension's actions run as its user holding what they declare, for an
//! app in `/usr/local` unasked or once the person allows it; its own program
//! is a principal only while it runs that program; privd reads an extension's
//! actions again as it serves; its command comes last on `PATH`, its app entry
//! replaces none, a configuration edited or signed restarts the service that
//! reads it, a rescue shell's `extctl --root` reaches a root it mounted, and
//! its mechanisms end once it stops.
//!
//! The runner hands this binary every capability as uid 0, which privd's
//! `root` admits unasked to the base's actions and to none of an extension's;
//! the person's requests come from a copy of this binary in `/usr/local/bin`,
//! run at uid 1000 holding nothing. A test boot runs no compositor, so a
//! request that needs the person is refused for want of anyone to ask.

use std::ffi::CString;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use slopos_abi::caps::{SYS_INSPECT, UNIVERSAL};
use slopos_abi::fs::{DEFAULT_PATH, S_IFREG, inode_flags::FS_IMMUTABLE_FL};
use slopos_abi::signal::SIGTERM;
use slopos_abi::syscall::SYSCALL_CAPS_DROP;
use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};
use slopos_extension_core::config::{self, Signed};
use slopos_extension_core::image::{Image, Packer};
use slopos_extension_core::record::Record;
use slopos_extension_core::sidecar::Sidecar;
use slopos_extension_core::{NAMESPACE, RECORD, STORE, generation, hex, sha256};
use slopos_priv_core::wire::{Exit, Reply, Request};
use slopos_priv_core::{DELEGABLE, ENV_FD};
use slopos_slibc::test_harness::note;
use slopos_tls_core::ed25519::SigningKey;
use slopos_tls_core::sshsig::{self, KEY_TYPE, PublicKey};
use slopos_userland::privd::{Connection, RESTART_WAIT, STATUS_REFUSED, request_id};
use slopos_userland::syscall::process::{self, Endowment};
use slopos_userland::syscall::{OwnedFd, fs, net};
use slopos_userland::{app_entry, init_control};

const NAME: &str = "act";
const EXTCTL: &str = "/bin/extctl";
const DAEMON: &str = "/bin/ext_daemon";
const LIB: &str = "/lib/libexttest-fixture.so";
const SELF: &str = "/bin/extension_action_test";
/// The person's app: this binary outside the base.
const APP: &str = "/usr/local/bin/act-app-probe";
const COMMAND: &str = "act-hello";
/// The argument of the action whose mechanism runs until it is ended.
const LINGER: &str = "linger";
const DIR: &str = "/tmp/extension-action-test";
/// A root as a rescue shell finds one mounted.
const RESCUED: &str = "/var/tmp/extension-action-root";
const PORT: u16 = 7421;
const ROOT: u32 = 0;
const USER: u32 = 1000;
/// `argv[1]` of the app, which asks for `argv[2]` and says how it went.
const ASK: &str = "--ask";
const ASKED: &str = "ASKED";
/// Long enough for privd or a service to start again under TCG.
const DEADLINE: Duration = Duration::from_secs(120);

fn fail(why: impl AsRef<str>) -> bool {
    note(why.as_ref());
    eprintln!("extension_action_test: {}", why.as_ref());
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

struct Ran {
    code: Option<i32>,
    out: String,
    err: String,
}

impl Ran {
    fn ok(&self) -> bool {
        self.code == Some(0)
    }

    fn says(&self, words: &str) -> bool {
        self.out.contains(words) || self.err.contains(words)
    }

    fn shown(&self) -> String {
        format!(
            "exit {:?}: {}{}",
            self.code,
            self.out.trim(),
            self.err.trim()
        )
    }
}

fn ran(cmd: &mut Command) -> Ran {
    match cmd.env_remove(ENV_FD).output() {
        Ok(out) => Ran {
            code: out.status.code(),
            out: String::from_utf8_lossy(&out.stdout).into_owned(),
            err: String::from_utf8_lossy(&out.stderr).into_owned(),
        },
        Err(e) => Ran {
            code: None,
            out: String::new(),
            err: e.to_string(),
        },
    }
}

fn extctl(uid: u32, args: &[&str]) -> Ran {
    ran(Command::new(EXTCTL)
        .args(args)
        .uid(uid)
        .gid(uid)
        .current_dir(DIR))
}

fn refused(ran: &Ran, says: &str) -> bool {
    !ran.ok() && ran.says(says)
        || fail(format!(
            "expected a refusal saying {says:?}: {}",
            ran.shown()
        ))
}

/// `bytes` at `DIR/<file>`, readable by uid 0 and the person alike.
fn put(file: &str, bytes: &[u8]) -> String {
    let path = format!("{DIR}/{file}");
    let _ = std::fs::write(&path, bytes);
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));
    path
}

fn key(seed: u8) -> SigningKey {
    SigningKey::from_seed(&[seed; 32])
}

fn fingerprint(key: &SigningKey) -> String {
    PublicKey(key.public()).fingerprint()
}

fn trust(key: &SigningKey, ceiling: &str, user: &str) -> bool {
    let line = format!("{KEY_TYPE} {} test key\n", PublicKey(key.public()).base64());
    let file = put(&format!("{}.pub", hex(&key.public()[..4])), line.as_bytes());
    let ran = extctl(
        ROOT,
        &["key", "add", &file, "ceiling", ceiling, "user", user],
    );
    ran.ok() || fail(format!("key add: {}", ran.shown()))
}

fn caps_drop() -> String {
    SYSCALL_CAPS_DROP.to_string()
}

/// The test extension at `version`: its `greet` asks the person from
/// version 2 on.
fn declaration(version: u64) -> String {
    let greet = if version == 1 { "user" } else { "user confirm" };
    let who = format!("who {}", caps_drop());
    format!(
        "user\nconfig\ncommand {COMMAND}\napp act-app\n\
         service watch\n  program /opt/{NAME}/bin/daemon\n  args serve {}\n  \
         reads /etc/{NAME}/act.conf\n  listen tcp {PORT}\n\
         action greet\n  program /opt/{NAME}/bin/daemon\n  args {who}\n  caps SysInspect\n  \
         allow {greet}\n  describe \"Say who it runs as\"\n\
         action careful\n  program /opt/{NAME}/bin/daemon\n  args {who}\n  caps SysInspect\n  \
         allow user confirm\n  describe \"Say who it runs as, once allowed\"\n\
         action own\n  program /opt/{NAME}/bin/daemon\n  args {who}\n  caps\n  \
         allow program /opt/{NAME}/bin/client\n  describe \"Answer its own client\"\n\
         action linger\n  program /opt/{NAME}/bin/daemon\n  args {LINGER}\n  caps\n  \
         allow user\n  describe \"Run until ended\"\n",
        caps_drop()
    )
}

fn read(path: &str) -> Option<Vec<u8>> {
    std::fs::read(path)
        .map_err(|e| fail(format!("{path}: {e}")))
        .ok()
}

fn pack(name: &str, version: u64, declare: &str) -> Option<String> {
    let (daemon, lib) = (read(DAEMON)?, read(LIB)?);
    let entry = |id: &str, title: &str| {
        format!("id={id}\nname={title}\nexec=/opt/{name}/bin/{COMMAND}\n").into_bytes()
    };
    let mut packer = Packer::default();
    for program in ["daemon", "client", COMMAND] {
        packer.add(&format!("/bin/{program}"), S_IFREG | 0o755, daemon.clone());
    }
    packer.add("/lib/libexttest.so", S_IFREG | 0o755, lib);
    packer.add(
        "/share/slopos/apps/act-app.app",
        S_IFREG | 0o644,
        entry("act-app", "Act"),
    );
    packer.add(
        "/share/slopos/apps/org.slopos.terminal.app",
        S_IFREG | 0o644,
        entry("org.slopos.terminal", "Not the terminal"),
    );
    let bytes = packer
        .finish(&format!("name {name}\nversion {version}\n"), declare)
        .map_err(|e| fail(format!("{name} {version} does not pack: {e}")))
        .ok()?;
    Some(put(&format!("{name}-{version}.ext"), &bytes))
}

/// The image at `path` signed by `key`, its sidecar beside it.
fn sign(key: &SigningKey, path: &str) -> bool {
    let Some(bytes) = read(path) else {
        return false;
    };
    let Ok(image) = Image::check(&bytes) else {
        return fail(format!("{path} does not check"));
    };
    let sidecar = Sidecar {
        name: image.release.name.clone(),
        version: image.release.version,
        sha256: sha256(&bytes),
    }
    .render();
    let stem = path.strip_suffix(".ext").unwrap_or(path);
    let file = |suffix: &str| format!("{}{suffix}", stem.rsplit('/').next().unwrap_or(stem));
    put(&file(".meta"), sidecar.as_bytes());
    put(
        &file(".meta.sig"),
        &sshsig::sign(key, NAMESPACE, sidecar.as_bytes()),
    );
    true
}

fn sealed(path: &str) -> bool {
    std::fs::File::open(path).is_ok_and(|file| {
        fs::inode_flags(file.as_raw_fd()).is_ok_and(|flags| flags & FS_IMMUTABLE_FL != 0)
    })
}

fn set_sealed(path: &Path, seal: bool) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(flags) = fs::inode_flags(file.as_raw_fd()) else {
        return false;
    };
    let flags = if seal {
        flags | FS_IMMUTABLE_FL
    } else {
        flags & !FS_IMMUTABLE_FL
    };
    fs::set_inode_flags(file.as_raw_fd(), flags).is_ok()
}

/// The record the store at `root` believes: its newest sealed generation.
fn record_at(root: &str, name: &str) -> Option<Record> {
    let dir = format!("{root}{STORE}/{name}");
    let mut generations: Vec<u64> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| generation(e.ok()?.file_name().to_str()?, RECORD))
        .collect();
    generations.sort_unstable_by(|a, b| b.cmp(a));
    let path = generations
        .into_iter()
        .map(|g| format!("{dir}/{RECORD}.{g}"))
        .find(|path| sealed(path))?;
    Record::parse(&std::fs::read_to_string(path).ok()?)
}

fn record(name: &str) -> Option<Record> {
    record_at("", name)
}

/// How a request to privd went.
#[derive(Debug)]
enum Outcome {
    Ran(i32, String),
    /// Put to the person, or refused because nobody can be asked.
    Asked,
    Denied(String),
    Lost(String),
}

/// Ask `privd` for `action` with `who`'s arguments, its output caught.
fn ask_privd(privd: &mut Connection, action: &str) -> Outcome {
    ask_privd_with(privd, action, &["who", &caps_drop()])
}

fn ask_privd_with(privd: &mut Connection, action: &str, args: &[&str]) -> Outcome {
    let (Ok((out, write)), Ok(null)) = (
        fs::pipe(),
        fs::open_cstr(c"/dev/null", slopos_abi::fs::O_RDWR),
    ) else {
        return Outcome::Lost("no pipe".into());
    };
    let request = Request {
        id: request_id(),
        action: action.to_owned(),
        args: args.iter().map(|arg| arg.as_bytes().to_vec()).collect(),
        cwd: false,
    };
    if let Err(why) = privd.send(&request, &[null.raw(), write.raw(), 2]) {
        return Outcome::Lost(why);
    }
    drop(write);
    loop {
        match privd.reply(request.id) {
            Ok(Some(Reply::Started { .. })) => {}
            Ok(Some(Reply::Asking)) => return Outcome::Asked,
            Ok(Some(Reply::Exited(exit))) => {
                let mut said = String::new();
                // SAFETY: `out` is a pipe end this function owns.
                let _ =
                    unsafe { std::fs::File::from_raw_fd(out.into_raw()) }.read_to_string(&mut said);
                let code = match exit {
                    Exit::Code(code) => code,
                    Exit::Signal(signal) => -i32::from(signal),
                };
                return Outcome::Ran(code, said.trim().to_owned());
            }
            Ok(Some(Reply::Denied { reason, .. })) if reason.contains("nothing can ask them") => {
                return Outcome::Asked;
            }
            Ok(Some(Reply::Denied { reason, .. })) => return Outcome::Denied(reason),
            Ok(None) => return Outcome::Lost("privd closed the connection".into()),
            Err(why) => return Outcome::Lost(why),
        }
    }
}

/// The app's half: ask for `action`, with `args` if any, as this process and
/// say how it went.
fn app(action: &str, args: &[String]) -> i32 {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let outcome = match Connection::open_within(RESTART_WAIT) {
        Ok(mut privd) if args.is_empty() => ask_privd(&mut privd, action),
        Ok(mut privd) => ask_privd_with(&mut privd, action, &args),
        Err(why) => Outcome::Lost(why),
    };
    match outcome {
        Outcome::Ran(code, said) => println!("EXIT {code} {said}"),
        Outcome::Asked => println!("{ASKED}"),
        Outcome::Denied(why) => println!("DENIED {why}"),
        Outcome::Lost(why) => println!("LOST {why}"),
    }
    0
}

/// What the person's app says of asking for `action`.
fn from_the_app(action: &str) -> String {
    let ran = ran(Command::new(APP).args([ASK, action]).uid(USER).gid(USER));
    ran.out.trim().to_owned()
}

/// What `who` printed, by key.
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

fn the_app_asks_its_extension_s_actions() -> bool {
    let owner = key(41);
    if !trust(&owner, "SysInspect", "SysInspect") {
        return false;
    }
    let Some(v1) = pack(NAME, 1, &declaration(1)) else {
        return false;
    };
    let installed = sign(&owner, &v1) && {
        let ran = extctl(ROOT, &["install-signed", &v1]);
        ran.ok() || fail(format!("the signed install: {}", ran.shown()))
    };
    if !installed {
        return false;
    }
    let copied = std::fs::create_dir_all("/usr/local/bin")
        .and_then(|()| std::fs::copy(SELF, APP))
        .and_then(|_| std::fs::set_permissions(APP, std::fs::Permissions::from_mode(0o755)));
    if let Err(e) = copied {
        return fail(format!("{APP}: {e}"));
    }
    let mut said = String::new();
    if !until(|| {
        said = from_the_app("ext.act.greet");
        said.starts_with("EXIT 0 ")
    }) {
        return fail(format!("greet, asked by the app: {said}"));
    }
    let who = Who(said.trim_start_matches("EXIT 0 ").to_owned());
    let Some(uid) = record(NAME).and_then(|r| r.uid(NAME)) else {
        return fail("the record gives act no uid");
    };
    let path = format!("{}:/opt/{NAME}/bin", slopos_priv_core::MECHANISM_PATH);
    let checks = [
        (who.number("uid") == Some(uid.into()), "its uid"),
        (who.number("gid") == Some(uid.into()), "its gid"),
        (
            who.number("caps").map(|caps| caps as u64) == Some(SYS_INSPECT | UNIVERSAL),
            "its capabilities",
        ),
        (who.get("path") == Some(path.as_str()), "its search path"),
        (who.get("action") == Some("ext.act.greet"), "its action"),
        (who.get("cwd") == Some("/"), "its working directory"),
        (who.get("lib") == Some("its-own-lib"), "its own library"),
    ];
    if let Some((_, what)) = checks.iter().find(|(ok, _)| !ok) {
        return fail(format!("greet's mechanism: {what}: {said}"));
    }
    let careful = from_the_app("ext.act.careful");
    if careful != ASKED {
        return fail(format!("careful, asked by the app: {careful}"));
    }
    let as_root = Connection::open().map(|mut privd| ask_privd(&mut privd, "ext.act.greet"));
    matches!(&as_root, Ok(Outcome::Denied(why)) if why.contains("may not ask"))
        || fail(format!("greet, asked by root: {as_root:?}"))
}

/// The extension's client asks privd on the connection it made: its own
/// action runs for it until it maps code from outside its activation.
fn its_own_program_is_a_principal_until_it_maps_foreign_code() -> bool {
    let foreign = format!("{DIR}/foreign.so");
    let staged = std::fs::copy(LIB, &foreign)
        .and_then(|_| std::fs::set_permissions(&foreign, std::fs::Permissions::from_mode(0o644)));
    if staged.is_err() {
        return fail("could not stage code from outside the activation");
    }
    let child = Command::new(format!("/opt/{NAME}/bin/client"))
        .args(["client", &caps_drop()])
        .uid(USER)
        .gid(USER)
        .env_remove(ENV_FD)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn();
    let Ok(mut child) = child else {
        return fail(format!("the client did not start: {child:?}"));
    };
    let (Some(mut to), Some(from)) = (child.stdin.take(), child.stdout.take()) else {
        return fail("the client has no pipes");
    };
    let mut from = BufReader::new(from);
    let mut tell = |line: &str| {
        let mut said = String::new();
        let _ = writeln!(to, "{line}").and_then(|()| from.read_line(&mut said).map(drop));
        said.trim().to_owned()
    };
    let first = tell("ask ext.act.own");
    let mapped = tell(&format!("map {foreign}"));
    let lapsed = tell("ask ext.act.own");
    drop(tell);
    let _ = child.kill();
    let _ = child.wait();
    if first != "EXIT 0" {
        return fail(format!("own, asked by the client: {first}"));
    }
    if mapped != "mapped" {
        return fail(format!("the client could not map {foreign}: {mapped}"));
    }
    lapsed.starts_with("DENIED") && lapsed.contains("no longer runs")
        || fail(format!(
            "own, once the client mapped foreign code: {lapsed}"
        ))
}

fn its_command_comes_last_on_path() -> bool {
    let link = format!("/opt/bin/{COMMAND}");
    let target = std::fs::read_link(&link);
    if target.as_deref().ok() != Some(Path::new(&format!("/opt/{NAME}/bin/{COMMAND}"))) {
        return fail(format!("{link} links to {target:?}"));
    }
    if !DEFAULT_PATH
        .to_bytes()
        .ends_with(b":/usr/local/bin:/opt/bin")
    {
        return fail(format!("the default path is {DEFAULT_PATH:?}"));
    }
    let shell = || {
        ran(Command::new("/bin/shell")
            .args(["-c", COMMAND])
            .env_remove("PATH")
            .uid(USER)
            .gid(USER))
    };
    let found = shell();
    if !found.ok() || !found.says("ext_daemon: pid") {
        return fail(format!("{COMMAND} from /opt/bin: {}", found.shown()));
    }
    let mine = format!("/usr/local/bin/{COMMAND}");
    let planted = std::fs::write(&mine, "#!/bin/shell\necho the session-owned one\n")
        .and_then(|()| std::fs::set_permissions(&mine, std::fs::Permissions::from_mode(0o755)));
    if let Err(e) = planted {
        return fail(format!("{mine}: {e}"));
    }
    let shadowed = shell();
    let _ = std::fs::remove_file(&mine);
    shadowed.ok() && shadowed.says("the session-owned one")
        || fail(format!("{COMMAND} beside {mine}: {}", shadowed.shown()))
}

fn its_app_entry_replaces_none() -> bool {
    let shares = app_entry::shares(None);
    if shares.first().map(|p| p.as_path()) != Some(Path::new(&format!("/opt/{NAME}/share"))) {
        return fail(format!("the dock reads {shares:?}"));
    }
    let entries = app_entry::read(&shares);
    let exec = |id: &str| entries.iter().find(|e| e.id == id).map(|e| e.exec.as_str());
    let ours = format!("/opt/{NAME}/bin/{COMMAND}");
    if exec("act-app") != Some(ours.as_str()) {
        return fail(format!("the dock's act-app runs {:?}", exec("act-app")));
    }
    exec("org.slopos.terminal") == Some("/bin/terminal")
        || fail(format!(
            "the dock's terminal runs {:?}",
            exec("org.slopos.terminal")
        ))
}

/// The pid privd answers as.
fn privd_pid() -> Option<i32> {
    let privd = Connection::open().ok()?;
    net::peer_cred(privd.raw()).ok().map(|cred| cred.pid)
}

/// An update that makes `greet` ask the person reaches privd while it keeps
/// serving: the same process answers before and after.
fn privd_reads_an_extension_s_actions_again_as_it_serves() -> bool {
    let Some(before) = privd_pid() else {
        return fail("privd does not answer");
    };
    let Some(v2) = pack(NAME, 2, &declaration(2)) else {
        return false;
    };
    let updated = sign(&key(41), &v2) && {
        let ran = extctl(ROOT, &["install-signed", &v2]);
        ran.ok() || fail(format!("the signed update: {}", ran.shown()))
    };
    if !updated {
        return false;
    }
    let mut said = String::new();
    if !until(|| {
        said = from_the_app("ext.act.greet");
        said == ASKED
    }) {
        return fail(format!(
            "greet after an update that asks the person: {said}"
        ));
    }
    privd_pid() == Some(before)
        || fail(format!(
            "privd {before} gave way to {:?} for an action change",
            privd_pid()
        ))
}

fn ask_port(line: &str) -> Option<String> {
    let mut stream = TcpStream::connect(SocketAddrV4::new(Ipv4Addr::LOCALHOST, PORT)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    stream.write_all(format!("{line}\n").as_bytes()).ok()?;
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer).ok()?;
    (!answer.is_empty()).then(|| answer.trim_end().to_owned())
}

/// The service's pid and what it reads of its configuration, once both
/// differ from `before` and the second is `want`.
fn served(before: Option<i64>, want: &str) -> Option<i64> {
    let mut pid = None;
    let conf = format!("read /etc/{NAME}/act.conf");
    until(|| {
        pid = ask_port("who").and_then(|who| Who(who).number("pid"));
        pid.is_some() && pid != before && ask_port(&conf).as_deref() == Some(want)
    })
    .then_some(pid)
    .flatten()
}

/// A configuration file signed by `key` for serial `serial`, its sidecar
/// beside it.
fn signed_config(key: &SigningKey, file: &str, text: &str, serial: u64) -> String {
    let path = put(file, text.as_bytes());
    let sidecar = Signed {
        name: NAME.into(),
        file: "act.conf".into(),
        serial,
        sha256: sha256(text.as_bytes()),
    }
    .render();
    put(&format!("{file}.meta"), sidecar.as_bytes());
    put(
        &format!("{file}.meta.sig"),
        &sshsig::sign(key, config::NAMESPACE, sidecar.as_bytes()),
    );
    path
}

fn a_configuration_edited_or_signed_restarts_what_reads_it() -> bool {
    let Some(first) = served(None, "-") else {
        return fail("the service does not answer");
    };
    let edited = put("edited.conf", b"edited by the person\n");
    let editor = format!("/bin/cp {edited}");
    let edit = |uid| {
        ran(Command::new(EXTCTL)
            .args(["config", "edit", NAME, "act.conf"])
            .env("EDITOR", &editor)
            .uid(uid)
            .gid(uid)
            .current_dir(DIR))
    };
    let installed = format!("/etc/{NAME}/act.conf");
    let by_person = edit(USER);
    if by_person.code != Some(STATUS_REFUSED) || !by_person.says("nothing can ask them") {
        return fail(format!("the person's edit: {}", by_person.shown()));
    }
    if Path::new(&installed).exists() {
        return fail("a refused edit went in");
    }
    let by_root = edit(ROOT);
    if !by_root.ok() || !sealed(&installed) || !sealed(&format!("/etc/{NAME}")) {
        return fail(format!("root's edit: {}", by_root.shown()));
    }
    let Some(second) = served(Some(first), "edited by the person") else {
        return fail("the service did not start again on the edited file");
    };
    let owner = key(41);
    let applied = signed_config(&owner, "signed.conf", "signed by its key\n", 1);
    let apply = extctl(USER, &["config", "apply", &applied]);
    if !apply.ok() {
        return fail(format!("the signed configuration: {}", apply.shown()));
    }
    if served(Some(second), "signed by its key").is_none() {
        return fail("the service did not start again on the signed file");
    }
    if record(NAME).map(|r| r.config) != Some(1) {
        return fail(format!(
            "the record keeps serial {:?}",
            record(NAME).map(|r| r.config)
        ));
    }
    let old = signed_config(&owner, "old.conf", "an old one\n", 1);
    let stranger = key(42);
    if !trust(&stranger, "-", "-") {
        return false;
    }
    let theirs = signed_config(&stranger, "theirs.conf", "not its key's\n", 2);
    refused(&extctl(USER, &["config", "apply", &old]), "not past 1")
        && refused(
            &extctl(USER, &["config", "apply", &theirs]),
            "not the key that installed",
        )
        && (std::fs::read_to_string(&installed).ok().as_deref() == Some("signed by its key\n")
            || fail("a refused configuration went in"))
}

/// `extctl` run holding every capability a mechanism can, as the rescue
/// shell runs what it starts.
fn rescued(args: &[&str]) -> Ran {
    let fail = |why: &str| Ran {
        code: None,
        out: String::new(),
        err: why.to_owned(),
    };
    let (Ok((out, out_end)), Ok((err, err_end)), Ok(null)) = (
        fs::pipe(),
        fs::pipe(),
        fs::open_cstr(c"/dev/null", slopos_abi::fs::O_RDWR),
    ) else {
        return fail("no pipes");
    };
    let argv: Vec<CString> = std::iter::once("extctl")
        .chain(args.iter().copied())
        .map(|arg| CString::new(arg).expect("no NUL in an argument"))
        .collect();
    let argv: Vec<*const u8> = argv.iter().map(|arg| arg.as_ptr().cast()).collect();
    let actions = [
        process::clone_fd(null.raw(), 0),
        process::clone_fd(out_end.raw(), 1),
        process::clone_fd(err_end.raw(), 2),
    ];
    let pid = process::spawn_endowed(
        EXTCTL.as_bytes(),
        &argv,
        &[],
        b"/",
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &actions,
        0,
        Endowment {
            caps: DELEGABLE,
            ..Endowment::NONE
        },
    );
    drop((out_end, err_end));
    if pid <= 0 {
        return fail("extctl did not start");
    }
    let drain = |fd: OwnedFd| {
        let mut said = String::new();
        // SAFETY: `fd` is a pipe end this function owns.
        let _ = unsafe { std::fs::File::from_raw_fd(fd.into_raw()) }.read_to_string(&mut said);
        said
    };
    let (out, err) = (drain(out), drain(err));
    let code = match process::wait_exit_code(pid as u32) {
        -1 => None,
        code => Some(code),
    };
    Ran { code, out, err }
}

/// Every file and directory under `path` unsealed, then taken away.
fn clear(path: &Path) {
    if path.is_dir() {
        set_sealed(path, false);
        for entry in std::fs::read_dir(path).into_iter().flatten().flatten() {
            clear(&entry.path());
        }
        let _ = std::fs::remove_dir(path);
    } else {
        set_sealed(path, false);
        let _ = std::fs::remove_file(path);
    }
}

/// A root mounted beside this one holding a copy of the extension's record,
/// as a rescue shell finds an installed disk's.
fn stage_rescued_root() -> bool {
    clear(Path::new(RESCUED));
    let store = format!("{RESCUED}{STORE}");
    let dir = format!("{store}/{NAME}");
    let Some(record) = record(NAME) else {
        return fail("no record to copy");
    };
    let copy = format!("{dir}/{RECORD}.1");
    let staged = std::fs::create_dir_all(&dir).is_ok()
        && std::fs::write(&copy, record.render()).is_ok()
        && [copy.as_str(), dir.as_str(), store.as_str()]
            .iter()
            .all(|path| set_sealed(Path::new(path), true));
    staged || fail(format!("could not stage {RESCUED}"))
}

fn a_rescue_shell_s_extctl_reaches_a_root_it_mounted() -> bool {
    if !stage_rescued_root() {
        return false;
    }
    let outcome = (|| {
        let without = extctl(ROOT, &["--root", RESCUED, "disable", NAME]);
        if !refused(&without, "not an argument list") {
            return false;
        }
        let disable = rescued(&["--root", RESCUED, "disable", NAME]);
        let there = record_at(RESCUED, NAME).map(|r| r.enabled);
        let here = record(NAME).map(|r| r.enabled);
        if !disable.ok() || there != Some(false) || here != Some(true) {
            return fail(format!(
                "disable on the mounted root: {}; enabled there {there:?}, here {here:?}",
                disable.shown()
            ));
        }
        let status = rescued(&["--root", RESCUED, "status", NAME]);
        status.ok() && status.says(&format!("{NAME}: disabled, floor 2"))
            || fail(format!("status on the mounted root: {}", status.shown()))
    })();
    clear(Path::new(RESCUED));
    outcome
}

/// A mechanism its extension runs is ended once the extension stops, as its
/// services are.
fn a_stopped_extension_s_mechanisms_end() -> bool {
    let Some(tag) = record(NAME)
        .and_then(|record| record.number())
        .and_then(init_control::action_tag)
    else {
        return fail("the record gives act no number");
    };
    let app = Command::new(APP)
        .args([ASK, "ext.act.linger", LINGER])
        .uid(USER)
        .gid(USER)
        .stdout(Stdio::piped())
        .spawn();
    let mut app = match app {
        Ok(app) => app,
        Err(e) => return fail(format!("{APP}: {e}")),
    };
    let ended = until(|| !init_control::processes(tag).is_empty())
        && {
            let ran = extctl(ROOT, &["disable", NAME]);
            ran.ok() || fail(format!("disable {NAME}: {}", ran.shown()))
        }
        && (until(|| init_control::processes(tag).is_empty())
            || fail("the mechanism outlived its extension"));
    if !ended {
        let _ = app.kill();
    }
    let mut said = String::new();
    if let Some(mut out) = app.stdout.take() {
        let _ = out.read_to_string(&mut said);
    }
    let _ = app.wait();
    let enabled = extctl(ROOT, &["enable", NAME]);
    ended
        && (said.trim() == format!("EXIT -{SIGTERM}")
            || fail(format!("the requester, its mechanism ended: {said}")))
        && (enabled.ok() || fail(format!("enable {NAME}: {}", enabled.shown())))
}

fn removing_takes_its_actions_and_command_away() -> bool {
    let ran = extctl(ROOT, &["remove", NAME]);
    if !ran.ok() {
        return fail(format!("remove {NAME}: {}", ran.shown()));
    }
    let _ = std::fs::remove_file(APP);
    for seed in [41, 42] {
        let _ = extctl(ROOT, &["key", "remove", &fingerprint(&key(seed))]);
    }
    if Path::new(&format!("/opt/bin/{COMMAND}")).exists() {
        return fail("the removed extension's command is still on PATH");
    }
    let mut said = None;
    until(|| {
        said = Connection::open()
            .ok()
            .map(|mut privd| ask_privd(&mut privd, "ext.act.greet"));
        matches!(&said, Some(Outcome::Denied(why)) if why.contains("no action"))
    }) || fail(format!("greet once removed: {said:?}"))
}

const CASES: &[(&str, fn() -> bool)] = &[
    (
        "the_app_asks_its_extension_s_actions",
        the_app_asks_its_extension_s_actions,
    ),
    (
        "its_own_program_is_a_principal_until_it_maps_foreign_code",
        its_own_program_is_a_principal_until_it_maps_foreign_code,
    ),
    (
        "its_command_comes_last_on_path",
        its_command_comes_last_on_path,
    ),
    ("its_app_entry_replaces_none", its_app_entry_replaces_none),
    (
        "privd_reads_an_extension_s_actions_again_as_it_serves",
        privd_reads_an_extension_s_actions_again_as_it_serves,
    ),
    (
        "a_configuration_edited_or_signed_restarts_what_reads_it",
        a_configuration_edited_or_signed_restarts_what_reads_it,
    ),
    (
        "a_rescue_shell_s_extctl_reaches_a_root_it_mounted",
        a_rescue_shell_s_extctl_reaches_a_root_it_mounted,
    ),
    (
        "a_stopped_extension_s_mechanisms_end",
        a_stopped_extension_s_mechanisms_end,
    ),
    (
        "removing_takes_its_actions_and_command_away",
        removing_takes_its_actions_and_command_away,
    ),
];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some(ASK) {
        let action = args.get(2).map_or("", String::as_str);
        std::process::exit(app(action, args.get(3..).unwrap_or_default()));
    }
    let _ = std::fs::create_dir_all(DIR);
    let _ = std::fs::set_permissions(DIR, std::fs::Permissions::from_mode(0o755));
    slopos_slibc::test_harness::run(CASES);
}
