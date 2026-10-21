//! `/bin/tree-install NAME TREE DEST` — installs the directory TREE at DEST as
//! the tree NAME, by the rule every tree on a root follows: what NAME's last
//! install put down is replaced, what anyone else put beside it is kept, and
//! anything in the way refuses the install before a file is written.
//!
//! It makes everything as whoever runs it, writable by them alone, which is
//! all a person needs for what is their own: `/usr/local` while the first
//! person holds it, and their home, where a tree's manifest is kept in
//! `~/.local/share/slopos/trees`. Asked for `/usr/local` by anyone who does
//! not hold it, it asks privd's `org.slopos.software.install`, which runs it
//! as uid 0 and lets it install there and nowhere else, from a copy of TREE
//! holding what everyone may read and links staying within it
//! (`tree::stage`).

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use slopos_priv_core::ENV_ACTION;
use slopos_tree_core::MANIFEST_DIR;

use crate::privd::{self, Input};
use crate::root_layout::LOCAL;
use crate::syscall::process::geteuid;
use crate::tree::{self, Install};

const USAGE: &str = "usage: tree-install NAME TREE DEST";
const SOFTWARE: &str = "org.slopos.software.install";

/// Whether this process holds `path`, which it then writes itself.
fn holds(path: &str) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir() && meta.uid() == geteuid())
}

/// Where the manifest of a tree at `dest` is kept: in the home of whoever
/// installs it there, the system's directory elsewhere.
fn manifests(dest: &str) -> String {
    let home = crate::accounts::home_of(geteuid()).filter(|home| home.starts_with('/'));
    match home {
        Some(home) if Path::new(dest).starts_with(&home) && home != "/" => {
            format!("{}/.local/share/slopos/trees", home.trim_end_matches('/'))
        }
        _ => MANIFEST_DIR.to_owned(),
    }
}

fn run(name: &str, source: &str, dest: &str) -> Result<String, String> {
    if !slopos_tree_core::valid_name(name) {
        return Err(format!(
            "{name:?} is not a tree's name: lowercase letters, digits and _"
        ));
    }
    if !slopos_tree_core::valid_destination(dest) {
        return Err(format!("{dest:?} is not an absolute path without . or .."));
    }
    let by_privd = std::env::var_os(ENV_ACTION).is_some();
    if by_privd || dest == LOCAL && !holds(LOCAL) {
        privd::elevate(Some(SOFTWARE), &privd::own_args(), Input::Closed);
        if dest != LOCAL {
            return Err(format!("privd installs at {LOCAL} alone"));
        }
    }
    let staged = by_privd
        .then(|| tree::stage(Path::new(source)))
        .transpose()?;
    let source = staged
        .as_ref()
        .map_or(Path::new(source), tree::Staged::path);
    let new = tree::scan(source)?;
    let manifests = manifests(dest);
    let install = Install {
        root: "",
        name,
        guest: dest,
        manifests: &manifests,
        source,
        owner: None,
        keeper: None,
        remedy: "move it aside",
    };
    Ok(match install.run(&new, &mut |_| {})? {
        Some(bytes) => format!("installed {name} at {dest} ({bytes} bytes copied)"),
        None => format!("{dest} already holds this {name}"),
    })
}

pub fn tree_install_main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [name, source, dest] = args.as_slice() else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    match run(name, source, dest) {
        Ok(said) => println!("tree-install: {said}"),
        Err(e) => {
            eprintln!("tree-install: {e}");
            std::process::exit(1);
        }
    }
}
