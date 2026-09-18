//! Parses slibc's own `#[unsafe(no_mangle)]` C exports out of `src/**`.
//!
//! This is the other half of the anti-drift story: the contract says what the
//! `libc` crate declares, and this says what slibc actually defines. A header
//! prototype whose export is missing is reported; a header prototype whose
//! export disagrees about the ABI is a hard error, because that is the failure
//! mode that miscompiles instead of failing to link.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use crate::contract;
use crate::contract::Signature;

pub struct Export {
    pub signature: Signature,
    /// `false` for an exported `static`, e.g. `environ`.
    pub is_function: bool,
    pub file: String,
}

const MARKER: &str = "#[unsafe(no_mangle)]";

/// slibc's own `pub const` items, by name: `(type, initialiser)` plus the file
/// it came from, so a name defined twice with different values is caught
/// rather than silently resolved.
pub type Constants = BTreeMap<String, Vec<(String, String, String)>>;

/// Walks `root`, parsing every `no_mangle` item and every `pub const`. Returns
/// them with the files that were read, so the build script can ask cargo to
/// rerun when any of them changes.
pub fn scan(root: &Path) -> Result<(BTreeMap<String, Export>, Constants, Vec<PathBuf>), String> {
    let mut files = Vec::new();
    collect_rust_files(root, &mut files)?;
    files.sort();

    let mut exports = BTreeMap::new();
    let mut constants: Constants = BTreeMap::new();
    for file in &files {
        let text = fs::read_to_string(file)
            .map_err(|err| format!("cannot read `{}`: {err}", file.display()))?;
        let label = file
            .strip_prefix(root.parent().unwrap_or(root))
            .unwrap_or(file)
            .display()
            .to_string();
        for (name, export) in parse_file(&text, &label)? {
            exports.entry(name).or_insert(export);
        }
        for (name, ty, expr) in parse_constants(&text) {
            constants
                .entry(name)
                .or_default()
                .push((ty, expr, label.clone()));
        }
    }
    Ok((exports, constants, files))
}

/// `pub const NAME: TYPE = EXPR;` on one line, which is how slibc writes the
/// C-visible constants (`EOF`, the `_IO*BF` buffering modes) the `libc` crate
/// has no reason to declare.
fn parse_constants(text: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("pub const ") else {
            continue;
        };
        let Some(body) = rest.strip_suffix(';') else {
            continue;
        };
        let Some((name, rest)) = body.split_once(':') else {
            continue;
        };
        let Some((ty, expr)) = rest.split_once('=') else {
            continue;
        };
        out.push((
            name.trim().to_string(),
            crate::contract::normalise(ty),
            crate::contract::normalise(expr),
        ));
    }
    out
}

fn collect_rust_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|err| format!("cannot read `{}`: {err}", dir.display()))?;
    for entry in entries {
        let path = entry
            .map_err(|err| format!("cannot walk `{}`: {err}", dir.display()))?
            .path();
        if path.is_dir() {
            collect_rust_files(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

fn parse_file(text: &str, label: &str) -> Result<Vec<(String, Export)>, String> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while let Some(found) = text[cursor..].find(MARKER) {
        let at = cursor + found + MARKER.len();
        cursor = at;
        // The item follows, possibly behind further attributes. Anything past
        // the next blank line is a different item and means this marker sits
        // on something the generator does not model.
        let window = &text[at..text.len().min(at + 600)];
        let Some(item) = window.find("pub ") else {
            continue;
        };
        let window = &window[item..];

        if let Some(rest) = strip_static(window) {
            let (name, ty) = rest
                .split_once(':')
                .ok_or_else(|| format!("{label}: exported static without a type"))?;
            let ty = ty
                .split('=')
                .next()
                .ok_or_else(|| format!("{label}: exported static without an initialiser"))?;
            out.push((
                name.trim().to_string(),
                Export {
                    signature: Signature {
                        name: name.trim().to_string(),
                        params: Vec::new(),
                        ret: contract::normalise(ty),
                        variadic: false,
                    },
                    is_function: false,
                    file: label.to_string(),
                },
            ));
            continue;
        }

        let Some(sig) = signature_text(window) else {
            continue;
        };
        let signature = contract::parse_signature(&sig).map_err(|err| format!("{label}: {err}"))?;
        out.push((
            signature.name.clone(),
            Export {
                signature,
                is_function: true,
                file: label.to_string(),
            },
        ));
    }
    Ok(out)
}

fn strip_static(window: &str) -> Option<&str> {
    window
        .strip_prefix("pub static mut ")
        .or_else(|| window.strip_prefix("pub static "))
        .map(|rest| rest.split(';').next().unwrap_or(rest))
}

/// Recovers `name(params) -> ret` from a `pub [unsafe] extern "C" fn` item.
fn signature_text(window: &str) -> Option<String> {
    let after_fn = ["pub unsafe extern \"C\" fn ", "pub extern \"C\" fn "]
        .iter()
        .find_map(|prefix| window.strip_prefix(prefix))?;
    let name_end = after_fn.find('(')?;
    let name = after_fn[..name_end].trim();
    let mut depth = 0i32;
    let mut close = None;
    for (offset, ch) in after_fn[name_end..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(name_end + offset);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close?;
    let params = &after_fn[name_end + 1..close];
    let tail = &after_fn[close + 1..];
    let body = tail.find('{')?;
    let ret = tail[..body].trim();
    Some(format!("{name}({params}) {ret}"))
}
