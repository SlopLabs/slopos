//! The window's way out: privd, which starts the engine on two pipes and
//! restarts the machine, and the engine's frames.

use slopos_abi::fs::O_RDWR;
use slopos_abi::syscall::O_CLOEXEC;
use slopos_appkit::AppSender;
use slopos_installer_core::protocol::{self, Reply, Request};
use slopos_priv_core::wire::{Reply as PrivdReply, Request as PrivdRequest};

use super::{EngineSays, Msg};
use crate::privd::{self, Connection};
use crate::syscall::{OwnedFd, SyscallError, fs};

const INSTALL: &str = "org.slopos.install";
const REBOOT: &str = "org.slopos.power.reboot";

/// What the window asks of the world beyond it.
pub trait Link {
    /// Ask privd for the engine; what privd and then the engine say comes
    /// back as [`Msg::Engine`].
    fn start(&mut self, sender: &AppSender<Msg>);
    fn send(&mut self, request: &Request);
    /// Ask privd to restart the machine; a refusal comes back as
    /// [`Msg::RestartRefused`].
    fn restart(&mut self, sender: &AppSender<Msg>);
}

/// The engine as privd runs it: `installer --serve` on two pipes.
#[derive(Default)]
pub struct Privd {
    to: Option<OwnedFd>,
}

impl Link for Privd {
    fn start(&mut self, sender: &AppSender<Msg>) {
        let pipe = || fs::pipe2(O_CLOEXEC as u32).map_err(|e| format!("pipe: {e}"));
        let ((engine_in, to), (from, engine_out)) = match pipe().and_then(|a| Ok((a, pipe()?))) {
            Ok(pipes) => pipes,
            Err(why) => return sender.send(Msg::Engine(EngineSays::Refused(why))),
        };
        self.to = Some(to);
        let sender = sender.clone();
        std::thread::spawn(move || match ask(engine_in, engine_out, &sender) {
            Ok(connection) => {
                relay(from, &sender);
                drop(connection);
            }
            Err(why) => sender.send(Msg::Engine(EngineSays::Refused(why))),
        });
    }

    fn send(&mut self, request: &Request) {
        let Some(to) = &self.to else {
            return;
        };
        let frame = request.frame();
        let mut sent = 0;
        while sent < frame.len() {
            match fs::write_slice(to.raw(), &frame[sent..]) {
                Ok(n) if n > 0 => sent += n,
                Err(SyscallError::EINTR) => {}
                _ => return,
            }
        }
    }

    fn restart(&mut self, sender: &AppSender<Msg>) {
        let sender = sender.clone();
        std::thread::spawn(move || {
            let ran = null().and_then(|null| {
                privd::run_feeding(REBOOT, &["reboot"], &[], null.raw(), null.raw())
            });
            let why = match ran {
                Ok(status) => format!("the restart exited {status}"),
                Err(why) => why,
            };
            sender.send(Msg::RestartRefused(why));
        });
    }
}

/// Where a mechanism's output goes: this window's own may be a terminal, and
/// a mechanism runs outside its session, where a terminal answers `EIO`.
fn null() -> Result<OwnedFd, String> {
    fs::open_cstr(c"/dev/null", O_RDWR).map_err(|e| format!("/dev/null: {e}"))
}

/// Waits on the person's consent; the connection is held while the engine
/// runs.
fn ask(
    engine_in: OwnedFd,
    engine_out: OwnedFd,
    sender: &AppSender<Msg>,
) -> Result<Connection, String> {
    let mut connection = Connection::open_within(privd::RESTART_WAIT)
        .map_err(|why| format!("cannot reach privd: {why}"))?;
    let request = PrivdRequest {
        id: privd::request_id(),
        action: INSTALL.into(),
        args: vec![b"--serve".to_vec()],
        cwd: false,
        terminal: false,
    };
    let null = null()?;
    connection.send(&request, &[engine_in.raw(), engine_out.raw(), null.raw()])?;
    drop((engine_in, engine_out));
    loop {
        match connection.reply(request.id)? {
            Some(PrivdReply::Asking { .. }) => sender.send(Msg::Engine(EngineSays::Asking)),
            Some(PrivdReply::Started { .. }) => {
                sender.send(Msg::Engine(EngineSays::Started));
                return Ok(connection);
            }
            Some(PrivdReply::Denied { reason, .. }) => return Err(reason),
            Some(PrivdReply::Exited(_)) => return Err("the installer exited at once".into()),
            Some(PrivdReply::Prompt { .. }) => {
                return Err("privd asked for a password at a terminal".into());
            }
            None => return Err("privd closed the connection".into()),
        }
    }
}

/// Send each reply the engine writes to `from` that it can read, then
/// [`EngineSays::Ended`] once its output ends. Past framing it cannot follow,
/// the rest is read and dropped: the engine may still be writing a disk.
pub fn relay(from: OwnedFd, sender: &AppSender<Msg>) {
    let mut replies = Some(protocol::replies());
    let mut buf = [0u8; 4096];
    loop {
        if let Some(frames) = &mut replies {
            match frames.next_frame() {
                Ok(Some((kind, payload))) => {
                    if let Ok(reply) = Reply::decode(kind, &payload) {
                        sender.send(Msg::Engine(EngineSays::Reply(reply)));
                    }
                    continue;
                }
                Ok(None) => {}
                Err(_) => replies = None,
            }
        }
        match fs::read_slice(from.raw(), &mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if let Some(frames) = &mut replies {
                    frames.push(&buf[..n]);
                }
            }
            Err(SyscallError::EINTR) => {}
            Err(_) => break,
        }
    }
    sender.send(Msg::Engine(EngineSays::Ended));
}
