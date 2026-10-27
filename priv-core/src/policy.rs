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
//!   keep     300
//!   allow    root
//!   allow    admin confirm
//!   allow    paired
//!   describe "install a new kernel beside the one that starts by default"
//!   detail   "The kernel that starts by default stays, so you can go back to it."
//!   verb     "Install"
//! ```
//!
//! `args` repeats, one shape each: fixed leading words, then `*` for any
//! further ones. `allow` lines are tried in order and the first whose
//! principal matches decides, at its level: `user` takes `alone`, `confirm`
//! or `password`, `admin` takes `confirm` or `password`, and no level runs
//! the request unasked. A level asks only a person whose session can be
//! reached: its desktop in front and unlocked, or its terminal. `greeter`
//! takes `alone`, which runs it while nobody is logged in. A person no line matches is answered by an
//! administrator's password when an `admin` line stands, which `user alone`
//! becomes while another person is logged in. With `cwd` the mechanism runs
//! in the directory the request must send, and without it in `/`;
//! `exclusive` lets one mechanism of its group run at a time, the group being
//! the action's name unless one is given; `keep` keeps an answer for that
//! many seconds for the same session, parent process, command line and
//! directory. An action marked `medium` exists only on a boot of the install
//! medium.
//!
//! `uid requester` runs the mechanism as whoever asked, in their own groups
//! and login session, which only `root`, `user` and `admin` may be, holding
//! no more than [`REQUESTER_CEILING`](crate::REQUESTER_CEILING).

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use slopos_abi::extension::Label;

use crate::consent::Kind;
use crate::{DELEGABLE, REQUESTER_CEILING, words};

/// The one program whose connections a pairing speaks through.
pub const REMOTED: &str = "/bin/remoted";

const MAX_NAME: usize = 64;
const MAX_WORD: usize = 64;
const MAX_SETTING: usize = crate::words::DETAIL_MAX;
const MAX_PATH: usize = 255;
/// The longest an answer is kept, as sudo's timestamps commonly last.
pub const MAX_KEEP: u32 = 900;
const PROGRAM_DIRS: [&str; 3] = ["/bin/", "/sbin/", "/usr/bin/"];

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
    pub runs_as: RunsAs,
    pub allows: Vec<Allow>,
    pub describe: String,
    pub detail: Option<String>,
    pub verb: Option<String>,
    pub destructive: bool,
    pub cwd: bool,
    pub exclusive: Option<String>,
    /// Seconds an answer is kept for the same session, parent process,
    /// command line and directory.
    pub keep: Option<u32>,
    pub medium: bool,
}

/// Whom a mechanism runs as.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunsAs {
    /// uid 0 for the base's actions, an extension's first user for its own.
    Uid(u32),
    /// Whoever asked, in their own groups and login session.
    Requester,
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
    pub level: Level,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Principal {
    /// A uid-0 requester running a base program: uid 0 alone is no authority.
    Root,
    /// The login screen: the base's greeter user running a base program.
    Greeter,
    /// A person the accounts record names, from any program but `remoted` on
    /// a paired base.
    User,
    /// A person the record names as an administrator, likewise.
    Admin,
    /// `remoted`'s connection on a base that carries a pairing.
    Paired,
    /// A requester running the base program at this path.
    Program(String),
    /// A requester running the program at this path of the activation the
    /// label names: an extension's own program, which only that extension's
    /// actions allow.
    Extension(String, Label),
}

/// What a person must do for their request to run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    /// Nothing: it runs unasked.
    None,
    /// Nothing while nobody else is logged in; otherwise as `admin`. For the
    /// greeter, nothing while nobody is logged in, and refused otherwise.
    Alone,
    /// Click Allow.
    Confirm,
    /// Type their password.
    Password,
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

/// Where a person asking can be asked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reach {
    /// Their session's desktop is in front and unlocked, and asks in its
    /// dialog; `theirs` when it is their own session's.
    Screen { theirs: bool },
    /// Their session is a terminal's, where the requester itself asks for
    /// the person's own password.
    Terminal,
    /// No question reaches them.
    Nowhere,
}

