//! An extension's configuration, sealed files in a sealed `/etc/<name>`: the
//! person installs a file they edited, as sudoedit has them edit a copy, and
//! a file the key that installed the extension signed goes in unasked once
//! its serial is past the last. Init restarts each service that reads a file
//! that changed.
//!
//! A person's file reaches the mechanism as the requester's standard input,
//! never by a path the mechanism opens as uid 0, so it installs only what the
//! requester could read.

use std::fs::{self, File};
use std::io::ErrorKind;
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use slopos_extension_core::config::{self, Signed};
use slopos_extension_core::image::Image;
use slopos_extension_core::record::Record;
use slopos_extension_core::sha256;

use super::store::{self, Store};
use super::{SIDECAR_MAX, image_max, read_file, record_of};

const EXTCTL: &str = "/bin/extctl";

/// Who vouches for a configuration file, and where it is.
pub enum Source {
    /// The person approved standard input, whose SHA-256 is `digest`, as
    /// `/etc/<name>/<file>`.
    Approved {
        name: String,
        file: String,
        digest: [u8; 32],
    },
    /// The extension's key signed the sidecar at `sidecar` over the file at
    /// `from`.
    Signed {
        from: String,
        sidecar: String,
        signature: String,
    },
}

/// Standard input, a regular file, read whole without moving its offset,
/// which the mechanism it is handed to shares.
fn stdin_bytes() -> Result<Vec<u8>, String> {
    // SAFETY: fd 0 is this process's for its life, and ManuallyDrop leaves it
    // open.
    let stdin = ManuallyDrop::new(unsafe { File::from_raw_fd(0) });
    let shown = |why: &str| format!("standard input {why}");
    let meta = stdin.metadata().map_err(|e| shown(&e.to_string()))?;
    if !meta.is_file() || meta.len() > config::FILE_MAX {
        return Err(shown(&format!(
            "is not a file of at most {} bytes",
            config::FILE_MAX
        )));
    }
    let mut bytes = vec![0; meta.len() as usize];
    stdin
        .read_exact_at(&mut bytes, 0)
        .map_err(|e| shown(&e.to_string()))?;
    Ok(bytes)
}

/// Make `from` this process's standard input, as the requester of a
/// `config install` hands its file over, and give its SHA-256.
pub fn take_as_stdin(from: &str) -> Result<[u8; 32], String> {
    let file = File::open(from).map_err(|e| format!("{from}: {e}"))?;
    if file.as_raw_fd() == 0 {
        let _ = file.into_raw_fd();
    } else {
        crate::syscall::fs::dup2(file.as_raw_fd(), 0).map_err(|e| format!("{from}: {e}"))?;
    }
    stdin_bytes()
        .map(|bytes| sha256(&bytes))
        .map_err(|why| format!("{from}: {why}"))
}

/// A file read and held to the store as it stands.
pub struct Prepared {
    record: Record,
    file: String,
    bytes: Vec<u8>,
    /// The serial a key signed it under.
    serial: Option<u64>,
}

/// What `source` would install, once its extension is installed and
/// declares `config`, and for a signed file once the key that installed the
/// extension signed it, the owner still trusts that key, and its serial is
/// past the last that went in.
pub fn prepare(store: &Store, source: &Source) -> Result<Prepared, String> {
    let (bytes, name, file, signed) = match source {
        Source::Approved { name, file, digest } => {
            let bytes = stdin_bytes()?;
            if sha256(&bytes) != *digest {
                return Err("standard input is not the file whose sha256 the request names".into());
            }
            (bytes, name.clone(), file.clone(), None)
        }
        Source::Signed {
            from,
            sidecar,
            signature,
        } => {
            let bytes = read_file(from, config::FILE_MAX)?;
            let digest = sha256(&bytes);
            let text = read_file(sidecar, SIDECAR_MAX)?;
            let signature_bytes = read_file(signature, SIDECAR_MAX)?;
            let said = Signed::parse(&text)
                .ok_or_else(|| format!("{sidecar}: not a configuration's sidecar"))?;
            if said.sha256 != digest {
                return Err(format!("{from}: not the file {sidecar} describes"));
            }
            let key =
                Signed::signer(&text, &signature_bytes).map_err(|e| format!("{signature}: {e}"))?;
            (bytes, said.name, said.file, Some((said.serial, key)))
        }
    };
    let record = record_of(store, &name)?;
    let current = record
        .current
        .as_ref()
        .ok_or_else(|| format!("{name}: nothing is installed"))?;
    let image = store.image(&name, &current.sha256, image_max())?;
    let declared = Image::check(&image)
        .map_err(|e| format!("{name}: {e}"))?
        .declaration
        .config;
    if !declared {
        return Err(format!(
            "{name} declares no configuration in {}",
            config::dir(&name)
        ));
    }
    let serial = match signed {
        None => None,
        Some((serial, key)) => {
            let fingerprint = key.fingerprint();
            if record.signer.as_deref() != Some(fingerprint.as_str()) {
                return Err(format!(
                    "it is signed by {fingerprint}, not the key that installed {name}"
                ));
            }
            if store.keys()?.find(&key).is_none() {
                return Err(format!(
                    "it is signed by {fingerprint}, which the owner no longer trusts"
                ));
            }
            if serial <= record.config {
                return Err(format!(
                    "its serial {serial} is not past {}, the last that went in",
                    record.config
                ));
            }
            Some(serial)
        }
    };
    Ok(Prepared {
        record,
        file,
        bytes,
        serial,
    })
}

