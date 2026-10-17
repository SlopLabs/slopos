//! `/bin/extctl`: installs extensions and keeps their store. A first
//! install's command line names what it grants, which the person approves; a
//! signed one goes in unasked within its key's ceilings; every activation
//! holds the image to its record, its key and the base's ceiling again.
//! `status` and `build` need nothing, `activate-all` the capabilities its
//! caller holds, and the rest are privd's `org.slopos.extension.*` actions.

mod store;

use std::path::Path;

use slopos_abi::extension::valid_name;
use slopos_abi::syscall::BOOT_FLAG_EXTENSIONS_OFF;
use slopos_extension_core::admit::{self, Admitted, Authority, Bounds};
use slopos_extension_core::grant::Grant;
use slopos_extension_core::image::Image;
use slopos_extension_core::keys::{Key, Keys, valid_comment};
use slopos_extension_core::record::{Record, valid_fingerprint};
use slopos_extension_core::{CEILING, LIBC, ceiling, hex, parse_digest, sha256};
use slopos_tls_core::sshsig::{KEY_TYPE, PublicKey};

use self::store::Store;
use crate::app_entry::read_bounded;
use crate::privd::{self, Input};
use crate::syscall::UserSysInfo;
use crate::syscall::core as sys_core;

const USAGE: &str = "usage: extctl <command>
  status [<name>]                 what the store holds and what runs
  build <dir> [<out-dir>]         pack <dir> as <name>.ext and its sidecar <name>.meta
  install <image> [<sha256>]      install it, granting what it declares once allowed
  install-signed <image> [<meta> <meta.sig>]
                                  install or update it unasked under a trusted key
  key add <key.pub> ceiling <caps> user <caps>
                                  trust a key; <caps> are names joined by commas, or -
  key remove <SHA256:fingerprint>
  enable|disable|remove|rollback <name>
  activate-all                    run every enabled extension the store holds";

const SIDECAR_MAX: u64 = 4096;
const LIBC_MAX: u64 = 16 << 20;

fn caps_of(mask: u64) -> String {
    slopos_extension_core::render_caps(mask)
}

/// The most `ext.memory=` lets every activation take together, so a larger
/// image is refused before it is read whole; the kernel holds one to the
/// budget it booted with.
fn image_max() -> u64 {
    let mut info = UserSysInfo::default();
    if sys_core::sys_info(&mut info) != 0 {
        return u64::MAX;
    }
    u64::from(info.total_pages) * 4096 / 2
}

fn read_file(path: &str, max: u64) -> Result<Vec<u8>, String> {
    read_bounded(Path::new(path), max)
        .ok_or_else(|| format!("{path}: no regular file of at most {max} bytes this can read"))
}

/// `path`'s bytes, held to `digest` when one is named: what was put there
/// for this request may have been replaced since.
fn read_image(path: &str, digest: Option<[u8; 32]>) -> Result<(Vec<u8>, [u8; 32]), String> {
    let bytes = read_file(path, image_max())?;
    let found = sha256(&bytes);
    if digest.is_some_and(|want| want != found) {
        return Err(format!(
            "{path}: not the image whose sha256 the request names"
        ));
    }
    Ok((bytes, found))
}

/// The base's ceiling and the running `libc.so`, with `keys`.
struct Base {
    ceiling: u64,
    libc: Vec<u8>,
}

impl Base {
    fn read() -> Result<Base, String> {
        let text = std::fs::read_to_string(CEILING).map_err(|e| format!("{CEILING}: {e}"))?;
        Ok(Base {
            ceiling: ceiling::parse(&text).map_err(|e| format!("{CEILING}: {e}"))?,
            libc: read_file(LIBC, LIBC_MAX)?,
        })
    }

    fn bounds<'a>(&'a self, keys: &'a Keys) -> Bounds<'a> {
        Bounds {
            keys,
            ceiling: self.ceiling,
            libc: &self.libc,
        }
    }
}

