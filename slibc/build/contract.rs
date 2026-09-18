//! Reads the libc contract out of the toolchain fork's patch series and parses
//! the declarations the headers are generated from.
//!
//! The input is deliberately the patch and nothing else: `src/unix/slopos/mod.rs`
//! is a pure creation hunk in `toolchain/libc/*.patch`, so the bytes parsed
//! here are byte-for-byte the bytes the `libc` crate compiles. There is no
//! vendored second copy that could drift, and re-cutting the patch re-cuts the
//! headers.

use std::fs;
use std::path::Path;
use std::path::PathBuf;

/// The module path whose creation hunk carries the contract.
pub const CONTRACT_MODULE: &str = "src/unix/slopos/mod.rs";
/// Where the patch series lives, relative to the workspace root.
pub const PATCH_DIR: &str = "toolchain/libc";

pub struct Struct {
    pub name: String,
    pub fields: Vec<Field>,
}

pub struct Field {
    pub name: String,
    pub ty: String,
}

pub struct Const {
    pub name: String,
    pub ty: String,
    pub expr: String,
}

pub struct Signature {
    pub name: String,
    pub params: Vec<Field>,
    pub ret: String,
    pub variadic: bool,
}

#[derive(Default)]
pub struct Contract {
    pub aliases: Vec<Field>,
    pub opaques: Vec<String>,
    pub structs: Vec<Struct>,
    pub consts: Vec<Const>,
    pub macros: Vec<Signature>,
    pub functions: Vec<Signature>,
    /// `pub static mut NAME: TY;` in the `extern "C"` block, e.g. `environ`.
    pub statics: Vec<Field>,
}

/// Locates the patch series and extracts the contract module's creation hunk.
/// Every failure names the patch path, because a missing or reshaped hunk is
/// exactly the drift this generator exists to refuse.
pub fn read_from_patch_series(workspace_root: &Path) -> Result<(PathBuf, String), String> {
    let dir = workspace_root.join(PATCH_DIR);
    let mut patches: Vec<PathBuf> = fs::read_dir(&dir)
        .map_err(|err| {
            format!(
                "cannot read the libc patch series at `{}`: {err}. The generated headers in \
                 slibc/include are produced from the `{CONTRACT_MODULE}` creation hunk of a \
                 patch in that directory; slibc cannot be built without it.",
                dir.display()
            )
        })?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "patch"))
        .collect();
    patches.sort();

    if patches.is_empty() {
        return Err(format!(
            "no `*.patch` in `{}`: nothing to generate slibc/include from",
            dir.display()
        ));
    }

    for patch in &patches {
        let text = fs::read_to_string(patch)
            .map_err(|err| format!("cannot read `{}`: {err}", patch.display()))?;
        if let Some(source) = creation_hunk(&text, CONTRACT_MODULE)? {
            return Ok((patch.clone(), source));
        }
    }

    Err(format!(
        "none of the {} patch(es) in `{}` creates `{CONTRACT_MODULE}`",
        patches.len(),
        dir.display()
    ))
}

/// Extracts the added text of a `--- /dev/null` / `+++ b/<module>` hunk.
/// A hunk that is not a pure creation (any context or removal line) is an
/// error rather than a partial answer.
fn creation_hunk(patch: &str, module: &str) -> Result<Option<String>, String> {
    let mut lines = patch.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(path) = line.strip_prefix("+++ ") else {
            continue;
        };
        let path = path.trim();
        let path = path.strip_prefix("b/").unwrap_or(path);
        if path != module {
            continue;
        }
        let mut source = String::new();
        let mut hunks = 0usize;
        for line in lines.by_ref() {
            if line.starts_with("@@") {
                hunks += 1;
                continue;
            }
            if line.starts_with("diff ") || line.starts_with("--- ") || line == "-- " {
                break;
            }
            if line.starts_with("\\ No newline") {
                continue;
            }
            match line.strip_prefix('+') {
                Some(body) => {
                    source.push_str(body);
                    source.push('\n');
                }
                None if line.is_empty() => break,
                None => {
                    return Err(format!(
                        "`{module}` is not a pure creation hunk: found `{line}`. The header \
                         generator reads added lines only."
                    ));
                }
            }
        }
        if hunks != 1 {
            return Err(format!(
                "expected exactly one hunk creating `{module}`, found {hunks}"
            ));
        }
        return Ok(Some(source));
    }
    Ok(None)
}

