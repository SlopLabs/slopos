//! `/bin/priv` — run a command holding power over the machine's settings, as
//! `run0` does: privd's `org.slopos.priv` starts this program again as the
//! person who asked, in their groups and session, on a terminal of its own and
//! in their directory, with nothing else of theirs but their name and home,
//! holding [`REQUESTER_CEILING`]. That copy runs the command holding it too,
//! with an environment of its own, and ends as it ends.
//!
//! ```text
//! priv [--] <command> [argument...]
//! priv -s
//! ```
//!
//! The command is a program in one of [`DEFAULT_PATH`]'s directories that only
//! uid 0 can change, named or found there, so the program found is nothing the
//! person's own processes could have put or changed. `-s`, or the shell named
//! with nothing after it, runs the shell, which hands the power on to what it
//! runs, as an action of its own whose answer is never kept, since the shell
//! runs whatever its input says.
//!
//! [`REQUESTER_CEILING`]: slopos_priv_core::REQUESTER_CEILING

use std::ffi::{CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use slopos_abi::fs::DEFAULT_PATH;
use slopos_abi::syscall::termios::{POSIX_VDISABLE, VSUSP};
use slopos_priv_core::{ENV_ACTION, MECHANISM_LANG};

use crate::apps::shell::args::{Invocation, Source};
use crate::apps::shell::env::account;
use crate::held::{self, PRIV_ENV};
use crate::privd::{self, Input};
use crate::root_layout;
use crate::syscall::{SyscallError, core as sys_core, fs};

pub const ACTION: &str = "org.slopos.priv";
pub const SHELL_ACTION: &str = "org.slopos.priv.shell";
const SHELL: &str = "/bin/shell";
const PRIV: &str = "/bin/priv";
/// The most links a program's name may lead through, as Linux's `MAXSYMLINKS`.
const MAX_LINKS: usize = 40;
/// The most `#!` interpreters a script may lead through, as Linux's
/// `BINPRM_MAX_RECURSION`.
const MAX_INTERPRETERS: usize = 4;
const USAGE: &str = "usage: priv [--] <command> [argument...]\n       priv -s";
const STATUS_USAGE: i32 = 2;
const STATUS_CANNOT_RUN: i32 = 126;
const STATUS_NOT_FOUND: i32 = 127;

/// What to run: the shell, or a program found where only uid 0 can change
/// it, with its arguments.
enum Command {
    Shell,
    Program(Found, Vec<Vec<u8>>),
}

/// A program as found by the name or path given, and the file that leads to,
/// which is what runs.
struct Found {
    path: PathBuf,
    target: PathBuf,
}

impl Command {
    fn action(&self) -> &'static str {
        match self {
            Command::Shell => SHELL_ACTION,
            Command::Program(..) => ACTION,
        }
    }

    /// As privd is sent it, so a kept answer covers the program found, and
    /// the copy privd starts finds it again.
    fn argv(&self) -> Vec<Vec<u8>> {
        match self {
            Command::Shell => vec![b"-s".to_vec()],
            Command::Program(found, args) => {
                std::iter::once(found.path.as_os_str().as_bytes().to_vec())
                    .chain(args.iter().cloned())
                    .collect()
            }
        }
    }
}

pub fn priv_main() -> ! {
    let command = match parse(&privd::own_args()) {
        Ok(command) => command,
        Err((status, why)) => {
            eprintln!("priv: {why}");
            sys_core::exit_with_code(status);
        }
    };
    privd::elevate(Some(command.action()), &command.argv(), Input::Terminal);
    if std::env::var_os(ENV_ACTION).is_some() && fs::isatty(0) && fs::tcgetsid(0).is_err() {
        crate::secret::claim_terminal();
        if matches!(command, Command::Program(..)) {
            no_suspend();
        }
    }
    let (path, argv) = match &command {
        Command::Shell => (PathBuf::from(SHELL), vec![b"shell".to_vec()]),
        Command::Program(found, args) => {
            let name = found
                .path
                .file_name()
                .map_or(b"".as_slice(), OsStr::as_bytes);
            let argv = std::iter::once(name.to_vec())
                .chain(args.iter().cloned())
                .collect();
            (found.target.clone(), argv)
        }
    };
    let rc = run(&path, &argv);
    let shown = match &command {
        Command::Shell => PathBuf::from(SHELL),
        Command::Program(found, _) => found.path.clone(),
    };
    eprintln!(
        "priv: {}: {}",
        shown.display(),
        SyscallError::from_errno((-rc) as i32)
    );
    sys_core::exit_with_code(STATUS_CANNOT_RUN);
}

