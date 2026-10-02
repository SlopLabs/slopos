//! `#!` dispatch per Linux `execve(2)`: an interpreter and at most one
//! argument, run with the script's path as its operand. Grants key on the
//! interpreter, never the script, so a script is as privileged as `interp script`.

use slopos_abi::Errno;
use slopos_fs::vfs::CanonPath;
use slopos_fs::vfs::canon::canonicalise_at;
use slopos_ostd::KVec;

use super::{open_executable, read_exact_at, resolve_program, trim_nul_bytes};

/// Bytes examined for a `#!` line: Linux's `BINPRM_BUF_SIZE`.
pub const SCRIPT_HEADER_MAX: usize = 256;

/// Scripts an exec may pass through; one more is `ELOOP`, as on Linux.
pub const SCRIPT_NESTING_MAX: usize = 5;

/// A program after `#!` dispatch.
pub struct ExecProgram {
    /// The file the loader maps and the grant table is keyed on.
    pub image: CanonPath,
    /// Words the interpreter chain puts before the caller's `argv[1..]`;
    /// empty for a binary.
    prefix: KVec<KVec<u8>>,
}

impl ExecProgram {
    /// Whether `path`, the caller's spelling against `cwd`, names the image
    /// itself, with no `#!` line or link between: only then are the
    /// arguments the image runs with the caller's own.
    pub fn named_directly(&self, path: &[u8], cwd: &[u8]) -> bool {
        self.prefix.is_empty()
            && canonicalise_at(trim_nul_bytes(path), cwd)
                .is_ok_and(|named| named.as_bytes() == self.image.as_bytes())
    }

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
/// runs it. Relative interpreters resolve against `cwd`, as Linux's do.
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
    // Linux reads a zero-padded 256-byte buffer and ends an unterminated line
    // one byte short of it, so only a full header can cut the interpreter.
    let full = &body[..body.len().min(SCRIPT_HEADER_MAX - 2)];
    let window = &full[..full.len().min(SCRIPT_HEADER_MAX - 3)];
    let line = match full.iter().position(|&b| b == b'\n') {
        Some(nl) => &body[..nl],
        None => {
            if header.len() >= SCRIPT_HEADER_MAX {
                let start = full
                    .iter()
                    .position(|&b| !is_blank(b))
                    .ok_or(Errno::ENOEXEC)?;
                let terminated = full[start..].iter().any(|&b| is_blank(b) || b == 0);
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
