//! Packing an image from the directory it is laid out in: `bin/`,
//! `libexec/`, `lib/` and `share/` as they are to be shown under
//! `/opt/<name>`, and `.extension/release` and `.extension/declare`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::string::{String, ToString};
use std::vec::Vec;
use std::{format, io};

use slopos_abi::fs::{S_IFDIR, S_IFLNK, S_IFREG};

use crate::image::{DIR, Image, Packer, TOP};
use crate::sha256;
use crate::sidecar::Sidecar;

/// The image `dir` lays out and its sidecar.
pub fn pack(dir: &Path) -> Result<(Vec<u8>, Sidecar), String> {
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
    let image = packer.finish(&release, &declare)?;
    let checked = Image::check(&image).map_err(|e| e.to_string())?;
    let sidecar = Sidecar {
        name: checked.release.name.clone(),
        version: checked.release.version,
        sha256: sha256(&image),
    };
    Ok((image, sidecar))
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
        packer.add(inside, S_IFREG | permissions, fs::read(host).map_err(fail)?);
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
        let (image, sidecar) = pack(&dir).unwrap();
        let checked = Image::check(&image).unwrap();
        assert_eq!((sidecar.name.as_str(), sidecar.version), ("hello", 2));
        assert_eq!(sidecar.sha256, sha256(&image));
        assert_eq!(
            checked.entry("/bin/hello").map(|e| e.mode),
            Some(S_IFREG | 0o755)
        );
        assert_eq!(
            checked.entry("/share/hello").map(|e| e.data),
            Some(&b"doc/README"[..])
        );
        assert_eq!(
            checked.entry("/share/doc").map(|e| e.mode & S_IFDIR),
            Some(S_IFDIR)
        );
        assert_eq!(
            pack(&dir).unwrap().0,
            image,
            "the same tree packs the same bytes"
        );
        fs::set_permissions(dir.join("share/doc"), fs::Permissions::from_mode(0o775)).unwrap();
        fs::set_permissions(
            dir.join("share/doc/README"),
            fs::Permissions::from_mode(0o664),
        )
        .unwrap();
        fs::set_permissions(dir.join("bin/hello"), fs::Permissions::from_mode(0o775)).unwrap();
        assert_eq!(
            pack(&dir).unwrap().0,
            image,
            "a looser umask packs the same bytes"
        );
        fs::write(dir.join("README"), b"x").unwrap();
        assert!(pack(&dir).unwrap_err().contains("outside bin"));
        let _ = fs::remove_dir_all(&dir);
    }
}
