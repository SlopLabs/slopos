//! `gzip`, `gunzip` and `zcat`: RFC 1952 members over the DEFLATE engine.

use std::fs::{self, File};
use std::io::Write;

use super::input::{self, as_str};
use super::opts::{Opt, Opts};
use super::{Ctx, Tool, deflate, fsutil};

pub static TOOLS: &[Tool] = &[
    Tool {
        name: "gzip",
        desc: "Compress files",
        usage: GZIP_USAGE,
        run: gzip,
    },
    Tool {
        name: "gunzip",
        desc: "Decompress gzip files",
        usage: GUNZIP_USAGE,
        run: gunzip,
    },
    Tool {
        name: "zcat",
        desc: "Decompress gzip files to standard output",
        usage: ZCAT_USAGE,
        run: zcat,
    },
];

const GZIP_USAGE: &str = "gzip [-dckfv] [-1..-9] [file...]";
const GUNZIP_USAGE: &str = "gunzip [-ckfv] [file...]";
const ZCAT_USAGE: &str = "zcat [file...]";
const DEFAULT_LEVEL: u8 = 6;

struct Config {
    decompress: bool,
    to_stdout: bool,
    keep: bool,
    force: bool,
    verbose: bool,
    level: u8,
}

fn gzip(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let config = Config {
        decompress: false,
        to_stdout: false,
        keep: false,
        force: false,
        verbose: false,
        level: DEFAULT_LEVEL,
    };
    run(ctx, argv, config, "dckfv123456789", GZIP_USAGE)
}

fn gunzip(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let config = Config {
        decompress: true,
        to_stdout: false,
        keep: false,
        force: false,
        verbose: false,
        level: DEFAULT_LEVEL,
    };
    run(ctx, argv, config, "ckfv", GUNZIP_USAGE)
}

fn zcat(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let config = Config {
        decompress: true,
        to_stdout: true,
        keep: true,
        force: true,
        verbose: false,
        level: DEFAULT_LEVEL,
    };
    run(ctx, argv, config, "f", ZCAT_USAGE)
}

fn run(
    ctx: &mut Ctx,
    argv: &[&[u8]],
    mut config: Config,
    spec: &'static str,
    usage: &'static str,
) -> i32 {
    let mut opts = Opts::new(argv, spec);
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'd') => config.decompress = true,
            Opt::Flag(b'c') => config.to_stdout = true,
            Opt::Flag(b'k') => config.keep = true,
            Opt::Flag(b'f') => config.force = true,
            Opt::Flag(b'v') => config.verbose = true,
            // All nine levels are accepted; the encoder emits fixed-Huffman
            // blocks at every one and the level only sets how far the match
            // finder searches.
            Opt::Flag(digit @ b'1'..=b'9') => config.level = digit - b'0',
            Opt::Long(name, _) => {
                ctx.warn_at(name, b"invalid option");
                return ctx.usage(usage);
            }
            Opt::Unknown(flag) => {
                ctx.warn_at(&[flag], b"invalid option");
                return ctx.usage(usage);
            }
            Opt::Missing(flag) => {
                ctx.warn_at(&[flag], b"option requires an argument");
                return ctx.usage(usage);
            }
            _ => {}
        }
    }

    let operands = opts.operands();
    if operands.is_empty() {
        return filter(ctx, &config);
    }
    let mut status = 0;
    for operand in operands {
        if !one(ctx, &config, operand) {
            status = 1;
        }
    }
    status
}

fn filter(ctx: &mut Ctx, config: &Config) -> i32 {
    let mut stdin = input::Stdin;
    let data = match input::read_to_end(&mut stdin) {
        Ok(data) => data,
        Err(error) => {
            ctx.warn_io(b"stdin", &error);
            return 1;
        }
    };
    if config.decompress {
        match deflate::gzip_unwrap(&data) {
            Ok((out, _)) => {
                ctx.out.write(&out);
                0
            }
            Err(error) => {
                ctx.warn_at(b"stdin", error.message().as_bytes());
                1
            }
        }
    } else {
        let framed = deflate::gzip_wrap(&data, config.level, None, 0);
        ctx.out.write(&framed);
        0
    }
}

