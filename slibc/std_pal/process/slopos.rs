#![deny(unsafe_op_in_unsafe_fn)]

use super::env::{CommandEnv, CommandEnvs, CommandResolvedEnvs};
pub use crate::ffi::OsString as EnvKey;
use crate::ffi::{OsStr, OsString};
use crate::num::NonZero;
use crate::path::Path;
use crate::process::StdioPipes;
use crate::sys::pipe::Pipe;
use crate::{fmt, io};

use crate::io::Read;

unsafe extern "C" {
    fn slopos_waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    fn slopos_pipe(fds: *mut i32) -> i32;
    fn close(fd: i32) -> i32;
    fn open(path: *const u8, flags: i32) -> i32;
    fn slopos_kill(pid: i32, sig: i32) -> i32;
    fn slopos_spawn_path(
        path: *const u8,
        path_len: usize,
        argv: *const *const u8,
        argc: u32,
        attrs: *const SpawnAttrs,
    ) -> i32;
    #[link_name = "getpid"]
    fn libc_getpid() -> i32;
}

const WNOHANG: i32 = 1;
const EINTR: i32 = 4;
const O_RDWR: i32 = 2;
const DEV_NULL: &[u8] = b"/dev/null\0";

/// `SpawnFdActionKind::CloneFd` / `TransferFd`.
const SPAWN_CLONE_FD: u32 = 1;
const SPAWN_TRANSFER_FD: u32 = 2;

/// `TaskPriority::Normal`.
const TASK_PRIORITY_NORMAL: u8 = 2;

/// Mirrors `slopos_abi::spawn::SpawnFdAction`; `std` cannot depend on the ABI
/// crate, so the asserts below pin the layout.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SpawnFdAction {
    kind: u32,
    src_fd: i32,
    target_fd: i32,
    _pad: u32,
    open_path_ptr: u64,
    open_path_len: u64,
    open_flags: u32,
    _pad2: u32,
}

/// Mirrors `slopos_abi::spawn::SpawnAttrs`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SpawnAttrs {
    priority: u8,
    _pad: [u8; 3],
    flags: u16,
    _pad2: u16,
    actions_ptr: u64,
    actions_len: u64,
    sigdefault_mask: u64,
    envp_ptr: u64,
    envp_len: u64,
    cwd_ptr: u64,
    cwd_len: u64,
}

const _: () = assert!(core::mem::size_of::<SpawnFdAction>() == 40);
const _: () = assert!(core::mem::size_of::<SpawnAttrs>() == 64);
const _: () = assert!(core::mem::offset_of!(SpawnAttrs, flags) == 4);
const _: () = assert!(core::mem::offset_of!(SpawnAttrs, envp_ptr) == 32);
const _: () = assert!(core::mem::offset_of!(SpawnAttrs, cwd_ptr) == 48);

pub fn getpid() -> u32 {
    unsafe { libc_getpid() as u32 }
}

pub struct Command {
    program: OsString,
    args: Vec<OsString>,
    env: CommandEnv,
    cwd: Option<OsString>,
    stdin: Option<Stdio>,
    stdout: Option<Stdio>,
    stderr: Option<Stdio>,
}

pub struct CommandArgs<'a> {
    iter: crate::slice::Iter<'a, OsString>,
}

impl<'a> fmt::Debug for CommandArgs<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter.clone()).finish()
    }
}

impl<'a> ExactSizeIterator for CommandArgs<'a> {
    fn len(&self) -> usize {
        self.iter.len()
    }
    fn is_empty(&self) -> bool {
        self.iter.is_empty()
    }
}

pub type ChildPipe = crate::sys::pipe::Pipe;

#[derive(Debug)]
#[allow(dead_code)]
pub enum Stdio {
    Inherit,
    Null,
    MakePipe,
    ParentStdout,
    ParentStderr,
    InheritFile(crate::sys::fs::File),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ExitStatus(i32);

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ExitStatusError(NonZero<i32>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitCode(u8);

#[derive(Debug, Clone, Copy)]
pub struct Process {
    pid: i32,
}

impl Command {
    pub fn new(program: &OsStr) -> Command {
        let program = program.to_os_string();
        Command {
            args: vec![program.clone()],
            program,
            env: CommandEnv::default(),
            cwd: None,
            stdin: None,
            stdout: None,
            stderr: None,
        }
    }

    pub fn arg(&mut self, arg: &OsStr) {
        self.args.push(arg.to_os_string());
    }

    pub fn env_mut(&mut self) -> &mut CommandEnv {
        &mut self.env
    }

    pub fn cwd(&mut self, dir: &Path) {
        self.cwd = Some(dir.as_os_str().to_os_string());
    }

    pub fn stdin(&mut self, stdin: Stdio) {
        self.stdin = Some(stdin);
    }

    pub fn stdout(&mut self, stdout: Stdio) {
        self.stdout = Some(stdout);
    }

    pub fn stderr(&mut self, stderr: Stdio) {
        self.stderr = Some(stderr);
    }

    pub fn get_program(&self) -> &OsStr {
        &self.program
    }

    pub fn get_args(&self) -> CommandArgs<'_> {
        CommandArgs {
            iter: self.args[1..].iter(),
        }
    }

    pub fn get_envs(&self) -> CommandEnvs<'_> {
        self.env.iter()
    }

