//! The frames between the installer's engine, `installer --serve` as privd
//! runs it on two pipes, and the program that drives it. Each frame is
//! `remote-core`'s: a kind, a big-endian length and a [`Fields`] payload, a
//! structure inside one carried as a field holding its own [`Fields`].
//!
//! ```text
//! driver Hello{version}            engine Hello{version}, and exits unless they match
//! driver Probe                     engine System | Issue
//! driver Open{disk}                engine System | Issue
//! driver Propose{choices}          engine Proposal{id, proposal} | Issue
//! driver Install{choices, proposal}
//!                                  engine Step | Progress | Log .., then Done | Failed
//! ```
//!
//! The engine answers each request in turn. It installs only the proposal it
//! made last, for the same choices, and serves on after `Failed`, once probed
//! again; it exits after `Done` and at the end of its input, which before
//! `Install` leaves every disk as it was.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;
use core::str::FromStr;

use slopos_boot_core::disk_id::Bus;
use slopos_boot_core::install::{PlanError, Role};
use slopos_remote_core::{Deframer, Fields, MAX_PAYLOAD, frame};

use crate::{
    Action, Choices, Contents, Disk, DiskState, EspTrouble, Event, Failure, FirstPerson, Fit,
    Issue, Layout, Listing, MbrPartition, Medium, Mode, NoTable, Opened, Part, Person, Piece,
    Proposal, Refusal, Reinstall, Segment, Setup, Span, Step, Summary, System, Tag, Target,
};

pub const VERSION: u32 = 3;
/// No request comes near it.
pub const MAX_REQUEST: usize = 64 * 1024;

pub mod kind {
    pub const HELLO: u8 = 0x01;
    pub const PROBE: u8 = 0x02;
    pub const OPEN: u8 = 0x03;
    pub const PROPOSE: u8 = 0x04;
    pub const INSTALL: u8 = 0x05;
    pub const SYSTEM: u8 = 0x10;
    pub const PROPOSAL: u8 = 0x11;
    pub const ISSUE: u8 = 0x12;
    pub const STEP: u8 = 0x20;
    pub const PROGRESS: u8 = 0x21;
    pub const LOG: u8 = 0x22;
    pub const DONE: u8 = 0x30;
    pub const FAILED: u8 = 0x31;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Malformed;

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a malformed installer frame")
    }
}

pub fn requests() -> Deframer {
    Deframer::with_limit(MAX_REQUEST)
}

