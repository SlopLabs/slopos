//! Creating, copying, moving and removing files — the utilities a build step
//! runs between compiling and installing, which is why they recurse and
//! create parents rather than handling one flat name at a time.

use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::ErrorKind;
use std::time::SystemTime;

use crate::syscall::core;

use super::fsutil::{self, Kind, Visit, Walk};
use super::input::{as_str, copy_through};
use super::opts::{Opt, Opts, parse_octal};
use super::{Ctx, Tool};

const USAGE_CP: &str = "cp [-rRfp] source... target";
const USAGE_MV: &str = "mv [-f] source... target";
const USAGE_RM: &str = "rm [-rRf] file...";
const USAGE_MKDIR: &str = "mkdir [-p] [-m mode] directory...";
const USAGE_RMDIR: &str = "rmdir [-p] directory...";
const USAGE_LN: &str = "ln [-sf] target... link_name";
const USAGE_TOUCH: &str = "touch [-c] file...";
const USAGE_INSTALL: &str = "install [-d] [-m mode] source... target";
const USAGE_MKTEMP: &str = "mktemp [-d] [-p dir] [template]";

pub static TOOLS: &[Tool] = &[
    Tool {
        name: "cp",
        desc: "Copy files and directory trees",
        usage: USAGE_CP,
        run: cp,
    },
    Tool {
        name: "mv",
        desc: "Move or rename files",
        usage: USAGE_MV,
        run: mv,
    },
    Tool {
        name: "rm",
        desc: "Remove files and directory trees",
        usage: USAGE_RM,
        run: rm,
    },
    Tool {
        name: "mkdir",
        desc: "Create directories",
        usage: USAGE_MKDIR,
        run: mkdir,
    },
    Tool {
        name: "rmdir",
        desc: "Remove empty directories",
        usage: USAGE_RMDIR,
        run: rmdir,
    },
    Tool {
        name: "ln",
        desc: "Create hard or symbolic links",
        usage: USAGE_LN,
        run: ln,
    },
    Tool {
        name: "touch",
        desc: "Create a file or update its timestamp",
        usage: USAGE_TOUCH,
        run: touch,
    },
    Tool {
        name: "install",
        desc: "Copy a file into place with a mode",
        usage: USAGE_INSTALL,
        run: install,
    },
    Tool {
        name: "mktemp",
        desc: "Create a uniquely named file or directory",
        usage: USAGE_MKTEMP,
        run: mktemp,
    },
];

/// The mode `install` gives what it places when `-m` is absent.
const INSTALL_MODE: u32 = 0o755;

const TEMPLATE_DEFAULT: &str = "tmp.XXXXXX";
const TEMPLATE_MIN_X: usize = 6;
const TEMPLATE_TRIES: usize = 64;

struct CopyFlags {
    recursive: bool,
    force: bool,
    preserve: bool,
}

fn cp(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut flags = CopyFlags {
        recursive: false,
        force: false,
        preserve: false,
    };
    let mut opts = Opts::new(argv, "rRfp");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'r') | Opt::Flag(b'R') => flags.recursive = true,
            Opt::Flag(b'f') => flags.force = true,
            Opt::Flag(b'p') => flags.preserve = true,
            other => return reject(ctx, other, USAGE_CP),
        }
    }
    let operands = opts.operands();
    if operands.len() < 2 {
        return ctx.usage(USAGE_CP);
    }
    let (sources, last) = operands.split_at(operands.len() - 1);
    let Some(target) = as_str(ctx, last[0]) else {
        return 1;
    };
    let into_dir = fsutil::is_dir(target);
    if sources.len() > 1 && !into_dir {
        ctx.warn_at(target.as_bytes(), b"not a directory");
        return 1;
    }
    let mut status = 0;
    for source in sources {
        let Some(source) = as_str(ctx, source) else {
            status = 1;
            continue;
        };
        let dest = destination(target, source, into_dir);
        if flags.recursive && copies_into_itself(source, &dest) {
            ctx.warn_at(source.as_bytes(), b"cannot copy a directory into itself");
            status = 1;
            continue;
        }
        if !copy_operand(ctx, source, &dest, &flags) {
            status = 1;
        }
    }
    status
}

