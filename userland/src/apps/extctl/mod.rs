//! `/bin/extctl` — installs extensions and keeps the store of them.
//!
//! A first install names what it grants on its command line, which is what
//! the person approving it reads; a signed install names no grant and goes in
//! unasked within the ceilings of the key that signed it. Every activation
//! holds the image again to its record, its key and the base's ceiling, and
//! one that no longer fits is stopped and says why.
//!
//! `status` and `build` need nothing. The rest run as privd's
//! `org.slopos.extension.*` actions, holding `ExtensionUpdate` and `Seal`;
//! `activate-all` is init's, which holds both.

mod store;

use std::path::Path;

use slopos_abi::extension::valid_name;
use slopos_abi::syscall::BOOT_FLAG_EXTENSIONS_OFF;
use slopos_extension_core::admit::{self, Admitted, Authority, Bounds};
use slopos_extension_core::grant::Grant;
use slopos_extension_core::image::Image;
use slopos_extension_core::keys::{Key, Keys};
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

/// What every activation of an extension may take together: the kernel's
/// own budget, so a larger image is refused before it is read whole.
fn image_max() -> u64 {
    let mut info = UserSysInfo::default();
    if sys_core::sys_info(&mut info) != 0 {
        return u64::MAX;
    }
    u64::from(info.total_pages) * 4096 / 8
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
                    let line = String::from_utf8_lossy(&text).trim().to_owned();
                    let mut words = line.splitn(3, ' ');
                    let key = key_of(words.next().unwrap_or(""), words.next().unwrap_or(""))
                        .map_err(|e| format!("{file}: {e}"))?;
                    (key, words.next().unwrap_or("").to_owned(), tail)
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

/// The action a command line belongs to, `None` for one that needs nothing.
fn action_for(args: &[String]) -> Option<&'static str> {
    Some(match args.first()?.as_str() {
        "install" => "org.slopos.extension.install",
        "install-signed" => "org.slopos.extension.install-signed",
        "key" => "org.slopos.extension.trust",
        "enable" | "disable" | "remove" | "rollback" => "org.slopos.extension.manage",
        _ => return None,
    })
}

fn named(name: &str) -> Result<&str, String> {
    if valid_name(name.as_bytes()) {
        Ok(name)
    } else {
        Err(format!("{name:?} is no extension's name"))
    }
}

fn run(args: &[String]) -> Result<String, String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["status"] => status(None),
        ["status", name] => status(Some(named(name)?)),
        ["build", dir] => build(dir, "."),
        ["build", dir, out] => build(dir, out),
        ["install", image, digest, "name", name, grant @ ..] => {
            let digest = parse_digest(digest).ok_or(USAGE)?;
            let grant = Grant::parse(grant).ok_or(USAGE)?;
            install_approved(image, digest, named(name)?, &grant)
        }
        ["install-signed", image] => {
            let stem = image.strip_suffix(".ext").unwrap_or(image);
            install_signed(image, &format!("{stem}.meta"), &format!("{stem}.meta.sig"))
        }
        ["install-signed", image, sidecar, signature] => install_signed(image, sidecar, signature),
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
            let key = Key::new(key, caps(ceiling)?, caps(user)?, &comment.join(" "))?;
            key_add(key)
        }
        ["key", "remove", fingerprint] => key_remove(fingerprint),
        ["enable", name] => enable(named(name)?),
        ["disable", name] => disable(named(name)?),
        ["remove", name] => remove(named(name)?),
        ["rollback", name] => rollback(named(name)?),
        ["activate-all"] => activate_all(),
        _ => Err(USAGE.into()),
    }
}

fn others(store: &Store, name: &str) -> Result<Vec<(String, Vec<String>)>, String> {
    let mut out = Vec::new();
    for other in store.names()? {
        if other == name {
            continue;
        }
        if let Some(current) = store.record(&other)?.and_then(|r| r.current) {
            out.push((other, current.grant.commands));
        }
    }
    Ok(out)
}

/// Put `admitted` in the store as `name`'s current image and run it.
fn install(
    store: &Store,
    record: Option<Record>,
    name: &str,
    image: &[u8],
    digest: [u8; 32],
    admitted: Admitted,
    signed: Option<(&[u8], &[u8])>,
) -> Result<String, String> {
    let installed = match admitted {
        Admitted::Current => return Ok(format!("{name} is installed already")),
        Admitted::Install(installed) => installed,
    };
    let version = installed.version;
    let mut record = record.unwrap_or_else(|| Record::new(name));
    record.install(installed);
    store.edit(name, |edit| {
        edit.put_image(&digest, image, signed)?;
        edit.put_record(&record)
    })?;
    let ran = run_image(store, &mut record, image, digest)?;
    Ok(format!("{name} {version} installed; {ran}"))
}

fn install_approved(
    path: &str,
    digest: [u8; 32],
    name: &str,
    grant: &Grant,
) -> Result<String, String> {
    let (bytes, digest) = read_image(path, Some(digest))?;
    let image = Image::check(&bytes).map_err(|e| format!("{path}: {e}"))?;
    let store = Store::open(true)?;
    let keys = store.keys()?;
    let base = Base::read()?;
    let record = store.record(name)?;
    let others = others(&store, name)?;
    let others: Vec<(&str, &[String])> = others
        .iter()
        .map(|(n, c)| (n.as_str(), c.as_slice()))
        .collect();
    let admitted = admit::install(
        record.as_ref(),
        &image,
        digest,
        &Authority::Person { name, grant },
        &base.bounds(&keys),
        &others,
    )
    .map_err(|e| format!("{path}: {e}"))?;
    install(&store, record, name, &bytes, digest, admitted, None)
}

