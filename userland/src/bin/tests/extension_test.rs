//! Extensions installed end to end through `/bin/extctl` and privd. The
//! binary runs as uid 0 holding everything, which privd's `root` admits
//! unasked; the session's requests run in children at uid 1000 holding
//! nothing, and a test boot runs no compositor, so one that needs the person
//! is refused for want of anyone to ask.

use slopos_priv_core::Refused;
use std::os::unix::fs::{FileExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use slopos_abi::fs::{S_IFREG, inode_flags::FS_IMMUTABLE_FL};
use slopos_extension_core::elf::{self, Kind};
use slopos_extension_core::image::{Image, Packer};
use slopos_extension_core::record::Record;
use slopos_extension_core::sidecar::Sidecar;
use slopos_extension_core::source::write_image;
use slopos_extension_core::{INTERPRETER, NAMESPACE, RECORD, STORE, generation, hex};
use slopos_priv_core::{ENV_ACTION, ENV_FD};
use slopos_slibc::test_harness::note;
use slopos_tls_core::ed25519::SigningKey;
use slopos_tls_core::sshsig::{self, KEY_TYPE, PublicKey};
use slopos_userland::privd::STATUS_REFUSED;
use slopos_userland::syscall::{UserSysInfo, core as sys_core, fs};

const EXTCTL: &str = "/bin/extctl";
/// A program linked against `libc.so` and a library of its image's, as an
/// extension's are, which declared with `exit` exits once started.
const PROBE: &str = "/bin/ext_daemon";
const PROBE_LIB: &str = "/lib/libexttest-fixture.so";
const DIR: &str = "/tmp/extension-test";
const ROOT: u32 = 0;
const USER: u32 = 1000;

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

fn extctl(uid: u32, args: &[&str]) -> Ran {
    let out = Command::new(EXTCTL)
        .args(args)
        .uid(uid)
        .gid(uid)
        .env_remove(ENV_FD)
        .current_dir(DIR)
        .output();
    match out {
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

fn fail(why: impl AsRef<str>) -> bool {
    note(why.as_ref());
    eprintln!("extension_test: {}", why.as_ref());
    false
}

/// `bytes` at `DIR/<file>`, readable by uid 0 and the session alike.
fn put(file: &str, bytes: &[u8]) -> String {
    let path = format!("{DIR}/{file}");
    let _ = std::fs::write(&path, bytes);
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));
    path
}

fn declaration(name: &str, caps: &str, unasked: &str) -> String {
    let mut text = format!(
        "user\nservice probe\n  program /opt/{name}/bin/probe\n  args exit\n  caps {caps}\n  restart never\n"
    );
    if !unasked.is_empty() {
        text.push_str(&format!(
            "action act\n  program /opt/{name}/bin/probe\n  args act\n  caps {unasked}\n  allow user\n  describe \"act\"\n"
        ));
    }
    text
}

/// `name` at `version`, its program `program`, declared by `declare`.
fn pack(name: &str, version: u64, declare: &str, program: &[u8]) -> Option<Vec<u8>> {
    let lib = std::fs::read(PROBE_LIB)
        .map_err(|e| fail(format!("{PROBE_LIB}: {e}")))
        .ok()?;
    let mut packer = Packer::default();
    packer.add("/bin/probe", S_IFREG | 0o755, program.to_vec());
    packer.add("/lib/libexttest.so", S_IFREG | 0o755, lib);
    packer
        .finish(&format!("name {name}\nversion {version}\n"), declare)
        .map_err(|e| fail(format!("{name} {version} does not pack: {e}")))
        .ok()
}

fn image(name: &str, version: u64, caps: &str, unasked: &str) -> Option<String> {
    let probe = std::fs::read(PROBE)
        .map_err(|e| fail(format!("{PROBE}: {e}")))
        .ok()?;
    let bytes = pack(name, version, &declaration(name, caps, unasked), &probe)?;
    Some(put(&format!("{name}-{version}.ext"), &bytes))
}

/// The sidecar of the image at `path` and `key`'s signature of it, beside
/// the image as `extctl install-signed` looks for them.
fn sign(key: &SigningKey, path: &str) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return fail(format!("{path} is gone"));
    };
    let Ok(image) = Image::check(&bytes[..]) else {
        return fail(format!("{path} does not check"));
    };
    let sidecar = Sidecar {
        name: image.release.name.clone(),
        version: image.release.version,
        sha256: image.digest,
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

fn key(seed: u8) -> SigningKey {
    SigningKey::from_seed(&[seed; 32])
}

fn fingerprint(key: &SigningKey) -> String {
    PublicKey(key.public()).fingerprint()
}

/// Trust `key` as root, from a public key file as `ssh-keygen` writes one.
fn trust(key: &SigningKey, ceiling: &str, user: &str) -> bool {
    let line = format!("{KEY_TYPE} {} test key\n", PublicKey(key.public()).base64());
    let file = put(&format!("{}.pub", hex(&key.public()[..4])), line.as_bytes());
    let ran = extctl(
        ROOT,
        &["key", "add", &file, "ceiling", ceiling, "user", user],
    );
    ran.ok() || fail(format!("key add: {}", ran.shown()))
}

fn flags(path: &str) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let file = std::fs::File::open(path).ok()?;
    fs::inode_flags(file.as_raw_fd()).ok()
}