fn mv(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut force = false;
    let mut opts = Opts::new(argv, "f");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'f') => force = true,
            other => return reject(ctx, other, USAGE_MV),
        }
    }
    let operands = opts.operands();
    if operands.len() < 2 {
        return ctx.usage(USAGE_MV);
    }
    let (sources, last) = operands.split_at(operands.len() - 1);
    let Some(target) = as_str(ctx, last[0]) else {
        return 1;
    };
    let into_dir = fsutil::is_dir(target);
    if sources.len() > 1 && !into_dir {
        ctx.warn_at(target.as_bytes(), b"not a directory");
        return 1;
    }
    let mut status = 0;
    for source in sources {
        let Some(source) = as_str(ctx, source) else {
            status = 1;
            continue;
        };
        let dest = destination(target, source, into_dir);
        if !move_operand(ctx, source, &dest, force) {
            status = 1;
        }
    }
    status
}

fn rm(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut recursive = false;
    let mut force = false;
    let mut opts = Opts::new(argv, "rRf");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'r') | Opt::Flag(b'R') => recursive = true,
            Opt::Flag(b'f') => force = true,
            other => return reject(ctx, other, USAGE_RM),
        }
    }
    let operands = opts.operands();
    if operands.is_empty() {
        // `rm -f` with nothing to remove is the one case POSIX calls success.
        return if force { 0 } else { ctx.usage(USAGE_RM) };
    }
    let mut status = 0;
    for operand in operands {
        let Some(path) = as_str(ctx, operand) else {
            status = 1;
            continue;
        };
        if !remove_operand(ctx, path, recursive, force) {
            status = 1;
        }
    }
    status
}

fn mkdir(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut parents = false;
    let mut mode = None;
    let mut opts = Opts::new(argv, "pm:");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'p') => parents = true,
            Opt::Value(b'm', value) => match parse_octal(value) {
                Some(parsed) => mode = Some(parsed),
                None => {
                    ctx.warn_at(value, b"invalid mode");
                    return 1;
                }
            },
            other => return reject(ctx, other, USAGE_MKDIR),
        }
    }
    let operands = opts.operands();
    if operands.is_empty() {
        return ctx.usage(USAGE_MKDIR);
    }
    let mut status = 0;
    for operand in operands {
        let Some(path) = as_str(ctx, operand) else {
            status = 1;
            continue;
        };
        let made = if parents {
            fs::create_dir_all(path)
        } else {
            fs::create_dir(path)
        };
        if let Err(error) = made {
            ctx.warn_io(path.as_bytes(), &error);
            status = 1;
            continue;
        }
        // `-m` applies to the operands, never to the parents `-p` invents.
        if let Some(mode) = mode {
            if let Err(error) = fsutil::chmod(path, mode) {
                ctx.warn_io(path.as_bytes(), &error);
                status = 1;
            }
        }
    }
    status
}

fn rmdir(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut parents = false;
    let mut opts = Opts::new(argv, "p");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'p') => parents = true,
            other => return reject(ctx, other, USAGE_RMDIR),
        }
    }
    let operands = opts.operands();
    if operands.is_empty() {
        return ctx.usage(USAGE_RMDIR);
    }
    let mut status = 0;
    for operand in operands {
        let Some(path) = as_str(ctx, operand) else {
            status = 1;
            continue;
        };
        let mut path = path.to_string();
        loop {
            if let Err(error) = fs::remove_dir(&path) {
                ctx.warn_io(path.as_bytes(), &error);
                status = 1;
                break;
            }
            if !parents {
                break;
            }
            let parent = fsutil::dir_name(&path).to_string();
            if parent == "/" || parent == "." || parent == path {
                break;
            }
            path = parent;
        }
    }
    status
}

