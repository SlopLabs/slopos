//! The requests a connection carries: EXEC, PUT and GET.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use slopos_abi::signal::SIGKILL;
use slopos_remote_core::{CHUNK, Digest256, Fields, hex, kind};

use super::link::{Frame, Link};
use crate::syscall::process;
use crate::tls;

/// A child's whole environment, besides what the request adds.
const DEFAULT_PATH: &str = "/bin:/sbin:/usr/local/bin";

pub(super) fn serve(link: &Link, (request, payload): Frame) {
    let result = Fields::decode(&payload)
        .map_err(|e| e.to_string())
        .and_then(|fields| match request {
            kind::EXEC => exec(link, &fields),
            kind::PUT => put(link, &fields),
            kind::GET => get(link, &fields),
            other => Err(format!("unknown request {other:#04x}")),
        });
    if let Err(msg) = result {
        let _ = link.send(kind::ERROR, &Fields::new().with("msg", msg).encode());
    }
    link.finish();
}

fn absolute<'a>(fields: &'a Fields, key: &str) -> Result<&'a str, String> {
    let path = fields
        .text(key)
        .ok_or_else(|| format!("the request names no {key}"))?;
    if !path.starts_with('/') {
        return Err(format!("{path}: only absolute paths"));
    }
    Ok(path)
}

fn send(link: &Link, kind: u8, fields: Fields) -> Result<(), String> {
    link.send(kind, &fields.encode())
        .map_err(|e| format!("the link ended: {e}"))
}