fn parse(args: &[Vec<u8>]) -> Result<Command, (i32, String)> {
    let usage = || (STATUS_USAGE, USAGE.to_owned());
    let (name, rest) = match args {
        [flag] if flag == b"-s" => return Ok(Command::Shell),
        [dashes, name, rest @ ..] if dashes == b"--" => (name, rest),
        [name, rest @ ..] if !name.starts_with(b"-") => (name, rest),
        _ => return Err(usage()),
    };
    let found = find(name).map_err(|why| (STATUS_NOT_FOUND, why))?;
    match found.target.as_os_str().as_bytes() {
        target if target == SHELL.as_bytes() && rest.is_empty() => Ok(Command::Shell),
        target if target == SHELL.as_bytes() && reads_its_input(rest) => Err((
            STATUS_USAGE,
            "a shell that reads what it runs is priv -s".into(),
        )),
        target if target == PRIV.as_bytes() => Err((
            STATUS_USAGE,
            "priv runs no priv: ask for what it would run".into(),
        )),
        _ => Ok(Command::Program(found, rest.to_vec())),
    }
}

/// Whether the shell given `args` reads its commands from its input, or
/// cannot be told not to.
fn reads_its_input(args: &[Vec<u8>]) -> bool {
    let Ok(args) = args
        .iter()
        .map(|arg| std::str::from_utf8(arg))
        .collect::<Result<Vec<&str>, _>>()
    else {
        return true;
    };
    let argv: Vec<&str> = std::iter::once("shell").chain(args).collect();
    !matches!(
        crate::apps::shell::args::parse(&argv),
        Ok(Invocation {
            source: Source::CommandString(_) | Source::File(_),
            rescue: false,
            ..
        })
    )
}

/// The program `name` names: by its path into one of the directories
/// [`trusted_dirs`] gives, or else the first of that name in one.
fn find(name: &[u8]) -> Result<Found, String> {
    let shown = String::from_utf8_lossy(name);
    if name.is_empty() {
        return Err("an empty name is no command".into());
    }
    let dirs = trusted_dirs();
    let listed: Vec<String> = dirs.iter().map(|d| d.display().to_string()).collect();
    let listed = listed.join(", ");
    let refusal =
        || format!("{shown}: priv runs only a program in {listed}, which only uid 0 can change");
    if name.contains(&b'/') {
        let path =
            std::path::absolute(OsStr::from_bytes(name)).map_err(|e| format!("{shown}: {e}"))?;
        let listed = path
            .parent()
            .is_some_and(|dir| dirs.iter().any(|d| d == dir));
        return listed
            .then(|| trusted_program(path))
            .flatten()
            .ok_or_else(refusal);
    }
    let named: Vec<PathBuf> = dirs
        .iter()
        .map(|dir| dir.join(OsStr::from_bytes(name)))
        .filter(|path| std::fs::symlink_metadata(path).is_ok())
        .collect();
    if named.is_empty() {
        return Err(format!("{shown}: no program of that name in {listed}"));
    }
    named
        .into_iter()
        .find_map(trusted_program)
        .ok_or_else(refusal)
}

/// [`DEFAULT_PATH`]'s directories that only uid 0 can change, in its order:
/// `/usr/local/bin` only once uid 0 has held `/usr/local` since the boot
/// began.
fn trusted_dirs() -> Vec<PathBuf> {
    DEFAULT_PATH
        .to_bytes()
        .split(|&b| b == b':')
        .map(|dir| PathBuf::from(OsStr::from_bytes(dir)))
        .filter(|dir| trusted_dir(dir))
        .collect()
}

/// `path` when it is in a directory only uid 0 can change and leads, through
/// links each in such a directory, to a regular executable file in one, which
/// only uid 0 can change either, and so is the interpreter a script names.
fn trusted_program(path: PathBuf) -> Option<Found> {
    trusted_within(path, MAX_INTERPRETERS)
}