/// Who asks, as the kernel attests it: the ids as of the connection, the
/// program as of the request, and where the session it carries can be asked.
#[derive(Clone, Copy, Debug)]
pub struct Requester<'a> {
    uid: u32,
    program: Option<&'a str>,
    extension: Option<(&'a str, Label)>,
    paired: bool,
    reach: Reach,
}

impl<'a> Requester<'a> {
    /// `program` is the path of the base program the requester runs, `None`
    /// for any other program, an extension's included; `base_paired` whether
    /// the booted base carries a remote pairing.
    pub fn new(uid: u32, program: Option<&'a str>, base_paired: bool) -> Self {
        Self {
            uid,
            program,
            extension: None,
            paired: base_paired && program == Some(REMOTED),
            reach: Reach::Nowhere,
        }
    }

    /// A requester running `path` of the activation `label` names.
    pub fn extension(uid: u32, path: &'a str, label: Label) -> Self {
        Self {
            uid,
            program: None,
            extension: Some((path, label)),
            paired: false,
            reach: Reach::Nowhere,
        }
    }

    /// The requester, asked where `reach` says.
    pub fn reached(self, reach: Reach) -> Self {
        Self { reach, ..self }
    }

    pub fn is_paired(&self) -> bool {
        self.paired
    }

    /// Whether the requester is `principal`, one that names no person.
    fn is(&self, principal: &Principal) -> bool {
        match principal {
            Principal::Root => self.uid == 0 && self.program.is_some(),
            Principal::Greeter => self.uid == slopos_abi::fs::GREETER_UID && self.program.is_some(),
            Principal::Paired => self.paired,
            Principal::User | Principal::Admin => false,
            Principal::Program(path) => !self.paired && self.program == Some(path.as_str()),
            Principal::Extension(path, label) => self.extension == Some((path.as_str(), *label)),
        }
    }

    /// The person the requester is, when `people` knows them and the
    /// requester is no paired agent.
    fn person<'p>(&self, people: &'p People) -> Option<&'p Person> {
        people.person(self.uid).filter(|_| !self.paired)
    }
}

/// A person as privd judges their requests.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Person {
    pub uid: u32,
    pub name: String,
    pub admin: bool,
    /// Whether they have a password to type; without one they confirm by a
    /// click where another would type it.
    pub password: bool,
}

/// The people as of a request: whom the accounts record names, and who is
/// logged in, at a desktop or a terminal.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct People {
    pub people: Vec<Person>,
    pub logged_in: Vec<u32>,
}

impl People {
    pub fn person(&self, uid: u32) -> Option<&Person> {
        self.people.iter().find(|p| p.uid == uid)
    }

    fn alone(&self, uid: u32) -> bool {
        self.logged_in.iter().all(|&other| other == uid)
    }

    /// What `person` must do to answer for themselves where `reach` asks: a
    /// click at their own desktop, else their password, which a person
    /// without one cannot type.
    fn ask_self(&self, person: &Person, confirm: bool, reach: Reach) -> Verdict {
        let kind = match (reach, confirm, person.password) {
            (Reach::Nowhere, ..) => return Verdict::Refuse(Refused::Unreached),
            (Reach::Screen { theirs: true }, true, _)
            | (Reach::Screen { theirs: true }, false, false) => Kind::Confirm,
            (_, _, true) => Kind::Password,
            (Reach::Screen { theirs: false }, _, false) => {
                return Verdict::Refuse(Refused::Away);
            }
            (Reach::Terminal, _, false) => {
                return Verdict::Refuse(Refused::NoPasswordToType);
            }
        };
        Verdict::Ask(Question {
            kind,
            person: person.clone(),
            admins: Vec::new(),
        })
    }