pub fn replies() -> Deframer {
    Deframer::with_limit(MAX_PAYLOAD)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Hello { version: u32 },
    Probe,
    Open { disk: String },
    Propose(Choices),
    Install { choices: Choices, proposal: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Hello { version: u32 },
    System(System),
    Proposal { id: u64, proposal: Proposal },
    Issue(Issue),
    Event(Event),
    Done(Summary),
    Failed(Failure),
}

impl Request {
    pub fn frame(&self) -> Vec<u8> {
        let (kind, fields) = match self {
            Request::Hello { version } => (kind::HELLO, W::new().num("version", *version)),
            Request::Probe => (kind::PROBE, W::new()),
            Request::Open { disk } => (kind::OPEN, W::new().text("disk", disk)),
            Request::Propose(choices) => (
                kind::PROPOSE,
                W::new().nest("choices", encode_choices(choices)),
            ),
            Request::Install { choices, proposal } => (
                kind::INSTALL,
                W::new()
                    .nest("choices", encode_choices(choices))
                    .num("proposal", *proposal),
            ),
        };
        frame(kind, &fields.0.encode())
    }

    pub fn decode(kind: u8, payload: &[u8]) -> Result<Request, Malformed> {
        Ok(match kind {
            kind::HELLO => Request::Hello {
                version: R::of(payload, &["version"])?.num("version")?,
            },
            kind::PROBE => {
                R::of(payload, &[])?;
                Request::Probe
            }
            kind::OPEN => Request::Open {
                disk: R::of(payload, &["disk"])?.text("disk")?.into(),
            },
            kind::PROPOSE => {
                let r = R::of(payload, &["choices"])?;
                Request::Propose(decode_choices(r.nested("choices")?)?)
            }
            kind::INSTALL => {
                let r = R::of(payload, &["choices", "proposal"])?;
                Request::Install {
                    choices: decode_choices(r.nested("choices")?)?,
                    proposal: r.num("proposal")?,
                }
            }
            _ => return Err(Malformed),
        })
    }
}

impl Reply {
    pub fn frame(&self) -> Vec<u8> {
        let (kind, payload) = self.encode();
        frame(kind, &payload)
    }

    /// The frame's kind and payload, which [`frame`](Self::frame) panics
    /// on past [`MAX_PAYLOAD`].
    pub fn encode(&self) -> (u8, Vec<u8>) {
        let (kind, fields) = match self {
            Reply::Hello { version } => (kind::HELLO, W::new().num("version", *version)),
            Reply::System(system) => (kind::SYSTEM, encode_system(system)),
            Reply::Proposal { id, proposal } => (
                kind::PROPOSAL,
                W::new()
                    .num("id", *id)
                    .nest("proposal", encode_proposal(proposal)),
            ),
            Reply::Issue(issue) => (kind::ISSUE, encode_issue(issue)),
            Reply::Event(Event::Step(step)) => (kind::STEP, W::new().num("step", step.number())),
            Reply::Event(Event::Progress { done, total }) => (
                kind::PROGRESS,
                W::new().num("done", *done).num("total", *total),
            ),
            Reply::Event(Event::Log { line }) => (kind::LOG, W::new().text("line", line)),
            Reply::Done(summary) => (
                kind::DONE,
                W::new()
                    .text("disk", &summary.disk)
                    .num("entry", summary.entry),
            ),
            Reply::Failed(Failure::Refused(issue)) => {
                (kind::FAILED, W::new().nest("issue", encode_issue(issue)))
            }
            Reply::Failed(Failure::Stopped { step, why }) => (
                kind::FAILED,
                W::new()
                    .opt_num("step", step.map(Step::number))
                    .text("why", why),
            ),
        };
        (kind, fields.0.encode())
    }

    pub fn decode(kind: u8, payload: &[u8]) -> Result<Reply, Malformed> {
        Ok(match kind {
            kind::HELLO => Reply::Hello {
                version: R::of(payload, &["version"])?.num("version")?,
            },
            kind::SYSTEM => Reply::System(decode_system(payload)?),
            kind::PROPOSAL => {
                let r = R::of(payload, &["id", "proposal"])?;
                Reply::Proposal {
                    id: r.num("id")?,
                    proposal: decode_proposal(r.nested("proposal")?)?,
                }
            }
            kind::ISSUE => Reply::Issue(decode_issue(payload)?),
            kind::STEP => {
                let r = R::of(payload, &["step"])?;
                Reply::Event(Event::Step(step_numbered(r.num("step")?)?))
            }
            kind::PROGRESS => {
                let r = R::of(payload, &["done", "total"])?;
                Reply::Event(Event::Progress {
                    done: r.num("done")?,
                    total: r.num("total")?,
                })
            }
            kind::LOG => Reply::Event(Event::Log {
                line: R::of(payload, &["line"])?.text("line")?.into(),
            }),
            kind::DONE => {
                let r = R::of(payload, &["disk", "entry"])?;
                Reply::Done(Summary {
                    disk: r.text("disk")?.into(),
                    entry: r.num("entry")?,
                })
            }
            kind::FAILED => {
                let r = R::of(payload, &["issue", "step", "why"])?;
                Reply::Failed(match r.opt("issue")? {
                    Some(issue) => {
                        r.absent(&["step", "why"])?;
                        Failure::Refused(decode_issue(issue)?)
                    }
                    None => Failure::Stopped {
                        step: r.opt_num("step")?.map(step_numbered).transpose()?,
                        why: r.text("why")?.into(),
                    },
                })
            }
            _ => return Err(Malformed),
        })
    }
}

/// Fields as they are written.
struct W(Fields);

impl W {
    fn new() -> W {
        W(Fields::new())
    }

    fn text(mut self, key: &str, value: &str) -> W {
        self.0.push(key, value);
        self
    }

    fn num(self, key: &str, value: impl ToString) -> W {
        self.text(key, &value.to_string())
    }

    fn flag(self, key: &str, on: bool) -> W {
        if on { self.text(key, "1") } else { self }
    }

    fn opt_text(self, key: &str, value: Option<&str>) -> W {
        match value {
            Some(value) => self.text(key, value),
            None => self,
        }
    }

    fn opt_num(self, key: &str, value: Option<impl ToString>) -> W {
        match value {
            Some(value) => self.num(key, value),
            None => self,
        }
    }

    fn nest(mut self, key: &str, inner: W) -> W {
        self.0.push(key, inner.0.encode());
        self
    }

    fn texts<'a>(mut self, key: &str, values: impl IntoIterator<Item = &'a String>) -> W {
        for value in values {
            self.0.push(key, value);
        }
        self
    }
}

/// Fields as they are read: no key outside the ones a structure has, a
/// single field once, a number in decimal with no sign or padding and a flag
/// present only as `1`, so a structure has one encoding up to its fields'
/// order.
struct R(Fields);

impl R {
    fn of(bytes: &[u8], keys: &[&str]) -> Result<R, Malformed> {
        let fields = Fields::decode(bytes).map_err(|_| Malformed)?;
        if fields.keys().any(|key| !keys.contains(&key)) {
            return Err(Malformed);
        }
        Ok(R(fields))
    }

