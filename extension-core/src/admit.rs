//! Whether an image goes in, and whether an installed one may run. Three
//! things stay apart: the measurement, the digest of the bytes the kernel is
//! to serve; the authority that vouched for them, a person's approval of a
//! grant or a trusted key's signature; and the grant, which every activation
//! holds again to the key's ceilings and the base's.

use alloc::format;
use alloc::string::{String, ToString};

use slopos_priv_core::capability_names;

use crate::ceiling::NEVER;
use crate::grant::Grant;
use crate::image::{Image, Source};
use crate::keys::{Key, Keys};
use crate::record::{Installed, Record};
use crate::release::Release;
use crate::sidecar::Sidecar;
use crate::symbols;
use crate::words::render_list;

/// What every check is made against: the keys the owner trusts, the base's
/// ceiling and the running `libc.so`.
pub struct Bounds<'a> {
    pub keys: &'a Keys,
    pub ceiling: u64,
    pub libc: &'a [u8],
}

/// Who vouches for an image.
pub enum Authority<'a> {
    /// The person approved the grant a first install's command line names.
    Person { name: &'a str, grant: &'a Grant },
    /// Its sidecar, signed.
    Signed {
        sidecar: &'a [u8],
        signature: &'a [u8],
    },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Admitted {
    /// The same version and bytes are current.
    Current,
    Install(Installed),
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
/// other installed extension's name and grant, claims already.
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

/// Whether `image` goes in under `authority`, beside `record` when the store
/// keeps one, and beside `others`.
pub fn install<S: Source>(
    record: Option<&Record>,
    image: &mut Image<S>,
    authority: &Authority<'_>,
    bounds: &Bounds<'_>,
    others: &[(&str, &Grant)],
) -> Result<Admitted, Refusal> {
    let release = image.release.clone();
    let digest = image.digest;
    let declared = image.declaration.grant();
    let signer = match authority {
        Authority::Person { name, grant } => {
            if *name != release.name {
                return refuse(format!("the image is {}, not {name}", release.name));
            }
            if !grant.same(&declared) {
                return refuse(format!(
                    "the image asks for `{}`, not what was approved",
                    declared.words().join(" ")
                ));
            }
            None
        }
        Authority::Signed { sidecar, signature } => {
            let key = signed(sidecar, signature, &release, digest, bounds.keys)?;
            let fingerprint = key.key.fingerprint();
            under_key(record, &fingerprint, release.version)?;
            within_key(&declared, &image.declaration, key)?;
            Some(fingerprint)
        }
    };
    if let Some(admitted) = beside(record, release.version, digest)? {
        return Ok(admitted);
    }
    within_base(&declared, bounds.ceiling)?;
    clashes(&release.name, &declared, others)?;
    symbols::check_image(image, bounds.libc).map_err(|e| Refusal(e.to_string()))?;
    Ok(Admitted::Install(Installed {
        version: release.version,
        sha256: digest,
        signer,
        grant: declared,
    }))
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
    under_key(record, &key.key.fingerprint(), said.version)?;
    beside(record, said.version, said.sha256)
}

/// What a key's install of `version` meets in `record` alone.
fn under_key(record: Option<&Record>, fingerprint: &str, version: u64) -> Result<(), Refusal> {
    let Some(record) = record else {
        return Ok(());
    };
    if record.signer.as_deref() != Some(fingerprint)
        && (record.signer.is_some() || record.floor.is_some())
    {
        return refuse(
            "the store's record names another authority than this key, so a person approves it",
        );
    }
    if record.current.is_none() {
        return refuse("it was removed, so it goes back in only as a first install");
    }
    if record.refused.contains(&version) {
        return refuse(format!(
            "version {version} was rolled back from, so it goes in only as a first install"
        ));
    }
    Ok(())
}

/// Whether `record` takes `version` of the bytes `digest`: `Current` when
/// it runs them already.
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
/// sidecar and signature beside it when a key installed it.
pub fn activation<S: Source>(
    record: &Record,
    image: &mut Image<S>,
    signed: Option<(&[u8], &[u8])>,
    bounds: &Bounds<'_>,
) -> Result<(), Refusal> {
    let current = record
        .current
        .as_ref()
        .ok_or_else(|| Refusal("nothing is installed".into()))?;
    let digest = image.digest;
    if current.sha256 != digest {
        return refuse("the stored image is not the one installed");
    }
    if image.release.name != record.name || image.release.version != current.version {
        return refuse("the stored image says it is another extension or version");
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
    symbols::check_image(image, bounds.libc).map_err(|e| Refusal(e.to_string()))
}

/// The trusted key `signature` names over `sidecar`, once the sidecar
/// describes the image `release` and `digest` name.
fn signed<'k>(
    sidecar: &[u8],
    signature: &[u8],
    release: &Release,
    digest: [u8; 32],
    keys: &'k Keys,
) -> Result<&'k Key, Refusal> {
    let (said, key) = signer(sidecar, signature, keys)?;
    if said.name != release.name || said.version != release.version || said.sha256 != digest {
        return refuse("the sidecar describes another image");
    }
    Ok(key)
}

/// The sidecar `sidecar` says and the trusted key `signature` names over it.
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
            "it asks for {}, which no extension is granted here",
            caps_of(past)
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
