//! The consent channel between privd and the compositor: its messages, the
//! questions privd keeps for it, and the dialog the compositor draws.
//!
//! An agent's `READY` names the version it speaks, none for the first. A
//! question to one of the first version asks only for a click, in the first
//! version's words; from the second on a question says what answers it, the
//! agent says when it is on screen, and an answer may carry a password; from
//! the third on it names the app that asks and its icon, what follows from
//! allowing it, the allowing button's verb, whether it destroys, and the
//! extension whose action it is, which an earlier question tells by its
//! `describe` instead.

mod dialog;
mod queue;

pub use dialog::{ARMING_MS, Dialog, Focus, Key};
pub use queue::{DECLINED_MS, Questions, WAITING_MAX};

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use slopos_accounts_core::password::SECRET_MAX;
use slopos_accounts_core::valid_name as valid_person;
use slopos_remote_core::{Fields, frame};
pub use slopos_tls_core::ct::wipe;

use crate::policy::valid_name;
use crate::wire::{MAX_ARGS, Malformed, known, number};

/// How long a question on screen waits for the person before it is a refusal.
pub const EXPIRY_MS: u64 = 120_000;
/// The longest command line, and the longest directory, a question may carry:
/// the person sees all of it or is not asked.
pub const MAX_SHOWN: usize = 320;
/// The version of the channel this side speaks.
pub const VERSION: u32 = 3;
/// What the dialog calls a program the kernel attests as none of the
/// system's.
pub const UNIDENTIFIED: &str = "An unidentified program";
/// The longest app name a question carries.
pub const APP_MAX: usize = 64;
const ICON_PATH_MAX: usize = 255;

/// What the dialog calls a program of the extension `name`.
pub fn named_extension(name: &str) -> String {
    format!("The {name} extension")
}

/// Whether an agent of `version` is asked for more than a click.
pub fn asks_more(version: u32) -> bool {
    version >= 2
}

/// Whether an agent of `version` says when a question is on screen.
pub fn says_shown(version: u32) -> bool {
    version >= 2
}

/// Whether an agent of `version` names the app that asks.
pub fn names_app(version: u32) -> bool {
    version >= 3
}

/// The base interface from which privd reads a version in `READY`, at least
/// the second. A privd from before ends an agent whose `READY` names any
/// version, so a compositor names one only to a privd of such a base.
pub const SINCE_INTERFACE: u32 = 8;
/// The most administrators a question lists.
pub const MAX_ADMINS: usize = 32;

pub mod kind {
    pub const READY: u8 = 0x20;
    pub const ASK: u8 = 0x21;
    pub const ANSWER: u8 = 0x22;
    pub const CANCEL: u8 = 0x23;
    pub const RESET: u8 = 0x24;
    pub const RESET_SEEN: u8 = 0x25;
    pub const SHOWN: u8 = 0x26;
}

/// What answers a question.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// The person asking clicks Allow.
    Confirm,
    /// The person asking types their password.
    Password,
    /// One of the administrators listed types their own password.
    Admin,
}

impl Kind {
    fn word(self) -> &'static str {
        match self {
            Kind::Confirm => "confirm",
            Kind::Password => "password",
            Kind::Admin => "admin",
        }
    }

    fn parse(word: &[u8]) -> Option<Kind> {
        [Kind::Confirm, Kind::Password, Kind::Admin]
            .into_iter()
            .find(|kind| kind.word().as_bytes() == word)
    }

    pub fn takes_secret(self) -> bool {
        self != Kind::Confirm
    }
}

/// A password on its way to `/sbin/authcheck`, wiped once dropped.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Secret(Vec<u8>);

impl Secret {
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `ch` typed at the end, refused past [`SECRET_MAX`] bytes. The room
    /// for all of them is taken at once, so nothing typed is ever copied.
    pub fn push(&mut self, ch: char) -> bool {
        let mut buf = [0u8; 4];
        let bytes = ch.encode_utf8(&mut buf).as_bytes();
        if self.0.len() + bytes.len() > SECRET_MAX {
            return false;
        }
        self.0.reserve_exact(SECRET_MAX - self.0.len());
        self.0.extend_from_slice(bytes);
        true
    }

