//! `tar`, in the POSIX ustar format: 512-byte headers with a verified
//! checksum, the `prefix`/`name` split for long paths, and two zero blocks to
//! end the archive.

use std::fs::{self, File};
use std::io::{Read, Write};

use slopos_slibc_core::calendar;

use super::input::{self, Input, as_str};
use super::io::io_message;
use super::opts::{Opt, Opts};
use super::{Ctx, Tool, deflate, fsutil};

pub static TOOLS: &[Tool] = &[Tool {
    name: "tar",
    desc: "Create, list or extract a ustar archive",
    usage: USAGE,
    run: tar,
}];

const USAGE: &str = "tar [-c|-x|-t] [-vz] [-f archive] [-C dir] [file...]";
const BLOCK: usize = 512;
const BODY: usize = 8192;
const ZEROS: [u8; BLOCK] = [0u8; BLOCK];
const LEVEL: u8 = 6;

fn tar(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut mode = 0u8;
    let mut archive: &[u8] = b"-";
    let mut directory: Option<&[u8]> = None;
    let mut verbose = false;
    let mut compress = false;

    let mut opts = Opts::new(argv, "cxtvzf:C:");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(flag @ (b'c' | b'x' | b't')) => {
                if mode != 0 && mode != flag {
                    ctx.warn(b"only one of -c, -x or -t may be given");
                    return ctx.usage(USAGE);
                }
                mode = flag;
            }
            Opt::Flag(b'v') => verbose = true,
            Opt::Flag(b'z') => compress = true,
            Opt::Value(b'f', value) => archive = value,
            Opt::Value(b'C', value) => directory = Some(value),
            Opt::Long(name, _) => {
                ctx.warn_at(name, b"invalid option");
                return ctx.usage(USAGE);
            }
            Opt::Unknown(flag) => {
                ctx.warn_at(&[flag], b"invalid option");
                return ctx.usage(USAGE);
            }
            Opt::Missing(flag) => {
                ctx.warn_at(&[flag], b"option requires an argument");
                return ctx.usage(USAGE);
            }
            _ => {}
        }
    }
    let operands = opts.operands();
    if mode == 0 {
        return ctx.usage(USAGE);
    }

    // The archive is opened before `-C` takes effect, so its own path is
    // resolved where the caller typed it rather than inside the destination.
    match mode {
        b'c' => {
            if operands.is_empty() {
                ctx.warn(b"no files given");
                return ctx.usage(USAGE);
            }
            let Some(writer) = open_writer(ctx, archive, compress) else {
                return 2;
            };
            if !chdir(ctx, directory) {
                return 2;
            }
            create(ctx, archive, writer, operands, verbose)
        }
        _ => {
            let Some(reader) = open_reader(ctx, archive, compress) else {
                return 2;
            };
            if !chdir(ctx, directory) {
                return 2;
            }
            scan(ctx, archive, reader, operands, verbose, mode == b'x')
        }
    }
}

fn chdir(ctx: &mut Ctx, directory: Option<&[u8]>) -> bool {
    let Some(operand) = directory else {
        return true;
    };
    let Some(path) = as_str(ctx, operand) else {
        return false;
    };
    if let Err(error) = std::env::set_current_dir(path) {
        ctx.warn_io(operand, &error);
        return false;
    }
    true
}

enum Dest {
    Stdout,
    File(File),
}

struct Writer {
    dest: Dest,
    /// Set when `-z` is in force: gzip needs the whole member stream, so the
    /// archive is staged before framing.
    staged: Option<Vec<u8>>,
    error: Option<std::io::Error>,
}

impl Writer {
    fn put(&mut self, ctx: &mut Ctx, bytes: &[u8]) {
        if let Some(staged) = &mut self.staged {
            staged.extend_from_slice(bytes);
            return;
        }
        match &mut self.dest {
            Dest::Stdout => ctx.out.write(bytes),
            Dest::File(file) => {
                if self.error.is_none() {
                    if let Err(error) = file.write_all(bytes) {
                        self.error = Some(error);
                    }
                }
            }
        }
    }