/// Put what `prepared` holds in place. A signed file's serial goes in the
/// record first, so no file signed before it goes in after, whatever cuts
/// this off.
pub fn install(store: &Store, prepared: Prepared) -> Result<String, String> {
    let Prepared {
        mut record,
        file,
        bytes,
        serial,
    } = prepared;
    if let Some(serial) = serial {
        record.config = serial;
        store.edit(&record.name, |edit| edit.put_record(&record))?;
    }
    let dir = own_dir(&record.name)?;
    let at = dir.join(&file);
    store::unsealed(&dir, || store::replace_sealed(&at, &bytes))?;
    Ok(format!("{} installed", at.display()))
}

/// `/etc/<name>` as `extctl` keeps it, made so where there is none: a sealed
/// directory of uid 0's, mode 0755, holding sealed files alone. Anything else
/// there was put by no `Seal` holder, or left by a write cut short, so it is
/// taken away rather than sealed in as the extension's configuration.
pub fn own_dir(name: &str) -> Result<PathBuf, String> {
    let dir = PathBuf::from(config::dir(name));
    let shown = |e: std::io::Error| format!("{}: {e}", dir.display());
    let mode = match fs::symlink_metadata(&dir) {
        Ok(meta) if meta.is_dir() && meta.uid() == 0 => meta.mode() & 0o7777,
        Ok(_) => return Err(format!("{} is not uid 0's directory", dir.display())),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            fs::create_dir(&dir).map_err(shown)?;
            0
        }
        Err(e) => return Err(shown(e)),
    };
    let strays = fs::read_dir(&dir)
        .map_err(shown)?
        .map(|entry| entry.map(|entry| entry.path()).map_err(shown))
        .filter(|path| !path.as_ref().is_ok_and(|path| store::sealed_file(path)))
        .collect::<Result<Vec<_>, _>>()?;
    if mode != 0o755 || !strays.is_empty() {
        store::unsealed(&dir, || {
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).map_err(shown)?;
            strays.iter().try_for_each(|path| {
                eprintln!(
                    "extctl: {} is no sealed file, so no configuration of {name}'s; taken away",
                    path.display()
                );
                store::remove_entry(path)
            })
        })?;
    }
    store::seal_dir(&dir)?;
    Ok(dir)
}

/// The editor the person named, as words.
fn editor() -> Vec<String> {
    ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|var| std::env::var(var).ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| "editor".to_owned())
        .split_ascii_whitespace()
        .map(str::to_owned)
        .collect()
}

/// Edit a copy of `/etc/<name>/<file>` with the person's editor and have
/// what they saved installed, as `extctl config install` installs a file,
/// once it differs: the exit status of that install, or 0 for no change.
pub fn edit(name: &str, file: &str) -> Result<i32, String> {
    let installed = Path::new(&config::dir(name)).join(file);
    let before = match fs::read(&installed) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(format!("{}: {e}", installed.display())),
    };
    let dir = PathBuf::from(format!(
        "/tmp/extctl-{}-{:016x}",
        std::process::id(),
        crate::privd::request_id()
    ));
    let made = |e: std::io::Error| format!("{}: {e}", dir.display());
    fs::create_dir(&dir).map_err(made)?;
    let edited = (|| {
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).map_err(made)?;
        let copy = dir.join(file);
        fs::write(&copy, &before).map_err(made)?;
        let words = editor();
        let status = Command::new(&words[0])
            .args(&words[1..])
            .arg(&copy)
            .status()
            .map_err(|e| format!("{}: {e}", words[0]))?;
        if !status.success() {
            return Err(format!("the editor exited {status}; nothing installed"));
        }
        let after = fs::read(&copy).map_err(made)?;
        if after == before {
            println!("{} is unchanged", installed.display());
            return Ok(0);
        }
        let copy = copy.to_str().ok_or("a path that is not UTF-8")?;
        let status = Command::new(EXTCTL)
            .args(["config", "install", name, file, copy])
            .status()
            .map_err(|e| format!("{EXTCTL}: {e}"))?;
        Ok(status.code().unwrap_or(1))
    })();
    let _ = fs::remove_dir_all(&dir);
    edited
}