    pub fn get_resolved_envs(&self) -> CommandResolvedEnvs {
        CommandResolvedEnvs::new(self.env.capture())
    }

    pub fn get_env_clear(&self) -> bool {
        self.env.is_unchanged()
    }

    pub fn get_current_dir(&self) -> Option<&Path> {
        self.cwd.as_ref().map(Path::new)
    }

    /// Everything is built in the parent because a forked child that allocated
    /// would deadlock: slibc's malloc is one global spinlock, and `fork` can
    /// copy it locked by a thread the child does not have.
    pub fn spawn(
        &mut self,
        default: Stdio,
        _needs_stdin: bool,
    ) -> io::Result<(Process, StdioPipes)> {
        let stdin_cfg = self.stdin.as_ref().unwrap_or(&default);
        let stdout_cfg = self.stdout.as_ref().unwrap_or(&default);
        let stderr_cfg = self.stderr.as_ref().unwrap_or(&default);

        let mut stdin_pipe = None;
        let mut stdout_pipe = None;
        let mut stderr_pipe = None;

        if matches!(stdin_cfg, Stdio::MakePipe) {
            stdin_pipe = Some(create_pipe()?);
        }
        if matches!(stdout_cfg, Stdio::MakePipe) {
            stdout_pipe = Some(create_pipe()?);
        }
        if matches!(stderr_cfg, Stdio::MakePipe) {
            stderr_pipe = Some(create_pipe()?);
        }

        let program = osstr_to_cstring_bytes(self.program.as_os_str());
        let argv_store: Vec<Vec<u8>> = self
            .args
            .iter()
            .map(|a| osstr_to_cstring_bytes(a.as_os_str()))
            .collect();
        let argv: Vec<*const u8> = argv_store.iter().map(|s| s.as_ptr()).collect();

        // The whole environment, never just the changes: the syscall reads a
        // zero `envp_ptr` as "no environment", not as "inherit".
        let mut env_store: Vec<Vec<u8>> = Vec::new();
        for (key, value) in self.env.capture() {
            let mut item = Vec::new();
            item.extend_from_slice(key.as_os_str().as_encoded_bytes());
            item.push(b'=');
            item.extend_from_slice(value.as_encoded_bytes());
            item.push(0);
            env_store.push(item);
        }
        let envp: Vec<*const u8> = env_store.iter().map(|s| s.as_ptr()).collect();

        let cwd_store = self
            .cwd
            .as_ref()
            .map(|dir| osstr_to_cstring_bytes(dir.as_os_str()));

        let mut actions: Vec<SpawnFdAction> = Vec::new();
        let mut opened: Vec<i32> = Vec::new();
        let staged = (|| -> io::Result<()> {
            push_stdio_action(&mut actions, &mut opened, 0, stdin_cfg, stdin_pipe)?;
            push_stdio_action(&mut actions, &mut opened, 1, stdout_cfg, stdout_pipe)?;
            push_stdio_action(&mut actions, &mut opened, 2, stderr_cfg, stderr_pipe)?;
            Ok(())
        })();

        let rc = match staged {
            Ok(()) => {
                let attrs = SpawnAttrs {
                    priority: TASK_PRIORITY_NORMAL,
                    _pad: [0; 3],
                    flags: 0,
                    _pad2: 0,
                    actions_ptr: actions.as_ptr() as u64,
                    actions_len: actions.len() as u64,
                    sigdefault_mask: 0,
                    envp_ptr: if envp.is_empty() {
                        0
                    } else {
                        envp.as_ptr() as u64
                    },
                    envp_len: envp.len() as u64,
                    cwd_ptr: cwd_store.as_ref().map_or(0, |c| c.as_ptr() as u64),
                    cwd_len: cwd_store.as_ref().map_or(0, |c| (c.len() - 1) as u64),
                };
                unsafe {
                    slopos_spawn_path(
                        program.as_ptr(),
                        program.len() - 1,
                        argv.as_ptr(),
                        argv.len() as u32,
                        &attrs as *const SpawnAttrs,
                    )
                }
            }
            Err(e) => {
                close_fds(&opened);
                close_pipe_pair(stdin_pipe);
                close_pipe_pair(stdout_pipe);
                close_pipe_pair(stderr_pipe);
                return Err(e);
            }
        };

        if rc < 0 {
            // Nothing was transferred, so the staged descriptors are still ours.
            close_fds(&opened);
            close_pipe_pair(stdin_pipe);
            close_pipe_pair(stdout_pipe);
            close_pipe_pair(stderr_pipe);
            return Err(errno_from_ret(rc));
        }

        let mut pipes = StdioPipes {
            stdin: None,
            stdout: None,
            stderr: None,
        };

        if let Some((read_end, write_end)) = stdin_pipe {
            unsafe {
                let _ = close(read_end);
            }
            pipes.stdin = Some(unsafe { Pipe::from_raw_fd(write_end) });
        }

        if let Some((read_end, write_end)) = stdout_pipe {
            unsafe {
                let _ = close(write_end);
            }
            pipes.stdout = Some(unsafe { Pipe::from_raw_fd(read_end) });
        }

        if let Some((read_end, write_end)) = stderr_pipe {
            unsafe {
                let _ = close(write_end);
            }
            pipes.stderr = Some(unsafe { Pipe::from_raw_fd(read_end) });
        }

        Ok((Process { pid: rc }, pipes))
    }
}

