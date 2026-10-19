//! `/bin/extctl`: installs extensions, keeps their store and their
//! configuration. A first install's command line names what it grants, which
//! the person approves; a signed one goes in unasked within its key's
//! ceilings; every activation holds the image to its record, its key and the
//! base's ceiling again. `status`, `build` and `config edit` need nothing,
//! `activate-all` and `--root` the capabilities their caller holds, and the
//! rest are privd's `org.slopos.extension.*` actions.

mod accounts;
mod commands;
mod configure;
pub(crate) mod store;

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use slopos_abi::fs::O_NOFOLLOW;
use slopos_abi::syscall::BOOT_FLAG_EXTENSIONS_OFF;
use slopos_abi::syscall::posix::{O_NOCTTY, O_NONBLOCK};
use slopos_extension_core::admit::{self, Admitted, Authority, Bounds};
use slopos_extension_core::grant::Grant;
use slopos_extension_core::image::{Image, ImageError, archive_form};
use slopos_extension_core::keys::{Key, Keys, valid_comment};
use slopos_extension_core::record::{Installed, Record, valid_fingerprint};
use slopos_extension_core::{CEILING, LIBC, ceiling, hex, parse_digest, valid_name};
use slopos_tls_core::sshsig::{KEY_TYPE, PublicKey};

use self::accounts::give_uids;
use self::store::Store;
use crate::app_entry::read_bounded;
use crate::init_control::{self, Command};
use crate::privd::{self, Input};
use crate::syscall::core as sys_core;

const USAGE: &str = "usage: extctl <command>
  status [<name>]                 what the store holds and what runs
  build <dir> [<out-dir>]         pack <dir> as <name>.ext and its sidecar <name>.meta
  install <image> [<root>]        install it, granting what it declares once allowed
  install-signed <image> [<meta> <meta.sig>]
                                  install or update it unasked under a trusted key
  key add <key.pub> ceiling <caps> user <caps>
                                  trust a key; <caps> are names joined by commas, or -
  key remove <SHA256:fingerprint>
  enable|disable|remove|rollback <name>
  activate-all                    run every enabled extension the store holds
  config edit <name> <file>       edit /etc/<name>/<file>, installing it once allowed
  config install <name> <file> <from>
                                  install <from> as /etc/<name>/<file> once allowed
  config apply <from> [<meta> <meta.sig>]
                                  install a file the extension's key signed, unasked
  --root <dir> status [<name>]|enable <name>|disable <name>
                                  act on the store of the root mounted at <dir>";

pub(super) const SIDECAR_MAX: u64 = 4096;
const LIBC_MAX: u64 = 16 << 20;

fn caps_of(mask: u64) -> String {
    slopos_extension_core::render_caps(mask)
}

pub(super) fn read_file(path: &str, max: u64) -> Result<Vec<u8>, String> {
    read_bounded(Path::new(path), max)
        .ok_or_else(|| format!("{path}: no regular file of at most {max} bytes this can read"))
}

/// The image at `path`, held to the root `named` names when there is one:
/// what was put there for this request may have been replaced since. Its
/// files' bytes are checked as they are read.
fn open_image(path: &str, named: Option<([u8; 32], &str)>) -> Result<Image<File>, String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags((O_NONBLOCK | O_NOCTTY) as i32 | O_NOFOLLOW as i32)
        .open(path)
        .map_err(|e| format!("{path}: {e}"))?;
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return Err(format!("{path}: not a regular file"));
    }
    Image::open(file, named.map(|(root, _)| root)).map_err(|e| shown_image(path, named, e))
}

/// [`open_image`], every file held to the manifest.
fn read_image(path: &str, named: Option<([u8; 32], &str)>) -> Result<Image<File>, String> {
    let mut image = open_image(path, named)?;
    image
        .check_contents()
        .map_err(|e| shown_image(path, named, e))?;
    Ok(image)
}