    fn finish(&mut self, ctx: &mut Ctx) {
        if let Some(staged) = self.staged.take() {
            let framed = deflate::gzip_wrap(&staged, LEVEL, None, 0);
            self.put(ctx, &framed);
        }
    }
}

fn open_writer(ctx: &mut Ctx, archive: &[u8], compress: bool) -> Option<Writer> {
    let dest = if archive == b"-" {
        Dest::Stdout
    } else {
        let path = as_str(ctx, archive)?;
        match File::create(path) {
            Ok(file) => Dest::File(file),
            Err(error) => {
                ctx.warn_io(archive, &error);
                return None;
            }
        }
    };
    Some(Writer {
        dest,
        staged: if compress { Some(Vec::new()) } else { None },
        error: None,
    })
}

fn create(
    ctx: &mut Ctx,
    archive: &[u8],
    mut writer: Writer,
    operands: &[&[u8]],
    verbose: bool,
) -> i32 {
    let mut status = 0;
    for operand in operands {
        let Some(root) = as_str(ctx, operand) else {
            status = 1;
            continue;
        };
        for step in fsutil::Walk::new(root) {
            match step {
                Ok(visit) => {
                    if !add_member(ctx, &mut writer, visit.entry(), verbose) {
                        status = 1;
                    }
                }
                Err(walk) => {
                    ctx.warn_io(walk.path.as_bytes(), &walk.error);
                    status = 1;
                }
            }
        }
    }
    writer.put(ctx, &ZEROS);
    writer.put(ctx, &ZEROS);
    writer.finish(ctx);
    if let Some(error) = writer.error.take() {
        ctx.warn_io(archive, &error);
        status = 1;
    }
    status
}

fn add_member(ctx: &mut Ctx, writer: &mut Writer, entry: &fsutil::Entry, verbose: bool) -> bool {
    // A leading `/` is dropped on the way in, so no archive this tool writes
    // can ask an extractor to overwrite an absolute path.
    let mut name = entry.path.trim_start_matches('/').to_string();
    if name.is_empty() {
        ctx.warn_at(entry.path.as_bytes(), b"cannot archive the root directory");
        return false;
    }
    let mtime = mtime_of(&entry.meta);
    let mut link = Vec::new();
    let (kind, size, mode) = match entry.kind {
        fsutil::Kind::Dir => {
            if !name.ends_with('/') {
                name.push('/');
            }
            (b'5', 0u64, 0o755)
        }
        fsutil::Kind::Symlink => match fsutil::read_link(&entry.path) {
            Ok(target) => {
                link = target;
                (b'2', 0, 0o777)
            }
            Err(error) => {
                ctx.warn_io(entry.path.as_bytes(), &error);
                return false;
            }
        },
        fsutil::Kind::File => {
            // `Metadata` carries no mode on this target, so the permission
            // bits are reconstructed from the one bit it does expose.
            let mode = if entry.meta.permissions().readonly() {
                0o444
            } else {
                0o644
            };
            (b'0', entry.meta.len(), mode)
        }
        fsutil::Kind::Other => {
            ctx.warn_at(
                entry.path.as_bytes(),
                b"unsupported file type, not archived",
            );
            return false;
        }
    };

    let header = match build_header(name.as_bytes(), kind, size, mtime, mode, &link) {
        Ok(header) => header,
        Err(why) => {
            ctx.warn_at(entry.path.as_bytes(), why.as_bytes());
            return false;
        }
    };
    writer.put(ctx, &header);
    if kind == b'0' && !copy_body(ctx, writer, &entry.path, size) {
        return false;
    }
    if verbose {
        note(ctx, name.as_bytes());
    }
    true
}

