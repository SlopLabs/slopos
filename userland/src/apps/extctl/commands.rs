//! `/opt/bin`: a link to each command of every extension shown now, as its
//! grant names it. Only the links into `/opt` are this file's; anything else
//! there stays as it is.

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};

use slopos_extension_core::declare::mount_point;

use super::store::Store;
use crate::activation;

pub const DIR: &str = "/opt/bin";

/// Each command of every extension that runs and is shown, and the program
/// its link names.
fn wanted(store: &Store) -> Result<Vec<(String, PathBuf)>, String> {
    let mut out = Vec::new();
    for name in store.names()? {
        let Some(current) = store
            .record(&name)?
            .filter(|record| record.runs())
            .and_then(|record| record.current)
        else {
            continue;
        };
        if activation::open(&name).is_err() {
            continue;
        }
        let bin = Path::new(&mount_point(&name)).join("bin");
        out.extend(
            current
                .grant
                .commands
                .into_iter()
                .map(|command| (command.clone(), bin.join(command))),
        );
    }
    Ok(out)
}

/// Make `/opt/bin` link each command of every extension shown now, and no
/// other command into `/opt`.
pub fn sync(store: &Store) -> Result<(), String> {
    let dir = Path::new(DIR);
    let shown = |e: std::io::Error| format!("{DIR}: {e}");
    let meta = fs::symlink_metadata(dir).map_err(shown)?;
    if !meta.is_dir() || meta.uid() != 0 {
        return Err(format!("{DIR} is not uid 0's directory"));
    }
    let wanted = wanted(store)?;
    for entry in fs::read_dir(dir).map_err(shown)? {
        let path = entry.map_err(shown)?.path();
        let Ok(target) = fs::read_link(&path) else {
            continue;
        };
        let command = path.file_name().and_then(|name| name.to_str());
        let kept = wanted
            .iter()
            .any(|(c, t)| Some(c.as_str()) == command && *t == target);
        if !kept && target.starts_with(slopos_abi::extension::DIR) {
            fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        }
    }
    let mut taken = Vec::new();
    for (command, target) in wanted {
        let at = dir.join(&command);
        match fs::read_link(&at) {
            Ok(held) if held == target => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {
                symlink(&target, &at).map_err(|e| format!("{}: {e}", at.display()))?
            }
            _ => taken.push(command),
        }
    }
    if taken.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} held by something else in {DIR}",
            taken.join(", ")
        ))
    }
}