/// The file `program` names: itself when it holds a slash, else the first
/// regular file of that name along `path`. Resolved here so the spawn is by
/// absolute path, which is what the kernel's program grants key on.
fn resolve(program: &OsStr, path: &OsStr) -> Option<PathBuf> {
    if program.as_bytes().contains(&b'/') {
        return Some(PathBuf::from(program));
    }
    path.as_bytes()
        .split(|&b| b == b':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(OsStr::from_bytes(dir)).join(program))
        .find(|candidate| candidate.is_file())
}

fn exec(link: &Link, fields: &Fields) -> Result<(), String> {
    let argv: Vec<OsString> = fields
        .all("arg")
        .map(|arg| OsStr::from_bytes(arg).to_owned())
        .collect();
    let (program, args) = argv.split_first().ok_or("EXEC names no program")?;
    let cwd = match fields.get("cwd") {
        Some(_) => absolute(fields, "cwd")?,
        None => "/",
    };
    let mut env: Vec<(&OsStr, &OsStr)> = vec![(OsStr::new("PATH"), OsStr::new(DEFAULT_PATH))];
    for pair in fields.all("env") {
        let at = pair
            .iter()
            .position(|&b| b == b'=')
            .filter(|&at| at > 0)
            .ok_or("an env override is not NAME=value")?;
        let (name, value) = (
            OsStr::from_bytes(&pair[..at]),
            OsStr::from_bytes(&pair[at + 1..]),
        );
        env.retain(|(n, _)| *n != name);
        env.push((name, value));
    }
    let path = env
        .iter()
        .find(|(name, _)| *name == "PATH")
        .map_or(OsStr::new(""), |&(_, value)| value);
    let image = resolve(program, path)
        .ok_or_else(|| format!("{}: command not found", program.to_string_lossy()))?;
    let mut cmd = Command::new(image);
    cmd.arg0(program)
        .args(args)
        .env_clear()
        .envs(env)
        .current_dir(cwd)
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let wants_stdin = fields.get("stdin") == Some(b"1");
    cmd.stdin(if wants_stdin {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let timeout = fields
        .number("timeout_ms")
        .filter(|&ms| ms > 0)
        .map(Duration::from_millis);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("{}: {e}", program.to_string_lossy()))?;
    let pid = child.id();
    let group = pid as i32;
    let kill_group = || {
        let _ = process::kill_pid(-group, SIGKILL);
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdin = child.stdin.take();
    if send(
        link,
        kind::ACCEPT,
        Fields::new().with("pid", pid.to_string()),
    )
    .is_err()
    {
        kill_group();
        let _ = child.wait();
        return Ok(());
    }

    let reaped = &AtomicBool::new(false);
    let timed_out = &AtomicBool::new(false);
    thread::scope(|scope| {
        let out = scope.spawn(|| pump(link, stdout, kind::STDOUT));
        let err = scope.spawn(|| pump(link, stderr, kind::STDERR));
        // Feeds stdin and notices the broker going away, which ends the
        // command: nobody is left to hear how it went. It returns once the
        // broker closes after EXIT.
        scope.spawn(|| {
            let mut stdin = stdin;
            if !feed(link, &mut stdin) && !reaped.load(Ordering::Acquire) {
                kill_group();
            }
        });
        let timer = timeout.map(|limit| {
            scope.spawn(move || {
                let deadline = Instant::now() + limit;
                while !reaped.load(Ordering::Acquire) {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        timed_out.store(true, Ordering::Release);
                        kill_group();
                        return;
                    }
                    thread::park_timeout(left);
                }
            })
        });
        let status = child.wait();
        reaped.store(true, Ordering::Release);
        if let Some(timer) = &timer {
            timer.thread().unpark();
        }
        let _ = out.join();
        let _ = err.join();
        // Answered here, inside the scope: the stdin thread returns only
        // once the broker has its answer and closes.
        let (answer, fields) = match status {
            Ok(status) => {
                let mut exit = Fields::new();
                match (status.code(), status.signal()) {
                    (Some(code), _) => exit.push("code", code.to_string()),
                    (None, Some(signal)) => exit.push("signal", signal.to_string()),
                    (None, None) => exit.push("code", "-1"),
                }
                if timed_out.load(Ordering::Acquire) {
                    exit.push("timed_out", "1");
                }
                (kind::EXIT, exit)
            }
            Err(e) => (
                kind::ERROR,
                Fields::new().with("msg", format!("waiting for {pid}: {e}")),
            ),
        };
        let _ = link.send(answer, &fields.encode());
        link.finish();
    });
    Ok(())
}

fn pump(link: &Link, mut from: impl Read, kind: u8) {
    let mut buf = vec![0u8; CHUNK];
    loop {
        match from.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                if link.send(kind, &buf[..n]).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// Copy the broker's DATA to the child's stdin, closing it at EOF; `false`
/// once the link is gone or the broker breaks the protocol.
fn feed(link: &Link, stdin: &mut Option<ChildStdin>) -> bool {
    loop {
        match link.recv() {
            Ok((kind::DATA, bytes)) => {
                // A child that closed its stdin takes no more; the rest is dropped.
                if let Some(pipe) = stdin
                    && pipe.write_all(&bytes).is_err()
                {
                    *stdin = None;
                }
            }
            Ok((kind::EOF, _)) => *stdin = None,
            Ok(_) | Err(_) => return false,
        }
    }
}

/// Removes a half-written file unless the transfer finished.
struct Scratch<'a>(Option<&'a str>);

impl Drop for Scratch<'_> {
    fn drop(&mut self) {
        if let Some(path) = self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

/// Write the DATA that follows into a file beside `path`, then rename it
/// over `path`: a reader sees the old file or the whole new one.
fn put(link: &Link, fields: &Fields) -> Result<(), String> {
    let path = absolute(fields, "path")?;
    let mode = match fields.text("mode") {
        Some(octal) => {
            u32::from_str_radix(octal, 8).map_err(|_| format!("mode {octal} is not octal"))?
        }
        None => 0o644,
    } & 0o7777;
    let target = Path::new(path);
    let name = target
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| format!("{path}: names no file"))?;
    let dir = target
        .parent()
        .and_then(Path::to_str)
        .filter(|d| !d.is_empty())
        .unwrap_or("/");
    if target.is_dir() {
        return Err(format!("{path}: is a directory"));
    }
    let mut salt = [0u8; 4];
    tls::random(&mut salt);
    let temp = format!(
        "{}/.{name}.remoted-{}",
        dir.trim_end_matches('/'),
        hex(&salt)
    );
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&temp)
        .map_err(|e| format!("{temp}: {e}"))?;
    let mut scratch = Scratch(Some(&temp));
    file.set_permissions(Permissions::from_mode(mode))
        .map_err(|e| format!("{temp}: {e}"))?;
    send(link, kind::ACCEPT, Fields::new())?;

    let mut digest = Digest256::new();
    let mut size = 0u64;
    loop {
        match link.recv() {
            Ok((kind::DATA, bytes)) => {
                file.write_all(&bytes).map_err(|e| format!("{temp}: {e}"))?;
                digest.update(&bytes);
                size += bytes.len() as u64;
            }
            Ok((kind::EOF, _)) => break,
            Ok((other, _)) => return Err(format!("frame {other:#04x} in the middle of a PUT")),
            Err(why) => return Err(format!("the link ended mid-transfer: {why}")),
        }
    }
    file.sync_all().map_err(|e| format!("{temp}: {e}"))?;
    drop(file);
    fs::rename(&temp, path).map_err(|e| format!("{path}: {e}"))?;
    scratch.0 = None;
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| format!("{dir}: {e}"))?;
    send(
        link,
        kind::DONE,
        Fields::new()
            .with("size", size.to_string())
            .with("sha256", digest.finish_hex()),
    )
}

fn get(link: &Link, fields: &Fields) -> Result<(), String> {
    let path = absolute(fields, "path")?;
    let mut file = File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let meta = file.metadata().map_err(|e| format!("{path}: {e}"))?;
    if meta.is_dir() {
        return Err(format!("{path}: is a directory"));
    }
    send(
        link,
        kind::ACCEPT,
        Fields::new().with("size", meta.len().to_string()),
    )?;
    let mut digest = Digest256::new();
    let mut size = 0u64;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("{path}: {e}")),
        };
        digest.update(&buf[..n]);
        size += n as u64;
        link.send(kind::DATA, &buf[..n])
            .map_err(|e| format!("the link ended: {e}"))?;
    }
    link.send(kind::EOF, b"")
        .map_err(|e| format!("the link ended: {e}"))?;
    send(
        link,
        kind::DONE,
        Fields::new()
            .with("size", size.to_string())
            .with("sha256", digest.finish_hex()),
    )
}