/// The command line a request carries: an `install` of an image alone names
/// the image's digest and what it is granted, and a `key add` of a key file
/// or line names the key's fingerprint, so the person reads both.
fn canonical(args: &[String]) -> Result<Vec<String>, String> {
    match args {
        [cmd, image, digest @ ..] if cmd == "install" && digest.len() < 2 => {
            let want = match digest {
                [digest] => Some(parse_digest(digest).ok_or(USAGE)?),
                _ => None,
            };
            let (bytes, digest) = read_image(image, want)?;
            let checked = Image::check(&bytes).map_err(|e| format!("{image}: {e}"))?;
            let mut out = vec![
                cmd.clone(),
                image.clone(),
                hex(&digest),
                "name".into(),
                checked.release.name.clone(),
            ];
            out.extend(checked.declaration.grant().words());
            Ok(out)
        }
        [cmd, sub, rest @ ..] if cmd == "key" && sub == "add" => {
            let (key, comment, tail) = match rest {
                [fingerprint, ..] if fingerprint.starts_with("SHA256:") => return Ok(args.to_vec()),
                [kind, base64, tail @ ..] if kind == KEY_TYPE => {
                    (key_of(kind, base64)?, String::new(), tail)
                }
                [file, tail @ ..] => {
                    let text = read_file(file, 4096)?;
                    let line = String::from_utf8_lossy(&text);
                    let mut words = line.split_ascii_whitespace();
                    let key = key_of(words.next().unwrap_or(""), words.next().unwrap_or(""))
                        .map_err(|e| format!("{file}: {e}"))?;
                    let comment = words.collect::<Vec<_>>().join(" ");
                    let comment = if valid_comment(&comment) {
                        comment
                    } else {
                        String::new()
                    };
                    (key, comment, tail)
                }
                [] => return Err(USAGE.into()),
            };
            let mut out = vec![
                cmd.clone(),
                sub.clone(),
                key.fingerprint(),
                KEY_TYPE.into(),
                key.base64(),
            ];
            out.extend(tail.iter().cloned());
            out.extend(comment.split_ascii_whitespace().map(str::to_owned));
            Ok(out)
        }
        _ => Ok(args.to_vec()),
    }
}

fn key_of(kind: &str, base64: &str) -> Result<PublicKey, String> {
    PublicKey::from_openssh(kind, base64).ok_or_else(|| "not an ssh-ed25519 public key".into())
}

fn named(name: &str) -> Result<String, String> {
    if valid_name(name.as_bytes()) {
        Ok(name.to_owned())
    } else {
        Err(format!("{name:?} is no extension's name"))
    }
}

/// Who vouches for an install, as its command line names them.
enum Vouched {
    /// The person approved `grant` for `name` and the image of `digest`.
    Approved {
        digest: [u8; 32],
        name: String,
        grant: Grant,
    },
    /// A key signed the sidecar at `sidecar` with the signature at
    /// `signature`.
    Signed { sidecar: String, signature: String },
}

/// A command line, read.
enum Request {
    Status(Option<String>),
    Build { dir: String, out: String },
    Install { image: String, vouched: Vouched },
    KeyAdd(Key),
    KeyRemove(String),
    Enable(String),
    Disable(String),
    Remove(String),
    Rollback(String),
    ActivateAll,
}

fn parse(args: &[String]) -> Result<Request, String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    Ok(match words.as_slice() {
        ["status"] => Request::Status(None),
        ["status", name] => Request::Status(Some(named(name)?)),
        ["build", dir] => Request::Build {
            dir: (*dir).into(),
            out: ".".into(),
        },
        ["build", dir, out] => Request::Build {
            dir: (*dir).into(),
            out: (*out).into(),
        },
        ["install", image, digest, "name", name, grant @ ..] => Request::Install {
            image: (*image).into(),
            vouched: Vouched::Approved {
                digest: parse_digest(digest).ok_or(USAGE)?,
                name: named(name)?,
                grant: Grant::parse(grant).ok_or(USAGE)?,
            },
        },
        ["install-signed", image] => {
            let stem = image.strip_suffix(".ext").unwrap_or(image);
            Request::Install {
                image: (*image).into(),
                vouched: Vouched::Signed {
                    sidecar: format!("{stem}.meta"),
                    signature: format!("{stem}.meta.sig"),
                },
            }
        }
        ["install-signed", image, sidecar, signature] => Request::Install {
            image: (*image).into(),
            vouched: Vouched::Signed {
                sidecar: (*sidecar).into(),
                signature: (*signature).into(),
            },
        },
        [
            "key",
            "add",
            fingerprint,
            kind,
            base64,
            "ceiling",
            ceiling,
            "user",
            user,
            comment @ ..,
        ] => {
            let key = key_of(kind, base64)?;
            if key.fingerprint() != *fingerprint || !valid_fingerprint(fingerprint) {
                return Err(format!("{fingerprint} is not that key's fingerprint"));
            }
            let caps = |list: &str| {
                slopos_extension_core::parse_caps(list)
                    .ok_or_else(|| format!("{list} names no capabilities"))
            };
            Request::KeyAdd(Key::new(
                key,
                caps(ceiling)?,
                caps(user)?,
                &comment.join(" "),
            )?)
        }
        ["key", "remove", fingerprint] => Request::KeyRemove((*fingerprint).into()),
        ["enable", name] => Request::Enable(named(name)?),
        ["disable", name] => Request::Disable(named(name)?),
        ["remove", name] => Request::Remove(named(name)?),
        ["rollback", name] => Request::Rollback(named(name)?),
        ["activate-all"] => Request::ActivateAll,
        _ => return Err(USAGE.into()),
    })
}