    /// An administrator's password for `person`'s request: only an
    /// administrator with a password answers for another, and only in the
    /// dialog, which nothing of the requester's reads.
    fn ask_admins(&self, person: &Person, reach: Reach) -> Verdict {
        let admins: Vec<String> = self
            .people
            .iter()
            .filter(|p| p.admin && p.password && p.uid != person.uid)
            .take(crate::consent::MAX_ADMINS)
            .map(|p| p.name.clone())
            .collect();
        if admins.is_empty() {
            return Verdict::Refuse(Refused::NoAdminPassword);
        }
        match reach {
            Reach::Screen { .. } => {}
            Reach::Terminal => return Verdict::Refuse(Refused::AdminAtTerminal),
            Reach::Nowhere => return Verdict::Refuse(Refused::Unreached),
        }
        Verdict::Ask(Question {
            kind: Kind::Admin,
            person: person.clone(),
            admins,
        })
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Verdict {
    Run,
    /// Run once the question is answered.
    Ask(Question),
    Refuse(Refused),
}

/// What the person at the machine is asked, and for whom.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Question {
    pub kind: Kind,
    pub person: Person,
    /// Who may answer a [`Kind::Admin`] question.
    pub admins: Vec<String>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Refused {
    /// No line names the requester.
    NotAllowed,
    /// Only an administrator may allow it, and none has a password to type.
    NoAdminPassword,
    /// The person must answer at another's screen, and has no password to
    /// type there.
    Away,
    /// The requester's session is not on the screen, unlocked, nor at a
    /// terminal of its own.
    Unreached,
    /// The person's session is a terminal's, where only a password answers,
    /// and they have none.
    NoPasswordToType,
    /// Only an administrator may allow it, which a terminal never asks.
    AdminAtTerminal,
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::NotAllowed => f.write_str("You aren't allowed to do this."),
            Refused::NoAdminPassword => {
                f.write_str("This needs an administrator's password, and no administrator has one.")
            }
            Refused::Away => {
                f.write_str("Without a password, you can allow this only from your own desktop.")
            }
            Refused::Unreached => f.write_str(
                "This needs your permission. Unlock your desktop or use a terminal, then try \
                 again.",
            ),
            Refused::NoPasswordToType => {
                f.write_str("This needs your password, and you haven't set one.")
            }
            Refused::AdminAtTerminal => f.write_str(
                "This needs an administrator's password, which only the desktop can ask for.",
            ),
        }
    }
}

impl Action {
    /// Whether `args`, the request's argv past its first word, has one of
    /// the action's shapes.
    pub fn accepts(&self, args: &[&[u8]]) -> bool {
        self.shapes.iter().any(|shape| shape.matches(args))
    }

    /// What it takes for `who` to run the action, `people` as they stand.
    pub fn admits(&self, who: &Requester<'_>, people: &People) -> Verdict {
        let person = who.person(people);
        let mut admins_answer = false;
        for allow in &self.allows {
            match (&allow.principal, person, allow.level) {
                (Principal::User, Some(p), Level::Alone) if people.alone(p.uid) || p.admin => {
                    return Verdict::Run;
                }
                (Principal::User, Some(_), Level::Alone) => admins_answer = true,
                (Principal::User, Some(_), Level::None) => return Verdict::Run,
                (Principal::User, Some(p), level) => {
                    return people.ask_self(p, level == Level::Confirm, who.reach);
                }
                (Principal::Admin, Some(p), Level::None) if p.admin => return Verdict::Run,
                (Principal::Admin, Some(p), level) if p.admin => {
                    return people.ask_self(p, level == Level::Confirm, who.reach);
                }
                (Principal::Admin, Some(_), _) => admins_answer = true,
                (Principal::Greeter, _, Level::Alone) if who.is(&Principal::Greeter) => {
                    if people.logged_in.is_empty() {
                        return Verdict::Run;
                    }
                    break;
                }
                (principal, _, _) if who.is(principal) => return Verdict::Run,
                _ => {}
            }
        }
        match person {
            Some(p) if admins_answer => people.ask_admins(p, who.reach),
            _ => Verdict::Refuse(Refused::NotAllowed),
        }
    }

    /// Whether a verdict on it reads the people: an `allow` names `user` or
    /// `admin`, or `greeter alone`, which asks who is logged in.
    pub fn names_people(&self) -> bool {
        self.allows.iter().any(|allow| {
            matches!(allow.principal, Principal::User | Principal::Admin)
                || (allow.principal == Principal::Greeter && allow.level == Level::Alone)
        })
    }

    /// What the allowing button says: one the words take, else theirs.
    pub fn verb(&self) -> &str {
        self.verb
            .as_deref()
            .filter(|verb| words::verb(verb).is_ok())
            .unwrap_or(words::VERB)
    }