impl<'a> Iterator for CommandArgs<'a> {
    type Item = &'a OsStr;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next().map(OsString::as_os_str)
    }
}

impl ExitStatus {
    pub fn exit_ok(&self) -> Result<(), ExitStatusError> {
        if self.exited() && self.code() == Some(0) {
            Ok(())
        } else {
            let status = if self.0 == 0 { 1 } else { self.0 };
            let nonzero = NonZero::new(status).expect("status must be non-zero");
            Err(ExitStatusError(nonzero))
        }
    }

    pub fn code(&self) -> Option<i32> {
        if self.exited() {
            Some((self.0 >> 8) & 0xff)
        } else {
            None
        }
    }

    fn exited(&self) -> bool {
        (self.0 & 0x7f) == 0
    }

    fn signaled(&self) -> bool {
        (((self.0 & 0x7f) + 1) >> 1) > 0
    }

    fn signal(&self) -> i32 {
        self.0 & 0x7f
    }
}

impl ExitStatusError {
    pub fn code(&self) -> Option<NonZero<i32>> {
        NonZero::new((self.0.get() >> 8) & 0xff)
    }
}

impl Into<ExitStatus> for ExitStatusError {
    fn into(self) -> ExitStatus {
        ExitStatus(self.0.get())
    }
}

impl ExitCode {
    pub const SUCCESS: ExitCode = ExitCode(0);
    pub const FAILURE: ExitCode = ExitCode(1);

    pub fn as_i32(self) -> i32 {
        self.0 as i32
    }
}

impl From<u8> for ExitCode {
    fn from(value: u8) -> Self {
        ExitCode(value)
    }
}

impl Process {
    pub fn id(&self) -> u32 {
        self.pid as u32
    }

    pub fn kill(&mut self) -> io::Result<()> {
        let rc = unsafe { slopos_kill(self.pid, 9) };
        if rc < 0 {
            Err(errno_from_ret(rc))
        } else {
            Ok(())
        }
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        let mut status: i32 = 0;
        loop {
            let rc = unsafe { slopos_waitpid(self.pid, &mut status as *mut i32, 0) };
            if rc == -EINTR {
                continue;
            }
            if rc < 0 {
                return Err(errno_from_ret(rc));
            }
            return Ok(ExitStatus(status));
        }
    }

    /// A zero return under `WNOHANG` means no child is ready, not a child that
    /// exited with code 0.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let mut status: i32 = 0;
        let rc = unsafe { slopos_waitpid(self.pid, &mut status as *mut i32, WNOHANG) };
        if rc < 0 {
            return Err(errno_from_ret(rc));
        }
        if rc == 0 {
            return Ok(None);
        }
        Ok(Some(ExitStatus(status)))
    }
}