    fn opt(&self, key: &str) -> Result<Option<&[u8]>, Malformed> {
        match self.0.all(key).count() {
            0 => Ok(None),
            1 => Ok(self.0.get(key)),
            _ => Err(Malformed),
        }
    }

    fn opt_text(&self, key: &str) -> Result<Option<&str>, Malformed> {
        self.opt(key)?
            .map(|bytes| core::str::from_utf8(bytes).map_err(|_| Malformed))
            .transpose()
    }

    fn text(&self, key: &str) -> Result<&str, Malformed> {
        self.opt_text(key)?.ok_or(Malformed)
    }

    fn opt_num<T: FromStr>(&self, key: &str) -> Result<Option<T>, Malformed> {
        let Some(text) = self.opt_text(key)? else {
            return Ok(None);
        };
        let canonical = !text.is_empty()
            && text.bytes().all(|b| b.is_ascii_digit())
            && (text == "0" || !text.starts_with('0'));
        if !canonical {
            return Err(Malformed);
        }
        text.parse().map(Some).map_err(|_| Malformed)
    }

    fn num<T: FromStr>(&self, key: &str) -> Result<T, Malformed> {
        self.opt_num(key)?.ok_or(Malformed)
    }

    fn flag(&self, key: &str) -> Result<bool, Malformed> {
        match self.opt(key)? {
            None => Ok(false),
            Some(b"1") => Ok(true),
            Some(_) => Err(Malformed),
        }
    }

    fn nested(&self, key: &str) -> Result<&[u8], Malformed> {
        self.opt(key)?.ok_or(Malformed)
    }

    fn texts(&self, key: &str) -> Result<Vec<String>, Malformed> {
        self.0
            .all(key)
            .map(|bytes| {
                core::str::from_utf8(bytes)
                    .map(String::from)
                    .map_err(|_| Malformed)
            })
            .collect()
    }

    fn absent(&self, keys: &[&str]) -> Result<(), Malformed> {
        if keys.iter().any(|key| self.0.get(key).is_some()) {
            Err(Malformed)
        } else {
            Ok(())
        }
    }
}

fn step_numbered(number: u32) -> Result<Step, Malformed> {
    Step::numbered(number).ok_or(Malformed)
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::Esp => "esp",
        Role::Boot => "boot",
        Role::Root => "root",
        Role::Crash => "crash",
    }
}

fn role_named(name: &str) -> Result<Role, Malformed> {
    Ok(match name {
        "esp" => Role::Esp,
        "boot" => Role::Boot,
        "root" => Role::Root,
        "crash" => Role::Crash,
        _ => return Err(Malformed),
    })
}

fn encode_choices(c: &Choices) -> W {
    let w = W::new().text("disk", &c.disk);
    let w = match c.mode {
        Mode::Erase => w.text("mode", "erase"),
        Mode::Free { region } => w.text("mode", "free").opt_num("region", region),
        Mode::Reuse { root, region, keep } => w
            .text("mode", "reuse")
            .opt_num("region", region)
            .num("root", root)
            .flag("keep", keep),
    };
    let w = w
        .opt_num("root_size", c.root_size)
        .opt_text("remote", c.remote.as_deref())
        .flag("first", c.first)
        .opt_text("cmdline", c.cmdline.as_deref());
    match &c.person {
        Some(person) => w.nest(
            "person",
            W::new()
                .text("name", &person.name)
                .opt_text("password", person.password.as_deref()),
        ),
        None => w,
    }
}

fn decode_choices(bytes: &[u8]) -> Result<Choices, Malformed> {
    let r = R::of(
        bytes,
        &[
            "disk",
            "mode",
            "region",
            "root",
            "keep",
            "root_size",
            "remote",
            "first",
            "cmdline",
            "person",
        ],
    )?;
    let mode = match r.text("mode")? {
        "erase" => {
            r.absent(&["region", "root", "keep"])?;
            Mode::Erase
        }
        "free" => {
            r.absent(&["root", "keep"])?;
            Mode::Free {
                region: r.opt_num("region")?,
            }
        }
        "reuse" => Mode::Reuse {
            root: r.num("root")?,
            region: r.opt_num("region")?,
            keep: r.flag("keep")?,
        },
        _ => return Err(Malformed),
    };
    let person = match r.opt("person")? {
        Some(bytes) => {
            let p = R::of(bytes, &["name", "password"])?;
            Some(Person {
                name: p.text("name")?.into(),
                password: p.opt_text("password")?.map(String::from),
            })
        }
        None => None,
    };
    Ok(Choices {
        disk: r.text("disk")?.into(),
        mode,
        root_size: r.opt_num("root_size")?,
        remote: r.opt_text("remote")?.map(String::from),
        first: r.flag("first")?,
        person,
        cmdline: r.opt_text("cmdline")?.map(String::from),
    })
}