    pub fn pop(&mut self) {
        let mut cut = self.0.len().saturating_sub(1);
        while cut > 0 && self.0[cut] & 0xC0 == 0x80 {
            cut -= 1;
        }
        wipe(&mut self.0[cut..]);
        self.0.truncate(cut);
    }

    pub fn chars(&self) -> usize {
        self.0.iter().filter(|&&b| b & 0xC0 != 0x80).count()
    }

    pub fn clear(&mut self) {
        wipe(&mut self.0);
        self.0.clear();
    }

    /// A secret as the channel carries one: no NUL, no newline, at most
    /// [`SECRET_MAX`] bytes of UTF-8.
    fn from_wire(bytes: &[u8]) -> Result<Secret, Malformed> {
        if bytes.is_empty() {
            return Err(Malformed);
        }
        Secret::typed(bytes)
    }

    /// What was typed, as [`from_wire`](Self::from_wire) holds it but empty
    /// too, which is how a person without a password answers.
    pub(crate) fn typed(bytes: &[u8]) -> Result<Secret, Malformed> {
        let fits = bytes.len() <= SECRET_MAX
            && !bytes.iter().any(|&b| b == 0 || b == b'\n')
            && core::str::from_utf8(bytes).is_ok();
        fits.then(|| Secret(bytes.to_vec())).ok_or(Malformed)
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        wipe(&mut self.0);
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret({} bytes)", self.0.len())
    }
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
    pub kind: Kind,
    /// The requester's name, when they are a person.
    pub person: Option<String>,
    /// Who may answer a [`Kind::Admin`] question.
    pub admins: Vec<String>,
    /// What the dialog names as asking: the base's app entry, the attested
    /// program's file name or its extension; `None` for [`UNIDENTIFIED`].
    pub app: Option<String>,
    /// That entry's icon, a PNG of the same share.
    pub icon: Option<String>,
    pub detail: Option<String>,
    /// What the allowing button says, which every question from the third
    /// version on names: one without is from a privd before it, whose words
    /// the dialog keeps.
    pub verb: Option<String>,
    pub destructive: bool,
    /// The extension whose action it is.
    pub extension: Option<String>,
}

impl Ask {
    /// The question as an agent of `version` reads it; one of the first
    /// version is asked nothing but to confirm.
    pub fn frame(&self, version: u32) -> Vec<u8> {
        let describe = match (&self.extension, names_app(version)) {
            (Some(extension), false) => format!("{} (extension {extension})", self.describe),
            _ => self.describe.clone(),
        };
        let mut fields = Fields::new()
            .with("id", self.id.to_string())
            .with("action", &self.action)
            .with("describe", describe)
            .with("uid", self.uid.to_string())
            .with("pid", self.pid.to_string())
            .with("cwd", &self.cwd);
        if let Some(program) = &self.program {
            fields.push("program", program);
        }
        for word in &self.command {
            fields.push("arg", word);
        }
        if asks_more(version) {
            fields.push("kind", self.kind.word());
            if let Some(person) = &self.person {
                fields.push("person", person);
            }
            for admin in &self.admins {
                fields.push("admin", admin);
            }
        }
        if names_app(version) {
            let optional = [
                ("app", &self.app),
                ("icon", &self.icon),
                ("detail", &self.detail),
                ("extension", &self.extension),
            ];
            for (key, value) in optional {
                if let Some(value) = value {
                    fields.push(key, value);
                }
            }
            fields.push("verb", self.verb.as_deref().unwrap_or(crate::words::VERB));
            if self.destructive {
                fields.push("destructive", "1");
            }
        }
        frame(kind::ASK, &fields.encode())
    }