    /// The group whose mechanisms run one at a time, if the action has one.
    pub fn exclusive_group(&self) -> Option<&str> {
        self.exclusive.as_deref()
    }
}

impl Shape {
    /// An `args` line's words: fixed ones, then `*` for any further ones.
    pub fn parse(rest: &str) -> Result<Shape, &'static str> {
        let mut shape = Shape {
            words: Vec::new(),
            open: false,
        };
        for word in rest.split_ascii_whitespace() {
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
        Ok(shape)
    }

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

/// How a policy is read: as privd holds its own base's, or as a requester
/// reads whatever base is current, which may be another's than its own.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reading {
    /// Every setting known and what the person reads held to
    /// [`crate::words`].
    Held,
    /// A setting this reader does not know passed over, and the words as
    /// they stand.
    Lenient,
}

const SETTINGS: [&str; 13] = [
    "program",
    "args",
    "caps",
    "uid",
    "allow",
    "keep",
    "describe",
    "detail",
    "verb",
    "destructive",
    "cwd",
    "medium",
    "exclusive",
];

impl Policy {
    /// `text` as privd holds its own base's policy.
    pub fn parse(text: &str) -> Result<Policy, PolicyError> {
        Self::parse_as(text, Reading::Held)
    }

    /// `text` as a requester reads the current base's policy, which may be
    /// another base's: a setting it does not know is passed over, and the
    /// words are not held.
    pub fn read(text: &str) -> Result<Policy, PolicyError> {
        Self::parse_as(text, Reading::Lenient)
    }

    fn parse_as(text: &str, reading: Reading) -> Result<Policy, PolicyError> {
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
            if reading == Reading::Lenient && !SETTINGS.contains(&keyword) {
                continue;
            }
            draft.set(keyword, rest, reading).map_err(fail)?;
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

    /// `action` beside the policy's own; `false`, adding nothing, when one of
    /// its name is there already.
    pub fn add(&mut self, action: Action) -> bool {
        if self.action(&action.name).is_some() {
            return false;
        }
        self.actions.push(action);
        true
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
    runs_as: Option<RunsAs>,
    allows: Vec<Allow>,
    describe: Option<String>,
    detail: Option<String>,
    verb: Option<String>,
    destructive: bool,
    cwd: bool,
    exclusive: Option<String>,
    keep: Option<u32>,
    medium: bool,
}

impl Draft {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            ..Self::default()
        }
    }

    fn set(&mut self, keyword: &str, rest: &str, reading: Reading) -> Result<(), &'static str> {
        let held = |rule| match reading {
            Reading::Held => said(rest, rule),
            Reading::Lenient => quoted(rest),
        };
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
                let shape = Shape::parse(rest)?;
                if self.shapes.contains(&shape) {
                    return Err("the shape is already listed");
                }
                self.shapes.push(shape);
                Ok(())
            }
            "caps" => {
                let mut mask = 0;
                for word in words {
                    let bit = crate::capability(word)
                        .filter(|bit| bit & DELEGABLE != 0)
                        .ok_or("caps names capabilities privd holds")?;
                    if mask & bit != 0 {
                        return Err("a capability named twice");
                    }
                    mask |= bit;
                }
                once(&mut self.caps, mask)
            }
            "uid" => {
                let runs_as = match single(&mut words) {
                    Some("0") => RunsAs::Uid(0),
                    Some("requester") => RunsAs::Requester,
                    _ => return Err("a mechanism runs as uid 0, or as its requester"),
                };
                once(&mut self.runs_as, runs_as)
            }
            "allow" => {
                let allow = allow(rest)?;
                if self.allows.iter().any(|a| a.principal == allow.principal) {
                    return Err("the principal is already allowed");
                }
                self.allows.push(allow);
                Ok(())
            }
            "keep" => once(&mut self.keep, keep(rest)?),
            "describe" => once(&mut self.describe, held(words::describe)?),
            "detail" => once(&mut self.detail, held(words::detail)?),
            "verb" => once(&mut self.verb, held(words::verb)?),
            "destructive" if rest.is_empty() => flag(&mut self.destructive),
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
        let caps = self.caps.ok_or(fail("the action names no caps"))?;
        let runs_as = self.runs_as.ok_or(fail("the action names no uid"))?;
        if runs_as == RunsAs::Requester {
            if caps & !REQUESTER_CEILING != 0 {
                return Err(fail(
                    "an action run as its requester holds no more than the ceiling",
                ));
            }
            let people_or_root = |a: &Allow| {
                matches!(
                    a.principal,
                    Principal::Root | Principal::User | Principal::Admin
                )
            };
            if !self.allows.iter().all(people_or_root) {
                return Err(fail(
                    "an action run as its requester allows only root, user and admin",
                ));
            }
        }
        Ok(Action {
            program: self.program.ok_or(fail("the action names no program"))?,
            caps,
            runs_as,
            describe: self.describe.ok_or(fail("the action has no describe"))?,
            detail: self.detail,
            verb: self.verb,
            destructive: self.destructive,
            name: self.name,
            shapes: self.shapes,
            allows: self.allows,
            cwd: self.cwd,
            exclusive: self.exclusive,
            keep: self.keep,
            medium: self.medium,
        })
    }
}