fn copy_body(ctx: &mut Ctx, writer: &mut Writer, path: &str, size: u64) -> bool {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) => {
            ctx.warn_io(path.as_bytes(), &error);
            return false;
        }
    };
    let mut buf = [0u8; BODY];
    let mut left = size;
    let mut ok = true;
    while left > 0 {
        let want = left.min(buf.len() as u64) as usize;
        match file.read(&mut buf[..want]) {
            Ok(0) => break,
            Ok(read) => {
                writer.put(ctx, &buf[..read]);
                left -= read as u64;
            }
            Err(error) => {
                ctx.warn_io(path.as_bytes(), &error);
                ok = false;
                break;
            }
        }
    }
    // The header's size is already on the wire, so a file that shrank under us
    // is padded rather than left short and desynchronising the archive.
    while left > 0 {
        let fill = left.min(BLOCK as u64) as usize;
        writer.put(ctx, &ZEROS[..fill]);
        left -= fill as u64;
    }
    let pad = padding(size);
    if pad > 0 {
        writer.put(ctx, &ZEROS[..pad]);
    }
    ok
}

fn padding(size: u64) -> usize {
    let tail = (size % BLOCK as u64) as usize;
    if tail == 0 { 0 } else { BLOCK - tail }
}

fn mtime_of(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

fn octal(field: &mut [u8], value: u64) {
    let digits = field.len() - 1;
    let mut rest = value;
    for slot in field[..digits].iter_mut().rev() {
        *slot = b'0' + (rest & 7) as u8;
        rest >>= 3;
    }
    field[digits] = 0;
}

fn split_name(name: &[u8]) -> Option<(&[u8], &[u8])> {
    if name.len() <= 100 {
        return Some((&[], name));
    }
    let cut = (1..name.len()).find(|&i| {
        name[i] == b'/' && i <= 155 && name.len() - i - 1 <= 100 && name.len() - i - 1 > 0
    })?;
    Some((&name[..cut], &name[cut + 1..]))
}

fn build_header(
    name: &[u8],
    kind: u8,
    size: u64,
    mtime: u64,
    mode: u32,
    link: &[u8],
) -> Result<[u8; BLOCK], &'static str> {
    let Some((prefix, base)) = split_name(name) else {
        return Err("file name too long for the ustar format");
    };
    if link.len() > 100 {
        return Err("link target too long for the ustar format");
    }
    if size > 0o77_777_777_777 {
        return Err("file too large for the ustar format");
    }

    let mut block = [0u8; BLOCK];
    block[..base.len()].copy_from_slice(base);
    block[345..345 + prefix.len()].copy_from_slice(prefix);
    octal(&mut block[100..108], mode as u64 & 0o7777);
    octal(&mut block[108..116], 0);
    octal(&mut block[116..124], 0);
    octal(&mut block[124..136], size);
    octal(&mut block[136..148], mtime);
    block[156] = kind;
    block[157..157 + link.len()].copy_from_slice(link);
    block[257..263].copy_from_slice(b"ustar\0");
    block[263..265].copy_from_slice(b"00");
    // SlopOS is single-user at uid 0, so every member belongs to root.
    block[265..269].copy_from_slice(b"root");
    block[297..301].copy_from_slice(b"root");

    for slot in block[148..156].iter_mut() {
        *slot = b' ';
    }
    let sum: u32 = block.iter().map(|&byte| byte as u32).sum();
    octal(&mut block[148..155], sum as u64);
    block[155] = b' ';
    Ok(block)
}

struct Member {
    name: Vec<u8>,
    kind: u8,
    size: u64,
    mtime: u64,
    mode: u32,
    link: Vec<u8>,
}

fn cstr(field: &[u8]) -> &[u8] {
    match field.iter().position(|&byte| byte == 0) {
        Some(end) => &field[..end],
        None => field,
    }
}

fn parse_octal(field: &[u8]) -> Option<u64> {
    let mut value = 0u64;
    let mut seen = false;
    for &byte in field {
        match byte {
            b'0'..=b'7' => {
                value = value.checked_mul(8)?.checked_add((byte - b'0') as u64)?;
                seen = true;
            }
            b' ' | 0 => {
                if seen {
                    break;
                }
            }
            _ => return None,
        }
    }
    if seen { Some(value) } else { None }
}