fn encode_issue(issue: &Issue) -> W {
    let w = W::new();
    match issue {
        Issue::Medium(why) => w.text("issue", "medium").text("text", why),
        Issue::Probe(why) => w.text("issue", "probe").text("text", why),
        Issue::Disk(why) => w.text("issue", "disk").text("text", why),
        Issue::NothingToKeep(node) => w.text("issue", "nothing-to-keep").text("node", node),
        Issue::NoRoot(node) => w.text("issue", "no-root").text("node", node),
        Issue::Esp {
            node,
            trouble: EspTrouble::Full { free, needed },
        } => w
            .text("issue", "esp-full")
            .text("node", node)
            .num("free", *free)
            .num("needed", *needed),
        Issue::Esp {
            node,
            trouble: EspTrouble::WrongType,
        } => w.text("issue", "esp-wrong-type").text("node", node),
        Issue::Esp {
            node,
            trouble: EspTrouble::Unreadable(why),
        } => w
            .text("issue", "esp-unreadable")
            .text("node", node)
            .text("text", why),
        Issue::Person(why) => w.text("issue", "person").text("text", why),
        Issue::Menu(why) => w.text("issue", "menu").text("text", why),
        Issue::Remote(url) => w.text("issue", "remote").text("text", url),
        Issue::RootSize => w.text("issue", "root-size"),
        Issue::Unprobed => w.text("issue", "unprobed"),
        Issue::Unopened(node) => w.text("issue", "unopened").text("node", node),
        Issue::Unproposed => w.text("issue", "unproposed"),
        Issue::Changed(node) => w.text("issue", "changed").text("node", node),
        Issue::OnlyErase { node, contents } => w
            .text("issue", "only-erase")
            .text("node", node)
            .nest("contents", encode_no_table(contents)),
        Issue::Plan { error, node, root } => {
            let w = w
                .text("issue", "plan")
                .text("node", node)
                .opt_text("root", root.as_deref());
            match *error {
                PlanError::NoRegion => w.text("error", "no-region"),
                PlanError::NoRoom { needed, available } => w
                    .text("error", "no-room")
                    .num("needed", needed)
                    .num("available", available),
                PlanError::NoPartition(n) => w.text("error", "no-partition").num("number", n),
                PlanError::NotForRoot(n) => w.text("error", "not-for-root").num("number", n),
                PlanError::Installed(n) => w.text("error", "installed").num("number", n),
                PlanError::Ambiguous(role) => {
                    w.text("error", "ambiguous").text("role", role_name(role))
                }
                PlanError::RootTooSmall { needed, available } => w
                    .text("error", "root-too-small")
                    .num("needed", needed)
                    .num("available", available),
                PlanError::RootTooLarge { limit, available } => w
                    .text("error", "root-too-large")
                    .num("limit", limit)
                    .num("available", available),
                PlanError::RootElsewhere(n) => w.text("error", "root-elsewhere").num("number", n),
                PlanError::NoSlot => w.text("error", "no-slot"),
            }
        }
    }
}

