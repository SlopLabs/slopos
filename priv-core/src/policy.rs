//! The action policy: for each named action, the base program privd runs,
//! the argument shapes it accepts, what the program runs with and who may ask.
//!
//! ```text
//! action org.slopos.boot.install
//!   program  /bin/bootctl
//!   args     install *
//!   caps     RawBlock Power
//!   uid      0
//!   cwd
//!   exclusive boot-slots
//!   allow    root
//!   allow    session consent
//!   allow    paired
//!   describe "Install a kernel into a boot slot"
//! ```
//!
//! `args` repeats, one shape each: fixed leading words, then `*` for any
//! further ones. `allow` lines are tried in order and the first whose
//! principal matches decides. With `cwd` the mechanism runs in the directory
//! the request must send, and without it in `/`; `exclusive` lets one
//! mechanism of its group run at a time, the group being the action's name
//! unless one is given. An action marked `medium` exists only on a boot of
//! the install medium.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use slopos_abi::caps;
use slopos_abi::fs::SESSION_UID;

use crate::DELEGABLE;

/// The one program whose connections a pairing speaks through.
pub const REMOTED: &str = "/bin/remoted";

const MAX_NAME: usize = 64;
const MAX_WORD: usize = 64;
const MAX_DESCRIBE: usize = 120;
const MAX_PATH: usize = 255;
const PROGRAM_DIRS: [&str; 3] = ["/bin/", "/sbin/", "/usr/bin/"];

const CAP_NAMES: [(&str, u64); 11] = [
    ("Power", caps::POWER),
    ("Mount", caps::MOUNT),
    ("RawBlock", caps::RAW_BLOCK),
    ("NetAdmin", caps::NET_ADMIN),
    ("ConsoleConfig", caps::CONSOLE_CONFIG),
    ("SysInspect", caps::SYS_INSPECT),
    ("Clock", caps::CLOCK),
    ("Seal", caps::SEAL),
    ("BootEntry", caps::BOOT_ENTRY),
    ("DacOverride", caps::DAC_OVERRIDE),
    ("BaseUpdate", caps::BASE_UPDATE),
];