fn parse_header(block: &[u8; BLOCK]) -> Result<Option<Member>, &'static str> {
    if block.iter().all(|&byte| byte == 0) {
        return Ok(None);
    }
    let stored = parse_octal(&block[148..156]).ok_or("malformed header checksum")?;
    let mut sum = 0u32;
    for (i, &byte) in block.iter().enumerate() {
        sum += if (148..156).contains(&i) {
            b' ' as u32
        } else {
            byte as u32
        };
    }
    if sum as u64 != stored {
        return Err("header checksum mismatch, not a tar archive");
    }

    let mut name = Vec::new();
    let prefix = cstr(&block[345..500]);
    if !prefix.is_empty() && &block[257..262] == b"ustar" {
        name.extend_from_slice(prefix);
        name.push(b'/');
    }
    name.extend_from_slice(cstr(&block[..100]));
    Ok(Some(Member {
        name,
        kind: block[156],
        size: parse_octal(&block[124..136]).ok_or("malformed size field")?,
        mtime: parse_octal(&block[136..148]).unwrap_or(0),
        mode: parse_octal(&block[100..108]).unwrap_or(0o644) as u32,
        link: cstr(&block[157..257]).to_vec(),
    }))
}

enum Reader {
    Stream(Input),
    Mem(Vec<u8>, usize),
}

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Reader::Stream(input) => input.read(buf),
            Reader::Mem(data, pos) => {
                let take = (data.len() - *pos).min(buf.len());
                buf[..take].copy_from_slice(&data[*pos..*pos + take]);
                *pos += take;
                Ok(take)
            }
        }
    }
}

fn open_reader(ctx: &mut Ctx, archive: &[u8], compress: bool) -> Option<Reader> {
    let mut source = input::open(ctx, archive)?;
    if !compress {
        return Some(Reader::Stream(source));
    }
    let raw = match input::read_to_end(&mut source) {
        Ok(raw) => raw,
        Err(error) => {
            ctx.warn_io(archive, &error);
            return None;
        }
    };
    match deflate::gzip_unwrap(&raw) {
        Ok((data, _)) => Some(Reader::Mem(data, 0)),
        Err(error) => {
            ctx.warn_at(archive, error.message().as_bytes());
            None
        }
    }
}

fn read_full(reader: &mut Reader, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..])? {
            0 => break,
            read => filled += read,
        }
    }
    Ok(filled)
}

fn short_read() -> std::io::Error {
    std::io::Error::from(std::io::ErrorKind::UnexpectedEof)
}

/// Read a member's body and its padding, optionally writing it out. The outer
/// error is the archive's; the inner one is the extracted file's, which leaves
/// the stream still aligned on the next header.
fn drain_body(
    reader: &mut Reader,
    size: u64,
    mut out: Option<&mut File>,
) -> std::io::Result<Option<std::io::Error>> {
    let mut buf = [0u8; BODY];
    let mut left = size;
    let mut failed = None;
    while left > 0 {
        let want = left.min(buf.len() as u64) as usize;
        let read = reader.read(&mut buf[..want])?;
        if read == 0 {
            return Err(short_read());
        }
        if failed.is_none() {
            if let Some(file) = out.as_deref_mut() {
                if let Err(error) = file.write_all(&buf[..read]) {
                    failed = Some(error);
                }
            }
        }
        left -= read as u64;
    }
    let pad = padding(size);
    if pad > 0 {
        let mut skip = [0u8; BLOCK];
        if read_full(reader, &mut skip[..pad])? != pad {
            return Err(short_read());
        }
    }
    Ok(failed)
}

enum Outcome {
    Done,
    Failed,
    Fatal,
}

fn skip_body(ctx: &mut Ctx, reader: &mut Reader, size: u64, outcome: Outcome) -> Outcome {
    match drain_body(reader, size, None) {
        Ok(_) => outcome,
        Err(error) => {
            ctx.warn(io_message(&error).as_bytes());
            Outcome::Fatal
        }
    }
}