fn decode_issue(bytes: &[u8]) -> Result<Issue, Malformed> {
    const KEYS: [&str; 12] = [
        "issue",
        "text",
        "node",
        "contents",
        "root",
        "error",
        "needed",
        "available",
        "limit",
        "number",
        "role",
        "free",
    ];
    let r = R::of(bytes, &KEYS)?;
    let kind = r.text("issue")?;
    let only = |keys: &[&str]| {
        let extra: Vec<&str> = KEYS[1..]
            .iter()
            .copied()
            .filter(|k| !keys.contains(k))
            .collect();
        r.absent(&extra)
    };
    let text = |make: fn(String) -> Issue| -> Result<Issue, Malformed> {
        only(&["text"])?;
        Ok(make(r.text("text")?.into()))
    };
    let node = |make: fn(String) -> Issue| -> Result<Issue, Malformed> {
        only(&["node"])?;
        Ok(make(r.text("node")?.into()))
    };
    Ok(match kind {
        "medium" => text(Issue::Medium)?,
        "probe" => text(Issue::Probe)?,
        "disk" => text(Issue::Disk)?,
        "nothing-to-keep" => node(Issue::NothingToKeep)?,
        "no-root" => node(Issue::NoRoot)?,
        "esp-full" => {
            only(&["node", "free", "needed"])?;
            Issue::Esp {
                node: r.text("node")?.into(),
                trouble: EspTrouble::Full {
                    free: r.num("free")?,
                    needed: r.num("needed")?,
                },
            }
        }
        "esp-wrong-type" => {
            only(&["node"])?;
            Issue::Esp {
                node: r.text("node")?.into(),
                trouble: EspTrouble::WrongType,
            }
        }
        "esp-unreadable" => {
            only(&["node", "text"])?;
            Issue::Esp {
                node: r.text("node")?.into(),
                trouble: EspTrouble::Unreadable(r.text("text")?.into()),
            }
        }
        "changed" => node(Issue::Changed)?,
        "unopened" => node(Issue::Unopened)?,
        "person" => text(Issue::Person)?,
        "menu" => text(Issue::Menu)?,
        "remote" => text(Issue::Remote)?,
        "root-size" => {
            only(&[])?;
            Issue::RootSize
        }
        "unprobed" => {
            only(&[])?;
            Issue::Unprobed
        }
        "unproposed" => {
            only(&[])?;
            Issue::Unproposed
        }
        "only-erase" => {
            only(&["node", "contents"])?;
            Issue::OnlyErase {
                node: r.text("node")?.into(),
                contents: decode_no_table(r.nested("contents")?)?,
            }
        }
        "plan" => {
            let (error, keys): (PlanError, &[&str]) = match r.text("error")? {
                "no-region" => (PlanError::NoRegion, &[]),
                "no-room" => (
                    PlanError::NoRoom {
                        needed: r.num("needed")?,
                        available: r.num("available")?,
                    },
                    &["needed", "available"],
                ),
                "no-partition" => (PlanError::NoPartition(r.num("number")?), &["number"]),
                "not-for-root" => (PlanError::NotForRoot(r.num("number")?), &["number"]),
                "installed" => (PlanError::Installed(r.num("number")?), &["number"]),
                "ambiguous" => (
                    PlanError::Ambiguous(role_named(r.text("role")?)?),
                    &["role"],
                ),
                "root-too-small" => (
                    PlanError::RootTooSmall {
                        needed: r.num("needed")?,
                        available: r.num("available")?,
                    },
                    &["needed", "available"],
                ),
                "root-too-large" => (
                    PlanError::RootTooLarge {
                        limit: r.num("limit")?,
                        available: r.num("available")?,
                    },
                    &["limit", "available"],
                ),
                "root-elsewhere" => (PlanError::RootElsewhere(r.num("number")?), &["number"]),
                "no-slot" => (PlanError::NoSlot, &[]),
                _ => return Err(Malformed),
            };
            let mut allowed: Vec<&str> = alloc::vec!["node", "root", "error"];
            allowed.extend_from_slice(keys);
            only(&allowed)?;
            Issue::Plan {
                error,
                node: r.text("node")?.into(),
                root: r.opt_text("root")?.map(String::from),
            }
        }
        _ => return Err(Malformed),
    })
}

fn encode_fit(fit: &Fit) -> W {
    match fit {
        Ok(()) => W::new(),
        Err(issue) => encode_issue(issue),
    }
}

fn decode_fit(bytes: &[u8]) -> Result<Fit, Malformed> {
    if bytes.is_empty() {
        Ok(Ok(()))
    } else {
        decode_issue(bytes).map(Err)
    }
}

fn encode_layout(layout: &Layout) -> W {
    let mut w = W::new().num("capacity", layout.capacity);
    for segment in &layout.segments {
        let s = W::new()
            .num("start", segment.start)
            .num("bytes", segment.bytes);
        let s = match &segment.piece {
            Piece::Free { region } => s.num("region", *region),
            Piece::Partition {
                number,
                node,
                kind,
                name,
                role,
            } => s
                .num("number", *number)
                .text("node", node)
                .text("kind", kind)
                .text("name", name)
                .opt_text("role", role.map(role_name)),
        };
        w = w.nest("segment", s);
    }
    w
}

fn decode_layout(bytes: &[u8]) -> Result<Layout, Malformed> {
    let r = R::of(bytes, &["capacity", "segment"])?;
    let segments =
        r.0.all("segment")
            .map(|bytes| {
                let s = R::of(
                    bytes,
                    &[
                        "start", "bytes", "region", "number", "node", "kind", "name", "role",
                    ],
                )?;
                let piece = match s.opt_num("region")? {
                    Some(region) => {
                        s.absent(&["number", "node", "kind", "name", "role"])?;
                        Piece::Free { region }
                    }
                    None => Piece::Partition {
                        number: s.num("number")?,
                        node: s.text("node")?.into(),
                        kind: s.text("kind")?.into(),
                        name: s.text("name")?.into(),
                        role: s.opt_text("role")?.map(role_named).transpose()?,
                    },
                };
                Ok(Segment {
                    start: s.num("start")?,
                    bytes: s.num("bytes")?,
                    piece,
                })
            })
            .collect::<Result<_, Malformed>>()?;
    Ok(Layout {
        capacity: r.num("capacity")?,
        segments,
    })
}

