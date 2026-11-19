//! Whether an image goes in, and whether an installed one may run. Three
//! things stay apart: the measurement, the digest of the bytes the kernel is
//! to serve; the authority that vouched for them, a person's approval of a
//! grant, a trusted key's signature or uid 0's offer of an image that asks
//! for nothing; and the grant, which every activation holds again to the
//! key's ceilings and the base's. A name's images are one publisher's.

use alloc::format;
use alloc::string::{String, ToString};

use slopos_priv_core::capability_names;
use slopos_tls_core::sshsig::PublicKey;

use crate::ceiling::NEVER;
use crate::grant::Grant;
use crate::image::{Image, Source};
use crate::keys::{Key, Keys};
use crate::record::{Installed, Record};
use crate::release::Release;
use crate::sidecar::Sidecar;
use crate::symbols::{self, Needs};
use crate::words::render_list;

pub struct Bounds<'a> {
    pub keys: &'a Keys,
    pub ceiling: u64,
    pub libc: &'a [u8],
}

/// Who vouches for an image.
pub enum Authority<'a> {
    /// The person approved the grant and the signer the command line names.
    Person {
        name: &'a str,
        grant: &'a Grant,
        signer: Option<&'a str>,
        signed: Option<(&'a [u8], &'a [u8])>,
    },
    Signed {
        sidecar: &'a [u8],
        signature: &'a [u8],
    },
    /// uid 0 offered it; it asks for nothing ([`offerable`]).
    Offered,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Admitted {
    /// The same version and bytes are current.
    Current,
    /// `signer` is its publisher's key, `None` for nobody.
    Install {
        image: Installed,
        signer: Option<String>,
    },
}

/// Why an image does not go in or does not run, said to whoever asked.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Refusal(pub String);

impl core::fmt::Display for Refusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

fn refuse<T>(why: impl Into<String>) -> Result<T, Refusal> {
    Err(Refusal(why.into()))
}

fn caps_of(mask: u64) -> String {
    render_list(capability_names(mask))
}

/// Whether `grant`, `name`'s, claims a command or a user `others`, every
/// other installed package's name and grant, claims already.
pub fn clashes(name: &str, grant: &Grant, others: &[(&str, &Grant)]) -> Result<(), Refusal> {
    for (other, theirs) in others.iter().filter(|(other, _)| *other != name) {
        if let Some(command) = grant.commands.iter().find(|c| theirs.commands.contains(c)) {
            return refuse(format!("{other} declares the command {command} already"));
        }
        if let Some(user) = grant.users.iter().find(|u| theirs.users.contains(u)) {
            return refuse(format!("{other} declares the user {user} already"));
        }
    }
    Ok(())
}

/// Policy only: the image's bytes are [`contents`]'s, left to the mechanism
/// because it reads the whole image.
pub fn install<S: Source>(
    record: Option<&Record>,
    image: &Image<S>,
    authority: &Authority<'_>,
    bounds: &Bounds<'_>,
    others: &[(&str, &Grant)],
) -> Result<Admitted, Refusal> {
    let release = image.release.clone();
    let digest = image.digest;
    let declared = image.declaration.grant();
    let held = record.filter(|r| !r.removed());
    let (voucher, signer) = match authority {
        Authority::Person {
            name,
            grant,
            signer,
            signed,
        } => {
            if *name != release.name {
                return refuse(format!("the image is {}, not {name}", release.name));
            }
            if !grant.same(&declared) {
                return refuse(format!(
                    "the image asks for `{}`, not what was approved",
                    declared.words().join(" ")
                ));
            }
            let found = match signed {
                Some((sidecar, signature)) => {
                    Some(signed_by(sidecar, signature, &release, digest)?.fingerprint())
                }
                None => None,
            };
            if found.as_deref() != *signer {
                return refuse(format!(
                    "it is signed by {}, not by {} as was approved",
                    by(found.as_deref()),
                    by(*signer)
                ));
            }
            let current = held.and_then(|r| r.current.as_ref());
            if current.is_some_and(|c| c.version == release.version && c.sha256 == digest) {
                return Ok(Admitted::Current);
            }
            publisher(record, &release.name, found.as_deref())?;
            (None, found)
        }
        Authority::Signed { sidecar, signature } => {
            let key = self::signed(sidecar, signature, &release, digest, bounds.keys)?;
            let fingerprint = key.key.fingerprint();
            unasked(record, &release.name, Some(&fingerprint), release.version)?;
            within_key(&declared, &image.declaration, key)?;
            (Some(fingerprint.clone()), Some(fingerprint))
        }
        Authority::Offered => {
            offerable(&image.declaration)?;
            unasked(record, &release.name, None, release.version)?;
            (None, None)
        }
    };
    if let Some(admitted) = beside(held, release.version, digest)? {
        return Ok(admitted);
    }
    within_base(&declared, bounds.ceiling)?;
    clashes(&release.name, &declared, others)?;
    Ok(Admitted::Install {
        image: Installed {
            version: release.version,
            sha256: digest,
            signer: voucher,
            grant: declared,
        },
        signer,
    })
}

