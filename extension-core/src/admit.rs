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
use crate::image::Image;
use crate::keys::{Key, Keys};
use crate::record::{Installed, Record};
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

/// Whether `image`, whose SHA-256 is `digest`, goes in under `authority`,
/// beside `record` when the store keeps one; `others` are the commands
/// every other extension the store keeps declares.
pub fn install(
    record: Option<&Record>,
    image: &Image<'_>,
    digest: [u8; 32],
    authority: &Authority<'_>,
    bounds: &Bounds<'_>,
    others: &[(&str, &[String])],
) -> Result<Admitted, Refusal> {
    let release = &image.release;
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
            let key = signed(sidecar, signature, image, digest, bounds.keys)?;
            let fingerprint = key.key.fingerprint();
            if let Some(record) = record
                && record.signer.as_deref() != Some(fingerprint.as_str())
                && (record.signer.is_some() || record.floor.is_some())
            {
                return refuse(
                    "the store's record names another authority than this key, so a person approves it",
                );
            }
            if record.is_some_and(|r| r.refused.contains(&release.version)) {
                return refuse(format!(
                    "version {} was rolled back from, so it goes in only as a first install",
                    release.version
                ));
            }
            within_key(&declared, &image.declaration, key)?;
            Some(fingerprint)
        }
    };
    if let Some(record) = record {
        if let Some((floor, floor_digest)) = record.floor {
            if release.version < floor {
                return refuse(format!(
                    "version {} is older than version {floor}, which was installed",
                    release.version
                ));
            }
            if release.version == floor && floor_digest != digest {
                return refuse(format!("version {floor} was installed with other bytes",));
            }
        }
        if record
            .current
            .as_ref()
            .is_some_and(|c| c.version == release.version && c.sha256 == digest)
        {
            return Ok(Admitted::Current);
        }
    }
    within_base(&declared, bounds.ceiling)?;
    if let Some((other, _)) = others.iter().find(|(name, commands)| {
        *name != release.name && commands.iter().any(|c| declared.commands.contains(c))
    }) {
        return refuse(format!("{other} declares one of its commands already"));
    }
    let files: alloc::vec::Vec<(&str, &[u8])> = image.files().collect();
    symbols::check(&files, bounds.libc).map_err(|e| Refusal(e.to_string()))?;
    Ok(Admitted::Install(Installed {
        version: release.version,
        sha256: digest,
        signer,
        grant: declared,
    }))
}

/// Whether the current image of `record` may run as `image` is now, its
/// sidecar and signature beside it when a key installed it.
pub fn activation(
    record: &Record,
    image: &Image<'_>,
    digest: [u8; 32],
    signed: Option<(&[u8], &[u8])>,
    bounds: &Bounds<'_>,
) -> Result<(), Refusal> {
    let current = record
        .current
        .as_ref()
        .ok_or_else(|| Refusal("nothing is installed".into()))?;
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
        let key = self::signed(sidecar, signature, image, digest, bounds.keys)?;
        if key.key.fingerprint() != *fingerprint {
            return refuse("it is signed by another key than the one that installed it");
        }
        within_key(&declared, &image.declaration, key)?;
    }
    if record
        .floor
        .is_some_and(|(floor, _)| current.version < floor)
    {
        return refuse("its version is below the floor");
    }
    within_base(&declared, bounds.ceiling)?;
    let files: alloc::vec::Vec<(&str, &[u8])> = image.files().collect();
    symbols::check(&files, bounds.libc).map_err(|e| Refusal(e.to_string()))
}

/// The trusted key `signature` names over `sidecar`, once the sidecar
/// describes `image`.
fn signed<'k>(
    sidecar: &[u8],
    signature: &[u8],
    image: &Image<'_>,
    digest: [u8; 32],
    keys: &'k Keys,
) -> Result<&'k Key, Refusal> {
    let said = Sidecar::parse(sidecar).ok_or_else(|| Refusal("the sidecar is malformed".into()))?;
    if said.name != image.release.name
        || said.version != image.release.version
        || said.sha256 != digest
    {
        return refuse("the sidecar describes another image");
    }
    let key = Sidecar::signer(sidecar, signature).map_err(|e| Refusal(e.to_string()))?;
    keys.find(&key).ok_or_else(|| {
        Refusal(format!(
            "it is signed by {}, which the owner does not trust",
            key.fingerprint()
        ))
    })
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