impl Request {
    /// The action it runs as, `None` for one that needs nothing.
    fn action(&self) -> Option<&'static str> {
        Some(match self {
            Request::Install {
                vouched: Vouched::Approved { .. },
                ..
            } => "org.slopos.extension.install",
            Request::Install {
                vouched: Vouched::Signed { .. },
                ..
            } => "org.slopos.extension.install-signed",
            Request::KeyAdd(_) | Request::KeyRemove(_) => "org.slopos.extension.trust",
            Request::Enable(_)
            | Request::Disable(_)
            | Request::Remove(_)
            | Request::Rollback(_) => "org.slopos.extension.manage",
            Request::Status(_) | Request::Build { .. } | Request::ActivateAll => return None,
        })
    }

    /// What the mechanism would refuse, judged against the store as the
    /// requester reads it, so the person is asked only about what would go in.
    fn check(&self) -> Result<(), String> {
        let store = || Store::open(false);
        match self {
            Request::Install { image, vouched } => prepare(&store()?, image, vouched).map(drop),
            Request::KeyAdd(key) => within_base(key),
            Request::KeyRemove(fingerprint) => store()?
                .keys()?
                .by_fingerprint(fingerprint)
                .map(drop)
                .ok_or_else(|| format!("no trusted key is {fingerprint}")),
            Request::Enable(name) | Request::Disable(name) | Request::Remove(name) => {
                record_of(&store()?, name).map(drop)
            }
            Request::Rollback(name) => rolled_back(&store()?, name).map(drop),
            Request::Status(_) | Request::Build { .. } | Request::ActivateAll => Ok(()),
        }
    }

    fn run(self) -> Result<String, String> {
        match self {
            Request::Status(only) => status(only.as_deref()),
            Request::Build { dir, out } => build(&dir, &out),
            Request::Install { image, vouched } => {
                let store = Store::open(true)?;
                let prepared = prepare(&store, &image, &vouched)?;
                install(&store, prepared)
            }
            Request::KeyAdd(key) => key_add(key),
            Request::KeyRemove(fingerprint) => key_remove(&fingerprint),
            Request::Enable(name) => enable(&name),
            Request::Disable(name) => disable(&name),
            Request::Remove(name) => remove(&name),
            Request::Rollback(name) => rollback(&name),
            Request::ActivateAll => activate_all(),
        }
    }
}

/// Every other installed extension's name and grant.
fn others(store: &Store, name: &str) -> Result<Vec<(String, Grant)>, String> {
    let mut out = Vec::new();
    for other in store.names()? {
        if other == name {
            continue;
        }
        if let Some(current) = store.record(&other)?.and_then(|r| r.current) {
            out.push((other, current.grant));
        }
    }
    Ok(out)
}

fn borrowed(others: &[(String, Grant)]) -> Vec<(&str, &Grant)> {
    others.iter().map(|(n, g)| (n.as_str(), g)).collect()
}

/// The authority an install goes in under, read.
enum Voucher {
    Person {
        name: String,
        grant: Grant,
    },
    Key {
        sidecar: Vec<u8>,
        signature: Vec<u8>,
    },
}