pub fn contents<S: Source>(image: &mut Image<S>, libc: &[u8]) -> Result<Needs, Refusal> {
    image.check_contents().map_err(|e| Refusal(e.to_string()))?;
    symbols::check_image(image, libc).map_err(|e| Refusal(e.to_string()))
}

/// What `record` alone decides of the install `key` vouches for with `said`,
/// before its image is read: `Current`, a refusal, or nothing yet.
pub fn ahead(
    record: Option<&Record>,
    said: &Sidecar,
    key: &Key,
) -> Result<Option<Admitted>, Refusal> {
    if record.is_some_and(|r| r.name != said.name) {
        return refuse("the sidecar describes another image");
    }
    unasked(
        record,
        &said.name,
        Some(&key.key.fingerprint()),
        said.version,
    )?;
    beside(record, said.version, said.sha256)
}

/// [`ahead`] for an offered image.
pub fn ahead_offered(record: Option<&Record>, said: &Sidecar) -> Result<Option<Admitted>, Refusal> {
    if record.is_some_and(|r| r.name != said.name) {
        return refuse("the sidecar describes another image");
    }
    unasked(record, &said.name, None, said.version)?;
    beside(record, said.version, said.sha256)
}

/// Only an image that hands out nothing goes in unasked.
pub fn offerable(declaration: &crate::declare::Declaration) -> Result<(), Refusal> {
    let asks = [
        (!declaration.users.is_empty(), "a user"),
        (!declaration.services.is_empty(), "a service"),
        (!declaration.actions.is_empty(), "an action"),
        (declaration.state, "/var/lib/<name>"),
        (declaration.config, "/etc/<name>"),
        (declaration.runtime, "/run/<name>"),
    ];
    match asks.iter().find(|(asked, _)| *asked) {
        Some((_, what)) => refuse(format!(
            "it asks for {what}, so it goes in only as a person allows it"
        )),
        None => Ok(()),
    }
}

pub const NOBODY: &str = "nobody";

fn by(signer: Option<&str>) -> &str {
    signer.unwrap_or(NOBODY)
}

/// Refuses another signer's image over a record that holds one.
fn publisher(record: Option<&Record>, name: &str, signer: Option<&str>) -> Result<(), Refusal> {
    let Some(record) = record.filter(|r| !r.removed()) else {
        return Ok(());
    };
    let held = record.signer.as_deref();
    if held == signer {
        return Ok(());
    }
    refuse(format!(
        "it is another publisher's image of {name}: {} signed the one installed and {} this one; \
         it goes in only once `pkgctl remove {name}` gives up the name",
        by(held),
        by(signer)
    ))
}

/// A key's or an offer's: never over what was removed or rolled back from.
fn unasked(
    record: Option<&Record>,
    name: &str,
    signer: Option<&str>,
    version: u64,
) -> Result<(), Refusal> {
    let Some(record) = record else {
        return Ok(());
    };
    if record.removed() {
        return refuse("it was removed, so it goes back in only as a first install");
    }
    publisher(Some(record), name, signer)?;
    if record.refused.contains(&version) {
        return refuse(format!(
            "version {version} was rolled back from, so it goes in only as a first install"
        ));
    }
    Ok(())
}

/// `Current` when `record` runs those bytes already.
fn beside(
    record: Option<&Record>,
    version: u64,
    digest: [u8; 32],
) -> Result<Option<Admitted>, Refusal> {
    let Some(record) = record else {
        return Ok(None);
    };
    if let Some((floor, floor_digest)) = record.floor {
        if version < floor {
            return refuse(format!(
                "version {version} is older than version {floor}, which was installed"
            ));
        }
        if version == floor && floor_digest != digest {
            return refuse(format!("version {floor} was installed with other bytes"));
        }
    }
    let current = record
        .current
        .as_ref()
        .is_some_and(|c| c.version == version && c.sha256 == digest);
    Ok(current.then_some(Admitted::Current))
}