fn scan(
    ctx: &mut Ctx,
    archive: &[u8],
    mut reader: Reader,
    operands: &[&[u8]],
    verbose: bool,
    extracting: bool,
) -> i32 {
    let mut status = 0;
    let mut block = [0u8; BLOCK];
    loop {
        match read_full(&mut reader, &mut block) {
            Ok(read) if read < BLOCK => {
                ctx.warn_at(archive, b"unexpected end of archive");
                return 2;
            }
            Ok(_) => {}
            Err(error) => {
                ctx.warn_io(archive, &error);
                return 2;
            }
        }
        let member = match parse_header(&block) {
            Ok(Some(member)) => member,
            Ok(None) => match read_full(&mut reader, &mut block) {
                Ok(BLOCK) if block == ZEROS => break,
                Ok(_) => {
                    ctx.warn_at(archive, b"unexpected end of archive");
                    return 2;
                }
                Err(error) => {
                    ctx.warn_io(archive, &error);
                    return 2;
                }
            },
            Err(why) => {
                ctx.warn_at(archive, why.as_bytes());
                return 2;
            }
        };

        if !selected(&member.name, operands) {
            match skip_body(ctx, &mut reader, member.size, Outcome::Done) {
                Outcome::Fatal => return 2,
                _ => continue,
            }
        }
        let outcome = if extracting {
            extract_member(ctx, &mut reader, &member, verbose)
        } else {
            if verbose {
                list_long(ctx, &member);
            } else {
                ctx.out.write(&member.name);
                ctx.out.nl();
            }
            skip_body(ctx, &mut reader, member.size, Outcome::Done)
        };
        match outcome {
            Outcome::Done => {}
            Outcome::Failed => status = 1,
            Outcome::Fatal => return 2,
        }
    }
    status
}

fn trim_slash(path: &[u8]) -> &[u8] {
    let mut end = path.len();
    while end > 1 && path[end - 1] == b'/' {
        end -= 1;
    }
    &path[..end]
}

fn selected(name: &[u8], operands: &[&[u8]]) -> bool {
    if operands.is_empty() {
        return true;
    }
    let name = trim_slash(name);
    operands.iter().any(|operand| {
        let wanted = trim_slash(operand);
        name == wanted
            || (name.len() > wanted.len() && name.starts_with(wanted) && name[wanted.len()] == b'/')
    })
}

fn extract_member(ctx: &mut Ctx, reader: &mut Reader, member: &Member, verbose: bool) -> Outcome {
    if fsutil::escapes(&member.name) {
        ctx.warn_at(
            &member.name,
            b"member path escapes the destination, skipped",
        );
        return skip_body(ctx, reader, member.size, Outcome::Failed);
    }
    if matches!(member.kind, b'1' | b'2') && fsutil::escapes(&member.link) {
        ctx.warn_at(
            &member.name,
            b"link target escapes the destination, skipped",
        );
        return skip_body(ctx, reader, member.size, Outcome::Failed);
    }
    let Some(path) = as_str(ctx, &member.name) else {
        return skip_body(ctx, reader, member.size, Outcome::Failed);
    };
    let outcome = match member.kind {
        b'0' | 0 | b'7' => extract_file(ctx, reader, member, path),
        b'5' => {
            let outcome = match fs::create_dir_all(path) {
                Ok(()) => match fsutil::chmod(path, member.mode & 0o7777) {
                    Ok(()) => Outcome::Done,
                    Err(error) => {
                        ctx.warn_io(member.name.as_slice(), &error);
                        Outcome::Failed
                    }
                },
                Err(error) => {
                    ctx.warn_io(member.name.as_slice(), &error);
                    Outcome::Failed
                }
            };
            skip_body(ctx, reader, member.size, outcome)
        }
        b'2' => {
            let outcome = make_symlink(ctx, member, path);
            skip_body(ctx, reader, member.size, outcome)
        }
        b'1' => {
            let outcome = match core::str::from_utf8(&member.link) {
                Ok(target) => match fsutil::hard_link(target, path) {
                    Ok(()) => Outcome::Done,
                    Err(error) => {
                        ctx.warn_io(member.name.as_slice(), &error);
                        Outcome::Failed
                    }
                },
                Err(_) => {
                    ctx.warn_at(&member.name, b"invalid byte sequence in link target");
                    Outcome::Failed
                }
            };
            skip_body(ctx, reader, member.size, outcome)
        }
        _ => {
            ctx.warn_at(&member.name, b"unsupported member type, skipped");
            skip_body(ctx, reader, member.size, Outcome::Failed)
        }
    };
    if verbose && matches!(outcome, Outcome::Done) {
        note(ctx, &member.name);
    }
    outcome
}