impl Voucher {
    fn read(vouched: &Vouched) -> Result<Voucher, String> {
        Ok(match vouched {
            Vouched::Approved { name, grant, .. } => Voucher::Person {
                name: name.clone(),
                grant: grant.clone(),
            },
            Vouched::Signed { sidecar, signature } => Voucher::Key {
                sidecar: read_file(sidecar, SIDECAR_MAX)?,
                signature: read_file(signature, SIDECAR_MAX)?,
            },
        })
    }

    fn authority(&self) -> Authority<'_> {
        match self {
            Voucher::Person { name, grant } => Authority::Person { name, grant },
            Voucher::Key { sidecar, signature } => Authority::Signed { sidecar, signature },
        }
    }

    fn signed(&self) -> Option<(&[u8], &[u8])> {
        match self {
            Voucher::Person { .. } => None,
            Voucher::Key { sidecar, signature } => Some((sidecar, signature)),
        }
    }
}

/// An image read and held to the store as it stands.
struct Prepared {
    bytes: Vec<u8>,
    digest: [u8; 32],
    name: String,
    voucher: Voucher,
    before: Option<Record>,
    admitted: Admitted,
}

fn prepare(store: &Store, path: &str, vouched: &Vouched) -> Result<Prepared, String> {
    let named_digest = match vouched {
        Vouched::Approved { digest, .. } => Some(*digest),
        Vouched::Signed { .. } => None,
    };
    let (bytes, digest) = read_image(path, named_digest)?;
    let voucher = Voucher::read(vouched)?;
    let image = Image::check(&bytes).map_err(|e| format!("{path}: {e}"))?;
    let name = image.release.name.clone();
    let keys = store.keys()?;
    let base = Base::read()?;
    let before = store.record(&name)?;
    let others = others(store, &name)?;
    let admitted = admit::install(
        before.as_ref(),
        &image,
        digest,
        &voucher.authority(),
        &base.bounds(&keys),
        &borrowed(&others),
    )
    .map_err(|e| format!("{path}: {e}"))?;
    Ok(Prepared {
        bytes,
        digest,
        name,
        voucher,
        before,
        admitted,
    })
}

/// Why `record` does not run, or `None` when it does.
fn idle(record: &Record) -> Option<String> {
    match (&record.why, record.enabled) {
        (Some(why), _) => Some(format!("it is stopped: {why}")),
        (None, false) => Some(format!(
            "it is disabled; `extctl enable {}` runs it",
            record.name
        )),
        (None, true) => None,
    }
}

/// Put what `prepared` admitted in the store as its current image and run
/// it. One that will not run leaves the store as it was.
fn install(store: &Store, prepared: Prepared) -> Result<String, String> {
    let Prepared {
        bytes,
        digest,
        name,
        voucher,
        before,
        admitted,
    } = prepared;
    let installed = match admitted {
        Admitted::Current => {
            let idle = before.as_ref().and_then(idle);
            return Ok(format!(
                "{name} is installed already{}",
                idle.map_or_else(String::new, |why| format!("; {why}"))
            ));
        }
        Admitted::Install(installed) => installed,
    };
    let version = installed.version;
    let mut record = before.clone().unwrap_or_else(|| Record::new(&name));
    record.install(installed);
    store.edit(&name, |edit| {
        edit.put_image(&digest, &bytes, voucher.signed())?;
        edit.put_record(&record)
    })?;
    if let Some(why) = idle(&record) {
        return Ok(format!("{name} {version} installed; {why}"));
    }
    match activate(&name, &bytes, digest) {
        Ok(ran) => Ok(format!("{name} {version} installed; {ran}")),
        Err(why) => match before {
            Some(before) if before.current.is_some() => {
                store.edit(&name, |edit| edit.put_record(&before))?;
                let kept = if idle(&before).is_none() {
                    "what ran before runs on"
                } else {
                    "the store keeps what it had"
                };
                Err(format!("{name} {version} not installed: {why}; {kept}"))
            }
            _ => {
                stop(store, &mut record, &why)?;
                Err(format!("{name}: {why}"))
            }
        },
    }
}