const _: () = {
    let mut all = 0;
    let mut i = 0;
    while i < CAP_NAMES.len() {
        all |= CAP_NAMES[i].1;
        i += 1;
    }
    assert!(all == DELEGABLE);
};

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Policy {
    actions: Vec<Action>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Action {
    pub name: String,
    pub program: String,
    pub shapes: Vec<Shape>,
    pub caps: u64,
    pub uid: u32,
    pub allows: Vec<Allow>,
    pub describe: String,
    pub cwd: bool,
    pub exclusive: Option<String>,
    pub medium: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Shape {
    pub words: Vec<String>,
    /// Any further arguments follow the fixed words.
    pub open: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Allow {
    pub principal: Principal,
    pub consent: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Principal {
    /// A uid-0 requester running a base program: uid 0 alone is no authority.
    Root,
    /// The person's uid, from any program but `remoted` on a paired base.
    Session,
    /// `remoted`'s connection on a base that carries a pairing.
    Paired,
    /// A requester running the base program at this path.
    Program(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PolicyError {
    pub line: usize,
    pub msg: &'static str,
}

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.msg)
    }
}

/// Who asks, as the kernel attests it: the ids as of the connection, the
/// program as of the request.
#[derive(Clone, Copy, Debug)]
pub struct Requester<'a> {
    uid: u32,
    program: Option<&'a str>,
    paired: bool,
}

impl<'a> Requester<'a> {
    /// `program` is the path of the base program the requester runs, `None`
    /// for any other program, an extension's included; `base_paired` whether
    /// the booted base carries a remote pairing.
    pub fn new(uid: u32, program: Option<&'a str>, base_paired: bool) -> Self {
        Self {
            uid,
            program,
            paired: base_paired && program == Some(REMOTED),
        }
    }

    pub fn is_paired(&self) -> bool {
        self.paired
    }

    fn is(&self, principal: &Principal) -> bool {
        match principal {
            Principal::Root => self.uid == 0 && self.program.is_some(),
            Principal::Paired => self.paired,
            Principal::Session => !self.paired && self.uid == SESSION_UID,
            Principal::Program(path) => !self.paired && self.program == Some(path.as_str()),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    Run,
    /// Run once the person at the machine allows it.
    Ask,
    Refuse,
}

impl Action {
    /// Whether `args`, the request's argv past its first word, has one of
    /// the action's shapes.
    pub fn accepts(&self, args: &[&[u8]]) -> bool {
        self.shapes.iter().any(|shape| shape.matches(args))
    }

    pub fn admits(&self, who: &Requester<'_>) -> Verdict {
        match self.allows.iter().find(|allow| who.is(&allow.principal)) {
            Some(allow) if allow.consent => Verdict::Ask,
            Some(_) => Verdict::Run,
            None => Verdict::Refuse,
        }
    }

    /// The group whose mechanisms run one at a time, if the action has one.
    pub fn exclusive_group(&self) -> Option<&str> {
        self.exclusive.as_deref()
    }
}

impl Shape {
    pub fn matches(&self, args: &[&[u8]]) -> bool {
        let fixed = self.words.len();
        let count_ok = if self.open {
            args.len() >= fixed
        } else {
            args.len() == fixed
        };
        count_ok
            && self
                .words
                .iter()
                .zip(args)
                .all(|(word, arg)| word.as_bytes() == *arg)
    }
}

impl Policy {
    pub fn parse(text: &str) -> Result<Policy, PolicyError> {
        let mut actions: Vec<Action> = Vec::new();
        let mut current: Option<(usize, Draft)> = None;
        for (index, raw) in text.lines().enumerate() {
            let line = index + 1;
            let fail = |msg| PolicyError { line, msg };
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let (keyword, rest) = match trimmed.split_once(|c: char| c.is_ascii_whitespace()) {
                Some((keyword, rest)) => (keyword, rest.trim()),
                None => (trimmed, ""),
            };
            if keyword == "action" {
                if let Some((start, draft)) = current.take() {
                    actions.push(draft.finish(start)?);
                }
                if !valid_name(rest) {
                    return Err(fail("an action is named by lowercase words joined by dots"));
                }
                if actions.iter().any(|a| a.name == rest) {
                    return Err(fail("the action is already defined"));
                }
                current = Some((line, Draft::new(rest)));
                continue;
            }
            let Some((_, draft)) = current.as_mut() else {
                return Err(fail("a setting outside an action"));
            };
            draft.set(keyword, rest).map_err(fail)?;
        }
        if let Some((start, draft)) = current {
            actions.push(draft.finish(start)?);
        }
        Ok(Policy { actions })
    }

    pub fn action(&self, name: &str) -> Option<&Action> {
        self.actions.iter().find(|action| action.name == name)
    }

    pub fn actions(&self) -> &[Action] {
        &self.actions
    }

    /// The actions that exist on a boot: those marked `medium` only when it
    /// booted the install medium.
    pub fn for_boot(mut self, medium: bool) -> Policy {
        self.actions.retain(|action| medium || !action.medium);
        self
    }
}

#[derive(Default)]
struct Draft {
    name: String,
    program: Option<String>,
    shapes: Vec<Shape>,
    caps: Option<u64>,
    uid: Option<u32>,
    allows: Vec<Allow>,
    describe: Option<String>,
    cwd: bool,
    exclusive: Option<String>,
    medium: bool,
}

impl Draft {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            ..Self::default()
        }
    }

    fn set(&mut self, keyword: &str, rest: &str) -> Result<(), &'static str> {
        let mut words = rest.split_ascii_whitespace();
        match keyword {
            "program" => {
                let path = single(&mut words).ok_or("program takes one path")?;
                if !valid_program(path) {
                    return Err("program is a base program's canonical path");
                }
                once(&mut self.program, path.to_string())
            }
            "args" => {
                let mut shape = Shape {
                    words: Vec::new(),
                    open: false,
                };
                for word in words {
                    if shape.open {
                        return Err("* ends a shape");
                    }
                    if word == "*" {
                        shape.open = true;
                    } else if valid_word(word) {
                        shape.words.push(word.to_string());
                    } else {
                        return Err("an argument word is printable and unquoted");
                    }
                }
                if self.shapes.contains(&shape) {
                    return Err("the shape is already listed");
                }
                self.shapes.push(shape);
                Ok(())
            }
            "caps" => {
                let mut mask = 0;
                for word in words {
                    let (_, bit) = CAP_NAMES
                        .iter()
                        .find(|(name, _)| *name == word)
                        .ok_or("caps names capabilities privd holds")?;
                    if mask & bit != 0 {
                        return Err("a capability named twice");
                    }
                    mask |= bit;
                }
                once(&mut self.caps, mask)
            }
            "uid" => {
                let uid = single(&mut words)
                    .and_then(|text| text.parse::<u32>().ok())
                    .filter(|_| rest.bytes().all(|b| b.is_ascii_digit()))
                    .ok_or("uid takes a number")?;
                if uid != 0 {
                    return Err("a mechanism runs as uid 0, as privd does");
                }
                once(&mut self.uid, uid)
            }
            "allow" => {
                let principal = match words.next() {
                    Some("root") => Principal::Root,
                    Some("session") => Principal::Session,
                    Some("paired") => Principal::Paired,
                    Some("program") => {
                        let path = words.next().ok_or("allow program takes a path")?;
                        if !valid_program(path) {
                            return Err("allow program names a base program's canonical path");
                        }
                        Principal::Program(path.to_string())
                    }
                    _ => return Err("allow names root, session, paired or program <path>"),
                };
                let consent = match words.next() {
                    None => false,
                    Some("consent") => true,
                    Some(_) => return Err("allow ends with consent or nothing"),
                };
                if words.next().is_some() {
                    return Err("allow ends with consent or nothing");
                }
                if consent && matches!(principal, Principal::Root | Principal::Paired) {
                    return Err("root and paired requesters are never asked about");
                }
                if self.allows.iter().any(|a| a.principal == principal) {
                    return Err("the principal is already allowed");
                }
                self.allows.push(Allow { principal, consent });
                Ok(())
            }
            "describe" => {
                let text = rest
                    .strip_prefix('"')
                    .and_then(|r| r.strip_suffix('"'))
                    .ok_or("describe takes one quoted line")?;
                if text.is_empty()
                    || text.len() > MAX_DESCRIBE
                    || !text.bytes().all(|b| (0x20..0x7f).contains(&b) && b != b'"')
                {
                    return Err("describe is printable ASCII, without quotes inside");
                }
                once(&mut self.describe, text.to_string())
            }
            "cwd" if rest.is_empty() => flag(&mut self.cwd),
            "medium" if rest.is_empty() => flag(&mut self.medium),
            "exclusive" => {
                let group = match single_or_none(&mut words)? {
                    Some(group) if valid_word(group) => group.to_string(),
                    Some(_) => return Err("an exclusive group is one word"),
                    None => self.name.clone(),
                };
                once(&mut self.exclusive, group)
            }
            _ => Err("unknown setting"),
        }
    }

    fn finish(self, line: usize) -> Result<Action, PolicyError> {
        let fail = |msg| PolicyError { line, msg };
        if self.shapes.is_empty() {
            return Err(fail("the action lists no args shape"));
        }
        if self.allows.is_empty() {
            return Err(fail("the action allows nobody"));
        }
        Ok(Action {
            program: self.program.ok_or(fail("the action names no program"))?,
            caps: self.caps.ok_or(fail("the action names no caps"))?,
            uid: self.uid.ok_or(fail("the action names no uid"))?,
            describe: self.describe.ok_or(fail("the action has no describe"))?,
            name: self.name,
            shapes: self.shapes,
            allows: self.allows,
            cwd: self.cwd,
            exclusive: self.exclusive,
            medium: self.medium,
        })
    }
}

fn once<T>(slot: &mut Option<T>, value: T) -> Result<(), &'static str> {
    if slot.is_some() {
        return Err("the setting is given twice");
    }
    *slot = Some(value);
    Ok(())
}