fn extract_file(ctx: &mut Ctx, reader: &mut Reader, member: &Member, path: &str) -> Outcome {
    let parent = fsutil::dir_name(path);
    if parent != "." && !parent.is_empty() {
        if let Err(error) = fs::create_dir_all(parent) {
            ctx.warn_io(parent.as_bytes(), &error);
            return skip_body(ctx, reader, member.size, Outcome::Failed);
        }
    }
    if let Ok(meta) = fs::symlink_metadata(path) {
        if !meta.is_dir() {
            let _ = fs::remove_file(path);
        }
    }
    let mut file = match File::create(path) {
        Ok(file) => file,
        Err(error) => {
            ctx.warn_io(member.name.as_slice(), &error);
            return skip_body(ctx, reader, member.size, Outcome::Failed);
        }
    };
    match drain_body(reader, member.size, Some(&mut file)) {
        Ok(None) => {
            if let Err(error) = fsutil::chmod(path, member.mode & 0o7777) {
                ctx.warn_io(member.name.as_slice(), &error);
                return Outcome::Failed;
            }
            Outcome::Done
        }
        Ok(Some(error)) => {
            ctx.warn_io(member.name.as_slice(), &error);
            Outcome::Failed
        }
        Err(error) => {
            ctx.warn(io_message(&error).as_bytes());
            Outcome::Fatal
        }
    }
}

fn make_symlink(ctx: &mut Ctx, member: &Member, path: &str) -> Outcome {
    let parent = fsutil::dir_name(path);
    if parent != "." && !parent.is_empty() {
        if let Err(error) = fs::create_dir_all(parent) {
            ctx.warn_io(parent.as_bytes(), &error);
            return Outcome::Failed;
        }
    }
    if fs::symlink_metadata(path).is_ok() {
        let _ = fs::remove_file(path);
    }
    match fsutil::symlink(&member.link, path) {
        Ok(()) => Outcome::Done,
        Err(error) => {
            ctx.warn_io(member.name.as_slice(), &error);
            Outcome::Failed
        }
    }
}

fn note(ctx: &mut Ctx, name: &[u8]) {
    ctx.err.write(name);
    ctx.err.nl();
    ctx.err.flush();
}

fn list_long(ctx: &mut Ctx, member: &Member) {
    let mut modes = [b'-'; 10];
    modes[0] = match member.kind {
        b'5' => b'd',
        b'2' => b'l',
        b'1' => b'h',
        _ => b'-',
    };
    for group in 0..3 {
        let bits = (member.mode >> (6 - group * 3)) & 7;
        if bits & 4 != 0 {
            modes[1 + group * 3] = b'r';
        }
        if bits & 2 != 0 {
            modes[2 + group * 3] = b'w';
        }
        if bits & 1 != 0 {
            modes[3 + group * 3] = b'x';
        }
    }
    ctx.out.write(&modes);
    ctx.out.s(" root/root ");
    ctx.out.u_right(member.size, 10);
    ctx.out.b(b' ');
    stamp(ctx, member.mtime);
    ctx.out.b(b' ');
    ctx.out.write(&member.name);
    if member.kind == b'2' {
        ctx.out.s(" -> ");
        ctx.out.write(&member.link);
    }
    ctx.out.nl();
}

fn two(ctx: &mut Ctx, value: u64) {
    if value < 10 {
        ctx.out.b(b'0');
    }
    ctx.out.u(value);
}

/// `YYYY-MM-DD HH:MM` in UTC.
fn stamp(ctx: &mut Ctx, secs: u64) {
    let days = (secs / 86400) as i64;
    let rest = secs % 86400;
    let (year, month, day) = calendar::civil_from_days(days);

    ctx.out.u(year as u64);
    ctx.out.b(b'-');
    two(ctx, month as u64);
    ctx.out.b(b'-');
    two(ctx, day as u64);
    ctx.out.b(b' ');
    two(ctx, rest / 3600);
    ctx.out.b(b':');
    two(ctx, (rest % 3600) / 60);
}