fn shown_image(path: &str, named: Option<([u8; 32], &str)>, e: ImageError) -> String {
    match e {
        ImageError::Other => format!(
            "{path}: not the image {} names",
            named.map_or("", |(_, by)| by)
        ),
        e => format!("{path}: {e}"),
    }
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
/// the image's digest and what it is granted, a `config install` of a file
/// hands the file over as standard input and names its digest, and a `key
/// add` of a key file or line names the key's fingerprint, so the person
/// reads each.
fn canonical(args: &[String]) -> Result<Vec<String>, String> {
    match args {
        [cmd, image, digest @ ..] if cmd == "install" && digest.len() < 2 => {
            let want = match digest {
                [digest] => Some(parse_digest(digest).ok_or(USAGE)?),
                _ => None,
            };
            let opened = open_image(image, want.map(|root| (root, "the request")))?;
            let mut out = vec![
                cmd.clone(),
                image.clone(),
                hex(&opened.digest),
                "name".into(),
                opened.release.name.clone(),
            ];
            out.extend(opened.declaration.grant().words());
            Ok(out)
        }
        [cmd, sub, name, file, from] if cmd == "config" && sub == "install" => {
            let digest = configure::take_as_stdin(from)?;
            Ok(vec![
                cmd.clone(),
                sub.clone(),
                name.clone(),
                file.clone(),
                hex(&digest),
            ])
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
    if valid_name(name) {
        Ok(name.to_owned())
    } else {
        Err(format!("{name:?} is no extension's name"))
    }
}

fn config_file(file: &str) -> Result<String, String> {
    if slopos_extension_core::config::valid_file(file) {
        Ok(file.to_owned())
    } else {
        Err(format!("{file:?} names no file of /etc/<name>"))
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
    Build {
        dir: String,
        out: String,
    },
    Install {
        image: String,
        vouched: Vouched,
    },
    KeyAdd(Key),
    KeyRemove(String),
    Enable(String),
    Disable(String),
    Remove(String),
    Rollback(String),
    ActivateAll,
    ConfigEdit {
        name: String,
        file: String,
    },
    Config(configure::Source),
    /// What a rescue shell asks of a root it mounted.
    Rooted {
        root: PathBuf,
        command: Rooted,
    },
}

enum Rooted {
    Status(Option<String>),
    Enable(String),
    Disable(String),
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
        ["config", "edit", name, file] => Request::ConfigEdit {
            name: named(name)?,
            file: config_file(file)?,
        },
        ["config", "install", name, file, digest] => Request::Config(configure::Source::Approved {
            name: named(name)?,
            file: config_file(file)?,
            digest: parse_digest(digest).ok_or(USAGE)?,
        }),
        ["config", "apply", from] => Request::Config(configure::Source::Signed {
            from: (*from).into(),
            sidecar: format!("{from}.meta"),
            signature: format!("{from}.meta.sig"),
        }),
        ["config", "apply", from, sidecar, signature] => {
            Request::Config(configure::Source::Signed {
                from: (*from).into(),
                sidecar: (*sidecar).into(),
                signature: (*signature).into(),
            })
        }
        ["--root", root, rest @ ..] => Request::Rooted {
            root: PathBuf::from(root),
            command: match rest {
                ["status"] => Rooted::Status(None),
                ["status", name] => Rooted::Status(Some(named(name)?)),
                ["enable", name] => Rooted::Enable(named(name)?),
                ["disable", name] => Rooted::Disable(named(name)?),
                _ => return Err(USAGE.into()),
            },
        },
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
            | Request::Rollback(_)
            | Request::Rooted {
                command: Rooted::Enable(_) | Rooted::Disable(_),
                ..
            } => "org.slopos.extension.manage",
            Request::Config(configure::Source::Approved { .. }) => "org.slopos.extension.config",
            Request::Config(configure::Source::Signed { .. }) => {
                "org.slopos.extension.config-signed"
            }
            Request::Status(_)
            | Request::Build { .. }
            | Request::ActivateAll
            | Request::ConfigEdit { .. }
            | Request::Rooted {
                command: Rooted::Status(_),
                ..
            } => return None,
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
            Request::Config(source) => configure::prepare(&store()?, source).map(drop),
            Request::Status(_)
            | Request::Build { .. }
            | Request::ActivateAll
            | Request::ConfigEdit { .. }
            | Request::Rooted { .. } => Ok(()),
        }
    }

    fn run(self) -> Result<String, String> {
        let settles = !matches!(
            self,
            Request::Status(_)
                | Request::Build { .. }
                | Request::ConfigEdit { .. }
                | Request::Rooted { .. }
        );
        let tells_init = settles && !matches!(self, Request::ActivateAll);
        let outcome = match self {
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
            Request::ConfigEdit { name, file } => {
                let code = configure::edit(&name, &file)?;
                sys_core::exit_with_code(code)
            }
            Request::Config(source) => {
                let store = Store::open(true)?;
                let prepared = configure::prepare(&store, &source)?;
                configure::install(&store, prepared)
            }
            Request::Rooted { root, command } => rooted(&root, command),
        };
        if !settles {
            return outcome;
        }
        let after = settle(tells_init);
        outcome
            .map(|said| said + &after)
            .map_err(|why| why + &after)
    }
}

/// What a change leaves: the user database the records describe written,
/// `/opt/bin` linking the commands of what is shown, and init told, which
/// starts, stops and restarts services and privd to match. Each failing is
/// said rather than fatal, since every boot does all of them again.
fn settle(tell_init: bool) -> String {
    let mut said = String::new();
    match Store::open(true) {
        Ok(store) => {
            if let Err(why) = accounts::sync() {
                said.push_str(&format!("\nthe user database was not written: {why}"));
            }
            if let Err(why) = commands::sync(&store) {
                said.push_str(&format!("\nthe commands were not linked: {why}"));
            }
        }
        Err(why) => said.push_str(&format!("\nthe store could not be read again: {why}")),
    }
    if tell_init && let Err(why) = init_control::ask(Command::Extensions) {
        said.push_str(&format!(
            "\ninit has not taken the change ({why}); the next boot does"
        ));
    }
    said
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

/// What an install comes to, held to the store as it stands.
enum Prepared {
    /// The bytes it names run already.
    Current {
        name: String,
        before: Option<Record>,
    },
    Install {
        image: Image<File>,
        name: String,
        voucher: Voucher,
        before: Option<Record>,
        /// `before` as the install was judged against it.
        judged: Option<Record>,
        installed: Installed,
    },
}

fn prepare(store: &Store, path: &str, vouched: &Vouched) -> Result<Prepared, String> {
    let voucher = Voucher::read(vouched)?;
    let keys = store.keys()?;
    let refused = |e: admit::Refusal| format!("{path}: {e}");
    let named = match (vouched, voucher.signed()) {
        (Vouched::Approved { digest, .. }, _) => (*digest, "the request"),
        (Vouched::Signed { .. }, Some((sidecar, signature))) => {
            let (said, key) = admit::signer(sidecar, signature, &keys).map_err(refused)?;
            let before = store.record(&said.name)?;
            let judged = before
                .clone()
                .map(|record| archive_floor(store, record, said.sha256));
            if let Some(Admitted::Current) =
                admit::ahead(judged.as_ref(), &said, key).map_err(refused)?
            {
                return Ok(Prepared::Current {
                    name: said.name,
                    before,
                });
            }
            (said.sha256, "its sidecar")
        }
        (Vouched::Signed { .. }, None) => return Err(format!("{path}: no signature was read")),
    };
    let mut image = read_image(path, Some(named))?;
    let name = image.release.name.clone();
    let base = Base::read()?;
    let before = store.record(&name)?;
    let others = others(store, &name)?;
    let judged = before
        .clone()
        .map(|record| archive_floor(store, record, image.digest));
    let admitted = admit::install(
        judged.as_ref(),
        &mut image,
        &voucher.authority(),
        &base.bounds(&keys),
        &borrowed(&others),
    )
    .map_err(refused)?;
    Ok(match admitted {
        Admitted::Current => Prepared::Current { name, before },
        Admitted::Install(installed) => Prepared::Install {
            image,
            name,
            voucher,
            before,
            judged,
            installed,
        },
    })
}

/// `record` as an install of `digest` is judged: a floor an archive-form
/// image set, which no install takes now, holds its version and not its
/// bytes.
fn archive_floor(store: &Store, mut record: Record, digest: [u8; 32]) -> Record {
    if let Some((version, floor)) = record.floor
        && store
            .image(&record.name, &floor)
            .is_ok_and(|mut held| archive_form(&mut held))
    {
        record.floor = Some((version, digest));
    }
    record
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
    let (image, name, voucher, before, judged, installed) = match prepared {
        Prepared::Current { name, before } => {
            if let Some(mut record) = before.clone()
                && give_uids(store, &mut record)?
            {
                store.edit(&name, |edit| edit.put_record(&record))?;
            }
            let idle = before.as_ref().and_then(idle);
            return Ok(format!(
                "{name} is installed already{}",
                idle.map_or_else(String::new, |why| format!("; {why}"))
            ));
        }
        Prepared::Install {
            image,
            name,
            voucher,
            before,
            judged,
            installed,
        } => (image, name, voucher, before, judged, installed),
    };
    let digest = image.digest;
    let version = installed.version;
    let mut record = judged.unwrap_or_else(|| Record::new(&name));
    record.install(installed);
    give_uids(store, &mut record)?;
    store.edit(&name, |edit| {
        edit.put_image(&digest, image.source(), image.bytes(), voucher.signed())?;
        edit.put_record(&record)
    })?;
    if let Some(why) = idle(&record) {
        return Ok(format!("{name} {version} installed; {why}"));
    }
    let stored = store
        .image(&name, &digest)
        .and_then(|file| Image::open(file, Some(digest)).map_err(|e| e.to_string()));
    match stored.and_then(|stored| activate(&name, &stored)) {
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

/// Show `image`, a file of the store, at `/opt/<name>` in place of what is
/// shown there, its `/etc/<name>` laid out first when it declares `config`.
fn activate(name: &str, image: &Image<File>) -> Result<String, String> {
    if image.declaration.config {
        configure::own_dir(name)?;
    }
    if sys_core::boot_flag(BOOT_FLAG_EXTENSIONS_OFF) {
        return Ok(format!(
            "extensions are off this boot, so it is not shown; `extctl enable {name}` shows it on a boot that allows them"
        ));
    }
    crate::syscall::base::ext_activate(name.as_bytes(), image.source().as_raw_fd(), &image.digest)
        .map(|activation| format!("it runs as activation {activation}"))
        .map_err(|e| format!("it could not be activated: {e}"))
}

/// [`activate`], stopping the extension when it will not run.
fn run_image(store: &Store, record: &mut Record, image: &Image<File>) -> Result<String, String> {
    activate(&record.name, image).or_else(|why| {
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
/// under. Its files were held to its manifest when it went in, and its root
/// is the one installed, so they are read only as the check needs them.
fn checked(
    store: &Store,
    record: &Record,
    keys: &Keys,
    base: &Base,
) -> Result<Image<File>, String> {
    let current = record
        .current
        .as_ref()
        .ok_or_else(|| "nothing is installed".to_owned())?;
    let file = store.image(&record.name, &current.sha256)?;
    let signed = store.signed(&record.name, &current.sha256)?;
    let mut image = match Image::open(file, Some(current.sha256)) {
        Err(ImageError::Other) => return Err("the stored image is not the one installed".into()),
        image => image.map_err(|e| e.to_string())?,
    };
    let signed = signed.as_ref().map(|(s, g)| (s.as_slice(), g.as_slice()));
    admit::activation(record, &mut image, signed, &base.bounds(keys)).map_err(|e| e.to_string())?;
    Ok(image)
}

/// [`checked`], stopping one that no longer fits.
fn fitting(
    store: &Store,
    record: &mut Record,
    keys: &Keys,
    base: &Base,
) -> Result<Image<File>, String> {
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
    let image = fitting(store, record, keys, base)?;
    run_image(store, record, &image)
}

pub(super) fn record_of(store: &Store, name: &str) -> Result<Record, String> {
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
    give_uids(&store, &mut record)?;
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
fn rolled_back(store: &Store, name: &str) -> Result<(Record, Image<File>), String> {
    let mut record = record_of(store, name)?;
    record.roll_back().map_err(|e| format!("{name}: {e}"))?;
    record.enabled = true;
    record.why = None;
    give_uids(store, &mut record)?;
    if let Some(current) = &record.current {
        admit::clashes(name, &current.grant, &borrowed(&others(store, name)?))
            .map_err(|e| format!("{name}: {e}"))?;
    }
    let image = checked(store, &record, &store.keys()?, &Base::read()?)
        .map_err(|why| format!("{name}: the previous image does not fit: {why}"))?;
    Ok((record, image))
}

fn rollback(name: &str) -> Result<String, String> {
    let store = Store::open(true)?;
    let before = record_of(&store, name)?;
    let (record, image) = rolled_back(&store, name)?;
    store.edit(name, |edit| edit.put_record(&record))?;
    let version = record.current.as_ref().map_or(0, |c| c.version);
    match activate(name, &image) {
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
            Ok(Some(mut record)) if record.enabled => give_uids(&store, &mut record)
                .and_then(|given| {
                    if given {
                        store.edit(&name, |edit| edit.put_record(&record))?;
                    }
                    bring_up(&store, &mut record, &keys, &base)
                })
                .map(|ran| format!("{name}: {ran}")),
            Ok(Some(_)) => deactivate(&name).map(|()| format!("{name}: disabled")),
            Ok(None) => continue,
            Err(why) => Err(why),
        };
        said.push(ran.unwrap_or_else(|why| why));
    }
    Ok(said.join("\n"))
}

/// `command` on the store of the root at `root`, which runs nothing here:
/// that root's next boot holds what it enables to its record again.
fn rooted(root: &Path, command: Rooted) -> Result<String, String> {
    let (name, enabled) = match command {
        Rooted::Status(only) => {
            return status_of(&Store::open_at(root, false)?, only.as_deref(), false);
        }
        Rooted::Enable(name) => (name, true),
        Rooted::Disable(name) => (name, false),
    };
    let store = Store::open_at(root, true)?;
    let mut record = record_of(&store, &name)?;
    record.enabled = enabled;
    record.why = None;
    store.edit(&name, |edit| edit.put_record(&record))?;
    Ok(format!(
        "{name} {} on the root at {}",
        if enabled { "enabled" } else { "disabled" },
        root.display()
    ))
}

fn status(only: Option<&str>) -> Result<String, String> {
    status_of(&Store::open(false)?, only, true)
}

/// What `store` keeps, and whether each extension runs when `running` asks
/// it of this root.
fn status_of(store: &Store, only: Option<&str>, running: bool) -> Result<String, String> {
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
            Ok(Some(record)) => out.push(describe(&record, running)),
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

fn describe(record: &Record, running: bool) -> String {
    let running = match running {
        false => "",
        true if crate::activation::open(&record.name).is_ok() => ", running",
        true => ", not running",
    };
    let mut out = format!(
        "{}: {}{running}, floor {}{}",
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
    if !record.uids.is_empty() {
        let uids: Vec<String> = record
            .uids
            .iter()
            .map(|(user, uid)| format!("{user}={uid}"))
            .collect();
        out.push_str(&format!("\n  uids {}", uids.join(",")));
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
    let packed = slopos_extension_core::source::pack_into(Path::new(dir), Path::new(out))?;
    Ok(format!(
        "{} {}: {} (root {}) and {}",
        packed.sidecar.name,
        packed.sidecar.version,
        packed.image_at.display(),
        hex(&packed.sidecar.sha256),
        packed.sidecar_at.display()
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
