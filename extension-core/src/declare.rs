//! `/.extension/declare`: what an extension asks for. A line at the margin
//! declares; the indented lines after a `service` or `action` are its
//! settings. Capabilities are named as [`slopos_priv_core::CAPABILITIES`]
//! names them, and a program by the path it has once the image is shown at
//! `/opt/<name>`.
//!
//! ```text
//! user                          the user `<name>`
//! user worker                   the user `<name>-worker`
//! state                         /var/lib/<name>
//! config                        /etc/<name>
//! runtime                       /run/<name>
//! command hello                 /opt/<name>/bin/hello on everyone's PATH
//! app hello                     /opt/<name>/share/slopos/apps/hello.app
//! service hellod
//!   program /opt/hello/bin/hellod
//!   args    --quiet
//!   user    worker              which declared user it runs as
//!   caps    NetAdmin
//!   after   other               repeats
//!   reads   /etc/hello/         a file or, ending in /, a directory; repeats
//!   listen  tcp 7000            or udp <port>, or unix <name> under /run/<name>
//!   restart on-failure          or always, never
//!   session                     in each person's desktop session, as them
//! action greet
//!   program  /opt/hello/libexec/greet
//!   args     greet *
//!   caps     NetAdmin
//!   cwd
//!   exclusive                   one at a time; a group is the extension's own
//!   keep     300                an answer kept for the same command and directory
//!   allow    admin confirm      admin [confirm|password], user [alone|
//!   allow    program /bin/x     confirm|password], paired, program <path>
//!   describe "Greet the network"
//! ```
//!
//! An action is `ext.<name>.<action>` to privd, and never allows `root`.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use slopos_abi::extension::Label;
pub use slopos_priv_core::Level;
use slopos_priv_core::policy::{self, Shape};

use crate::grant::Grant;
use crate::words::{LineError, caps_words, flag, lines, once, single, valid_word};