/// An `allow` line's principal and level.
fn allow(rest: &str) -> Result<Allow, &'static str> {
    let mut words = rest.split_ascii_whitespace();
    let principal = match words.next() {
        Some("root") => Principal::Root,
        Some("greeter") => Principal::Greeter,
        Some("user") => Principal::User,
        Some("admin") => Principal::Admin,
        Some("paired") => Principal::Paired,
        Some("program") => {
            let path = words.next().ok_or("allow program takes a path")?;
            if !valid_program(path) {
                return Err("allow program names a base program's canonical path");
            }
            Principal::Program(path.to_string())
        }
        _ => return Err("allow names root, greeter, user, admin, paired or program <path>"),
    };
    let mut words = words.peekable();
    if principal == Principal::Greeter && words.next_if_eq(&"alone").is_some() {
        if words.next().is_some() {
            return Err("allow ends with its level");
        }
        return Ok(Allow {
            principal,
            level: Level::Alone,
        });
    }
    let level = level(
        words,
        principal == Principal::User,
        principal == Principal::Admin,
    )?;
    Ok(Allow { principal, level })
}

/// What an `allow` line ends with after its principal: a level only `user`
/// and `admin` take, and `alone` only `user`.
pub fn level<'a>(
    mut words: impl Iterator<Item = &'a str>,
    user: bool,
    admin: bool,
) -> Result<Level, &'static str> {
    let level = match words.next() {
        None => Level::None,
        Some("alone") if user => Level::Alone,
        Some("confirm") if user || admin => Level::Confirm,
        Some("password") if user || admin => Level::Password,
        Some(_) => return Err("the level is not one that principal takes"),
    };
    if words.next().is_some() {
        return Err("allow ends with its level");
    }
    Ok(level)
}

/// A `keep` line's seconds, 1 to [`MAX_KEEP`].
pub fn keep(rest: &str) -> Result<u32, &'static str> {
    rest.parse::<u32>()
        .ok()
        .filter(|s| (1..=MAX_KEEP).contains(s) && rest.bytes().all(|b| b.is_ascii_digit()))
        .ok_or("keep takes the seconds an answer is kept, 1 to 900")
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

/// One quoted line of printable ASCII, the shape `describe`, `detail` and
/// `verb` take; [`crate::words`] holds what it says.
pub fn quoted(rest: &str) -> Result<String, &'static str> {
    let text = rest
        .strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .ok_or("the setting takes one quoted line")?;
    if text.is_empty()
        || text.len() > MAX_SETTING
        || !text.bytes().all(|b| (0x20..0x7f).contains(&b) && b != b'"')
    {
        return Err("the setting is printable ASCII, without quotes inside");
    }
    Ok(text.to_string())
}

fn said(rest: &str, rule: fn(&str) -> Result<(), &'static str>) -> Result<String, &'static str> {
    let text = quoted(rest)?;
    rule(&text)?;
    Ok(text)
}

pub fn valid_word(word: &str) -> bool {
    !word.is_empty()
        && word.len() <= MAX_WORD
        && word
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'*')
}

/// A base program's canonical path, under a directory the base holds
/// programs in.
pub fn valid_program(path: &str) -> bool {
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
