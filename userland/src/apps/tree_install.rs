//! `/bin/tree-install NAME TREE DEST` — installs the directory TREE at DEST as
//! the tree NAME, by the rule every tree on a root follows: what NAME's last
//! install put down is replaced, what anyone else put beside it is kept, and
//! anything in the way refuses the install before a file is written.
//!
//! It holds no capability and makes everything as whoever runs it, so the
//! session installs into what is its own: the apps at `/usr/local`, as
//! `scripts/selfhost.sh install` does.

use std::path::Path;

use crate::tree::{self, Install};

const USAGE: &str = "usage: tree-install NAME TREE DEST";

fn run(name: &str, source: &str, dest: &str) -> Result<String, String> {
    if !slopos_tree_core::valid_name(name) {
        return Err(format!(
            "{name:?} is not a tree's name: lowercase letters, digits and _"
        ));
    }
    if !slopos_tree_core::valid_destination(dest) {
        return Err(format!("{dest:?} is not an absolute path without . or .."));
    }
    let source = Path::new(source);
    let new = tree::scan(source)?;
    let install = Install {
        root: "",
        name,
        guest: dest,
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