fn encode_contents(contents: &Contents) -> W {
    match contents {
        Contents::Gpt => W::new().text("contents", "gpt"),
        Contents::NoTable(no_table) => encode_no_table(no_table),
    }
}

fn decode_contents(bytes: &[u8]) -> Result<Contents, Malformed> {
    let r = R::of(bytes, &["contents", "mbr"])?;
    if r.text("contents")? == "gpt" {
        r.absent(&["mbr"])?;
        return Ok(Contents::Gpt);
    }
    decode_no_table(bytes).map(Contents::NoTable)
}

fn encode_no_table(no_table: &NoTable) -> W {
    let name = match no_table {
        NoTable::Blank => "blank",
        NoTable::Mbr(_) => "mbr",
        NoTable::UnreadableGpt => "unreadable-gpt",
        NoTable::Ext4 => "ext4",
        NoTable::Data => "data",
    };
    let mut w = W::new().text("contents", name);
    if let NoTable::Mbr(parts) = no_table {
        for part in parts {
            w = w.nest(
                "mbr",
                W::new()
                    .num("index", part.index)
                    .num("kind", part.kind)
                    .num("bytes", part.bytes),
            );
        }
    }
    w
}

fn decode_no_table(bytes: &[u8]) -> Result<NoTable, Malformed> {
    let r = R::of(bytes, &["contents", "mbr"])?;
    let kind = r.text("contents")?;
    if kind != "mbr" {
        r.absent(&["mbr"])?;
    }
    Ok(match kind {
        "blank" => NoTable::Blank,
        "unreadable-gpt" => NoTable::UnreadableGpt,
        "ext4" => NoTable::Ext4,
        "data" => NoTable::Data,
        "mbr" => NoTable::Mbr(
            r.0.all("mbr")
                .map(|bytes| {
                    let p = R::of(bytes, &["index", "kind", "bytes"])?;
                    Ok(MbrPartition {
                        index: p.num("index")?,
                        kind: p.num("kind")?,
                        bytes: p.num("bytes")?,
                    })
                })
                .collect::<Result<_, Malformed>>()?,
        ),
        _ => return Err(Malformed),
    })
}

fn encode_opened(o: &Opened) -> W {
    let mut w = W::new()
        .nest("erase", encode_fit(&o.erase))
        .nest("free", encode_fit(&o.free))
        .opt_num("largest", o.largest)
        .nest("reuse", encode_fit(&o.reuse));
    for fit in &o.regions {
        w = w.nest("region", encode_fit(fit));
    }
    for (number, fit) in &o.roots {
        w = w.nest(
            "root",
            W::new().num("number", *number).nest("fit", encode_fit(fit)),
        );
    }
    match &o.reinstall {
        Some(r) => w.nest(
            "reinstall",
            W::new()
                .num("root", r.root)
                .flag("keep", r.keep)
                .nest("fit", encode_fit(&r.fit)),
        ),
        None => w,
    }
}

fn decode_opened(bytes: &[u8]) -> Result<Opened, Malformed> {
    let r = R::of(
        bytes,
        &[
            "erase",
            "free",
            "largest",
            "reuse",
            "region",
            "root",
            "reinstall",
        ],
    )?;
    let regions =
        r.0.all("region")
            .map(decode_fit)
            .collect::<Result<_, _>>()?;
    let roots =
        r.0.all("root")
            .map(|bytes| {
                let p = R::of(bytes, &["number", "fit"])?;
                Ok((p.num("number")?, decode_fit(p.nested("fit")?)?))
            })
            .collect::<Result<_, Malformed>>()?;
    let reinstall = match r.opt("reinstall")? {
        Some(bytes) => {
            let p = R::of(bytes, &["root", "keep", "fit"])?;
            Some(Reinstall {
                root: p.num("root")?,
                keep: p.flag("keep")?,
                fit: decode_fit(p.nested("fit")?)?,
            })
        }
        None => None,
    };
    Ok(Opened {
        erase: decode_fit(r.nested("erase")?)?,
        free: decode_fit(r.nested("free")?)?,
        largest: r.opt_num("largest")?,
        regions,
        reuse: decode_fit(r.nested("reuse")?)?,
        roots,
        reinstall,
    })
}