fn sealed(path: &str) -> bool {
    flags(path).is_some_and(|flags| flags & FS_IMMUTABLE_FL != 0)
}

fn set_sealed(path: &str, seal: bool) -> bool {
    use std::os::fd::AsRawFd;
    let (Ok(file), Some(flags)) = (std::fs::File::open(path), flags(path)) else {
        return false;
    };
    let flags = if seal {
        flags | FS_IMMUTABLE_FL
    } else {
        flags & !FS_IMMUTABLE_FL
    };
    fs::set_inode_flags(file.as_raw_fd(), flags).is_ok()
}

/// The record the store believes: its newest sealed generation.
fn record(name: &str) -> Option<Record> {
    let dir = format!("{STORE}/{name}");
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

fn shown(name: &str) -> bool {
    Path::new(&format!("/opt/{name}/bin/probe")).exists()
}

fn installed(name: &str, version: u64) -> bool {
    let current = record(name).and_then(|r| r.current.map(|c| c.version));
    if current != Some(version) || !shown(name) {
        return fail(format!(
            "{name}: the store holds version {current:?} and it is shown: {}",
            shown(name)
        ));
    }
    true
}

fn refused(ran: &Ran, says: &str) -> bool {
    !ran.ok() && ran.says(says)
        || fail(format!(
            "expected a refusal saying {says:?}: {}",
            ran.shown()
        ))
}

/// The session's first install needs the person, whom nobody can ask on a
/// test boot; root's goes in, granted exactly what the image declares and
/// the command line named.
fn a_first_install_asks_and_grants_what_it_read() -> bool {
    let Some(path) = image("ext-first", 1, "NetAdmin", "") else {
        return false;
    };
    let asked = extctl(USER, &["install", &path]);
    if asked.code != Some(STATUS_REFUSED) || !asked.says(&Refused::Unreached.to_string()) {
        return fail(format!("the session's first install: {}", asked.shown()));
    }
    if record("ext-first").is_some() {
        return fail("a refused install left a record");
    }
    let ran = extctl(ROOT, &["install", &path]);
    if !ran.ok() || !installed("ext-first", 1) {
        return fail(format!("root's first install: {}", ran.shown()));
    }
    let Some(current) = record("ext-first").and_then(|r| r.current) else {
        return fail("no current image");
    };
    let line = current.grant.words().join(" ");
    if line != "grant NetAdmin unasked - users ext-first commands -" || current.signer.is_some() {
        return fail(format!("granted {line}, signed by {:?}", current.signer));
    }
    let unexpanded = Command::new(EXTCTL)
        .args(["install", &path])
        .env(ENV_ACTION, "org.slopos.extension.install")
        .env_remove(ENV_FD)
        .output();
    let mechanism_refused = unexpanded.is_ok_and(|out| {
        !out.status.success() && String::from_utf8_lossy(&out.stderr).contains("usage")
    });
    mechanism_refused || fail("a mechanism read an image the person was not shown")
}

/// A command line approving less than the image declares is refused, and
/// nothing goes in.
fn one_declaring_more_than_its_grant_line_is_refused() -> bool {
    let Some(path) = image("ext-wider", 1, "NetAdmin Power", "") else {
        return false;
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return false;
    };
    let Ok(image) = Image::check(&bytes[..]) else {
        return fail(format!("{path} does not check"));
    };
    let digest = hex(&image.digest);
    let ran = extctl(
        ROOT,
        &[
            "install",
            &path,
            &digest,
            "name",
            "ext-wider",
            "grant",
            "NetAdmin",
            "unasked",
            "-",
            "users",
            "ext-wider",
            "commands",
            "-",
        ],
    );
    refused(&ran, "not what was approved")
        && (record("ext-wider").is_none() && !shown("ext-wider") || fail("the wider image went in"))
}

/// The session installs and updates what a key the owner trusts signed,
/// with nobody to ask, and an older version stays out.
fn a_signed_one_installs_and_updates_unasked() -> bool {
    let owner = key(1);
    if !trust(&owner, "NetAdmin,Power", "Power") {
        return false;
    }
    let (Some(v1), Some(v2)) = (
        image("ext-signed", 1, "NetAdmin", "Power"),
        image("ext-signed", 2, "NetAdmin", "Power"),
    ) else {
        return false;
    };
    if !sign(&owner, &v1) || !sign(&owner, &v2) {
        return false;
    }
    let first = extctl(USER, &["install-signed", &v1]);
    if !first.ok() || !installed("ext-signed", 1) {
        return fail(format!("the signed install: {}", first.shown()));
    }
    let signer = record("ext-signed").and_then(|r| r.signer);
    if signer.as_deref() != Some(fingerprint(&owner).as_str()) {
        return fail(format!("the record names {signer:?} as its signer"));
    }
    let update = extctl(USER, &["install-signed", &v2]);
    if !update.ok() || !installed("ext-signed", 2) {
        return fail(format!("the signed update: {}", update.shown()));
    }
    refused(
        &extctl(USER, &["install-signed", &v1]),
        "older than version 2",
    ) && installed("ext-signed", 2)
}

/// Past the key's ceiling, or an unasked action past its user ceiling, and
/// a key the owner does not trust: each refused.
fn one_past_either_ceiling_is_refused() -> bool {
    let owner = key(2);
    if !trust(&owner, "NetAdmin,Power", "-") {
        return false;
    }
    let (Some(wide), Some(unasked)) = (
        image("ext-past", 1, "Mount", ""),
        image("ext-past", 2, "NetAdmin", "Power"),
    ) else {
        return false;
    };
    let stranger = key(3);
    let Some(theirs) = image("ext-past", 3, "NetAdmin", "") else {
        return false;
    };
    sign(&owner, &wide)
        && sign(&owner, &unasked)
        && sign(&stranger, &theirs)
        && refused(
            &extctl(USER, &["install-signed", &wide]),
            "past its key's ceiling",
        )
        && refused(
            &extctl(USER, &["install-signed", &unasked]),
            "past its key's user ceiling",
        )
        && refused(
            &extctl(USER, &["install-signed", &theirs]),
            "does not trust",
        )
        && (record("ext-past").is_none() || fail("a refused image left a record"))
}

/// Rolled back, the version left is refused to a signed install.
fn a_rollback_refuses_the_version_it_left() -> bool {
    let back = extctl(ROOT, &["rollback", "ext-signed"]);
    if !back.ok() || !installed("ext-signed", 1) {
        return fail(format!("the rollback: {}", back.shown()));
    }
    let refused_now = record("ext-signed").map(|r| r.refused);
    if refused_now.as_deref() != Some(&[2][..]) {
        return fail(format!("the record refuses {refused_now:?}"));
    }
    refused(
        &extctl(
            USER,
            &["install-signed", &format!("{DIR}/ext-signed-2.ext")],
        ),
        "rolled back from",
    ) && installed("ext-signed", 1)
}

/// A key's update of what the person disabled goes in disabled, and only the
/// person enables it again.
fn a_disabled_one_stays_disabled_until_enabled() -> bool {
    let Some(v3) = image("ext-signed", 3, "NetAdmin", "Power") else {
        return false;
    };
    let disable = extctl(ROOT, &["disable", "ext-signed"]);
    if !disable.ok() || shown("ext-signed") {
        return fail(format!("disable: {}", disable.shown()));
    }
    if !sign(&key(1), &v3) {
        return false;
    }
    let update = extctl(USER, &["install-signed", &v3]);
    let held = record("ext-signed").is_some_and(|r| {
        !r.enabled && r.current.is_some_and(|c| c.version == 3) && r.refused.is_empty()
    });
    if !update.ok() || !update.says("it is disabled") || !held || shown("ext-signed") {
        return fail(format!("the disabled one's update: {}", update.shown()));
    }
    let enable = extctl(ROOT, &["enable", "ext-signed"]);
    enable.ok() && installed("ext-signed", 3) || fail(format!("enable: {}", enable.shown()))
}

/// A key file's comment that would not read back stays out of the keys.
fn a_key_file_s_comment_never_breaks_the_keys() -> bool {
    let line = format!(
        "{KEY_TYPE} {} J\u{fc}rgen's \"key\"\n",
        PublicKey(key(6).public()).base64()
    );
    let file = put("odd.pub", line.as_bytes());
    let add = extctl(ROOT, &["key", "add", &file, "ceiling", "-", "user", "-"]);
    let status = extctl(USER, &["status"]);
    let remove = extctl(ROOT, &["key", "remove", &fingerprint(&key(6))]);
    add.ok() && status.ok() && status.says(&fingerprint(&key(6))) && remove.ok()
        || fail(format!(
            "add: {}; status: {}; remove: {}",
            add.shown(),
            status.shown(),
            remove.shown()
        ))
}

/// What a key let in stops once the key's ceiling no longer covers it, and
/// once the key is no longer trusted, saying why.
fn a_removed_key_or_a_lowered_ceiling_stops_its_extension() -> bool {
    let (lowered, removed) = (key(4), key(5));
    if !trust(&lowered, "NetAdmin", "-") || !trust(&removed, "NetAdmin", "-") {
        return false;
    }
    let (Some(a), Some(b)) = (
        image("ext-lowered", 1, "NetAdmin", ""),
        image("ext-removed", 1, "NetAdmin", ""),
    ) else {
        return false;
    };
    for (owner, path, name) in [(&lowered, &a, "ext-lowered"), (&removed, &b, "ext-removed")] {
        if !sign(owner, path) {
            return false;
        }
        let ran = extctl(USER, &["install-signed", path]);
        if !ran.ok() || !installed(name, 1) {
            return fail(format!("{name} did not go in: {}", ran.shown()));
        }
    }
    if !trust(&lowered, "-", "-") {
        return false;
    }
    let remove = extctl(ROOT, &["key", "remove", &fingerprint(&removed)]);
    if !remove.ok() {
        return fail(format!("key remove: {}", remove.shown()));
    }
    for (name, why) in [
        ("ext-lowered", "past its key's ceiling"),
        ("ext-removed", "does not trust"),
    ] {
        let stopped = record(name)
            .filter(|r| !r.enabled && r.why.as_deref().is_some_and(|w| w.contains(why)));
        if stopped.is_none() || shown(name) {
            return fail(format!("{name} still runs: {:?}", record(name)));
        }
    }
    true
}

/// A program binding a name `libc.so` does not export cannot run here, so
/// its image stays out.
fn one_needing_an_export_libc_lacks_is_refused() -> bool {
    let program = elf::build(
        Kind::Executable,
        Some(INTERPRETER),
        &["libc.so"],
        &["slopos_no_such_export"],
        &[],
    );
    let Some(bytes) = pack(
        "ext-lacks",
        1,
        &declaration("ext-lacks", "NetAdmin", ""),
        &program,
    ) else {
        return false;
    };
    let path = put("ext-lacks-1.ext", &bytes);
    refused(
        &extctl(ROOT, &["install", &path]),
        "binds slopos_no_such_export",
    ) && (record("ext-lacks").is_none() || fail("the image left a record"))
}

/// An image laid out as the packer lays one out but naming a reserved name,
/// which the packer's own check refuses.
fn reserved(name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let _ = Packer::default().write(&format!("name {name}\nversion 1\n"), "", &mut |block| {
        out.extend_from_slice(block);
        Ok(())
    });
    out
}

fn a_reserved_name_is_refused() -> bool {
    let path = put("reserved.ext", &reserved("bin"));
    refused(&extctl(ROOT, &["install", &path]), "not reserved")
        && refused(&extctl(ROOT, &["enable", "bin"]), "no extension's name")
        && (!Path::new(&format!("{STORE}/bin")).exists() || fail("the store holds bin"))
}

fn users(args: &[&str]) -> bool {
    Command::new("/bin/users")
        .args(args)
        .env_remove(ENV_FD)
        .status()
        .is_ok_and(|status| status.success())
}

/// A person and an extension never share a name: a person takes no name an
/// extension's record keeps a uid for, and an extension no person's.
fn a_person_and_an_extension_share_no_name() -> bool {
    let Some(path) = image("ext-person", 1, "NetAdmin", "") else {
        return false;
    };
    if users(&["add", "ext-first", "--keep-autologin"]) {
        let _ = users(&["remove", "ext-first", "--home"]);
        return fail("a person took ext-first's user");
    }
    if !users(&["add", "ext-person", "--keep-autologin"]) {
        return fail("ext-person could not be added as a person");
    }
    let ran = extctl(ROOT, &["install", &path]);
    let removed = users(&["remove", "ext-person", "--home"]);
    refused(&ran, "a person's name")
        && (record("ext-person").is_none() || fail("an extension took a person's name"))
        && (removed || fail("ext-person could not be removed"))
}

/// The store is sealed, so nothing goes in without unsealing it, the kernel
/// keeps it, so no rename moves it or the directory it is in and no mount
/// covers it, and a record nobody sealed is not believed.
fn the_store_is_sealed_and_kept_where_it_is() -> bool {
    if !sealed(STORE) {
        return fail("the store is not sealed");
    }
    if std::fs::write(format!("{STORE}/keys.forged"), b"x").is_ok() {
        return fail("a file was planted in the store");
    }
    if std::fs::rename(STORE, "/var/lib/slopos/moved").is_ok() {
        return fail("the store was renamed");
    }
    if std::fs::rename("/var/lib/slopos", "/var/lib/moved").is_ok() {
        let _ = std::fs::rename("/var/lib/moved", "/var/lib/slopos");
        return fail("the directory the store is in was renamed");
    }
    let dir = format!("{STORE}/ext-signed");
    let forged = format!("{dir}/{RECORD}.999999");
    let planted = set_sealed(&dir, false)
        && std::fs::write(
            &forged,
            "name ext-signed\nenabled no\nsigner -\nrefused -\n",
        )
        .is_ok()
        && set_sealed(&dir, true);
    if !planted {
        return fail("could not plant an unsealed record");
    }
    for target in ["/var/lib", STORE] {
        if fs::mount(b"", target.as_bytes(), b"ramfs", 0).is_ok() {
            return fail(format!("a ramfs covered {target}"));
        }
    }
    let status = extctl(USER, &["status"]);
    status.ok() && status.says(&fingerprint(&key(1))) && status.says("ext-signed: enabled, running")
        || fail(format!("the session's status: {}", status.shown()))
}

/// An image past the eighth of memory an extension once had to fit in,
/// mostly a hole, goes in and is read at both ends through `/opt`. It is
/// packed on the disk root, since `/tmp` is memory.
fn one_past_the_old_memory_budget_goes_in() -> bool {
    let mut info = UserSysInfo::default();
    if sys_core::sys_info(&mut info) != 0 {
        return fail("no sys_info");
    }
    let size = u64::from(info.total_pages) * 4096 / 8 + (8 << 20);
    let (zeros, image) = ("/var/tmp/ext-large-zeros", "/var/tmp/ext-large-1.ext");
    let installed = install_large(size, zeros, image);
    let _ = std::fs::remove_file(zeros);
    let _ = std::fs::remove_file(image);
    if let Err(why) = installed {
        return fail(why);
    }
    let Ok(file) = std::fs::File::open("/opt/ext-large/share/large") else {
        return fail("the large extension is not shown");
    };
    let (mut first, mut last) = ([0; 5], [0; 4]);
    let read = file
        .read_exact_at(&mut first, 0)
        .and(file.read_exact_at(&mut last, size - 4));
    read.is_ok() && &first == b"first" && &last == b"last"
        || fail(format!("its ends read {read:?}: {first:?} {last:?}"))
}

/// An image of a file `size` long, its ends marked and the rest a hole,
/// packed from `zeros` into `image` and installed.
fn install_large(size: u64, zeros: &str, image: &str) -> Result<(), String> {
    let shown = |e: std::io::Error| format!("{zeros}: {e}");
    let file = std::fs::File::create(zeros).map_err(shown)?;
    file.set_len(size).map_err(shown)?;
    file.write_all_at(b"first", 0).map_err(shown)?;
    file.write_all_at(b"last", size - 4).map_err(shown)?;
    let mut packer = Packer::default();
    packer
        .add_file("/share/large", S_IFREG | 0o644, Path::new(zeros))
        .map_err(shown)?;
    write_image(packer, "name ext-large\nversion 1\n", "", Path::new(image))
        .map_err(|e| format!("ext-large does not pack: {e}"))?;
    let ran = extctl(ROOT, &["install", image]);
    if !ran.ok() {
        return Err(format!("the large install: {}", ran.shown()));
    }
    Ok(())
}

fn cleanup() -> bool {
    for name in ["ext-first", "ext-signed", "ext-large"] {
        let _ = extctl(ROOT, &["remove", name]);
    }
    let back = extctl(
        USER,
        &["install-signed", &format!("{DIR}/ext-signed-3.ext")],
    );
    for seed in [1, 2, 4] {
        let _ = extctl(ROOT, &["key", "remove", &fingerprint(&key(seed))]);
    }
    let gone = ["ext-first", "ext-signed"].iter().all(|n| !shown(n))
        && !Path::new("/opt/ext-large/share/large").exists();
    let kept = record("ext-signed").is_some_and(|r| r.current.is_none() && r.floor.is_some());
    let forged = Path::new(&format!("{STORE}/ext-signed/{RECORD}.999999")).exists();
    if !refused(&back, "it was removed") {
        return false;
    }
    gone && kept && !forged
        || fail("a removed extension still runs, its record went with it, or a forged one stayed")
}

const CASES: &[(&str, fn() -> bool)] = &[
    (
        "a_first_install_asks_and_grants_what_it_read",
        a_first_install_asks_and_grants_what_it_read,
    ),
    (
        "a_person_and_an_extension_share_no_name",
        a_person_and_an_extension_share_no_name,
    ),
    (
        "one_declaring_more_than_its_grant_line_is_refused",
        one_declaring_more_than_its_grant_line_is_refused,
    ),
    (
        "a_signed_one_installs_and_updates_unasked",
        a_signed_one_installs_and_updates_unasked,
    ),
    (
        "one_past_either_ceiling_is_refused",
        one_past_either_ceiling_is_refused,
    ),
    (
        "a_rollback_refuses_the_version_it_left",
        a_rollback_refuses_the_version_it_left,
    ),
    (
        "a_disabled_one_stays_disabled_until_enabled",
        a_disabled_one_stays_disabled_until_enabled,
    ),
    (
        "a_key_file_s_comment_never_breaks_the_keys",
        a_key_file_s_comment_never_breaks_the_keys,
    ),
    (
        "a_removed_key_or_a_lowered_ceiling_stops_its_extension",
        a_removed_key_or_a_lowered_ceiling_stops_its_extension,
    ),
    (
        "one_needing_an_export_libc_lacks_is_refused",
        one_needing_an_export_libc_lacks_is_refused,
    ),
    ("a_reserved_name_is_refused", a_reserved_name_is_refused),
    (
        "the_store_is_sealed_and_kept_where_it_is",
        the_store_is_sealed_and_kept_where_it_is,
    ),
    (
        "one_past_the_old_memory_budget_goes_in",
        one_past_the_old_memory_budget_goes_in,
    ),
    ("removing_keeps_the_record", cleanup),
];

fn main() {
    let _ = std::fs::create_dir_all(DIR);
    let _ = std::fs::set_permissions(DIR, std::fs::Permissions::from_mode(0o755));
    slopos_slibc::test_harness::run(CASES);
}