/// Show `image` at `/opt/<name>` in place of what is shown there.
fn activate(name: &str, image: &[u8], digest: [u8; 32]) -> Result<String, String> {
    if sys_core::boot_flag(BOOT_FLAG_EXTENSIONS_OFF) {
        return Ok(format!(
            "extensions are off this boot, so it is not shown; `extctl enable {name}` shows it on a boot that allows them"
        ));
    }
    crate::syscall::base::ext_activate(name.as_bytes(), image, &digest)
        .map(|activation| format!("it runs as activation {activation}"))
        .map_err(|e| format!("it could not be activated: {e}"))
}

/// [`activate`], stopping the extension when it will not run.
fn run_image(
    store: &Store,
    record: &mut Record,
    image: &[u8],
    digest: [u8; 32],
) -> Result<String, String> {
    activate(&record.name, image, digest).or_else(|why| {
        stop(store, record, &why)?;
        Err(format!("{}: {why}", record.name))
    })
}

/// Stop `record`'s extension because `why`, and say so in its record.
fn stop(store: &Store, record: &mut Record, why: &str) -> Result<(), String> {
    record.stop(why);
    store.edit(&record.name, |edit| edit.put_record(record))?;
    deactivate(&record.name)
}

fn deactivate(name: &str) -> Result<(), String> {
    match crate::syscall::base::ext_deactivate(name.as_bytes()) {
        Ok(()) | Err(crate::syscall::SyscallError::ENOENT) => Ok(()),
        Err(e) => Err(format!("{name}: deactivating: {e}")),
    }
}

/// `record`'s current image, once it holds to everything it was installed
/// under.
fn checked(store: &Store, record: &Record, keys: &Keys, base: &Base) -> Result<Vec<u8>, String> {
    let current = record
        .current
        .as_ref()
        .ok_or_else(|| "nothing is installed".to_owned())?;
    let bytes = store.image(&record.name, &current.sha256, image_max())?;
    let signed = store.signed(&record.name, &current.sha256)?;
    let image = Image::check(&bytes).map_err(|e| e.to_string())?;
    let signed = signed.as_ref().map(|(s, g)| (s.as_slice(), g.as_slice()));
    admit::activation(record, &image, current.sha256, signed, &base.bounds(keys))
        .map_err(|e| e.to_string())?;
    Ok(bytes)
}

/// [`checked`], stopping one that no longer fits.
fn fitting(
    store: &Store,
    record: &mut Record,
    keys: &Keys,
    base: &Base,
) -> Result<Vec<u8>, String> {
    checked(store, record, keys, base).or_else(|why| {
        stop(store, record, &why)?;
        Err(format!("{} stopped: {why}", record.name))
    })
}

fn bring_up(
    store: &Store,
    record: &mut Record,
    keys: &Keys,
    base: &Base,
) -> Result<String, String> {
    let bytes = fitting(store, record, keys, base)?;
    let digest = sha256(&bytes);
    run_image(store, record, &bytes, digest)
}

fn record_of(store: &Store, name: &str) -> Result<Record, String> {
    store
        .record(name)?
        .ok_or_else(|| format!("the store keeps no extension {name}"))
}

fn enable(name: &str) -> Result<String, String> {
    let store = Store::open(true)?;
    let mut record = record_of(&store, name)?;
    let keys = store.keys()?;
    let base = Base::read()?;
    record.enabled = true;
    record.why = None;
    let ran = bring_up(&store, &mut record, &keys, &base)?;
    store.edit(name, |edit| edit.put_record(&record))?;
    Ok(format!("{name} enabled; {ran}"))
}

fn disable(name: &str) -> Result<String, String> {
    let store = Store::open(true)?;
    let mut record = record_of(&store, name)?;
    record.enabled = false;
    record.why = None;
    store.edit(name, |edit| edit.put_record(&record))?;
    deactivate(name)?;
    Ok(format!("{name} disabled"))
}

fn remove(name: &str) -> Result<String, String> {
    let store = Store::open(true)?;
    let mut record = record_of(&store, name)?;
    deactivate(name)?;
    record.remove();
    store.edit(name, |edit| edit.put_record(&record))?;
    Ok(format!(
        "{name} removed; its record keeps version floor {}",
        floor_of(&record)
    ))
}