fn encode_system(system: &System) -> W {
    let m = &system.medium;
    let mut w = W::new().nest(
        "medium",
        W::new()
            .flag("toolchain", m.toolchain)
            .flag("source", m.source)
            .opt_num("payload", m.payload)
            .num("root_min", m.root_min)
            .opt_text("unmounted_payload", m.unmounted_payload.as_deref()),
    );
    for disk in &system.disks {
        let d = W::new()
            .text("node", &disk.node)
            .opt_text("bus", disk.bus.map(bus_name))
            .opt_text("model", disk.model.as_deref());
        let d = match &disk.state {
            DiskState::Refused(why) => d.text("state", "refused").text("why", refusal_name(*why)),
            DiskState::Unreadable(why) => d.text("state", "unreadable").text("why", why),
            DiskState::Listed(listing) => {
                let d = d
                    .text("state", "listed")
                    .num("capacity", listing.capacity)
                    .num("block", listing.block)
                    .nest("contents", encode_contents(&listing.contents))
                    .nest("layout", encode_layout(&listing.layout));
                match &listing.opened {
                    Some(opened) => d.nest("opened", encode_opened(opened)),
                    None => d,
                }
            }
        };
        w = w.nest("disk", d);
    }
    match &system.others {
        Ok(titles) => w.texts("other", titles),
        Err(why) => w.text("others_unread", why),
    }
}

fn decode_system(bytes: &[u8]) -> Result<System, Malformed> {
    let r = R::of(bytes, &["medium", "disk", "other", "others_unread"])?;
    let m = R::of(
        r.nested("medium")?,
        &[
            "toolchain",
            "source",
            "payload",
            "root_min",
            "unmounted_payload",
        ],
    )?;
    let medium = Medium {
        toolchain: m.flag("toolchain")?,
        source: m.flag("source")?,
        payload: m.opt_num("payload")?,
        root_min: m.num("root_min")?,
        unmounted_payload: m.opt_text("unmounted_payload")?.map(String::from),
    };
    let disks =
        r.0.all("disk")
            .map(|bytes| {
                let d = R::of(
                    bytes,
                    &[
                        "node", "bus", "model", "state", "why", "capacity", "block", "contents",
                        "layout", "opened",
                    ],
                )?;
                let listed = ["capacity", "block", "contents", "layout", "opened"];
                let state = match d.text("state")? {
                    "refused" => {
                        d.absent(&listed)?;
                        DiskState::Refused(refusal_named(d.text("why")?)?)
                    }
                    "unreadable" => {
                        d.absent(&listed)?;
                        DiskState::Unreadable(d.text("why")?.into())
                    }
                    "listed" => {
                        d.absent(&["why"])?;
                        DiskState::Listed(Listing {
                            capacity: d.num("capacity")?,
                            block: d.num("block")?,
                            contents: decode_contents(d.nested("contents")?)?,
                            layout: decode_layout(d.nested("layout")?)?,
                            opened: d.opt("opened")?.map(decode_opened).transpose()?,
                        })
                    }
                    _ => return Err(Malformed),
                };
                Ok(Disk {
                    node: d.text("node")?.into(),
                    bus: d.opt_text("bus")?.map(bus_named).transpose()?,
                    model: d.opt_text("model")?.map(String::from),
                    state,
                })
            })
            .collect::<Result<_, Malformed>>()?;
    let others = match r.opt_text("others_unread")? {
        Some(why) => {
            r.absent(&["other"])?;
            Err(why.into())
        }
        None => Ok(r.texts("other")?),
    };
    Ok(System {
        medium,
        disks,
        others,
    })
}

fn bus_name(bus: Bus) -> &'static str {
    match bus {
        Bus::Nvme => "nvme",
        Bus::Virtio => "virtio",
        Bus::Usb => "usb",
    }
}

fn bus_named(name: &str) -> Result<Bus, Malformed> {
    Ok(match name {
        "nvme" => Bus::Nvme,
        "virtio" => Bus::Virtio,
        "usb" => Bus::Usb,
        _ => return Err(Malformed),
    })
}

fn refusal_name(refusal: Refusal) -> &'static str {
    match refusal {
        Refusal::Medium => "medium",
        Refusal::ReadOnly => "read-only",
    }
}

fn refusal_named(name: &str) -> Result<Refusal, Malformed> {
    Ok(match name {
        "medium" => Refusal::Medium,
        "read-only" => Refusal::ReadOnly,
        _ => return Err(Malformed),
    })
}

fn tag_name(tag: Tag) -> &'static str {
    match tag {
        Tag::Erase => "erase",
        Tag::New => "new",
        Tag::Format => "format",
        Tag::Share => "share",
        Tag::Keep => "keep",
        Tag::Reuse => "reuse",
    }
}

fn tag_named(name: &str) -> Result<Tag, Malformed> {
    Ok(match name {
        "erase" => Tag::Erase,
        "new" => Tag::New,
        "format" => Tag::Format,
        "share" => Tag::Share,
        "keep" => Tag::Keep,
        "reuse" => Tag::Reuse,
        _ => return Err(Malformed),
    })
}

