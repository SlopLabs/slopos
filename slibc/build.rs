//! Generates `slibc/include/**` — the C headers for `libc.a` — and the
//! `slopos-abi` layout pins, from the SlopOS `libc` module's own declarations.
//!
//! The input is `toolchain/libc/*.patch`: `src/unix/slopos/mod.rs` is a pure
//! creation hunk there, so the types, constants, macros and 227 prototypes
//! parsed here are the bytes the `libc` crate compiles. Nothing is retyped,
//! which is what stops the headers from drifting: re-cutting the patch
//! re-cuts the headers, and a declaration the generator cannot translate is a
//! build error rather than a plausible guess.
//!
//! Three things a patch cannot carry live in `build/decls.rs` instead — which
//! POSIX header owns a declaration, the C body of a function-like macro, and
//! the prototypes for the C entry points slibc defines but `libc` never
//! declares (`<stdio.h>`, `<string.h>`). Those are checked rather than
//! trusted: every one is matched against slibc's real `#[unsafe(no_mangle)]`
//! export, parameter by parameter, and a *disagreement* about the ABI is a
//! hard error. A declaration with no export yet is reported instead, because
//! that one fails loudly at link time anyway.
//!
//! `SLIBC_CONTRACT_STRICT=1` turns those reports into errors, which is the
//! shape a CI gate wants once every contract symbol exists.

// A build script is its own crate root, so its modules resolve beside it
// rather than under a `build/` directory; these keep the generator's five
// files out of `slibc/`'s top level.
#[path = "build/contract.rs"]
mod contract;
#[path = "build/ctype.rs"]
mod ctype;
#[path = "build/decls.rs"]
mod decls;
#[path = "build/emit.rs"]
mod emit;
#[path = "build/exports.rs"]
mod exports;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use contract::Field;
use contract::Struct;
use ctype::Abi;
use ctype::Types;
use decls::HEADERS;

fn main() {
    if let Err(err) = generate() {
        // A build-script panic prints the message and nothing useful around
        // it; this keeps the failure to one line that names the cause.
        eprintln!("error: slibc header generation failed:\n  {err}");
        std::process::exit(1);
    }
}