fn floor_of(record: &Record) -> String {
    record
        .floor
        .map_or_else(|| "-".to_owned(), |(version, _)| version.to_string())
}

/// `name`'s record rolled back and its image, once that image fits and
/// claims nothing another extension claims.
fn rolled_back(store: &Store, name: &str) -> Result<(Record, Vec<u8>), String> {
    let mut record = record_of(store, name)?;
    record.roll_back().map_err(|e| format!("{name}: {e}"))?;
    record.enabled = true;
    record.why = None;
    if let Some(current) = &record.current {
        admit::clashes(name, &current.grant, &borrowed(&others(store, name)?))
            .map_err(|e| format!("{name}: {e}"))?;
    }
    let bytes = checked(store, &record, &store.keys()?, &Base::read()?)
        .map_err(|why| format!("{name}: the previous image does not fit: {why}"))?;
    Ok((record, bytes))
}

fn rollback(name: &str) -> Result<String, String> {
    let store = Store::open(true)?;
    let before = record_of(&store, name)?;
    let (record, bytes) = rolled_back(&store, name)?;
    store.edit(name, |edit| edit.put_record(&record))?;
    let version = record.current.as_ref().map_or(0, |c| c.version);
    match activate(name, &bytes, sha256(&bytes)) {
        Ok(ran) => Ok(format!(
            "{name} is back at version {version}, which updates are held above; {ran}"
        )),
        Err(why) => {
            store.edit(name, |edit| edit.put_record(&before))?;
            Err(format!(
                "{name} not rolled back: {why}; the store keeps what it had"
            ))
        }
    }
}

/// The key's ceilings, held to the base's.
fn within_base(key: &Key) -> Result<(), String> {
    let base = Base::read()?;
    let past = (key.ceiling | key.user) & !base.ceiling;
    if past != 0 {
        return Err(format!(
            "the ceilings name {}, which no extension is granted here",
            caps_of(past)
        ));
    }
    Ok(())
}

fn key_add(key: Key) -> Result<String, String> {
    within_base(&key)?;
    let base = Base::read()?;
    let store = Store::open(true)?;
    let mut keys = store.keys()?;
    let fingerprint = key.key.fingerprint();
    let said = format!(
        "{fingerprint} trusted with ceiling {} and user ceiling {}",
        caps_of(key.ceiling),
        caps_of(key.user)
    );
    keys.put(key)?;
    store.put_keys(&keys)?;
    Ok(format!(
        "{said}{}",
        recheck(&store, &keys, &base, &fingerprint)?
    ))
}

fn key_remove(fingerprint: &str) -> Result<String, String> {
    let store = Store::open(true)?;
    let mut keys = store.keys()?;
    keys.remove(fingerprint)
        .ok_or_else(|| format!("no trusted key is {fingerprint}"))?;
    store.put_keys(&keys)?;
    let base = Base::read()?;
    Ok(format!(
        "{fingerprint} no longer trusted{}",
        recheck(&store, &keys, &base, fingerprint)?
    ))
}

/// Hold every running extension `fingerprint` signed to the keys now, and
/// stop what no longer fits.
fn recheck(store: &Store, keys: &Keys, base: &Base, fingerprint: &str) -> Result<String, String> {
    let mut said = String::new();
    for name in store.names()? {
        let mut record = match store.record(&name) {
            Ok(Some(record)) => record,
            Ok(None) => continue,
            Err(why) => {
                said.push_str(&format!("\n{why}"));
                continue;
            }
        };
        let signed_by = record.current.as_ref().and_then(|c| c.signer.as_deref());
        if record.enabled && signed_by == Some(fingerprint) {
            match fitting(store, &mut record, keys, base) {
                Ok(_) => said.push_str(&format!("\n{name} still fits")),
                Err(why) => said.push_str(&format!("\n{why}")),
            }
        }
    }
    Ok(said)
}

fn activate_all() -> Result<String, String> {
    let store = Store::open(true)?;
    let keys = store.keys()?;
    let base = Base::read()?;
    let mut said = Vec::new();
    for name in store.names()? {
        let ran = match store.record(&name) {
            Ok(Some(mut record)) if record.enabled => {
                bring_up(&store, &mut record, &keys, &base).map(|ran| format!("{name}: {ran}"))
            }
            Ok(Some(_)) => deactivate(&name).map(|()| format!("{name}: disabled")),
            Ok(None) => continue,
            Err(why) => Err(why),
        };
        said.push(ran.unwrap_or_else(|why| why));
    }
    Ok(said.join("\n"))
}