fn encode_part(part: &Part) -> W {
    match part {
        Part::New {
            number,
            node,
            start,
            bytes,
        } => W::new()
            .num("number", *number)
            .text("node", node)
            .num("start", *start)
            .num("bytes", *bytes),
        Part::Existing {
            number,
            node,
            kind,
            bytes,
        } => W::new()
            .num("number", *number)
            .text("node", node)
            .text("kind", kind)
            .num("bytes", *bytes),
    }
}

fn decode_part(bytes: &[u8]) -> Result<Part, Malformed> {
    let r = R::of(bytes, &["number", "node", "start", "kind", "bytes"])?;
    let (number, node, bytes) = (r.num("number")?, r.text("node")?.into(), r.num("bytes")?);
    match (r.opt_num("start")?, r.opt_text("kind")?) {
        (Some(start), None) => Ok(Part::New {
            number,
            node,
            start,
            bytes,
        }),
        (None, Some(kind)) => Ok(Part::Existing {
            number,
            node,
            kind: kind.into(),
            bytes,
        }),
        _ => Err(Malformed),
    }
}

fn encode_proposal(p: &Proposal) -> W {
    let mut w = W::new()
        .text("disk", &p.disk)
        .nest("before", encode_layout(&p.before))
        .nest("after", encode_layout(&p.after))
        .flag("erase", p.erase)
        .num("root_min", p.root.min)
        .num("root_max", p.root.max)
        .num("root_chosen", p.root.chosen);
    for action in &p.actions {
        let a = W::new().text("tag", tag_name(action.tag));
        let a = match &action.target {
            Target::Disk => a.text("target", "disk"),
            Target::Role(role, part) => a
                .text("target", "role")
                .text("role", role_name(*role))
                .nest("part", encode_part(part)),
            Target::Other(part) => a.text("target", "other").nest("part", encode_part(part)),
        };
        w = w.nest("action", a);
    }
    let s = &p.setup;
    let mut setup = W::new()
        .flag("toolchain", s.toolchain)
        .opt_text("remote", s.remote.as_deref())
        .text("cmdline", &s.cmdline)
        .flag("first", s.first)
        .texts("menu", &s.menu)
        .texts("beside", &s.beside)
        .texts("left_out", &s.left_out)
        .texts("removed", &s.removed);
    if let Some(person) = &s.person {
        setup = setup.nest(
            "person",
            W::new()
                .text("name", &person.name)
                .flag("password", person.password),
        );
    }
    w.nest("setup", setup)
}

fn decode_proposal(bytes: &[u8]) -> Result<Proposal, Malformed> {
    let r = R::of(
        bytes,
        &[
            "disk",
            "before",
            "after",
            "erase",
            "root_min",
            "root_max",
            "root_chosen",
            "action",
            "setup",
        ],
    )?;
    let actions =
        r.0.all("action")
            .map(|bytes| {
                let a = R::of(bytes, &["tag", "target", "role", "part"])?;
                let target = match a.text("target")? {
                    "disk" => {
                        a.absent(&["role", "part"])?;
                        Target::Disk
                    }
                    "role" => Target::Role(
                        role_named(a.text("role")?)?,
                        decode_part(a.nested("part")?)?,
                    ),
                    "other" => {
                        a.absent(&["role"])?;
                        Target::Other(decode_part(a.nested("part")?)?)
                    }
                    _ => return Err(Malformed),
                };
                Ok(Action {
                    tag: tag_named(a.text("tag")?)?,
                    target,
                })
            })
            .collect::<Result<_, Malformed>>()?;
    let s = R::of(
        r.nested("setup")?,
        &[
            "toolchain",
            "remote",
            "cmdline",
            "first",
            "menu",
            "beside",
            "left_out",
            "removed",
            "person",
        ],
    )?;
    let person = match s.opt("person")? {
        Some(bytes) => {
            let p = R::of(bytes, &["name", "password"])?;
            Some(FirstPerson {
                name: p.text("name")?.into(),
                password: p.flag("password")?,
            })
        }
        None => None,
    };
    Ok(Proposal {
        disk: r.text("disk")?.into(),
        before: decode_layout(r.nested("before")?)?,
        after: decode_layout(r.nested("after")?)?,
        erase: r.flag("erase")?,
        actions,
        root: Span {
            min: r.num("root_min")?,
            max: r.num("root_max")?,
            chosen: r.num("root_chosen")?,
        },
        setup: Setup {
            toolchain: s.flag("toolchain")?,
            remote: s.opt_text("remote")?.map(String::from),
            person,
            cmdline: s.text("cmdline")?.into(),
            first: s.flag("first")?,
            menu: s.texts("menu")?,
            beside: s.texts("beside")?,
            left_out: s.texts("left_out")?,
            removed: s.texts("removed")?,
        },
    })
}

#[cfg(test)]
mod tests;