fn one(ctx: &mut Ctx, config: &Config, operand: &[u8]) -> bool {
    if operand == b"-" {
        return filter(ctx, config) == 0;
    }
    let Some(path) = as_str(ctx, operand) else {
        return false;
    };

    // The output name is settled before a byte is read, so a name that cannot
    // yield one is refused without the work of decompressing it.
    let target = if config.to_stdout {
        None
    } else if config.decompress {
        match output_name(path) {
            Some(name) => Some(name),
            None => {
                ctx.warn_at(operand, b"unknown suffix, no output name");
                return false;
            }
        }
    } else if compressed_name(path) && !config.force {
        ctx.warn_at(operand, b"already has a gzip suffix, unchanged");
        return false;
    } else {
        Some(format!("{path}.gz"))
    };

    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) => {
            ctx.warn_io(operand, &error);
            return false;
        }
    };
    let data = match input::read_to_end(&mut file) {
        Ok(data) => data,
        Err(error) => {
            ctx.warn_io(operand, &error);
            return false;
        }
    };

    let payload = if config.decompress {
        match deflate::gzip_unwrap(&data) {
            Ok((out, _)) => out,
            Err(error) => {
                ctx.warn_at(operand, error.message().as_bytes());
                return false;
            }
        }
    } else {
        let name = fsutil::base_name(path).as_bytes().to_vec();
        deflate::gzip_wrap(&data, config.level, Some(&name), mtime_of(path))
    };

    match &target {
        None => ctx.out.write(&payload),
        Some(out_path) => {
            if !config.force && fs::symlink_metadata(out_path).is_ok() {
                ctx.warn_at(out_path.as_bytes(), b"already exists; use -f to overwrite");
                return false;
            }
            let mut out = match File::create(out_path) {
                Ok(out) => out,
                Err(error) => {
                    ctx.warn_io(out_path.as_bytes(), &error);
                    return false;
                }
            };
            if let Err(error) = out.write_all(&payload) {
                ctx.warn_io(out_path.as_bytes(), &error);
                return false;
            }
            if !config.keep {
                if let Err(error) = fs::remove_file(path) {
                    ctx.warn_io(operand, &error);
                    return false;
                }
            }
        }
    }
    if config.verbose {
        // The figure quoted is always the compression ratio, so `-d` reports
        // the same number the compressing run did.
        let (plain, packed) = if config.decompress {
            (payload.len(), data.len())
        } else {
            (data.len(), payload.len())
        };
        report(ctx, operand, plain, packed, target.as_deref());
    }
    true
}

fn mtime_of(path: &str) -> u32 {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
        .map(|since| since.as_secs() as u32)
        .unwrap_or(0)
}

fn compressed_name(path: &str) -> bool {
    path.ends_with(".gz") || path.ends_with("-gz") || path.ends_with(".tgz")
}

/// The name a decompressed file takes. Without a recognised suffix there is no
/// output name, and the input is refused rather than overwritten.
fn output_name(path: &str) -> Option<String> {
    for suffix in [".gz", "-gz"] {
        if let Some(stem) = path.strip_suffix(suffix) {
            if !stem.is_empty() {
                return Some(stem.to_string());
            }
        }
    }
    for suffix in [".tgz", ".taz"] {
        if let Some(stem) = path.strip_suffix(suffix) {
            if !stem.is_empty() {
                return Some(format!("{stem}.tar"));
            }
        }
    }
    None
}

fn report(ctx: &mut Ctx, operand: &[u8], plain: usize, packed: usize, target: Option<&str>) {
    ctx.err.write(operand);
    ctx.err.s(":\t");
    if plain == 0 {
        ctx.err.s("0.0%");
    } else {
        let tenths = (plain as i64 - packed as i64) * 1000 / plain as i64;
        if tenths < 0 {
            ctx.err.b(b'-');
        }
        let size = tenths.unsigned_abs();
        ctx.err.u(size / 10);
        ctx.err.b(b'.');
        ctx.err.u(size % 10);
        ctx.err.b(b'%');
    }
    match target {
        Some(name) => {
            ctx.err.s(" -- replaced with ");
            ctx.err.s(name);
        }
        None => ctx.err.s(" -- standard output"),
    }
    ctx.err.nl();
    ctx.err.flush();
}
