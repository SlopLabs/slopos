//! Packing an image from the directory it is laid out in: `bin/`,
//! `libexec/`, `lib/` and `share/` as they are to be shown under
//! `/opt/<name>`, and `.extension/release` and `.extension/declare`.

use std::fs::{self, File};
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::string::{String, ToString};
use std::vec::Vec;
use std::{format, io};

use slopos_abi::fs::{S_IFDIR, S_IFLNK, S_IFREG};
use slopos_verity_core::{BLOCK, Digest};

use crate::image::{DIR, Image, Packer, TOP};
use crate::release::Release;
use crate::sidecar::Sidecar;

/// An image packed into a directory, beside its sidecar.
pub struct Packed {
    pub image: Image<File>,
    pub sidecar: Sidecar,
    pub image_at: PathBuf,
    pub sidecar_at: PathBuf,
}

/// The image `dir` lays out, written to `<out>/<name>.ext` and its sidecar to
/// `<out>/<name>.meta`; a signature an earlier sidecar had, or an image that
/// does not check out, is taken away.
pub fn pack_into(dir: &Path, out: &Path) -> Result<Packed, String> {
    let release = dir.join(".extension/release");
    let name = fs::read_to_string(&release)
        .map_err(|e| e.to_string())
        .and_then(|text| Release::parse(&text).map_err(|e| e.to_string()))
        .map_err(|e| format!("{}: {e}", release.display()))?
        .name;
    fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let at = |ext: &str| out.join(format!("{name}.{ext}"));
    let (image_at, sidecar_at) = (at("ext"), at("meta"));
    let _ = fs::remove_file(&sidecar_at);
    let _ = fs::remove_file(at("meta.sig"));
    let packed = pack(dir, &image_at);
    if packed.is_err() {
        let _ = fs::remove_file(&image_at);
    }
    let (image, sidecar) = packed?;
    fs::write(&sidecar_at, sidecar.render())
        .map_err(|e| format!("{}: {e}", sidecar_at.display()))?;
    Ok(Packed {
        image,
        sidecar,
        image_at,
        sidecar_at,
    })
}

/// The image `dir` lays out, written to `out`, and its sidecar.
pub fn pack(dir: &Path, out: &Path) -> Result<(Image<File>, Sidecar), String> {
    let read = |file: &str| {
        fs::read_to_string(dir.join(file)).map_err(|e| format!("{}: {e}", dir.join(file).display()))
    };
    let release = read(".extension/release")?;
    let declare = read(".extension/declare")?;
    let mut packer = Packer::default();
    let mut top: Vec<String> = names(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    top.sort();
    for name in top {
        let inside = format!("/{name}");
        if inside == DIR {
            continue;
        }
        if !TOP.contains(&inside.as_str()) {
            return Err(format!(
                "{}: outside bin, libexec, lib and share",
                dir.join(&name).display()
            ));
        }
        add(&mut packer, &dir.join(&name), &inside)?;
    }
    write_image(packer, &release, &declare, out)?;
    let file = File::open(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let checked = Image::check(file).map_err(|e| format!("{}: {e}", out.display()))?;
    let sidecar = Sidecar {
        name: checked.release.name.clone(),
        version: checked.release.version,
        sha256: checked.digest,
    };
    Ok((checked, sidecar))
}

/// Write the image `packer` makes to `out`, a hole where a block is zero;
/// answers its root.
pub fn write_image(
    packer: Packer,
    release: &str,
    declare: &str,
    out: &Path,
) -> Result<Digest, String> {
    let shown = |e: io::Error| format!("{}: {e}", out.display());
    let mut file = File::create(out).map_err(shown)?;
    let mut len = 0u64;
    let root = packer.write(release, declare, &mut |block| {
        len += BLOCK as u64;
        if block.iter().all(|&b| b == 0) {
            file.seek(SeekFrom::Current(BLOCK as i64)).map(drop)
        } else {
            file.write_all(block)
        }
        .map_err(shown)
    })?;
    file.set_len(len).map_err(shown)?;
    file.sync_all().map_err(shown)?;
    Ok(root)
}

fn names(dir: &Path) -> io::Result<Vec<String>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let name = entry?.file_name();
        out.push(
            name.into_string().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "a name that is not UTF-8")
            })?,
        );
    }
    Ok(out)
}