fn generate() -> Result<(), String> {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR")
            .ok_or_else(|| "CARGO_MANIFEST_DIR is unset".to_string())?,
    );
    let out_dir =
        PathBuf::from(env::var_os("OUT_DIR").ok_or_else(|| "OUT_DIR is unset".to_string())?);
    let workspace_root = manifest_dir
        .parent()
        .ok_or_else(|| "slibc has no parent directory".to_string())?
        .to_path_buf();

    println!("cargo::rerun-if-env-changed=SLIBC_CONTRACT_STRICT");
    println!(
        "cargo::rerun-if-changed={}",
        workspace_root.join(contract::PATCH_DIR).display()
    );
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=build");
    println!("cargo::rerun-if-changed=src");

    let (patch, source) = contract::read_from_patch_series(&workspace_root)?;
    println!("cargo::rerun-if-changed={}", patch.display());
    let parsed = contract::parse(&source)?;

    // The contract's own declarations, plus the shared libc items it reaches
    // through `crate::`.
    let (shared_aliases, shared_structs, shared_opaques) = parse_shared(decls::SHARED_TYPES)?;
    let mut aliases = parsed.aliases;
    aliases.extend(shared_aliases);
    aliases.push(Field {
        name: "va_list".to_string(),
        ty: "*mut c_void".to_string(),
    });
    let mut structs = parsed.structs;
    structs.extend(shared_structs);
    let mut opaques = parsed.opaques;
    opaques.extend(shared_opaques);

    let mut seen = BTreeSet::new();
    for name in aliases
        .iter()
        .map(|alias| alias.name.clone())
        .chain(structs.iter().map(|item| item.name.clone()))
        .chain(opaques.iter().cloned())
    {
        if !seen.insert(name.clone()) {
            return Err(format!("type `{name}` is declared twice"));
        }
    }

    let types = Types::new(
        aliases
            .iter()
            .map(|alias| (alias.name.clone(), alias.ty.clone()))
            .collect(),
        structs
            .iter()
            .map(|item| item.name.clone())
            .filter(|name| !Types::is_typedef_shaped(name))
            .collect(),
        aliases
            .iter()
            .map(|alias| alias.name.clone())
            .chain(
                structs
                    .iter()
                    .map(|item| item.name.clone())
                    .filter(|name| Types::is_typedef_shaped(name)),
            )
            .chain(opaques.iter().cloned())
            .collect(),
        // Array lengths: the contract spells `termios.c_cc` as `[cc_t; NCCS]`,
        // so a constant that is a plain integer can also be a length.
        parsed
            .consts
            .iter()
            .filter_map(|item| {
                item.expr
                    .replace('_', "")
                    .parse::<usize>()
                    .ok()
                    .map(|value| (item.name.clone(), value))
            })
            .collect(),
    );

    let (exported, slibc_consts, _) = exports::scan(&manifest_dir.join("src"))?;

    let type_defs = emit::index_types(&aliases, &structs, &opaques, decls::VA_LIST_TYPEDEF);
    let world = emit::World {
        types,
        type_defs,
        consts: parsed
            .consts
            .iter()
            .map(|item| (item.name.as_str(), item))
            .collect(),
        slibc_consts: resolve_slibc_consts(&slibc_consts)?,
        macros: parsed
            .macros
            .iter()
            .map(|item| (item.name.as_str(), item))
            .collect(),
        macro_bodies: decls::MACRO_BODIES.iter().copied().collect(),
        functions: parsed
            .functions
            .iter()
            .map(|item| (item.name.as_str(), item))
            .collect(),
        statics: parsed
            .statics
            .iter()
            .map(|item| (item.name.as_str(), item))
            .collect(),
        source: patch
            .strip_prefix(&workspace_root)
            .unwrap_or(&patch)
            .display()
            .to_string(),
    };

    check_coverage(&world)?;
    let audit = check_abi(&world, &structs, &exported)?;

    let mut written = Vec::new();
    for spec in HEADERS {
        let path = manifest_dir.join("include").join(spec.path);
        write_if_changed(&path, &world.render(spec)?)?;
        written.push(path);
    }
    let umbrella = manifest_dir.join("include/slibc.h");
    write_if_changed(&umbrella, &emit::umbrella(&world.source))?;
    written.push(umbrella);
    prune_stale_headers(&manifest_dir.join("include"), &written)?;

    write_if_changed(
        &out_dir.join("abi_layout_pins.rs"),
        &render_pins(&structs, &world)?,
    )?;
    fs::write(out_dir.join("contract-audit.txt"), audit.report())
        .map_err(|err| format!("cannot write the contract audit: {err}"))?;

    let strict = env::var_os("SLIBC_CONTRACT_STRICT").is_some();
    if !audit.missing.is_empty() {
        let summary = format!(
            "slibc: {} declared C entry point(s) are not exported yet; see {}",
            audit.missing.len(),
            out_dir.join("contract-audit.txt").display()
        );
        if strict {
            return Err(summary);
        }
        println!("cargo::warning={summary}");
    }

    Ok(())
}

/// Parses `decls::SHARED_TYPES`.
fn parse_shared(entries: &[&str]) -> Result<(Vec<Field>, Vec<Struct>, Vec<String>), String> {
    let mut aliases = Vec::new();
    let mut structs = Vec::new();
    let mut opaques = Vec::new();
    for entry in entries {
        if let Some(name) = entry.strip_prefix("opaque ") {
            opaques.push(name.trim().to_string());
            continue;
        }
        if let Some(rest) = entry.strip_prefix("struct ") {
            let (name, body) = rest
                .split_once('{')
                .ok_or_else(|| format!("shared struct without a body: `{entry}`"))?;
            let body = body
                .trim()
                .strip_suffix('}')
                .ok_or_else(|| format!("shared struct without a closing brace: `{entry}`"))?;
            let mut fields = Vec::new();
            for field in ctype::split_top_level(body) {
                let (fname, fty) = field
                    .split_once(':')
                    .ok_or_else(|| format!("shared field without a type: `{field}`"))?;
                fields.push(Field {
                    name: fname.trim().to_string(),
                    ty: contract::normalise(fty),
                });
            }
            structs.push(Struct {
                name: name.trim().to_string(),
                fields,
            });
            continue;
        }
        let (name, ty) = entry
            .split_once(':')
            .ok_or_else(|| format!("shared type without a `:`: `{entry}`"))?;
        aliases.push(Field {
            name: name.trim().to_string(),
            ty: contract::normalise(ty),
        });
    }
    Ok((aliases, structs, opaques))
}

