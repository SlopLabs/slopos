//! A base image's digest, and a base sealed again under another build tag, as
//! the tests that install bases make one out of the base a disk holds.

use slopos_boot_core::base::{
    self, BUILD_TAG_PATH, Digest, INTERFACE_PATH, MANIFEST_PATH, ManifestLine,
};
use slopos_boot_core::newc::{self, Record};
use slopos_tls_core::hash::{Hash, Sha256};

pub fn sha256(data: &[u8]) -> Digest {
    let mut digest = [0u8; 32];
    let mut hash = Sha256::new();
    hash.update(data);
    hash.finish_into(&mut digest);
    digest
}

/// `image` tagged `tag` and, when given, naming `interface`, its manifest
/// sealed again as the packer seals one.
pub fn retagged(image: &[u8], tag: &str, interface: Option<u32>) -> Result<Vec<u8>, String> {
    let replaced = |path: &[u8]| {
        [MANIFEST_PATH, BUILD_TAG_PATH]
            .iter()
            .chain(interface.is_some().then_some(&INTERFACE_PATH))
            .any(|own| newc::same_path(path, own.as_bytes()))
    };
    let mut records: Vec<(Vec<u8>, u32, Vec<u8>)> = Vec::new();
    for entry in newc::entries(image) {
        let entry = entry.map_err(|e| format!("the base: {e:?}"))?;
        if !replaced(entry.path) {
            records.push((entry.path.to_vec(), entry.mode, entry.data.to_vec()));
        }
    }
    let data = slopos_abi::fs::S_IFREG | 0o644;
    records.push((BUILD_TAG_PATH.into(), data, format!("{tag}\n").into_bytes()));
    if let Some(n) = interface {
        records.push((INTERFACE_PATH.into(), data, format!("{n}\n").into_bytes()));
    }
    let mut manifest = String::new();
    for (path, mode, bytes) in &records {
        let path = std::str::from_utf8(path).map_err(|e| e.to_string())?;
        ManifestLine {
            digest: sha256(bytes),
            mode: *mode,
            path,
        }
        .write(&mut manifest)
        .map_err(|_| format!("{path}: no manifest line"))?;
    }
    records.push((MANIFEST_PATH.into(), data, manifest.into_bytes()));
    let mut out = Vec::with_capacity(image.len() + 4096);
    for (path, mode, bytes) in &records {
        let record = Record::new(path, *mode, bytes).ok_or("a record too large")?;
        record
            .pieces()
            .iter()
            .for_each(|piece| out.extend_from_slice(piece));
    }
    Record::trailer()
        .pieces()
        .iter()
        .for_each(|piece| out.extend_from_slice(piece));
    base::check(&out, sha256).map_err(|e| format!("the retagged base: {e}"))?;
    Ok(out)
}