/// Parses the contract module. Only the item shapes the contract actually uses
/// are accepted; anything else is reported with its line number so a change in
/// the fork surfaces as a build error naming the line.
pub fn parse(source: &str) -> Result<Contract, String> {
    let lines: Vec<&str> = source.lines().collect();
    let mut out = Contract::default();
    let mut index = 0usize;

    while index < lines.len() {
        let line = lines[index].trim();
        index += 1;

        if line.is_empty()
            || line.starts_with("//")
            || line.starts_with("#[")
            || line.starts_with("#![")
            || line.starts_with("use ")
        {
            continue;
        }

        if let Some(rest) = line.strip_prefix("pub type ") {
            let body = rest
                .strip_suffix(';')
                .ok_or_else(|| format!("line {index}: type alias without `;`"))?;
            let (name, ty) = body
                .split_once('=')
                .ok_or_else(|| format!("line {index}: type alias without `=`"))?;
            out.aliases.push(Field {
                name: name.trim().to_string(),
                ty: normalise(ty),
            });
            continue;
        }

        if line.starts_with("pub const ") {
            let mut text = line.to_string();
            while !text.ends_with(';') {
                text.push(' ');
                text.push_str(
                    lines
                        .get(index)
                        .ok_or_else(|| "unterminated `pub const`".to_string())?
                        .trim(),
                );
                index += 1;
            }
            out.consts.push(parse_const(&text)?);
            continue;
        }

        if line == "extern_ty! {" {
            while index < lines.len() {
                let inner = lines[index].trim();
                index += 1;
                if inner == "}" {
                    break;
                }
                if let Some(name) = inner
                    .strip_prefix("pub type ")
                    .and_then(|rest| rest.strip_suffix(';'))
                {
                    out.opaques.push(name.trim().to_string());
                }
            }
            continue;
        }

        if line == "s! {" || line == "s_no_extra_traits! {" {
            index = parse_struct_block(&lines, index, &mut out.structs)?;
            continue;
        }

        if line == "f! {" || line == "safe_f! {" {
            index = parse_macro_block(&lines, index, &mut out.macros)?;
            continue;
        }

        if line == "extern \"C\" {" {
            index = parse_extern_block(&lines, index, &mut out.functions, &mut out.statics)?;
            continue;
        }

        // Everything else is contract-internal: the private `siginfo_t`
        // payload structs, the `union sifields`, the accessor `impl`, and the
        // private `ULONG_BITS`. None of it is part of the C surface.
        if line.starts_with("impl ")
            || line.starts_with("struct ")
            || line.starts_with("union ")
            || line.starts_with("const ")
        {
            if line.ends_with('{') {
                index = skip_braced(&lines, index)?;
            }
            continue;
        }

        return Err(format!("line {index}: unrecognised contract item `{line}`"));
    }

    Ok(out)
}

fn parse_const(text: &str) -> Result<Const, String> {
    let body = text
        .strip_prefix("pub const ")
        .and_then(|rest| rest.strip_suffix(';'))
        .ok_or_else(|| format!("malformed constant `{text}`"))?;
    let (name, rest) = body
        .split_once(':')
        .ok_or_else(|| format!("constant without a type: `{text}`"))?;
    let (ty, expr) = rest
        .split_once('=')
        .ok_or_else(|| format!("constant without an initialiser: `{text}`"))?;
    Ok(Const {
        name: name.trim().to_string(),
        ty: normalise(ty),
        expr: normalise(expr),
    })
}

fn parse_struct_block(
    lines: &[&str],
    mut index: usize,
    out: &mut Vec<Struct>,
) -> Result<usize, String> {
    while index < lines.len() {
        let line = lines[index].trim();
        index += 1;
        if line == "}" {
            return Ok(index);
        }
        if line.is_empty() || line.starts_with("//") || line.starts_with("#[") {
            continue;
        }
        let Some(name) = line
            .strip_prefix("pub struct ")
            .and_then(|rest| rest.strip_suffix('{'))
        else {
            return Err(format!("line {index}: expected a struct, found `{line}`"));
        };
        let mut fields = Vec::new();
        while index < lines.len() {
            let field = lines[index].trim();
            index += 1;
            if field == "}" {
                break;
            }
            if field.is_empty() || field.starts_with("//") || field.starts_with("#[") {
                continue;
            }
            let field = field
                .strip_suffix(',')
                .ok_or_else(|| format!("line {index}: struct field without `,`: `{field}`"))?;
            let field = field.strip_prefix("pub ").unwrap_or(field);
            let (fname, fty) = field
                .split_once(':')
                .ok_or_else(|| format!("line {index}: field without a type: `{field}`"))?;
            fields.push(Field {
                name: fname.trim().to_string(),
                ty: normalise(fty),
            });
        }
        out.push(Struct {
            name: name.trim().to_string(),
            fields,
        });
    }
    Err("unterminated struct block".to_string())
}