fn install_signed(path: &str, sidecar_path: &str, signature_path: &str) -> Result<String, String> {
    let (bytes, digest) = read_image(path, None)?;
    let sidecar = read_file(sidecar_path, SIDECAR_MAX)?;
    let signature = read_file(signature_path, SIDECAR_MAX)?;
    let image = Image::check(&bytes).map_err(|e| format!("{path}: {e}"))?;
    let name = image.release.name.clone();
    let store = Store::open(true)?;
    let keys = store.keys()?;
    let base = Base::read()?;
    let record = store.record(&name)?;
    let others = others(&store, &name)?;
    let others: Vec<(&str, &[String])> = others
        .iter()
        .map(|(n, c)| (n.as_str(), c.as_slice()))
        .collect();
    let authority = Authority::Signed {
        sidecar: &sidecar,
        signature: &signature,
    };
    let admitted = admit::install(
        record.as_ref(),
        &image,
        digest,
        &authority,
        &base.bounds(&keys),
        &others,
    )
    .map_err(|e| format!("{path}: {e}"))?;
    install(
        &store,
        record,
        &name,
        &bytes,
        digest,
        admitted,
        Some((&sidecar, &signature)),
    )
}

/// Show `image` at `/opt/<name>`; a refusal stops the extension and says why.
fn run_image(
    store: &Store,
    record: &mut Record,
    image: &[u8],
    digest: [u8; 32],
) -> Result<String, String> {
    if sys_core::boot_flag(BOOT_FLAG_EXTENSIONS_OFF) {
        return Ok(
            "extensions are off this boot, so it runs from the next that allows them".into(),
        );
    }
    match crate::syscall::base::ext_activate(record.name.as_bytes(), image, &digest) {
        Ok(activation) => Ok(format!("it runs as activation {activation}")),
        Err(e) => {
            let why = format!("it could not be activated: {e}");
            stop(store, record, &why)?;
            Err(format!("{}: {why}", record.name))
        }
    }
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
/// under; one that no longer fits is stopped.
fn fitting(
    store: &Store,
    record: &mut Record,
    keys: &Keys,
    base: &Base,
) -> Result<Vec<u8>, String> {
    let name = record.name.clone();
    let current = record
        .current
        .clone()
        .ok_or_else(|| format!("{name}: nothing is installed"))?;
    let checked = store
        .image(&name, &current.sha256, image_max())
        .and_then(|bytes| {
            let signed = store.signed(&name, &current.sha256)?;
            let image = Image::check(&bytes).map_err(|e| e.to_string())?;
            let signed = signed.as_ref().map(|(s, g)| (s.as_slice(), g.as_slice()));
            admit::activation(record, &image, current.sha256, signed, &base.bounds(keys))
                .map_err(|e| e.to_string())?;
            Ok(bytes)
        });
    checked.or_else(|why| {
        stop(store, record, &why)?;
        Err(format!("{name} stopped: {why}"))
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

fn rollback(name: &str) -> Result<String, String> {
    let store = Store::open(true)?;
    let mut record = record_of(&store, name)?;
    let keys = store.keys()?;
    let base = Base::read()?;
    record.roll_back().map_err(|e| format!("{name}: {e}"))?;
    record.enabled = true;
    record.why = None;
    store.edit(name, |edit| edit.put_record(&record))?;
    let version = record.current.as_ref().map_or(0, |c| c.version);
    let ran = bring_up(&store, &mut record, &keys, &base)?;
    Ok(format!(
        "{name} is back at version {version}, which updates are held above; {ran}"
    ))
}

fn key_add(key: Key) -> Result<String, String> {
    let base = Base::read()?;
    let past = (key.ceiling | key.user) & !base.ceiling;
    if past != 0 {
        return Err(format!(
            "the ceilings name {}, which no extension is granted here",
            caps_of(past)
        ));
    }
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
        let Some(mut record) = store.record(&name)? else {
            continue;
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
        let Some(mut record) = store.record(&name)? else {
            continue;
        };
        if !record.enabled {
            deactivate(&name)?;
            continue;
        }
        said.push(match bring_up(&store, &mut record, &keys, &base) {
            Ok(ran) => format!("{name}: {ran}"),
            Err(why) => why,
        });
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
        let Some(record) = store.record(&name)? else {
            continue;
        };
        out.push(describe(&record));
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
    let args = match canonical(&args) {
        Ok(args) => args,
        Err(why) => {
            eprintln!("extctl: {why}");
            sys_core::exit_with_code(2);
        }
    };
    let bytes: Vec<Vec<u8>> = args.iter().map(|a| a.clone().into_bytes()).collect();
    privd::elevate(action_for(&args), &bytes, Input::Closed);
    match run(&args) {
        Ok(said) => println!("{said}"),
        Err(why) => {
            eprintln!("extctl: {why}");
            sys_core::exit_with_code(1);
        }
    }
}