fn ln(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut symbolic = false;
    let mut force = false;
    let mut opts = Opts::new(argv, "sf");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b's') => symbolic = true,
            Opt::Flag(b'f') => force = true,
            other => return reject(ctx, other, USAGE_LN),
        }
    }
    let operands = opts.operands();
    if operands.len() < 2 {
        return ctx.usage(USAGE_LN);
    }
    let (targets, last) = operands.split_at(operands.len() - 1);
    let Some(directory) = as_str(ctx, last[0]) else {
        return 1;
    };
    let into_dir = fsutil::is_dir(directory);
    if targets.len() > 1 && !into_dir {
        ctx.warn_at(directory.as_bytes(), b"not a directory");
        return 1;
    }
    let mut status = 0;
    for target in targets {
        let Some(name) = as_str(ctx, target) else {
            status = 1;
            continue;
        };
        let link = destination(directory, name, into_dir);
        if force {
            let _ = fs::remove_file(&link);
        }
        // A symlink stores the operand's bytes verbatim; only a hard link has
        // to resolve to a file that exists now.
        let result = if symbolic {
            fsutil::symlink(target, &link)
        } else {
            fsutil::hard_link(name, &link)
        };
        if let Err(error) = result {
            ctx.warn_io(link.as_bytes(), &error);
            status = 1;
        }
    }
    status
}

fn touch(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut no_create = false;
    let mut opts = Opts::new(argv, "c");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'c') => no_create = true,
            other => return reject(ctx, other, USAGE_TOUCH),
        }
    }
    let operands = opts.operands();
    if operands.is_empty() {
        return ctx.usage(USAGE_TOUCH);
    }
    let mut status = 0;
    for operand in operands {
        let Some(path) = as_str(ctx, operand) else {
            status = 1;
            continue;
        };
        match OpenOptions::new().write(true).open(path) {
            Ok(file) => {
                let times = FileTimes::new().set_modified(SystemTime::now());
                if let Err(error) = file.set_times(times) {
                    ctx.warn_io(path.as_bytes(), &error);
                    status = 1;
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                if no_create {
                    continue;
                }
                if let Err(error) = File::create(path) {
                    ctx.warn_io(path.as_bytes(), &error);
                    status = 1;
                }
            }
            Err(error) => {
                ctx.warn_io(path.as_bytes(), &error);
                status = 1;
            }
        }
    }
    status
}

fn install(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut directories = false;
    let mut mode = None;
    let mut opts = Opts::new(argv, "dm:");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'd') => directories = true,
            Opt::Value(b'm', value) => match parse_octal(value) {
                Some(parsed) => mode = Some(parsed),
                None => {
                    ctx.warn_at(value, b"invalid mode");
                    return 1;
                }
            },
            other => return reject(ctx, other, USAGE_INSTALL),
        }
    }
    let operands = opts.operands();
    if operands.is_empty() {
        return ctx.usage(USAGE_INSTALL);
    }
    let mode = mode.unwrap_or(INSTALL_MODE);
    let mut status = 0;
    if directories {
        for operand in operands {
            let Some(path) = as_str(ctx, operand) else {
                status = 1;
                continue;
            };
            if let Err(error) = fs::create_dir_all(path) {
                ctx.warn_io(path.as_bytes(), &error);
                status = 1;
                continue;
            }
            if let Err(error) = fsutil::chmod(path, mode) {
                ctx.warn_io(path.as_bytes(), &error);
                status = 1;
            }
        }
        return status;
    }

    if operands.len() < 2 {
        return ctx.usage(USAGE_INSTALL);
    }
    let (sources, last) = operands.split_at(operands.len() - 1);
    let Some(target) = as_str(ctx, last[0]) else {
        return 1;
    };
    let into_dir = fsutil::is_dir(target);
    if sources.len() > 1 && !into_dir {
        ctx.warn_at(target.as_bytes(), b"not a directory");
        return 1;
    }
    // `install` replaces its destination even when it is unwritable: that is
    // the whole point of using it instead of `cp` in an install rule.
    let flags = CopyFlags {
        recursive: false,
        force: true,
        preserve: false,
    };
    for source in sources {
        let Some(source) = as_str(ctx, source) else {
            status = 1;
            continue;
        };
        let dest = destination(target, source, into_dir);
        if !copy_operand(ctx, source, &dest, &flags) {
            status = 1;
            continue;
        }
        if let Err(error) = fsutil::chmod(&dest, mode) {
            ctx.warn_io(dest.as_bytes(), &error);
            status = 1;
        }
    }
    status
}