/// Picks out the slibc constants the headers ask for, rejecting a name that
/// slibc defines more than once with different values.
fn resolve_slibc_consts(
    found: &exports::Constants,
) -> Result<BTreeMap<String, (String, String)>, String> {
    let mut out = BTreeMap::new();
    for spec in HEADERS {
        for name in spec.slibc_consts {
            let candidates = found
                .get(*name)
                .ok_or_else(|| format!("{}: slibc defines no `pub const {name}`", spec.path))?;
            let distinct: BTreeSet<&String> = candidates.iter().map(|(_, expr, _)| expr).collect();
            if distinct.len() > 1 {
                return Err(format!(
                    "slibc defines `{name}` {} times with different values ({})",
                    candidates.len(),
                    candidates
                        .iter()
                        .map(|(_, expr, file)| format!("{expr} in {file}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            let (ty, expr, _) = &candidates[0];
            out.insert((*name).to_string(), (ty.clone(), expr.clone()));
        }
    }
    Ok(out)
}

/// Every contract declaration is owned by exactly one header, and every header
/// entry names something the contract defines. Either direction failing means
/// the table has fallen behind the fork.
fn check_coverage(world: &emit::World<'_>) -> Result<(), String> {
    let mut claimed: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for spec in HEADERS {
        for name in spec.types {
            claimed.entry(name).or_default().push(spec.path);
        }
    }
    for name in world.type_defs.keys() {
        match claimed.get(name).map(Vec::as_slice) {
            None => return Err(format!("no header defines type `{name}`")),
            Some([_]) => {}
            Some(many) => {
                return Err(format!("type `{name}` is defined by {}", many.join(", ")));
            }
        }
    }
    for (name, owners) in &claimed {
        if !world.type_defs.contains_key(*name) {
            return Err(format!(
                "{}: type `{name}` is not declared by the contract",
                owners.join(", ")
            ));
        }
    }

    for kind in ["function", "macro"] {
        let mut claimed: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for spec in HEADERS {
            let names = if kind == "function" {
                spec.functions
            } else {
                spec.macros
            };
            for name in names {
                claimed.entry(name).or_default().push(spec.path);
            }
        }
        let declared = if kind == "function" {
            &world.functions
        } else {
            &world.macros
        };
        for name in declared.keys() {
            match claimed.get(name).map(Vec::as_slice) {
                None => return Err(format!("no header declares {kind} `{name}`")),
                Some([_]) => {}
                Some(many) => {
                    return Err(format!(
                        "{kind} `{name}` is declared by {}",
                        many.join(", ")
                    ));
                }
            }
        }
        for (name, owners) in &claimed {
            if !declared.contains_key(*name) {
                return Err(format!(
                    "{}: {kind} `{name}` is not declared by the contract",
                    owners.join(", ")
                ));
            }
        }
    }

    // The contract's `extern` objects (`environ`) must be published too, or a
    // C program has no way to name them.
    for name in world.statics.keys() {
        let owners: Vec<&str> = HEADERS
            .iter()
            .filter(|spec| {
                spec.variables
                    .iter()
                    .any(|entry| entry.split(':').next().is_some_and(|it| it.trim() == *name))
            })
            .map(|spec| spec.path)
            .collect();
        match owners.as_slice() {
            [] => return Err(format!("no header declares the object `{name}`")),
            [_] => {}
            many => {
                return Err(format!(
                    "the object `{name}` is declared by {}",
                    many.join(", ")
                ));
            }
        }
    }

    for (name, _) in decls::MACRO_BODIES {
        if !world.macros.contains_key(*name) {
            return Err(format!(
                "there is a C macro body for `{name}`, but the contract has no such macro"
            ));
        }
    }

    // Constants are claimed by pattern, so the checks are "exactly one most
    // specific owner" and "no dead pattern".
    for name in world.consts.keys() {
        let mut best = 0usize;
        let mut owners: Vec<&str> = Vec::new();
        for spec in HEADERS {
            let Some(specificity) = emit::claim(spec, name) else {
                continue;
            };
            if specificity > best {
                best = specificity;
                owners.clear();
            }
            if specificity == best {
                owners.push(spec.path);
            }
        }
        match owners.as_slice() {
            [] => return Err(format!("no header defines constant `{name}`")),
            [_] => {}
            many => {
                return Err(format!(
                    "constant `{name}` is claimed equally specifically by {}",
                    many.join(", ")
                ));
            }
        }
    }
    for spec in HEADERS {
        for pattern in spec.consts {
            let matches = world
                .consts
                .keys()
                .any(|name| emit::claim(spec, name).is_some() && matches_pattern(pattern, name));
            if !matches {
                return Err(format!(
                    "{}: constant pattern `{pattern}` matches nothing in the contract",
                    spec.path
                ));
            }
        }
    }

    Ok(())
}

fn matches_pattern(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

#[derive(Default)]
struct Audit {
    missing: Vec<String>,
    unchecked: Vec<String>,
    unexposed: Vec<String>,
}

impl Audit {
    fn report(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "slibc contract audit — generated by slibc/build.rs\n\
             \n\
             Declared in slibc/include but not exported by slibc/src ({}):",
            self.missing.len()
        );
        for entry in &self.missing {
            let _ = writeln!(out, "  {entry}");
        }
        let _ = writeln!(
            out,
            "\nDeclared and exported, ABI unverifiable because the export uses a \
             slibc-internal Rust type ({}):",
            self.unchecked.len()
        );
        for entry in &self.unchecked {
            let _ = writeln!(out, "  {entry}");
        }
        let _ = writeln!(
            out,
            "\nExported by slibc/src but not declared in any header ({}) — SlopOS-private \
             entry points and internal helpers belong here:",
            self.unexposed.len()
        );
        for entry in &self.unexposed {
            let _ = writeln!(out, "  {entry}");
        }
        out
    }
}

/// Compares every declaration against the real export. An ABI *disagreement*
/// is returned as an error, because that is the failure that miscompiles; a
/// missing export is recorded instead.
fn check_abi(
    world: &emit::World<'_>,
    structs: &[Struct],
    exported: &BTreeMap<String, exports::Export>,
) -> Result<Audit, String> {
    // A one-scalar-field struct of at most eight bytes is passed exactly as
    // that scalar in the System V ABI, which is why `inet_ntoa(struct in_addr)`
    // and slibc's `inet_ntoa(u32)` are the same function.
    let mut scalar_aggregates: BTreeMap<&str, u8> = BTreeMap::new();
    for item in structs {
        if let [only] = item.fields.as_slice() {
            if let Abi::Int { bytes, .. } = world.types.abi(&only.ty) {
                scalar_aggregates.insert(item.name.as_str(), bytes);
            }
        }
    }

    let mut audit = Audit::default();
    let mut declared: BTreeSet<String> = BTreeSet::new();
    let mut mismatched: Vec<String> = Vec::new();

    for spec in HEADERS {
        let mut signatures: Vec<(contract::Signature, bool)> = Vec::new();
        for name in spec.functions {
            let signature = world.functions[name];
            signatures.push((
                contract::Signature {
                    name: signature.name.clone(),
                    params: signature
                        .params
                        .iter()
                        .map(|param| Field {
                            name: param.name.clone(),
                            ty: param.ty.clone(),
                        })
                        .collect(),
                    ret: signature.ret.clone(),
                    variadic: signature.variadic,
                },
                true,
            ));
        }
        for declaration in spec.extra {
            signatures.push((contract::parse_signature(declaration)?, true));
        }
        for variable in spec.variables {
            let (name, ty) = world.variable(variable)?;
            signatures.push((
                contract::Signature {
                    name: name.to_string(),
                    params: Vec::new(),
                    ret: ty,
                    variadic: false,
                },
                false,
            ));
        }

        for (signature, is_function) in signatures {
            declared.insert(signature.name.clone());
            let Some(export) = exported.get(&signature.name) else {
                audit
                    .missing
                    .push(format!("{} ({})", signature.name, spec.path));
                continue;
            };
            if export.is_function != is_function {
                mismatched.push(format!(
                    "{}: declared as {}, exported as {} in {}",
                    signature.name,
                    if is_function {
                        "a function"
                    } else {
                        "an object"
                    },
                    if export.is_function {
                        "a function"
                    } else {
                        "an object"
                    },
                    export.file
                ));
                continue;
            }
            match compare(world, &scalar_aggregates, &signature, &export.signature) {
                Ok(true) => {}
                Ok(false) => audit
                    .unchecked
                    .push(format!("{} ({})", signature.name, export.file)),
                Err(reason) => mismatched.push(format!("{}: {reason}", signature.name)),
            }
        }
    }

    for (name, export) in exported {
        if !declared.contains(name) {
            audit.unexposed.push(format!("{name} ({})", export.file));
        }
    }

    if !mismatched.is_empty() {
        return Err(format!(
            "slibc's exports disagree with the C declarations generated for them. A header \
             that lies about the ABI miscompiles instead of failing to link, so this is \
             fatal:\n  {}",
            mismatched.join("\n  ")
        ));
    }

    Ok(audit)
}

/// `Ok(true)` when every position was verified, `Ok(false)` when some position
/// used a Rust type this generator cannot classify, `Err` on a real
/// disagreement.
fn compare(
    world: &emit::World<'_>,
    scalar_aggregates: &BTreeMap<&str, u8>,
    declared: &contract::Signature,
    exported: &contract::Signature,
) -> Result<bool, String> {
    if declared.params.len() != exported.params.len() {
        return Err(format!(
            "declared with {} parameter(s), exported with {}",
            declared.params.len(),
            exported.params.len()
        ));
    }
    if declared.variadic != exported.variadic {
        return Err(format!(
            "declared {} a `...` tail, exported {} one",
            if declared.variadic { "with" } else { "without" },
            if exported.variadic { "with" } else { "without" }
        ));
    }
    let mut verified = true;
    for (position, (left, right)) in declared
        .params
        .iter()
        .zip(exported.params.iter())
        .chain(std::iter::once((
            &Field {
                name: "return".to_string(),
                ty: declared.ret.clone(),
            },
            &Field {
                name: "return".to_string(),
                ty: exported.ret.clone(),
            },
        )))
        .enumerate()
    {
        let ours = world.types.abi(&left.ty);
        let theirs = world.types.abi(&right.ty);
        if matches!(theirs, Abi::Opaque(_)) {
            verified = false;
            continue;
        }
        if !compatible(&ours, &theirs, scalar_aggregates) {
            return Err(format!(
                "parameter {position} (`{}`) is declared `{}` ({}) but exported `{}` ({})",
                left.name,
                left.ty,
                ours.describe(),
                right.ty,
                theirs.describe()
            ));
        }
    }
    Ok(verified)
}

fn compatible(ours: &Abi, theirs: &Abi, scalar_aggregates: &BTreeMap<&str, u8>) -> bool {
    if ours == theirs {
        return true;
    }
    match (ours, theirs) {
        // `-> !` and `-> ()` differ only in whether the declaration promises
        // not to return; both are `void` to the linker.
        (Abi::Never, Abi::Void) | (Abi::Void, Abi::Never) => true,
        (Abi::Int { bytes: a, .. }, Abi::Int { bytes: b, .. }) => a == b,
        (Abi::Aggregate(name), Abi::Int { bytes, .. })
        | (Abi::Int { bytes, .. }, Abi::Aggregate(name)) => {
            scalar_aggregates.get(name.as_str()) == Some(bytes)
        }
        _ => false,
    }
}

/// The `const _: () = assert!` pins: the shared libc layouts this generator
/// hard-codes, against `slopos-abi`'s definition of the same struct.
fn render_pins(structs: &[Struct], world: &emit::World<'_>) -> Result<String, String> {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "// Generated by slibc/build.rs. The C headers in slibc/include hard-code the shared\n\
         // libc layouts (upstream `libc`'s `src/unix/mod.rs`, which the SlopOS patch does not\n\
         // touch). Where `slopos-abi` defines the same struct, these assertions make a\n\
         // divergence a compile error instead of a wrong header."
    );
    for (name, abi_path) in decls::ABI_PINS {
        let item = structs
            .iter()
            .find(|item| item.name == *name)
            .ok_or_else(|| format!("pinned struct `{name}` is not declared"))?;
        let layout = emit::layout(item, &world.types)?;
        let _ = writeln!(
            out,
            "const _: () = assert!(\n    \
             ::core::mem::size_of::<{abi_path}>() == {},\n    \
             \"{abi_path} must match the C `struct {name}` in slibc/include\",\n);",
            layout.size
        );
        for (field, offset) in &layout.offsets {
            let _ = writeln!(
                out,
                "const _: () = assert!(\n    \
                 ::core::mem::offset_of!({abi_path}, {field}) == {offset},\n    \
                 \"{abi_path}.{field} must match the C `struct {name}` in slibc/include\",\n);"
            );
        }
    }
    Ok(out)
}

/// Writes only when the content changed: the headers are checked in, and a
/// rewritten-but-identical file would churn mtimes on every build.
fn write_if_changed(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("cannot create `{}`: {err}", parent.display()))?;
    }
    if fs::read_to_string(path).is_ok_and(|existing| existing == content) {
        return Ok(());
    }
    fs::write(path, content).map_err(|err| format!("cannot write `{}`: {err}", path.display()))
}

/// Deletes headers the table no longer generates. A stale `.h` left behind
/// after a rename is still on the include path, which is exactly the drift
/// this generator exists to prevent.
fn prune_stale_headers(root: &Path, expected: &[PathBuf]) -> Result<(), String> {
    if !root.is_dir() {
        return Ok(());
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries =
            fs::read_dir(&dir).map_err(|err| format!("cannot read `{}`: {err}", dir.display()))?;
        for entry in entries {
            let path = entry
                .map_err(|err| format!("cannot walk `{}`: {err}", dir.display()))?
                .path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "h") && !expected.contains(&path) {
                fs::remove_file(&path)
                    .map_err(|err| format!("cannot remove `{}`: {err}", path.display()))?;
            }
        }
    }
    Ok(())
}
