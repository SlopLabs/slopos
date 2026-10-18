//! The consent channel between privd and the compositor: its messages, the
//! questions privd keeps for it, and the dialog the compositor draws.

mod dialog;
mod queue;

pub use dialog::{ARMING_MS, Button, Dialog, Key};
pub use queue::{DECLINED_MS, Questions, WAITING_MAX};

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use slopos_remote_core::{Fields, frame};

use crate::policy::valid_name;
use crate::wire::{MAX_ARGS, Malformed, known, number};

/// How long a question on screen waits for the person before it is a refusal.
pub const EXPIRY_MS: u64 = 120_000;
/// The longest command line, and the longest directory, a question may carry:
/// the person sees all of it or is not asked.
pub const MAX_SHOWN: usize = 320;

pub mod kind {
    pub const READY: u8 = 0x20;
    pub const ASK: u8 = 0x21;
    pub const ANSWER: u8 = 0x22;
    pub const CANCEL: u8 = 0x23;
    pub const RESET: u8 = 0x24;
    pub const RESET_SEEN: u8 = 0x25;
}

/// One end of the channel across restarts of either side. Init keeps the pair
/// for the boot, so neither end ever reads end of file, and a new instance may
/// find frames written for the one before it.
///
/// A new instance first sends `RESET` naming an epoch of its own, and acts on
/// nothing until a `RESET_SEEN` naming that epoch comes back: what precedes it
/// was written for its predecessor. A side that reads a `RESET` drops what it
/// had pending and answers it.
pub struct Link {
    epoch: u64,
    synced: bool,
    /// The other side's epoch, once its `RESET` is answered.
    peer: Option<u64>,
}

/// What a frame from the other side asks of this one.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Inbound {
    Frame(u8, Vec<u8>),
    /// The other side is new: drop what was pending, then send it this, the
    /// acknowledgement and, while this side's own hello is unanswered, the
    /// hello again, which init may have discarded unread.
    Reset(Vec<u8>),
    /// The other side said its hello again: send it this and nothing more.
    Again(Vec<u8>),
    /// Written for a predecessor, or an acknowledgement.
    Stale,
}

impl Link {
    /// `epoch` must differ from every earlier instance's at this end.
    pub fn new(epoch: u64) -> Link {
        Link {
            epoch,
            synced: false,
            peer: None,
        }
    }

    /// What a new instance sends before anything else.
    pub fn hello(&self) -> Vec<u8> {
        reset_frame(self.epoch)
    }

    pub fn take(&mut self, kind: u8, payload: Vec<u8>) -> Result<Inbound, Malformed> {
        match kind {
            kind::RESET => {
                let peer = decode_epoch(&payload)?;
                let mut reply = frame(kind::RESET_SEEN, &epoch_fields(peer).encode());
                if !self.synced {
                    reply.extend(self.hello());
                }
                if self.peer.replace(peer) == Some(peer) {
                    return Ok(Inbound::Again(reply));
                }
                Ok(Inbound::Reset(reply))
            }
            kind::RESET_SEEN => {
                self.synced |= decode_epoch(&payload)? == self.epoch;
                Ok(Inbound::Stale)
            }
            _ if self.synced => Ok(Inbound::Frame(kind, payload)),
            _ => Ok(Inbound::Stale),
        }
    }
}

/// A `RESET` for `epoch`. Init sends one on behalf of a side that has exited,
/// so the other drops what it was waiting on at once.
pub fn reset_frame(epoch: u64) -> Vec<u8> {
    frame(kind::RESET, &epoch_fields(epoch).encode())
}

fn epoch_fields(epoch: u64) -> Fields {
    Fields::new().with("epoch", epoch.to_string())
}

fn decode_epoch(payload: &[u8]) -> Result<u64, Malformed> {
    number(&known(payload, &["epoch"])?, "epoch")
}

/// One question: everything in it comes from the sealed policy, an
/// extension's sealed declaration, or the kernel's account of the requester,
/// bar the argv the policy matched.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Ask {
    pub id: u64,
    pub action: String,
    pub describe: String,
    /// The requester's program as the kernel attests it, a base program or
    /// an extension's; `None` for any other.
    pub program: Option<String>,
    pub uid: u32,
    pub pid: u32,
    /// Where the mechanism runs, which a relative argument names a file in.
    pub cwd: String,
    /// The mechanism's program, then its arguments.
    pub command: Vec<Vec<u8>>,
}