/// Whether the current image of `record` may run as `image` is now, its
/// sidecar and signature beside it when a key installed it. Without `kept`
/// the image's programs are read, and their needs answered for keeping.
pub fn activation<S: Source>(
    record: &Record,
    image: &mut Image<S>,
    signed: Option<(&[u8], &[u8])>,
    bounds: &Bounds<'_>,
    kept: Option<&Needs>,
) -> Result<Option<Needs>, Refusal> {
    let current = record
        .current
        .as_ref()
        .ok_or_else(|| Refusal("nothing is installed".into()))?;
    let digest = image.digest;
    if current.sha256 != digest {
        return refuse("the stored image is not the one installed");
    }
    if image.release.name != record.name || image.release.version != current.version {
        return refuse("the stored image says it is another package or version");
    }
    let declared = image.declaration.grant();
    if !declared.same(&current.grant) {
        return refuse("the image asks for other than it was granted");
    }
    if let Some(fingerprint) = &current.signer {
        let (sidecar, signature) =
            signed.ok_or_else(|| Refusal("its signature is not in the store".into()))?;
        let key = self::signed(sidecar, signature, &image.release, digest, bounds.keys)?;
        if key.key.fingerprint() != *fingerprint {
            return refuse("it is signed by another key than the one that installed it");
        }
        within_key(&declared, &image.declaration, key)?;
    }
    if record
        .floor
        .is_some_and(|(floor, _)| current.version < floor)
        || record.refused.contains(&current.version)
    {
        return refuse("its version is below the floor or refused");
    }
    within_base(&declared, bounds.ceiling)?;
    let refused = |e: symbols::SymbolError| Refusal(e.to_string());
    match kept {
        Some(needs) => needs.held_by(bounds.libc).map(|()| None).map_err(refused),
        None => symbols::check_image(image, bounds.libc)
            .map(Some)
            .map_err(refused),
    }
}

fn signed<'k>(
    sidecar: &[u8],
    signature: &[u8],
    release: &Release,
    digest: [u8; 32],
    keys: &'k Keys,
) -> Result<&'k Key, Refusal> {
    let (said, key) = signer(sidecar, signature, keys)?;
    describes(&said, release, digest)?;
    Ok(key)
}

fn describes(said: &Sidecar, release: &Release, digest: [u8; 32]) -> Result<(), Refusal> {
    if said.name != release.name || said.version != release.version || said.sha256 != digest {
        return refuse("the sidecar describes another image");
    }
    Ok(())
}

/// The key that signed the image, trusted or not.
pub fn signed_by(
    sidecar: &[u8],
    signature: &[u8],
    release: &Release,
    digest: [u8; 32],
) -> Result<PublicKey, Refusal> {
    let said = Sidecar::parse(sidecar).ok_or_else(|| Refusal("the sidecar is malformed".into()))?;
    describes(&said, release, digest)?;
    Sidecar::signer(sidecar, signature).map_err(|e| Refusal(e.to_string()))
}

pub fn signer<'k>(
    sidecar: &[u8],
    signature: &[u8],
    keys: &'k Keys,
) -> Result<(Sidecar, &'k Key), Refusal> {
    let said = Sidecar::parse(sidecar).ok_or_else(|| Refusal("the sidecar is malformed".into()))?;
    let key = Sidecar::signer(sidecar, signature).map_err(|e| Refusal(e.to_string()))?;
    let key = keys.find(&key).ok_or_else(|| {
        Refusal(format!(
            "it is signed by {}, which the owner does not trust",
            key.fingerprint()
        ))
    })?;
    Ok((said, key))
}

fn within_key(
    declared: &Grant,
    declaration: &crate::declare::Declaration,
    key: &Key,
) -> Result<(), Refusal> {
    let past = declared.caps & !key.ceiling;
    if past != 0 {
        return refuse(format!(
            "it asks for {}, past its key's ceiling",
            caps_of(past)
        ));
    }
    let unasked = declaration.unasked_caps() & !key.user;
    if unasked != 0 {
        return refuse(format!(
            "its unasked actions hold {}, past its key's user ceiling",
            caps_of(unasked)
        ));
    }
    Ok(())
}

fn within_base(declared: &Grant, ceiling: u64) -> Result<(), Refusal> {
    let past = declared.caps & (!ceiling | NEVER);
    if past != 0 {
        return refuse(format!(
            "it asks for {}, which no package is granted here",
            caps_of(past)
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