fn mktemp(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut directory = false;
    let mut base = None;
    let mut opts = Opts::new(argv, "dp:");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'd') => directory = true,
            Opt::Value(b'p', value) => base = Some(value),
            other => return reject(ctx, other, USAGE_MKTEMP),
        }
    }
    let operands = opts.operands();
    if operands.len() > 1 {
        return ctx.usage(USAGE_MKTEMP);
    }
    let template = match operands.first() {
        Some(operand) => match as_str(ctx, operand) {
            Some(text) => text,
            None => return 1,
        },
        None => TEMPLATE_DEFAULT,
    };
    let template = match base {
        Some(value) => {
            let Some(dir) = as_str(ctx, value) else {
                return 1;
            };
            fsutil::join(dir, template)
        }
        // A template naming a directory of its own is taken as written; only a
        // bare name gets the temporary directory prefixed.
        None if template.contains('/') => template.to_string(),
        None => fsutil::join(&tmpdir(), template),
    };

    let placeholders = template.len() - template.trim_end_matches('X').len();
    if placeholders < TEMPLATE_MIN_X {
        ctx.warn_at(template.as_bytes(), b"too few X's in template");
        return 1;
    }
    let prefix = &template[..template.len() - placeholders];
    for _ in 0..TEMPLATE_TRIES {
        let mut path = String::with_capacity(template.len());
        path.push_str(prefix);
        push_random(&mut path, placeholders);
        let made = if directory {
            fs::create_dir(&path)
        } else {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map(|_| ())
        };
        match made {
            Ok(()) => {
                ctx.out.s(&path);
                ctx.out.nl();
                return 0;
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                ctx.warn_io(path.as_bytes(), &error);
                return 1;
            }
        }
    }
    ctx.warn_at(template.as_bytes(), b"could not create a unique name");
    1
}

fn tmpdir() -> String {
    match std::env::var("TMPDIR") {
        Ok(dir) if !dir.is_empty() => dir,
        _ => "/tmp".to_string(),
    }
}

/// Append `count` characters drawn from the kernel's entropy pool.
fn push_random(out: &mut String, count: usize) {
    const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut raw = vec![0u8; count];
    if core::getrandom(&mut raw) != count as isize {
        // No entropy source: the clock is the only varying input left, run
        // through a mixer so consecutive calls do not share a prefix.
        let mut mix = core::clock_gettime_ns() | 1;
        for slot in raw.iter_mut() {
            mix = mix
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *slot = (mix >> 33) as u8;
        }
    }
    for byte in raw {
        out.push(ALPHABET[(byte % ALPHABET.len() as u8) as usize] as char);
    }
}

/// Where one source lands: inside `target` when that is a directory, else
/// `target` itself.
fn destination(target: &str, source: &str, into_dir: bool) -> String {
    if into_dir {
        fsutil::join(target, fsutil::base_name(source))
    } else {
        target.to_string()
    }
}