/// A user's name, as `/etc/passwd` takes one.
pub const USER_MAX: usize = 32;
/// The most services one extension declares: init's login tags for them are
/// made from their places.
pub const MAX_SERVICES: usize = 16;
const NAME_MAX: usize = 32;
const LISTEN_NAME_MAX: usize = 64;
const PATH_MAX: usize = 255;

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Declaration {
    /// Full names; the first is the extension's own, whose state is.
    pub users: Vec<String>,
    pub services: Vec<Service>,
    pub actions: Vec<Action>,
    pub commands: Vec<String>,
    pub apps: Vec<String>,
    pub state: bool,
    pub config: bool,
    pub runtime: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Service {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
    /// The full name of the user it runs as; `None` for a session service,
    /// which runs as the person.
    pub user: Option<String>,
    pub caps: u64,
    pub after: Vec<String>,
    pub reads: Vec<String>,
    pub listen: Vec<Listen>,
    pub restart: Restart,
    pub session: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Listen {
    Tcp(u16),
    Udp(u16),
    /// A socket at `/run/<name>/<this>`.
    Unix(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Restart {
    Always,
    OnFailure,
    Never,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Action {
    /// `ext.<extension>.<action>`.
    pub name: String,
    pub program: String,
    pub shapes: Vec<Shape>,
    pub caps: u64,
    pub cwd: bool,
    pub exclusive: Option<String>,
    pub keep: Option<u32>,
    pub allows: Vec<Allow>,
    pub describe: String,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Allow {
    pub principal: Principal,
    pub level: Level,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Principal {
    Admin,
    User,
    Paired,
    /// The extension's own program or a base program at this path.
    Program(String),
}

impl Action {
    /// Whether it runs with nobody asked: it allows `user` with no level or
    /// `alone`, `admin` with no level, a program or the paired agent, none of
    /// whom is asked.
    pub fn unasked(&self) -> bool {
        self.allows.iter().any(|a| {
            matches!(
                (&a.principal, a.level),
                (Principal::User, Level::None | Level::Alone)
                    | (Principal::Admin, Level::None)
                    | (Principal::Program(_) | Principal::Paired, _)
            )
        })
    }

    /// The name after `ext.<extension>.`.
    pub fn short(&self) -> &str {
        self.name.rsplit('.').next().unwrap_or(&self.name)
    }

    /// What privd runs for it: its program as `uid`, the extension's first
    /// user, holding what it declares within `held`, its `program` lines
    /// meaning programs of the activation `label` names.
    pub fn for_privd(&self, uid: u32, held: u64, label: Label) -> policy::Action {
        let extension = String::from_utf8_lossy(label.name()).into_owned();
        let own = format!("{}/", mount_point(&extension));
        let allows = self
            .allows
            .iter()
            .map(|allow| {
                let principal = match &allow.principal {
                    Principal::Admin => policy::Principal::Admin,
                    Principal::User => policy::Principal::User,
                    Principal::Paired => policy::Principal::Paired,
                    Principal::Program(path) if path.starts_with(&own) => {
                        policy::Principal::Extension(path.clone(), label)
                    }
                    Principal::Program(path) => policy::Principal::Program(path.clone()),
                };
                policy::Allow {
                    principal,
                    level: allow.level,
                }
            })
            .collect();
        policy::Action {
            name: self.name.clone(),
            program: self.program.clone(),
            shapes: self.shapes.clone(),
            caps: self.caps & held,
            runs_as: policy::RunsAs::Uid(uid),
            allows,
            describe: format!("{} (extension {extension})", self.describe),
            cwd: self.cwd,
            exclusive: self.exclusive.clone(),
            keep: self.keep,
            medium: false,
        }
    }
}

impl Declaration {
    /// `text` as the declaration of the extension `name`.
    pub fn parse(name: &str, text: &str) -> Result<Declaration, LineError> {
        let mut out = Declaration::default();
        let mut open: Option<(usize, Block)> = None;
        let mut starts = Vec::new();
        for (line, indented, keyword, rest) in lines(text) {
            let fail = |msg| LineError { line, msg };
            if indented {
                let (_, block) = open
                    .as_mut()
                    .ok_or(fail("a setting outside a service or action"))?;
                block.set(name, keyword, rest).map_err(fail)?;
                continue;
            }
            if let Some((start, block)) = open.take() {
                out.close(start, block, &mut starts)?;
            }
            match keyword {
                "service" | "action" => {
                    let word = single(rest)
                        .filter(|w| valid_word(w, NAME_MAX))
                        .ok_or(fail("a service or action is named by one lowercase word"))?;
                    open = Some((
                        line,
                        if keyword == "service" {
                            Block::Service(ServiceDraft::new(word))
                        } else {
                            Block::Action(ActionDraft::new(name, word))
                        },
                    ));
                }
                _ => out.declare(name, keyword, rest).map_err(fail)?,
            }
        }
        if let Some((start, block)) = open {
            out.close(start, block, &mut starts)?;
        }
        out.finish(&starts)?;
        Ok(out)
    }

    fn declare(&mut self, name: &str, keyword: &str, rest: &str) -> Result<(), &'static str> {
        match keyword {
            "user" => {
                let full = user_name(name, rest)?;
                if self.users.contains(&full) {
                    return Err("the user is declared already");
                }
                self.users.push(full);
            }
            "command" => {
                let word = single(rest)
                    .filter(|w| valid_command(w))
                    .ok_or("a command is one word of letters, digits, -, _ and .")?;
                if self.commands.iter().any(|c| c == word) {
                    return Err("the command is declared already");
                }
                self.commands.push(word.to_string());
            }
            "app" => {
                let word = single(rest)
                    .filter(|w| valid_command(w))
                    .ok_or("an app is one word of letters, digits, -, _ and .")?;
                if self.apps.iter().any(|a| a == word) {
                    return Err("the app is declared already");
                }
                self.apps.push(word.to_string());
            }
            "state" => flag(&mut self.state, rest)?,
            "config" => flag(&mut self.config, rest)?,
            "runtime" => flag(&mut self.runtime, rest)?,
            _ => return Err("unknown declaration"),
        }
        Ok(())
    }

    fn close(
        &mut self,
        start: usize,
        block: Block,
        starts: &mut Vec<usize>,
    ) -> Result<(), LineError> {
        let fail = |msg| LineError { line: start, msg };
        match block {
            Block::Service(draft) => {
                if self.services.iter().any(|s| s.name == draft.name) {
                    return Err(fail("the service is declared already"));
                }
                let service = draft.finish().map_err(fail)?;
                for path in &service.reads {
                    if path.starts_with("/etc/") && !self.config {
                        return Err(fail(
                            "a service reads /etc/<name> only once config is declared",
                        ));
                    }
                }
                if service.listen.iter().any(|l| matches!(l, Listen::Unix(_))) && !self.runtime {
                    return Err(fail(
                        "a unix socket is under /run/<name>, once runtime is declared",
                    ));
                }
                self.services.push(service);
                starts.push(start);
            }
            Block::Action(draft) => {
                let action = draft.finish().map_err(fail)?;
                if self.actions.iter().any(|a| a.name == action.name) {
                    return Err(fail("the action is declared already"));
                }
                self.actions.push(action);
            }
        }
        Ok(())
    }

    /// What needs every line, `starts` holding each service's: a service
    /// runs as a user declared anywhere, the first when it names none, and
    /// listens on no socket another does; its `after` names one declared,
    /// and no chain of them returns to where it began.
    fn finish(&mut self, starts: &[usize]) -> Result<(), LineError> {
        let first = self.users.first().cloned();
        for (at, &line) in starts.iter().enumerate() {
            let fail = |msg| LineError { line, msg };
            let service = &self.services[at];
            if service
                .user
                .as_ref()
                .is_some_and(|user| !self.users.contains(user))
            {
                return Err(fail("the service runs as a user the extension declares"));
            }
            if self.services[..at]
                .iter()
                .any(|other| other.listen.iter().any(|l| service.listen.contains(l)))
            {
                return Err(fail("another service listens on that socket"));
            }
        }
        for service in self.services.iter_mut().filter(|s| !s.session) {
            if service.user.is_none() {
                service.user.clone_from(&first);
            }
        }
        let whole = |msg| LineError { line: 0, msg };
        for service in &self.services {
            for after in &service.after {
                if after == &service.name || !self.services.iter().any(|s| &s.name == after) {
                    return Err(whole("a service starts after another declared service"));
                }
            }
        }
        for service in &self.services {
            let mut seen = Vec::new();
            let mut frontier: Vec<&str> = service.after.iter().map(String::as_str).collect();
            while let Some(next) = frontier.pop() {
                if next == service.name {
                    return Err(whole("services start after each other in a circle"));
                }
                if seen.contains(&next) {
                    continue;
                }
                seen.push(next);
                if let Some(s) = self.services.iter().find(|s| s.name == next) {
                    frontier.extend(s.after.iter().map(String::as_str));
                }
            }
        }
        if self.services.len() > MAX_SERVICES {
            return Err(whole("an extension declares at most 16 services"));
        }
        if (!self.services.is_empty() || self.state || self.runtime) && self.users.is_empty() {
            return Err(whole(
                "a service's log, state and runtime directory are its extension's first user's, so it declares a user",
            ));
        }
        if !self.actions.is_empty() && self.users.is_empty() {
            return Err(whole(
                "an action's mechanism runs as its extension's first user, so it declares a user",
            ));
        }
        Ok(())
    }

    /// What it asks to be granted.
    pub fn grant(&self) -> Grant {
        let caps = self
            .services
            .iter()
            .map(|s| s.caps)
            .chain(self.actions.iter().map(|a| a.caps))
            .fold(0, |all, c| all | c);
        Grant {
            caps,
            unasked: self
                .actions
                .iter()
                .filter(|a| a.unasked())
                .map(|a| a.short().to_string())
                .collect(),
            users: self.users.clone(),
            commands: self.commands.clone(),
        }
    }

    /// What every person's processes may run unasked holds together.
    pub fn unasked_caps(&self) -> u64 {
        self.actions
            .iter()
            .filter(|a| a.unasked())
            .fold(0, |all, a| all | a.caps)
    }

    /// Every program of the image it names: each service's and action's, a
    /// `program` principal's naming one, and each command's.
    pub fn programs<'a>(&'a self, name: &'a str) -> impl Iterator<Item = String> + 'a {
        let own = self
            .services
            .iter()
            .map(|s| s.program.clone())
            .chain(self.actions.iter().map(|a| a.program.clone()))
            .chain(self.actions.iter().flat_map(|a| {
                a.allows.iter().filter_map(|allow| match &allow.principal {
                    Principal::Program(path) => Some(path.clone()),
                    _ => None,
                })
            }));
        own.filter(move |path| path.starts_with(&mount_point(name)))
            .chain(
                self.commands
                    .iter()
                    .map(move |c| format!("{}/bin/{c}", mount_point(name))),
            )
    }
}

/// `/opt/<name>`.
pub fn mount_point(name: &str) -> String {
    format!("{}/{name}", slopos_abi::extension::DIR)
}

fn user_name(name: &str, rest: &str) -> Result<String, &'static str> {
    let full = match rest {
        "" => name.to_string(),
        suffix => match single(suffix) {
            Some(word) if valid_word(word, USER_MAX) => format!("{name}-{word}"),
            _ => return Err("a user's suffix is one lowercase word"),
        },
    };
    if full.len() > USER_MAX {
        return Err("a user's name is at most 32 bytes");
    }
    Ok(full)
}

/// A user's whole name: `<name>` or `<name>-<suffix>`.
pub fn valid_user(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= USER_MAX
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn valid_command(word: &str) -> bool {
    word.len() <= NAME_MAX
        && word
            .as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && word
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_.".contains(&b))
}

/// Canonical and absolute: no empty, `.` or `..` component and nothing but
/// printable ASCII, with a trailing `/` when `dir`.
fn canonical(path: &str, dir: bool) -> bool {
    let body = if dir {
        match path.strip_suffix('/') {
            Some(body) => body,
            None => return false,
        }
    } else {
        path
    };
    path.len() <= PATH_MAX
        && body.starts_with('/')
        && body[1..].split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part.bytes().all(|b| b.is_ascii_graphic())
        })
}

/// A file of the image, by the path it has at `/opt/<name>`.
fn own_file(name: &str, path: &str) -> bool {
    canonical(path, false)
        && path
            .strip_prefix(&mount_point(name))
            .is_some_and(|rest| rest.starts_with('/'))
}

/// A file of the image outside its `lib/`, where every ELF file is held to
/// the base's interpreter and `libc.so`.
fn own_program(name: &str, path: &str) -> bool {
    own_file(name, path)
        && !path
            .strip_prefix(&format!("{}/lib", mount_point(name)))
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

enum Block {
    Service(ServiceDraft),
    Action(ActionDraft),
}

impl Block {
    fn set(&mut self, name: &str, keyword: &str, rest: &str) -> Result<(), &'static str> {
        match self {
            Block::Service(draft) => draft.set(name, keyword, rest),
            Block::Action(draft) => draft.set(name, keyword, rest),
        }
    }
}

struct ServiceDraft {
    name: String,
    program: Option<String>,
    args: Option<Vec<String>>,
    user: Option<String>,
    caps: Option<u64>,
    after: Vec<String>,
    reads: Vec<String>,
    listen: Vec<Listen>,
    restart: Option<Restart>,
    session: bool,
}

impl ServiceDraft {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            program: None,
            args: None,
            user: None,
            caps: None,
            after: Vec::new(),
            reads: Vec::new(),
            listen: Vec::new(),
            restart: None,
            session: false,
        }
    }

    fn set(&mut self, name: &str, keyword: &str, rest: &str) -> Result<(), &'static str> {
        match keyword {
            "program" => {
                let path = single(rest)
                    .filter(|p| own_program(name, p))
                    .ok_or("program is a file of the image outside its lib/, under /opt/<name>")?;
                once(&mut self.program, path.to_string())
            }
            "args" => {
                let words = rest.split_ascii_whitespace();
                let words: Vec<String> = words
                    .map(|w| policy::valid_word(w).then(|| w.to_string()))
                    .collect::<Option<_>>()
                    .ok_or("an argument word is printable and unquoted")?;
                once(&mut self.args, words)
            }
            "user" => once(&mut self.user, user_name(name, rest)?),
            "caps" => once(&mut self.caps, caps_words(rest)?),
            "after" => {
                let word = single(rest)
                    .filter(|w| valid_word(w, NAME_MAX))
                    .ok_or("after names a service")?;
                if self.after.iter().any(|a| a == word) {
                    return Err("the service is named already");
                }
                self.after.push(word.to_string());
                Ok(())
            }
            "reads" => {
                let path = single(rest).ok_or("reads names one path")?;
                let dir = path.ends_with('/');
                let config = format!("/etc/{name}");
                let ours = own_file(name, path.trim_end_matches('/'))
                    || path.trim_end_matches('/') == config
                    || path.starts_with(&format!("{config}/"));
                if !canonical(path, dir) || !ours {
                    return Err("reads names a path under /opt/<name> or /etc/<name>");
                }
                if self.reads.iter().any(|r| r == path) {
                    return Err("the path is named already");
                }
                self.reads.push(path.to_string());
                Ok(())
            }
            "listen" => {
                let mut words = rest.split_ascii_whitespace();
                let listen = match (words.next(), words.next(), words.next()) {
                    (Some("tcp"), Some(port), None) => Listen::Tcp(port_of(port)?),
                    (Some("udp"), Some(port), None) => Listen::Udp(port_of(port)?),
                    (Some("unix"), Some(socket), None) if valid_socket(socket) => {
                        Listen::Unix(socket.to_string())
                    }
                    _ => return Err("listen takes tcp <port>, udp <port> or unix <name>"),
                };
                if self.listen.contains(&listen) {
                    return Err("the socket is named already");
                }
                self.listen.push(listen);
                Ok(())
            }
            "restart" => {
                let restart = match rest {
                    "always" => Restart::Always,
                    "on-failure" => Restart::OnFailure,
                    "never" => Restart::Never,
                    _ => return Err("restart is always, on-failure or never"),
                };
                once(&mut self.restart, restart)
            }
            "session" => flag(&mut self.session, rest),
            _ => Err("unknown service setting"),
        }
    }

    fn finish(self) -> Result<Service, &'static str> {
        let program = self.program.ok_or("the service names no program")?;
        let user = if self.session {
            if self.user.is_some() || !self.listen.is_empty() {
                return Err("a session service runs as the person and takes no socket");
            }
            None
        } else {
            self.user
        };
        Ok(Service {
            name: self.name,
            program,
            args: self.args.unwrap_or_default(),
            user,
            caps: self.caps.unwrap_or(0),
            after: self.after,
            reads: self.reads,
            listen: self.listen,
            restart: self.restart.unwrap_or(Restart::OnFailure),
            session: self.session,
        })
    }
}