fn status(only: Option<&str>) -> Result<String, String> {
    let store = Store::open(false)?;
    let mut out = Vec::new();
    if only.is_none() {
        for key in store.keys()?.iter() {
            out.push(format!(
                "key {} ceiling {} user {}{}",
                key.key.fingerprint(),
                caps_of(key.ceiling),
                caps_of(key.user),
                if key.comment.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", key.comment)
                }
            ));
        }
    }
    for name in store.names()? {
        if only.is_some_and(|only| only != name) {
            continue;
        }
        match store.record(&name) {
            Ok(Some(record)) => out.push(describe(&record)),
            Ok(None) => {}
            Err(why) => out.push(format!("{name}: unreadable: {why}")),
        }
    }
    if let Some(name) = only
        && out.is_empty()
    {
        return Err(format!("the store keeps no extension {name}"));
    }
    if out.is_empty() {
        out.push("no extensions".into());
    }
    Ok(out.join("\n"))
}

fn describe(record: &Record) -> String {
    let shown = format!(
        "{}/{}/.extension/release",
        slopos_abi::extension::DIR,
        record.name
    );
    let running = if Path::new(&shown).exists() {
        "running"
    } else {
        "not running"
    };
    let mut out = format!(
        "{}: {}, {running}, floor {}{}",
        record.name,
        if record.enabled {
            "enabled"
        } else {
            "disabled"
        },
        floor_of(record),
        if record.refused.is_empty() {
            String::new()
        } else {
            let refused: Vec<String> = record.refused.iter().map(u64::to_string).collect();
            format!(", refusing {} unasked", refused.join(","))
        }
    );
    if let Some(why) = &record.why {
        out.push_str(&format!("\n  stopped: {why}"));
    }
    for (slot, image) in [("current", &record.current), ("previous", &record.previous)] {
        if let Some(image) = image {
            out.push_str(&format!(
                "\n  {slot} {} {} by {}: {}",
                image.version,
                &hex(&image.sha256)[..12],
                image.signer.as_deref().unwrap_or("the person"),
                image.grant.words().join(" ")
            ));
        }
    }
    out
}

fn build(dir: &str, out: &str) -> Result<String, String> {
    let (image, sidecar) = slopos_extension_core::source::pack(Path::new(dir))?;
    let at = |ext: &str| Path::new(out).join(format!("{}.{ext}", sidecar.name));
    let (image_at, sidecar_at) = (at("ext"), at("meta"));
    std::fs::write(&image_at, &image).map_err(|e| format!("{}: {e}", image_at.display()))?;
    std::fs::write(&sidecar_at, sidecar.render())
        .map_err(|e| format!("{}: {e}", sidecar_at.display()))?;
    Ok(format!(
        "{} {}: {} ({} bytes) and {}",
        sidecar.name,
        sidecar.version,
        image_at.display(),
        image.len(),
        sidecar_at.display()
    ))
}

pub fn extctl_main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The person approved the command line privd was sent, so the mechanism
    // runs it as it came.
    let mechanism = std::env::var_os(slopos_priv_core::ENV_ACTION).is_some();
    let args = if mechanism {
        args
    } else {
        canonical(&args).unwrap_or_else(|why| {
            eprintln!("extctl: {why}");
            sys_core::exit_with_code(2)
        })
    };
    let fail = |code: i32, why: String| -> ! {
        eprintln!("extctl: {why}");
        sys_core::exit_with_code(code)
    };
    let request = parse(&args).unwrap_or_else(|why| fail(2, why));
    if !mechanism {
        request.check().unwrap_or_else(|why| fail(1, why));
    }
    let bytes: Vec<Vec<u8>> = args.iter().map(|a| a.clone().into_bytes()).collect();
    privd::elevate(request.action(), &bytes, Input::Closed);
    match request.run() {
        Ok(said) => println!("{said}"),
        Err(why) => fail(1, why),
    }
}