fn parse_macro_block(
    lines: &[&str],
    mut index: usize,
    out: &mut Vec<Signature>,
) -> Result<usize, String> {
    while index < lines.len() {
        let line = lines[index].trim();
        index += 1;
        if line == "}" {
            return Ok(index);
        }
        if line.is_empty() || line.starts_with("//") || line.starts_with("#[") {
            continue;
        }
        let signature = line
            .strip_suffix('{')
            .ok_or_else(|| format!("line {index}: expected a macro body, found `{line}`"))?;
        let signature = [
            "pub const unsafe fn",
            "pub const safe fn",
            "pub unsafe fn",
            "pub safe fn",
            "pub fn",
        ]
        .iter()
        .find_map(|prefix| signature.trim().strip_prefix(prefix))
        .ok_or_else(|| format!("line {index}: unrecognised macro signature `{line}`"))?;
        out.push(parse_signature(signature.trim())?);
        index = skip_braced(lines, index)?;
    }
    Err("unterminated macro block".to_string())
}

fn parse_extern_block(
    lines: &[&str],
    mut index: usize,
    functions: &mut Vec<Signature>,
    statics: &mut Vec<Field>,
) -> Result<usize, String> {
    let mut pending = String::new();
    while index < lines.len() {
        let line = lines[index].trim();
        index += 1;
        if pending.is_empty() && line == "}" {
            return Ok(index);
        }
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        if !pending.is_empty() {
            pending.push(' ');
        }
        pending.push_str(line);
        if !pending.ends_with(';') {
            continue;
        }
        let declaration = pending.trim_end_matches(';').to_string();
        pending.clear();
        if let Some(object) = declaration
            .strip_prefix("pub static mut ")
            .or_else(|| declaration.strip_prefix("pub static "))
        {
            let (name, ty) = object.split_once(':').ok_or_else(|| {
                format!("line {index}: extern static without a type: `{declaration}`")
            })?;
            statics.push(Field {
                name: name.trim().to_string(),
                ty: normalise(ty),
            });
            continue;
        }
        let signature = declaration
            .strip_prefix("pub fn ")
            .ok_or_else(|| format!("line {index}: unrecognised extern item `{declaration}`"))?;
        functions.push(parse_signature(signature)?);
    }
    Err("unterminated `extern \"C\"` block".to_string())
}

/// Parses `name(a: A, b: B, ...) -> R`, the one signature shape the contract,
/// the declaration table and slibc's own exports all share.
pub fn parse_signature(signature: &str) -> Result<Signature, String> {
    let signature = signature.trim();
    let open = signature
        .find('(')
        .ok_or_else(|| format!("signature without `(`: `{signature}`"))?;
    let close = matching_paren(signature, open)
        .ok_or_else(|| format!("signature without `)`: `{signature}`"))?;
    let name = signature[..open].trim().to_string();
    let mut params = Vec::new();
    let mut variadic = false;
    for param in crate::ctype::split_top_level(&signature[open + 1..close]) {
        if param == "..." {
            variadic = true;
            continue;
        }
        let (pname, pty) = param
            .split_once(':')
            .ok_or_else(|| format!("parameter without a type in `{signature}`"))?;
        let pname = pname.trim().trim_start_matches("mut ").trim();
        let pty = normalise(pty);
        // `mut args: ...` is how a Rust definition spells a C variadic tail.
        if pty == "..." {
            variadic = true;
            continue;
        }
        params.push(Field {
            name: pname.to_string(),
            ty: pty,
        });
    }
    let ret = signature[close + 1..]
        .trim()
        .strip_prefix("->")
        .map(normalise)
        .unwrap_or_else(|| "()".to_string());
    Ok(Signature {
        name,
        params,
        ret,
        variadic,
    })
}

fn matching_paren(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (offset, ch) in text[open..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

/// Skips a brace-balanced body whose opening brace was on the previous line.
fn skip_braced(lines: &[&str], mut index: usize) -> Result<usize, String> {
    let mut depth = 1i32;
    while index < lines.len() {
        for ch in lines[index].chars() {
            match ch {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
        }
        index += 1;
        if depth == 0 {
            return Ok(index);
        }
    }
    Err("unterminated block".to_string())
}

/// Collapses a Rust type to one canonical spelling: single spaces, no
/// trailing comma, no `'_` lifetime noise.
pub fn normalise(ty: &str) -> String {
    let ty = ty.replace("<'_>", "").replace('\n', " ");
    let mut out = String::with_capacity(ty.len());
    for token in ty.split_whitespace() {
        if !out.is_empty() && !out.ends_with('*') && !token.starts_with(',') {
            out.push(' ');
        }
        out.push_str(token);
    }
    out.trim().trim_end_matches(',').to_string()
}