fn port_of(text: &str) -> Result<u16, &'static str> {
    crate::words::parse_number(text)
        .and_then(|n| u16::try_from(n).ok())
        .filter(|&n| n != 0)
        .ok_or("a port is a number from 1 to 65535")
}

fn valid_socket(name: &str) -> bool {
    name.len() <= LISTEN_NAME_MAX && valid_command(name)
}

struct ActionDraft {
    name: String,
    program: Option<String>,
    shapes: Vec<Shape>,
    caps: Option<u64>,
    cwd: bool,
    exclusive: Option<String>,
    keep: Option<u32>,
    allows: Vec<Allow>,
    describe: Option<String>,
}

impl ActionDraft {
    fn new(extension: &str, short: &str) -> Self {
        Self {
            name: format!("ext.{extension}.{short}"),
            program: None,
            shapes: Vec::new(),
            caps: None,
            cwd: false,
            exclusive: None,
            keep: None,
            allows: Vec::new(),
            describe: None,
        }
    }

    fn set(&mut self, name: &str, keyword: &str, rest: &str) -> Result<(), &'static str> {
        match keyword {
            "program" => {
                let path = single(rest)
                    .filter(|p| own_program(name, p))
                    .ok_or("program is a file of the image outside its lib/, under /opt/<name>")?;
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
                let mask = caps_words(rest)?;
                if mask & !slopos_priv_core::DELEGABLE != 0 {
                    return Err("an action's caps are capabilities privd holds");
                }
                once(&mut self.caps, mask)
            }
            "cwd" => flag(&mut self.cwd, rest),
            "exclusive" => {
                let group = match rest {
                    "" => self.name.clone(),
                    group => single(group)
                        .filter(|g| valid_word(g, NAME_MAX))
                        .map(|g| format!("ext.{name}.{g}"))
                        .ok_or("an exclusive group is one lowercase word")?,
                };
                once(&mut self.exclusive, group)
            }
            "allow" => {
                let allow = allow(name, rest)?;
                if self.allows.iter().any(|a| a.principal == allow.principal) {
                    return Err("the principal is already allowed");
                }
                self.allows.push(allow);
                Ok(())
            }
            "keep" => once(&mut self.keep, policy::keep(rest)?),
            "describe" => once(&mut self.describe, policy::describe(rest)?),
            _ => Err("unknown action setting"),
        }
    }

    fn finish(self) -> Result<Action, &'static str> {
        if self
            .name
            .split('.')
            .any(|word| word.starts_with(|c: char| c.is_ascii_digit()))
        {
            return Err("privd names no action by a word that starts with a digit");
        }
        if !policy::valid_name(&self.name) {
            return Err("the action's whole name is too long for privd");
        }
        if self.shapes.is_empty() {
            return Err("the action lists no args shape");
        }
        if self.allows.is_empty() {
            return Err("the action allows nobody");
        }
        Ok(Action {
            name: self.name,
            program: self.program.ok_or("the action names no program")?,
            shapes: self.shapes,
            caps: self.caps.ok_or("the action names no caps")?,
            cwd: self.cwd,
            exclusive: self.exclusive,
            keep: self.keep,
            allows: self.allows,
            describe: self.describe.ok_or("the action has no describe")?,
        })
    }
}

fn allow(name: &str, rest: &str) -> Result<Allow, &'static str> {
    let mut words = rest.split_ascii_whitespace();
    let principal = match words.next() {
        Some("admin") => Principal::Admin,
        Some("user") => Principal::User,
        Some("paired") => Principal::Paired,
        Some("program") => {
            let path = words.next().ok_or("allow program takes a path")?;
            if !own_program(name, path) && !policy::valid_program(path) {
                return Err("allow program names the extension's program or a base program");
            }
            Principal::Program(path.to_string())
        }
        Some("root") => return Err("an extension's action never allows root"),
        _ => return Err("allow names admin, user, paired or program <path>"),
    };
    let level = policy::level(
        words,
        principal == Principal::User,
        principal == Principal::Admin,
    )?;
    Ok(Allow { principal, level })
}

#[cfg(test)]
mod tests;