fn flag(slot: &mut bool) -> Result<(), &'static str> {
    if *slot {
        return Err("the setting is given twice");
    }
    *slot = true;
    Ok(())
}

fn single<'a>(words: &mut impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let word = words.next()?;
    words.next().is_none().then_some(word)
}

fn single_or_none<'a>(
    words: &mut impl Iterator<Item = &'a str>,
) -> Result<Option<&'a str>, &'static str> {
    let word = words.next();
    if words.next().is_some() {
        return Err("one word at most");
    }
    Ok(word)
}

/// Lowercase words of letters, digits and dashes, joined by dots: at least
/// two of them.
pub fn valid_name(name: &str) -> bool {
    name.len() <= MAX_NAME
        && name.contains('.')
        && name.split('.').all(|word| {
            word.as_bytes()
                .first()
                .is_some_and(|b| b.is_ascii_lowercase())
                && word
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

fn valid_word(word: &str) -> bool {
    !word.is_empty()
        && word.len() <= MAX_WORD
        && word
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'*')
}

fn valid_program(path: &str) -> bool {
    path.len() <= MAX_PATH
        && PROGRAM_DIRS.iter().any(|dir| path.starts_with(dir))
        && path[1..].split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part.bytes().all(|b| b.is_ascii_graphic())
        })
}

#[cfg(test)]
mod tests;