impl Ask {
    pub fn frame(&self) -> Vec<u8> {
        let mut fields = Fields::new()
            .with("id", self.id.to_string())
            .with("action", &self.action)
            .with("describe", &self.describe)
            .with("uid", self.uid.to_string())
            .with("pid", self.pid.to_string())
            .with("cwd", &self.cwd);
        if let Some(program) = &self.program {
            fields.push("program", program);
        }
        for word in &self.command {
            fields.push("arg", word);
        }
        frame(kind::ASK, &fields.encode())
    }

    pub fn decode(payload: &[u8]) -> Result<Ask, Malformed> {
        let fields = known(
            payload,
            &[
                "id", "action", "describe", "uid", "pid", "cwd", "program", "arg",
            ],
        )?;
        let text = |key| match (fields.text(key), fields.all(key).count()) {
            (Some(text), 1) => Ok(text.to_string()),
            _ => Err(Malformed),
        };
        let action = text("action")?;
        if !valid_name(&action) {
            return Err(Malformed);
        }
        let program = match fields.all("program").count() {
            0 => None,
            _ => Some(text("program")?),
        };
        let command: Vec<Vec<u8>> = fields.all("arg").map(<[u8]>::to_vec).collect();
        if command.is_empty() || command.len() > MAX_ARGS + 1 {
            return Err(Malformed);
        }
        let ask = Ask {
            id: number(&fields, "id")?,
            action,
            describe: text("describe")?,
            program,
            uid: number(&fields, "uid")?,
            pid: number(&fields, "pid")?,
            cwd: text("cwd")?,
            command,
        };
        if !ask.fits() {
            return Err(Malformed);
        }
        Ok(ask)
    }

    /// Whether the dialog can show all of the question.
    pub fn fits(&self) -> bool {
        self.command_line().len() <= MAX_SHOWN && self.directory().len() <= MAX_SHOWN
    }

    /// What will run, every byte outside printable ASCII escaped so nothing
    /// in an argument can restyle or reorder the line.
    pub fn command_line(&self) -> String {
        let mut out = String::new();
        for (i, word) in self.command.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            quote(word, &mut out);
        }
        out
    }

    /// Where it runs, escaped as the command line is.
    pub fn directory(&self) -> String {
        let mut out = String::new();
        quote(self.cwd.as_bytes(), &mut out);
        out
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Answer {
    pub id: u64,
    pub allow: bool,
}

impl Answer {
    pub fn frame(&self) -> Vec<u8> {
        let fields = Fields::new()
            .with("id", self.id.to_string())
            .with("allow", if self.allow { "1" } else { "0" });
        frame(kind::ANSWER, &fields.encode())
    }

    pub fn decode(payload: &[u8]) -> Result<Answer, Malformed> {
        let fields = known(payload, &["id", "allow"])?;
        let allow = match (fields.get("allow"), fields.all("allow").count()) {
            (Some(b"1"), 1) => true,
            (Some(b"0"), 1) => false,
            _ => return Err(Malformed),
        };
        Ok(Answer {
            id: number(&fields, "id")?,
            allow,
        })
    }
}

pub fn ready_frame() -> Vec<u8> {
    frame(kind::READY, &[])
}

pub fn cancel_frame(id: u64) -> Vec<u8> {
    frame(
        kind::CANCEL,
        &Fields::new().with("id", id.to_string()).encode(),
    )
}

pub fn decode_cancel(payload: &[u8]) -> Result<u64, Malformed> {
    number(&known(payload, &["id"])?, "id")
}

/// Why a question has no allowing answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refusal {
    NoAgent,
    Busy,
    Declined,
    Refused,
    Expired,
    Withdrawn,
    Interrupted,
}

impl Refusal {
    /// Whether the person at the machine decided it, by answering or not.
    pub fn by_person(self) -> bool {
        matches!(self, Refusal::Refused | Refusal::Expired)
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Refusal::NoAgent => "it needs the person at the machine, and nothing can ask them",
            Refusal::Busy => "too many questions are waiting for the person at the machine",
            Refusal::Declined => "the person at the machine has just refused a request",
            Refusal::Refused => "the person at the machine did not allow it",
            Refusal::Expired => "the person at the machine did not answer",
            Refusal::Withdrawn => "the requester withdrew the request",
            Refusal::Interrupted => {
                "the question went from the screen before the person at the machine answered"
            }
        })
    }
}

fn quote(word: &[u8], out: &mut String) {
    let plain = !word.is_empty()
        && word
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || b"_-./=:,+@%".contains(&b));
    if plain {
        out.extend(word.iter().map(|&b| b as char));
        return;
    }
    out.push('\'');
    for &b in word {
        match b {
            b'\'' => out.push_str("'\\''"),
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    out.push('\'');
}

#[cfg(test)]
mod tests;
