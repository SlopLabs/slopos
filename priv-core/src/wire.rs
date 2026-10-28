//! The request protocol between a requester and privd.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use slopos_abi::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use slopos_abi::spawn::EXEC_MAX_ARG_STRLEN;
use slopos_remote_core::{Deframer, Fields, frame};

use crate::policy::valid_name;

/// No request or reply comes near it; a peer that announces more is not
/// speaking this protocol.
pub const MAX_FRAME: usize = 64 * 1024;
pub const MAX_ARGS: usize = 256;
pub const MAX_REASON: usize = 512;

pub const RELAYED_SIGNALS: [u8; 4] = [SIGHUP, SIGINT, SIGQUIT, SIGTERM];

pub mod kind {
    pub const REQUEST: u8 = 0x01;
    pub const SIGNAL: u8 = 0x02;
    pub const SECRET: u8 = 0x03;
    pub const STARTED: u8 = 0x10;
    pub const EXITED: u8 = 0x11;
    pub const DENIED: u8 = 0x12;
    pub const ASKING: u8 = 0x13;
    pub const PROMPT: u8 = 0x14;
}

pub const MAX_PROMPT: usize = 512;

/// The id of a reply about the connection rather than a request, which
/// answers whatever was asked on it. No request takes it.
pub const CONNECTION: u64 = 0;

/// The descriptors every request carries: stdin, stdout and stderr, then
/// the working directory when the request names one.
pub const STDIO: usize = 3;

pub fn deframer() -> Deframer {
    Deframer::with_limit(MAX_FRAME)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Malformed;

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a malformed message")
    }
}

/// A request. Its `id`, the requester's choice, comes back on every reply to
/// it, so a requester on an inherited connection can tell its replies from
/// what an earlier request on it left.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Request {
    pub id: u64,
    pub action: String,
    /// argv past its first word, which privd sets to the program's name.
    pub args: Vec<Vec<u8>>,
    pub cwd: bool,
    /// The requester can ask its person for their password at its terminal,
    /// which privd asks of it in a console session.
    pub terminal: bool,
}

impl Request {
    pub fn descriptors(&self) -> usize {
        STDIO + usize::from(self.cwd)
    }

    pub fn args(&self) -> Vec<&[u8]> {
        self.args.iter().map(Vec::as_slice).collect()
    }

    pub fn frame(&self) -> Vec<u8> {
        let mut fields = Fields::new()
            .with("id", self.id.to_string())
            .with("action", &self.action);
        for arg in &self.args {
            fields.push("arg", arg);
        }
        if self.cwd {
            fields.push("cwd", "1");
        }
        if self.terminal {
            fields.push("terminal", "1");
        }
        frame(kind::REQUEST, &fields.encode())
    }