pub fn output(cmd: &mut Command) -> io::Result<(ExitStatus, Vec<u8>, Vec<u8>)> {
    let (mut process, pipes) = cmd.spawn(Stdio::MakePipe, false)?;
    drop(pipes.stdin);

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    match (pipes.stdout, pipes.stderr) {
        (Some(out), Some(err)) => read_output(out, &mut stdout, err, &mut stderr)?,
        (Some(mut out), None) => {
            out.read_to_end(&mut stdout)?;
        }
        (None, Some(mut err)) => {
            err.read_to_end(&mut stderr)?;
        }
        (None, None) => {}
    }

    let status = process.wait()?;
    Ok((status, stdout, stderr))
}

pub fn read_output(
    mut out: ChildPipe,
    stdout: &mut Vec<u8>,
    mut err: ChildPipe,
    stderr: &mut Vec<u8>,
) -> io::Result<()> {
    out.read_to_end(stdout)?;
    err.read_to_end(stderr)?;
    Ok(())
}

impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Command")
            .field("program", &self.program)
            .field("args", &self.args)
            .finish()
    }
}

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.exited() {
            write!(f, "exit status: {}", (self.0 >> 8) & 0xff)
        } else if self.signaled() {
            write!(f, "signal: {}", self.signal())
        } else {
            write!(f, "exit status: {}", self.0)
        }
    }
}

impl fmt::Debug for ExitStatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ExitStatusError").field(&self.0).finish()
    }
}

impl From<ChildPipe> for Stdio {
    fn from(_pipe: ChildPipe) -> Self {
        Stdio::MakePipe
    }
}

impl From<io::Stdout> for Stdio {
    fn from(_: io::Stdout) -> Self {
        Stdio::ParentStdout
    }
}

impl From<io::Stderr> for Stdio {
    fn from(_: io::Stderr) -> Self {
        Stdio::ParentStderr
    }
}

impl From<crate::sys::fs::File> for Stdio {
    fn from(file: crate::sys::fs::File) -> Self {
        Stdio::InheritFile(file)
    }
}

fn osstr_to_cstring_bytes(s: &OsStr) -> Vec<u8> {
    let mut bytes = s.as_encoded_bytes().to_vec();
    if bytes.last().copied() != Some(0) {
        bytes.push(0);
    }
    bytes
}

fn errno_from_ret(ret: i32) -> io::Error {
    io::Error::from_raw_os_error(-ret)
}

fn create_pipe() -> io::Result<(i32, i32)> {
    let mut fds = [0_i32; 2];
    let rc = unsafe { slopos_pipe(fds.as_mut_ptr()) };
    if rc < 0 {
        Err(errno_from_ret(rc))
    } else {
        Ok((fds[0], fds[1]))
    }
}

fn close_pipe_pair(pipe: Option<(i32, i32)>) {
    if let Some((a, b)) = pipe {
        unsafe {
            let _ = close(a);
            let _ = close(b);
        }
    }
}

fn close_fds(fds: &[i32]) {
    for &fd in fds {
        unsafe {
            let _ = close(fd);
        }
    }
}

/// The child's descriptor table starts empty, so even an inherited fd is an
/// explicit `CloneFd`.
fn push_stdio_action(
    actions: &mut Vec<SpawnFdAction>,
    opened: &mut Vec<i32>,
    target_fd: i32,
    stdio: &Stdio,
    pipe: Option<(i32, i32)>,
) -> io::Result<()> {
    let (kind, src_fd) = match stdio {
        Stdio::MakePipe => match pipe {
            Some((read_end, write_end)) => {
                let chosen = if target_fd == 0 { read_end } else { write_end };
                (SPAWN_CLONE_FD, chosen)
            }
            None => return Ok(()),
        },
        Stdio::Inherit => (SPAWN_CLONE_FD, target_fd),
        Stdio::ParentStdout => (SPAWN_CLONE_FD, 1),
        Stdio::ParentStderr => (SPAWN_CLONE_FD, 2),
        Stdio::InheritFile(file) => (SPAWN_CLONE_FD, file.as_raw_fd()),
        Stdio::Null => {
            // The `Open` action kind is retired, so the parent opens it.
            let fd = unsafe { open(DEV_NULL.as_ptr(), O_RDWR) };
            if fd < 0 {
                return Err(errno_from_ret(fd));
            }
            opened.push(fd);
            (SPAWN_TRANSFER_FD, fd)
        }
    };

    actions.push(SpawnFdAction {
        kind,
        src_fd,
        target_fd,
        ..Default::default()
    });
    Ok(())
}