fn same_file(a: &str, b: &str) -> bool {
    match (absolute(a), absolute(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// A destination inside the source is created while the walk that reads the
/// source is still running, so the walk finds it and copies it again, without
/// end. GNU refuses the operand instead, and so does this.
fn copies_into_itself(source: &str, dest: &str) -> bool {
    let is_tree = fs::symlink_metadata(source)
        .map(|meta| meta.is_dir())
        .unwrap_or(false);
    if !is_tree {
        return false;
    }
    match (absolute(source), absolute(dest)) {
        (Some(source), Some(dest)) => dest == source || under(&source, &dest),
        _ => false,
    }
}

/// An absolute path with `.`, `..` and repeated separators folded away.
/// `fs::canonicalize` insists the path exist, which a destination need not.
fn absolute(path: &str) -> Option<String> {
    let base = if path.starts_with('/') {
        String::new()
    } else {
        std::env::current_dir().ok()?.to_str()?.to_string()
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in base.split('/').chain(path.split('/')) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            name => parts.push(name),
        }
    }
    let mut out = String::with_capacity(base.len() + path.len() + 1);
    for part in parts {
        out.push('/');
        out.push_str(part);
    }
    if out.is_empty() {
        out.push('/');
    }
    Some(out)
}

fn under(dir: &str, path: &str) -> bool {
    let dir = dir.trim_end_matches('/');
    path.len() > dir.len() && path.starts_with(dir) && path.as_bytes()[dir.len()] == b'/'
}

fn copy_operand(ctx: &mut Ctx, source: &str, dest: &str, flags: &CopyFlags) -> bool {
    let meta = match fs::symlink_metadata(source) {
        Ok(meta) => meta,
        Err(error) => {
            ctx.warn_io(source.as_bytes(), &error);
            return false;
        }
    };
    if same_file(source, dest) {
        ctx.warn_at(source.as_bytes(), b"and the destination are the same file");
        return false;
    }
    match Kind::of(&meta) {
        Kind::Dir => {
            if !flags.recursive {
                ctx.warn_at(source.as_bytes(), b"omitting directory");
                return false;
            }
            copy_tree(ctx, source, dest, flags)
        }
        kind => copy_node(ctx, source, kind, dest, flags),
    }
}

fn copy_tree(ctx: &mut Ctx, source: &str, dest: &str, flags: &CopyFlags) -> bool {
    let root = source.trim_end_matches('/');
    let mut ok = true;
    for visit in Walk::new(source) {
        let entry = match visit {
            Ok(Visit::Pre(entry)) => entry,
            Ok(Visit::Post(_)) => continue,
            Err(error) => {
                ctx.warn_io(error.path.as_bytes(), &error.error);
                ok = false;
                continue;
            }
        };
        let relative = entry.path[root.len()..].trim_start_matches('/');
        let target = if relative.is_empty() {
            dest.to_string()
        } else {
            fsutil::join(dest, relative)
        };
        let copied = match entry.kind {
            Kind::Dir => make_dir(ctx, &target),
            kind => copy_node(ctx, &entry.path, kind, &target, flags),
        };
        if !copied {
            ok = false;
        }
    }
    ok
}

fn make_dir(ctx: &mut Ctx, path: &str) -> bool {
    match fs::create_dir(path) {
        Ok(()) => true,
        Err(error) if error.kind() == ErrorKind::AlreadyExists && fsutil::is_dir(path) => true,
        Err(error) => {
            ctx.warn_io(path.as_bytes(), &error);
            false
        }
    }
}

fn copy_node(ctx: &mut Ctx, source: &str, kind: Kind, dest: &str, flags: &CopyFlags) -> bool {
    let result = match kind {
        Kind::File => copy_regular(source, dest, flags),
        Kind::Symlink => match fsutil::read_link(source) {
            // A symlink cannot be rewritten in place, so the destination goes
            // first; that is what makes a repeated `cp -R` succeed.
            Ok(target) => {
                let _ = fs::remove_file(dest);
                fsutil::symlink(&target, dest)
            }
            Err(error) => Err(error),
        },
        _ => {
            ctx.warn_at(source.as_bytes(), b"unsupported file type");
            return false;
        }
    };
    match result {
        Ok(()) => true,
        Err(error) => {
            ctx.warn_io(source.as_bytes(), &error);
            false
        }
    }
}

fn copy_regular(source: &str, dest: &str, flags: &CopyFlags) -> Result<(), std::io::Error> {
    let mut reader = File::open(source)?;
    let mut writer = match File::create(dest) {
        Ok(file) => file,
        Err(error) if flags.force && error.kind() == ErrorKind::PermissionDenied => {
            fs::remove_file(dest)?;
            File::create(dest)?
        }
        Err(error) => return Err(error),
    };
    copy_through(&mut reader, &mut writer)?;
    // Only the mtime: there is no path-based `utimensat` in std here, and no
    // owner or mode to carry on a single-user system.
    if flags.preserve {
        let modified = reader.metadata()?.modified()?;
        writer.set_times(FileTimes::new().set_modified(modified))?;
    }
    Ok(())
}

fn move_operand(ctx: &mut Ctx, source: &str, dest: &str, force: bool) -> bool {
    if same_file(source, dest) {
        ctx.warn_at(source.as_bytes(), b"and the destination are the same file");
        return false;
    }
    let mut result = fs::rename(source, dest);
    if force && matches!(&result, Err(error) if error.kind() == ErrorKind::PermissionDenied) {
        let _ = fs::remove_file(dest);
        result = fs::rename(source, dest);
    }
    match result {
        Ok(()) => true,
        // A rename cannot cross a mount point, so the move degrades to a copy
        // of the whole source followed by its removal.
        Err(error) if error.kind() == ErrorKind::CrossesDevices => {
            let flags = CopyFlags {
                recursive: true,
                force,
                preserve: true,
            };
            copy_operand(ctx, source, dest, &flags) && remove_operand(ctx, source, true, force)
        }
        Err(error) => {
            ctx.warn_io(source.as_bytes(), &error);
            false
        }
    }
}

fn remove_operand(ctx: &mut Ctx, path: &str, recursive: bool, force: bool) -> bool {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if force && error.kind() == ErrorKind::NotFound => return true,
        Err(error) => {
            ctx.warn_io(path.as_bytes(), &error);
            return false;
        }
    };
    if !meta.is_dir() {
        return unlink(ctx, path, force);
    }
    if !recursive {
        ctx.warn_at(path.as_bytes(), b"is a directory");
        return false;
    }
    let mut ok = true;
    for visit in Walk::new(path).with_post() {
        match visit {
            Ok(Visit::Pre(entry)) => {
                if entry.kind != Kind::Dir && !unlink(ctx, &entry.path, force) {
                    ok = false;
                }
            }
            Ok(Visit::Post(entry)) => {
                if let Err(error) = fs::remove_dir(&entry.path) {
                    if !(force && error.kind() == ErrorKind::NotFound) {
                        ctx.warn_io(entry.path.as_bytes(), &error);
                        ok = false;
                    }
                }
            }
            Err(error) => {
                if !(force && error.error.kind() == ErrorKind::NotFound) {
                    ctx.warn_io(error.path.as_bytes(), &error.error);
                    ok = false;
                }
            }
        }
    }
    ok
}

fn unlink(ctx: &mut Ctx, path: &str, force: bool) -> bool {
    match fs::remove_file(path) {
        Ok(()) => true,
        Err(error) if force && error.kind() == ErrorKind::NotFound => true,
        Err(error) => {
            ctx.warn_io(path.as_bytes(), &error);
            false
        }
    }
}

fn reject(ctx: &mut Ctx, opt: Opt<'_>, usage: &str) -> i32 {
    match opt {
        Opt::Unknown(flag) => ctx.warn_at(&[flag], b"invalid option"),
        Opt::Missing(flag) => ctx.warn_at(&[flag], b"option requires an argument"),
        Opt::Long(name, _) => ctx.warn_at(name, b"invalid option"),
        _ => {}
    }
    ctx.usage(usage)
}