fn trusted_within(path: PathBuf, interpreters: usize) -> Option<Found> {
    let mut at = path.clone();
    for _ in 0..=MAX_LINKS {
        let dir = at.parent().filter(|_| at.file_name().is_some())?;
        if !trusted_dir(dir) {
            return None;
        }
        let meta = std::fs::symlink_metadata(&at).ok()?;
        if !meta.file_type().is_symlink() {
            let runs = meta.is_file() && only_uid_0_writes(&meta) && meta.mode() & 0o111 != 0;
            let interpreted = match interpreter(&at) {
                Some(named) => interpreters
                    .checked_sub(1)
                    .and_then(|left| trusted_within(named, left))
                    .is_some(),
                None => true,
            };
            return (runs && interpreted).then_some(Found { path, target: at });
        }
        at = dir.join(std::fs::read_link(&at).ok()?);
    }
    None
}

/// The interpreter a script's `#!` line names.
fn interpreter(file: &Path) -> Option<PathBuf> {
    let mut head = [0u8; 256];
    let read = std::io::Read::read(&mut std::fs::File::open(file).ok()?, &mut head).ok()?;
    let line = head[..read].strip_prefix(b"#!")?;
    let line = line.split(|&b| b == b'\n').next()?;
    let named = line
        .split(|b| b.is_ascii_whitespace())
        .find(|word| !word.is_empty())?;
    Some(PathBuf::from(OsStr::from_bytes(named)))
}

/// Whether `dir` is a directory by its canonical path that only uid 0 can
/// change, and so is every directory above it, so none can be renamed away
/// beneath it; one in `/usr/local` only once that is [`root_layout::settled`].
fn trusted_dir(dir: &Path) -> bool {
    (!dir.starts_with(root_layout::LOCAL) || root_layout::settled())
        && std::fs::canonicalize(dir).is_ok_and(|canonical| canonical == dir)
        && dir.ancestors().all(|at| {
            std::fs::symlink_metadata(at).is_ok_and(|m| m.is_dir() && only_uid_0_writes(&m))
        })
}

fn only_uid_0_writes(meta: &std::fs::Metadata) -> bool {
    meta.uid() == 0 && meta.mode() & 0o022 == 0
}

/// Take Ctrl+Z from the terminal of a command run alone: nothing here would
/// continue one it stopped, and the requester relays no stop.
fn no_suspend() {
    if let Ok(mut termios) = fs::tcgetattr(0) {
        termios.c_cc[VSUSP] = POSIX_VDISABLE;
        let _ = fs::tcsetattr(0, &termios);
    }
}

/// What the command finds in its environment: the person as the database
/// names them, the search path priv finds programs on, and the mark that has
/// a shell hand its power on.
fn environment() -> Vec<CString> {
    let (user, home) = account();
    let path: Vec<String> = trusted_dirs()
        .iter()
        .map(|d| d.display().to_string())
        .collect();
    let mut env = vec![
        format!("PATH={}", path.join(":")).into_bytes(),
        [b"HOME=".as_slice(), &home].concat(),
        [b"USER=".as_slice(), &user].concat(),
        [b"LOGNAME=".as_slice(), &user].concat(),
        format!("SHELL={SHELL}").into_bytes(),
        format!("LANG={MECHANISM_LANG}").into_bytes(),
        format!("{PRIV_ENV}=1").into_bytes(),
    ];
    if fs::isatty(0) {
        env.push(b"TERM=slopos".to_vec());
    }
    env.into_iter()
        .filter_map(|var| CString::new(var).ok())
        .collect()
}

/// Run `path` holding this process's mask and end as it ends; the errno of
/// one that could not start.
fn run(path: &Path, argv: &[Vec<u8>]) -> i64 {
    let Ok(argv) = argv
        .iter()
        .map(|arg| CString::new(arg.as_slice()))
        .collect::<Result<Vec<_>, _>>()
    else {
        return i64::from(slopos_abi::Errno::EINVAL.raw());
    };
    let env = environment();
    let argv_ptrs: Vec<*const u8> = argv.iter().map(|a| a.as_ptr().cast()).collect();
    let env_ptrs: Vec<*const u8> = env.iter().map(|v| v.as_ptr().cast()).collect();
    held::run(path.as_os_str().as_bytes(), &argv_ptrs, &env_ptrs)
}