    pub fn decode(payload: &[u8]) -> Result<Ask, Malformed> {
        let fields = known(
            payload,
            &[
                "id",
                "action",
                "describe",
                "uid",
                "pid",
                "cwd",
                "program",
                "arg",
                "kind",
                "person",
                "admin",
                "app",
                "icon",
                "detail",
                "verb",
                "destructive",
                "extension",
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
        let kind = match fields.all("kind").count() {
            0 => Kind::Confirm,
            1 => fields.get("kind").and_then(Kind::parse).ok_or(Malformed)?,
            _ => return Err(Malformed),
        };
        let person = match fields.all("person").count() {
            0 => None,
            _ => Some(text("person")?),
        };
        if person.as_deref().is_some_and(|p| !valid_person(p)) {
            return Err(Malformed);
        }
        let admins: Vec<String> = fields
            .all("admin")
            .map(|name| {
                core::str::from_utf8(name)
                    .ok()
                    .filter(|n| valid_person(n))
                    .map(str::to_string)
                    .ok_or(Malformed)
            })
            .collect::<Result<_, _>>()?;
        let listed = match kind {
            Kind::Admin => (1..=MAX_ADMINS).contains(&admins.len()),
            _ => admins.is_empty(),
        };
        if !listed || (kind == Kind::Password && person.is_none()) {
            return Err(Malformed);
        }
        let shown = |key, valid: fn(&str) -> bool| match fields.all(key).count() {
            0 => Ok(None),
            _ => Some(text(key)?)
                .filter(|t| valid(t))
                .map(Some)
                .ok_or(Malformed),
        };
        let app = shown("app", valid_app)?;
        let icon = shown("icon", icon_path)?;
        let detail = shown("detail", |t| printable(t, crate::words::DETAIL_MAX))?;
        let verb = shown("verb", |t| printable(t, crate::words::VERB_MAX))?;
        let extension = shown("extension", |t| {
            slopos_abi::extension::valid_name(t.as_bytes())
        })?;
        let destructive = match (fields.get("destructive"), fields.all("destructive").count()) {
            (None, _) => false,
            (Some(b"1"), 1) => true,
            _ => return Err(Malformed),
        };
        let ask = Ask {
            id: number(&fields, "id")?,
            action,
            describe: text("describe")?,
            program,
            uid: number(&fields, "uid")?,
            pid: number(&fields, "pid")?,
            cwd: text("cwd")?,
            command,
            kind,
            person,
            admins,
            app,
            icon,
            detail,
            verb,
            destructive,
            extension,
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

fn printable(text: &str, max: usize) -> bool {
    !text.is_empty() && text.len() <= max && text.bytes().all(|b| (0x20..0x7f).contains(&b))
}

/// A name the dialog may show as asking: nothing that could restyle,
/// reorder or hide what is written.
pub fn valid_app(name: &str) -> bool {
    !name.is_empty() && name.len() <= APP_MAX && reads_as_written(name)
}

/// No control character, and none of Unicode's format characters that
/// reorder, join or hide text: the bidirectional overrides, isolates and
/// marks, the zero-width ones and the interlinear annotations.
pub(crate) fn reads_as_written(text: &str) -> bool {
    !text.chars().any(|c| {
        c.is_control()
            || matches!(c,
                '\u{061c}'
                | '\u{180e}'
                | '\u{200b}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{feff}'
                | '\u{fff9}'..='\u{fffb}')
    })
}

/// An icon's path in a share the person cannot write: the base's
/// `/usr/share/slopos/icons/` or an extension's
/// `/opt/<name>/share/slopos/icons/`, a PNG by name.
pub fn icon_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('/') else {
        return false;
    };
    let parts: Vec<&str> = rest.split('/').collect();
    let canonical = parts.iter().all(|part| {
        !part.is_empty()
            && *part != "."
            && *part != ".."
            && part.bytes().all(|b| b.is_ascii_graphic())
    });
    let share = match parts.as_slice() {
        ["usr", "share", "slopos", "icons", file] => Some(file),
        ["opt", name, "share", "slopos", "icons", file]
            if slopos_abi::extension::valid_name(name.as_bytes()) =>
        {
            Some(file)
        }
        _ => None,
    };
    path.len() <= ICON_PATH_MAX
        && canonical
        && share.is_some_and(|file| file.len() > ".png".len() && file.ends_with(".png"))
}

/// The person's answer: an allowing one to a question that takes a
/// password carries the password typed, and to a [`Kind::Admin`] question
/// whose it is.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Answer {
    pub id: u64,
    pub allow: bool,
    pub secret: Option<Secret>,
    pub admin: Option<String>,
}

impl Answer {
    pub fn deny(id: u64) -> Answer {
        Answer {
            id,
            allow: false,
            secret: None,
            admin: None,
        }
    }

    /// The frame, which holds the password until the caller wipes it.
    pub fn frame(&self) -> Vec<u8> {
        let mut fields = Fields::new()
            .with("id", self.id.to_string())
            .with("allow", if self.allow { "1" } else { "0" });
        if let Some(secret) = &self.secret {
            fields.push("secret", secret.bytes());
        }
        if let Some(admin) = &self.admin {
            fields.push("as", admin);
        }
        let mut payload = fields.encode();
        let framed = frame(kind::ANSWER, &payload);
        wipe(&mut payload);
        fields.wipe();
        framed
    }

    pub fn decode(payload: &[u8]) -> Result<Answer, Malformed> {
        let mut fields = known(payload, &["id", "allow", "secret", "as"])?;
        let answer = Self::from_fields(&fields);
        fields.wipe();
        answer
    }

    fn from_fields(fields: &Fields) -> Result<Answer, Malformed> {
        let allow = match (fields.get("allow"), fields.all("allow").count()) {
            (Some(b"1"), 1) => true,
            (Some(b"0"), 1) => false,
            _ => return Err(Malformed),
        };
        let secret = match (fields.get("secret"), fields.all("secret").count()) {
            (None, _) => None,
            (Some(bytes), 1) if allow => Some(Secret::from_wire(bytes)?),
            _ => return Err(Malformed),
        };
        let admin = match (fields.text("as"), fields.all("as").count()) {
            (None, 0) => None,
            (Some(name), 1) if secret.is_some() && valid_person(name) => Some(name.to_string()),
            _ => return Err(Malformed),
        };
        Ok(Answer {
            id: number(fields, "id")?,
            allow,
            secret,
            admin,
        })
    }

    /// Whether it answers `ask` in the form `ask` takes.
    pub fn fits(&self, ask_kind: Kind, admins: &[String]) -> bool {
        if !self.allow {
            return self.secret.is_none() && self.admin.is_none();
        }
        match ask_kind {
            Kind::Confirm => self.secret.is_none() && self.admin.is_none(),
            Kind::Password => self.secret.is_some() && self.admin.is_none(),
            Kind::Admin => {
                self.secret.is_some() && self.admin.as_ref().is_some_and(|a| admins.contains(a))
            }
        }
    }
}

/// The agent can draw a question, in `version`, which a `READY` of the
/// first version does not name.
pub fn ready_frame(version: u32) -> Vec<u8> {
    match version {
        0 | 1 => frame(kind::READY, &[]),
        _ => frame(
            kind::READY,
            &Fields::new().with("version", version.to_string()).encode(),
        ),
    }
}

/// The version a `READY` names, the first for none.
pub fn decode_ready(payload: &[u8]) -> Result<u32, Malformed> {
    if payload.is_empty() {
        return Ok(1);
    }
    let version: u32 = number(&known(payload, &["version"])?, "version")?;
    if version < 2 {
        return Err(Malformed);
    }
    Ok(version)
}

/// Question `id` is on screen.
pub fn shown_frame(id: u64) -> Vec<u8> {
    frame(
        kind::SHOWN,
        &Fields::new().with("id", id.to_string()).encode(),
    )
}

pub fn decode_shown(payload: &[u8]) -> Result<u64, Malformed> {
    number(&known(payload, &["id"])?, "id")
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

/// What privd answers a request whose password was typed wrong.
pub const WRONG_PASSWORD: &str = "Wrong password.";

/// Why a question has no allowing answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refusal {
    NoAgent,
    /// The agent speaks a version that cannot take what the question needs.
    Outdated,
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
            Refusal::NoAgent => "This needs your permission, and no desktop is there to ask you.",
            Refusal::Outdated => "Restart the desktop, then try again.",
            Refusal::Busy => "Another request is waiting. Try again in a moment.",
            Refusal::Declined => "You just refused a request. Try again in a moment.",
            Refusal::Refused => "You didn't allow this.",
            Refusal::Expired => "Nobody answered in time.",
            Refusal::Withdrawn => "The request was withdrawn.",
            Refusal::Interrupted => "The question closed before it was answered. Try again.",
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