    pub fn decode(payload: &[u8]) -> Result<Request, Malformed> {
        let fields = known(payload, &["id", "action", "arg", "cwd", "terminal"])?;
        let action = fields
            .text("action")
            .filter(|name| valid_name(name))
            .ok_or(Malformed)?;
        if fields.all("action").count() != 1 {
            return Err(Malformed);
        }
        let args: Vec<Vec<u8>> = fields.all("arg").map(<[u8]>::to_vec).collect();
        if args.len() > MAX_ARGS
            || args
                .iter()
                .any(|arg| arg.len() > EXEC_MAX_ARG_STRLEN || arg.contains(&0))
        {
            return Err(Malformed);
        }
        let flag = |key| match (fields.get(key), fields.all(key).count()) {
            (None, _) => Ok(false),
            (Some(b"1"), 1) => Ok(true),
            _ => Err(Malformed),
        };
        let cwd = flag("cwd")?;
        let terminal = flag("terminal")?;
        let id = number(&fields, "id")?;
        if id == CONNECTION {
            return Err(Malformed);
        }
        Ok(Request {
            id,
            action: action.to_string(),
            args,
            cwd,
            terminal,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Exit {
    Code(i32),
    Signal(u8),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Reply {
    /// The person at the machine is being asked, the dialog naming the
    /// requester `app`, as [`Ask::app`](crate::consent::Ask::app) does.
    Asking {
        app: Option<String>,
    },
    Started {
        pid: u32,
    },
    Exited(Exit),
    /// `by_person` when the person at the machine refused, or did not answer.
    Denied {
        reason: String,
        by_person: bool,
    },
    /// Ask the person at the requester's terminal for their password after
    /// showing them `about`, what they are asked to allow, and send it back
    /// in a [`secret_frame`].
    Prompt {
        about: String,
        prompt: String,
    },
}

impl Reply {
    pub fn denied(reason: impl Into<String>) -> Reply {
        let mut reason = reason.into();
        if reason.len() > MAX_REASON {
            let mut cut = MAX_REASON;
            while !reason.is_char_boundary(cut) {
                cut -= 1;
            }
            reason.truncate(cut);
        }
        Reply::Denied {
            reason,
            by_person: false,
        }
    }

    pub fn frame(&self, id: u64) -> Vec<u8> {
        let fields = Fields::new().with("id", id.to_string());
        match self {
            Reply::Asking { app } => {
                let mut fields = fields;
                if let Some(app) = app {
                    fields.push("app", app);
                }
                frame(kind::ASKING, &fields.encode())
            }
            Reply::Started { pid } => {
                frame(kind::STARTED, &fields.with("pid", pid.to_string()).encode())
            }
            Reply::Exited(Exit::Code(code)) => frame(
                kind::EXITED,
                &fields.with("code", code.to_string()).encode(),
            ),
            Reply::Exited(Exit::Signal(signal)) => frame(
                kind::EXITED,
                &fields.with("signal", signal.to_string()).encode(),
            ),
            Reply::Denied { reason, by_person } => {
                let mut fields = fields.with("reason", reason);
                if *by_person {
                    fields.push("person", "1");
                }
                frame(kind::DENIED, &fields.encode())
            }
            Reply::Prompt { about, prompt } => frame(
                kind::PROMPT,
                &fields.with("prompt", prompt).with("about", about).encode(),
            ),
        }
    }

    /// A reply and the id of the request it answers.
    pub fn decode(kind: u8, payload: &[u8]) -> Result<(u64, Reply), Malformed> {
        let reply = match kind {
            kind::ASKING => {
                let fields = known(payload, &["id", "app"])?;
                let app = optional(&fields, "app", crate::consent::valid_app)?;
                (number(&fields, "id")?, Reply::Asking { app })
            }
            kind::STARTED => {
                let fields = known(payload, &["id", "pid"])?;
                let pid = number(&fields, "pid")?;
                (number(&fields, "id")?, Reply::Started { pid })
            }
            kind::EXITED => {
                let fields = known(payload, &["id", "code", "signal"])?;
                let exit = match (fields.get("code"), fields.get("signal")) {
                    (Some(_), None) => Exit::Code(number(&fields, "code")?),
                    (None, Some(_)) => Exit::Signal(number(&fields, "signal")?),
                    _ => return Err(Malformed),
                };
                (number(&fields, "id")?, Reply::Exited(exit))
            }
            kind::DENIED => {
                let fields = known(payload, &["id", "reason", "person"])?;
                let reason = fields
                    .text("reason")
                    .filter(|r| r.len() <= MAX_REASON && fields.all("reason").count() == 1)
                    .ok_or(Malformed)?;
                let by_person = match (fields.get("person"), fields.all("person").count()) {
                    (None, _) => false,
                    (Some(b"1"), 1) => true,
                    _ => return Err(Malformed),
                };
                let reply = Reply::Denied {
                    reason: reason.to_string(),
                    by_person,
                };
                (number(&fields, "id")?, reply)
            }
            kind::PROMPT => {
                let fields = known(payload, &["id", "prompt", "about"])?;
                let prompt = fields
                    .text("prompt")
                    .filter(|p| p.len() <= MAX_PROMPT && fields.all("prompt").count() == 1)
                    .filter(|p| p.bytes().all(|b| (0x20..0x7f).contains(&b)))
                    .ok_or(Malformed)?;
                let about = fields
                    .text("about")
                    .filter(|_| fields.all("about").count() == 1)
                    .filter(|a| {
                        !a.is_empty()
                            && a.len() <= MAX_PROMPT
                            && crate::consent::reads_as_written(a)
                    })
                    .ok_or(Malformed)?;
                let reply = Reply::Prompt {
                    about: about.to_string(),
                    prompt: prompt.to_string(),
                };
                (number(&fields, "id")?, reply)
            }
            _ => return Err(Malformed),
        };
        Ok(reply)
    }
}

/// The password the person typed at the requester's terminal for request
/// `id`'s prompt, empty when they gave none. The frame holds it until the
/// caller wipes it.
pub fn secret_frame(id: u64, secret: &[u8]) -> Vec<u8> {
    let mut fields = Fields::new()
        .with("id", id.to_string())
        .with("secret", secret);
    let mut payload = fields.encode();
    let framed = frame(kind::SECRET, &payload);
    crate::consent::wipe(&mut payload);
    fields.wipe();
    framed
}

/// The request a SECRET frame answers and what was typed, at most
/// `slopos_accounts_core::password::SECRET_MAX` bytes of UTF-8 with no NUL or
/// newline, or nothing.
pub fn decode_secret(payload: &[u8]) -> Result<(u64, crate::consent::Secret), Malformed> {
    let mut fields = known(payload, &["id", "secret"])?;
    let decoded = (|| {
        let secret = match (fields.get("secret"), fields.all("secret").count()) {
            (Some(bytes), 1) => crate::consent::Secret::typed(bytes)?,
            _ => return Err(Malformed),
        };
        Ok((number(&fields, "id")?, secret))
    })();
    fields.wipe();
    decoded
}

/// Relay `signal` to the mechanism request `id` started, or withdraw the
/// request when nothing has started yet.
pub fn signal_frame(id: u64, signal: u8) -> Vec<u8> {
    frame(
        kind::SIGNAL,
        &Fields::new()
            .with("id", id.to_string())
            .with("signo", signal.to_string())
            .encode(),
    )
}

/// The request a SIGNAL frame names and the signal, if it is one a requester
/// may relay.
pub fn decode_signal(payload: &[u8]) -> Result<(u64, u8), Malformed> {
    let fields = known(payload, &["id", "signo"])?;
    let signal: u8 = number(&fields, "signo")?;
    if RELAYED_SIGNALS.contains(&signal) {
        Ok((number(&fields, "id")?, signal))
    } else {
        Err(Malformed)
    }
}

/// `payload` as fields carrying no key outside `keys`.
pub(crate) fn known(payload: &[u8], keys: &[&str]) -> Result<Fields, Malformed> {
    let fields = Fields::decode(payload).map_err(|_| Malformed)?;
    if fields.keys().any(|key| !keys.contains(&key)) {
        return Err(Malformed);
    }
    Ok(fields)
}

pub(crate) fn optional(
    fields: &Fields,
    key: &str,
    valid: fn(&str) -> bool,
) -> Result<Option<String>, Malformed> {
    match (fields.text(key), fields.all(key).count()) {
        (None, 0) => Ok(None),
        (Some(text), 1) if valid(text) => Ok(Some(text.to_string())),
        _ => Err(Malformed),
    }
}

/// A field that occurs once and is a canonical decimal number: no `+`, no
/// leading zero, no `-0`.
pub(crate) fn number<T: core::str::FromStr>(fields: &Fields, key: &str) -> Result<T, Malformed> {
    let text = fields.text(key).ok_or(Malformed)?;
    let canonical = !text.is_empty()
        && text
            .bytes()
            .enumerate()
            .all(|(i, b)| b.is_ascii_digit() || (i == 0 && b == b'-' && text.len() > 1))
        && !(text.len() > 1 && text.starts_with('0'))
        && !text.starts_with("-0");
    if !canonical || fields.all(key).count() != 1 {
        return Err(Malformed);
    }
    text.parse().map_err(|_| Malformed)
}

#[cfg(test)]
mod tests;
