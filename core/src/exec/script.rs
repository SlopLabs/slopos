//! `#!` dispatch, with the interpreter-script semantics Linux documents for
//! `execve(2)`: the first line names an interpreter and at most one argument,
//! and the interpreter runs with the script's path as its operand.
//!
//! Authority follows the file actually loaded: the grant table is consulted
//! for the interpreter this resolves to, never for the script, so running a
//! script is exactly as privileged as running `interp script`.

use slopos_abi::Errno;
use slopos_fs::vfs::CanonPath;
use slopos_ostd::KVec;

use super::{open_executable, read_exact_at, resolve_program, trim_nul_bytes};

/// Bytes of a file examined for a `#!` line, the interpreter-script header
/// size `execve(2)` documents.
pub const SCRIPT_HEADER_MAX: usize = 256;

/// Scripts an exec may pass through before the interpreter must be a binary;
/// one more is `ELOOP`, as `execve(2)` documents for Linux.
pub const SCRIPT_NESTING_MAX: usize = 5;

/// A program after `#!` dispatch.
pub struct ExecProgram {
    /// The file the loader maps and the grant table is keyed on.
    pub image: CanonPath,
    /// What the interpreter chain puts in front of the caller's `argv[1..]`;
    /// empty when `image` is the file the caller named.
    prefix: KVec<KVec<u8>>,
}

impl ExecProgram {
    /// The argument vector the loaded image receives: the caller's, or for a
    /// script `[interpreter, optional argument, script path, argv[1..]]`.
    pub fn argv<'a>(&'a self, argv: Option<&[&'a [u8]]>) -> Result<Option<KVec<&'a [u8]>>, Errno> {
        if self.prefix.is_empty() {
            return match argv {
                Some(argv) => KVec::from_iter_fallible(argv.iter().copied())
                    .map(Some)
                    .map_err(|_| Errno::ENOMEM),
                None => Ok(None),
            };
        }
        let rest = argv.map_or(&[][..], |argv| argv.get(1..).unwrap_or(&[]));
        KVec::from_iter_fallible(
            self.prefix
                .iter()
                .map(|word| word.as_slice())
                .chain(rest.iter().copied()),
        )
        .map(Some)
        .map_err(|_| Errno::ENOMEM)
    }
}

/// Resolve `path` against `cwd` and follow its `#!` chain to the binary that
/// runs it. Interpreters named relatively resolve against `cwd`, as `execve`
/// opens them.
#[inline(never)]
pub fn resolve_exec(path: &[u8], cwd: &[u8]) -> Result<ExecProgram, Errno> {
    let mut image = resolve_program(path, cwd)?;
    let mut prefix: KVec<KVec<u8>> = KVec::new();
    for _ in 0..=SCRIPT_NESTING_MAX {
        let header = read_header(&image)?;
        let Some(line) = parse_shebang(header.as_slice())? else {
            return Ok(ExecProgram { image, prefix });
        };
        let passed = match prefix.first() {
            Some(outer) => owned(outer.as_slice())?,
            None => owned(trim_nul_bytes(path))?,
        };
        let mut next = KVec::with_capacity(prefix.len() + 3).map_err(|_| Errno::ENOMEM)?;
        push(&mut next, owned(line.interpreter)?)?;
        if let Some(arg) = line.argument {
            push(&mut next, owned(arg)?)?;
        }
        push(&mut next, passed)?;
        for word in prefix.into_iter().skip(1) {
            push(&mut next, word)?;
        }
        prefix = next;
        image = resolve_program(line.interpreter, cwd)?;
    }
    Err(Errno::ELOOP)
}

fn owned(bytes: &[u8]) -> Result<KVec<u8>, Errno> {
    KVec::from_iter_fallible(bytes.iter().copied()).map_err(|_| Errno::ENOMEM)
}

fn push(list: &mut KVec<KVec<u8>>, word: KVec<u8>) -> Result<(), Errno> {
    list.push(word).map_err(|_| Errno::ENOMEM)
}

/// The script must be executable and non-empty exactly as a binary must.
fn read_header(image: &CanonPath) -> Result<KVec<u8>, Errno> {
    let (handle, size) = open_executable(image.as_bytes())?;
    let len = (size as usize).min(SCRIPT_HEADER_MAX);
    let mut header = KVec::<u8>::zeroed(len).map_err(|_| Errno::ENOMEM)?;
    read_exact_at(&handle, 0, header.as_mut_slice())?;
    Ok(header)
}

/// An interpreter line: the interpreter as written and the rest of the line,
/// unsplit, as its one optional argument.
#[derive(Debug, PartialEq, Eq)]
pub struct ShebangLine<'a> {
    pub interpreter: &'a [u8],
    pub argument: Option<&'a [u8]>,
}

fn is_blank(b: u8) -> bool {
    b == b' ' || b == b'\t'
}

/// `None` when `header` does not start with `#!`. `ENOEXEC` when the line
/// names no interpreter, or when no newline falls inside the header and the
/// interpreter runs to its end, since that name may have been cut.
pub fn parse_shebang(header: &[u8]) -> Result<Option<ShebangLine<'_>>, Errno> {
    let Some(body) = header.strip_prefix(b"#!") else {
        return Ok(None);
    };
    // The last header byte is the terminator a C string of the header keeps,
    // so a line without a newline ends one short of it.
    let window = &body[..body.len().min(SCRIPT_HEADER_MAX - 3)];
    let line = match body[..body.len().min(SCRIPT_HEADER_MAX - 2)]
        .iter()
        .position(|&b| b == b'\n')
    {
        Some(nl) => &body[..nl],
        None => {
            if header.len() >= SCRIPT_HEADER_MAX - 1 {
                let start = window
                    .iter()
                    .position(|&b| !is_blank(b))
                    .ok_or(Errno::ENOEXEC)?;
                let terminated = window[start..].iter().any(|&b| is_blank(b) || b == 0);
                if !terminated {
                    return Err(Errno::ENOEXEC);
                }
            }
            window
        }
    };
    let line = trim_nul_bytes(line);
    let end = line
        .iter()
        .rposition(|&b| !is_blank(b))
        .map_or(0, |i| i + 1);
    let line = &line[..end];
    let start = line
        .iter()
        .position(|&b| !is_blank(b))
        .ok_or(Errno::ENOEXEC)?;
    let line = &line[start..];
    let name_end = line.iter().position(|&b| is_blank(b)).unwrap_or(line.len());
    let (interpreter, rest) = line.split_at(name_end);
    let argument = rest.iter().position(|&b| !is_blank(b)).map(|i| &rest[i..]);
    Ok(Some(ShebangLine {
        interpreter,
        argument,
    }))
}