/// `host` at `inside`, a directory with what it holds in name order.
fn add(packer: &mut Packer, host: &Path, inside: &str) -> Result<(), String> {
    let fail = |e: io::Error| format!("{}: {e}", host.display());
    let meta = fs::symlink_metadata(host).map_err(fail)?;
    let permissions = if meta.is_dir() || meta.permissions().mode() & 0o111 != 0 {
        0o755
    } else {
        0o644
    };
    let kind = meta.file_type();
    if kind.is_symlink() {
        let target = fs::read_link(host).map_err(fail)?;
        let target = target
            .to_str()
            .ok_or_else(|| format!("{}: a link to a name that is not UTF-8", host.display()))?;
        packer.add(inside, S_IFLNK | 0o777, target.as_bytes().to_vec());
    } else if kind.is_dir() {
        packer.add(inside, S_IFDIR | permissions, Vec::new());
        let mut children = names(host).map_err(fail)?;
        children.sort();
        for child in children {
            add(packer, &host.join(&child), &format!("{inside}/{child}"))?;
        }
    } else if kind.is_file() {
        packer
            .add_file(inside, S_IFREG | permissions, host)
            .map_err(fail)?;
    } else {
        return Err(format!("{}: no file, directory or link", host.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn scratch() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("slopos-ext-source-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".extension")).unwrap();
        dir
    }

    #[test]
    fn a_directory_packs_into_the_image_it_lays_out() {
        let dir = scratch();
        fs::write(dir.join(".extension/release"), "name hello\nversion 2\n").unwrap();
        fs::write(dir.join(".extension/declare"), "command hello\n").unwrap();
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::create_dir_all(dir.join("share/doc")).unwrap();
        fs::write(dir.join("bin/hello"), b"program").unwrap();
        fs::set_permissions(dir.join("bin/hello"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(dir.join("share/doc/README"), b"hi").unwrap();
        symlink("doc/README", dir.join("share/hello")).unwrap();
        let out = dir.with_extension("ext");
        let (checked, sidecar) = pack(&dir, &out).unwrap();
        let image = fs::read(&out).unwrap();
        assert_eq!((sidecar.name.as_str(), sidecar.version), ("hello", 2));
        assert_eq!(sidecar.sha256, checked.digest);
        assert_eq!(
            checked.entry("/bin/hello").map(|e| u32::from(e.mode)),
            Some(S_IFREG | 0o755)
        );
        assert_eq!(
            checked.links().collect::<Vec<_>>(),
            [("/share/hello", "doc/README")]
        );
        assert_eq!(
            checked
                .entry("/share/doc")
                .map(|e| u32::from(e.mode) & S_IFDIR),
            Some(S_IFDIR)
        );
        pack(&dir, &out).unwrap();
        assert!(
            fs::read(&out).unwrap() == image,
            "the same tree packs the same bytes"
        );
        fs::set_permissions(dir.join("share/doc"), fs::Permissions::from_mode(0o775)).unwrap();
        fs::set_permissions(
            dir.join("share/doc/README"),
            fs::Permissions::from_mode(0o664),
        )
        .unwrap();
        fs::set_permissions(dir.join("bin/hello"), fs::Permissions::from_mode(0o775)).unwrap();
        pack(&dir, &out).unwrap();
        assert!(
            fs::read(&out).unwrap() == image,
            "a looser umask packs the same bytes"
        );
        fs::write(dir.join("README"), b"x").unwrap();
        assert!(pack(&dir, &out).unwrap_err().contains("outside bin"));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_file(&out);
    }
}
