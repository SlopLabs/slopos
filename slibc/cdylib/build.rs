//! Writes `libc.so`'s dynamic list: every object slibc exports, so each stays
//! preemptible while everything else binds inside the library.
//!
//! A program that names `environ` or `optind` gets its own copy of it from a
//! `COPY` relocation, and the library must then read and write that copy
//! through its GOT; bound to its own copy it would never see the program's
//! writes, nor the program its. Functions stay bound at link time, which is
//! what keeps the interpreter's self-relocation to `R_X86_64_RELATIVE`.

use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../src");
    let mut objects = Vec::new();
    let mut files = Vec::new();
    collect(&src, &mut files);
    files.sort();
    for file in &files {
        println!("cargo:rerun-if-changed={}", file.display());
        let text = fs::read_to_string(file).expect("slibc source is readable");
        let mut exported = false;
        for line in text.lines().map(str::trim) {
            if line == "#[unsafe(no_mangle)]" {
                exported = true;
                continue;
            }
            if exported
                && let Some(rest) = line
                    .strip_prefix("pub static mut ")
                    .or_else(|| line.strip_prefix("pub static "))
                && let Some(name) = rest.split(':').next()
            {
                objects.push(name.trim().to_owned());
            }
            if !line.starts_with("#[") && !line.starts_with("///") {
                exported = false;
            }
        }
    }
    objects.sort();
    objects.dedup();
    let list =
        PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("dynamic.list");
    let body: String = objects.iter().map(|name| format!("  {name};\n")).collect();
    fs::write(&list, format!("{{\n{body}}};\n")).expect("OUT_DIR is writable");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("slopos") {
        println!("cargo:rustc-link-arg=--dynamic-list={}", list.display());
    }
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("slibc/src is a directory") {
        let path = entry.expect("readable directory entry").path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}
