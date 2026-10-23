//! The window's pages over the engine: which page shows, what each answer is,
//! when the primary button acts and what Back restores, decided from the
//! person's input and the engine's replies alone, never from widget state.
//!
//! Welcome → Disk → How → Review → Install → Done or Error. Every change on
//! How proposes again, one proposal in flight at a time; Back keeps every
//! answer; Error's *Back to review* probes, opens the disk and proposes the
//! same choices again, landing on How when the disk no longer takes them.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use slopos_boot_core::install::Role;

use crate::protocol::{Reply, Request, VERSION};
use crate::{
    Choices, Event, Failure, Fit, Issue, Mode, Opened, Part, Proposal, STEPS, Summary, System, Tag,
    short,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Welcome,
    Disk,
    How,
    Review,
    Install,
    Done,
    Error,
}

/// How's tiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Erase,
    Free,
    Reuse,
    /// The SlopOS root the disk holds, installed over again.
    Reinstall,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    /// privd started the engine on the window's pipes.
    Started,
    /// privd would not start it, or the window could not ask, and why.
    Refused(String),
    /// The engine's output ended.
    Ended,
    Reply(Reply),
    Disk(String),
    Rescan,
    Kind(Kind),
    Region(usize),
    Root(u32),
    /// The root's size, or all the region gives.
    RootSize(Option<u64>),
    /// The `/src` remote as typed; empty for the engine's default.
    Remote(String),
    First(bool),
    Confirm(bool),
    /// The primary button.
    Next,
    Back,
    /// *Restart*, beside *Back to review* on the Error page.
    Restart,
    /// *Try SlopOS* or *Keep using the live system*, or the window's close.
    Close,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Out {
    /// Ask privd for `org.slopos.install` with `--serve` on two new pipes,
    /// then say [`Input::Started`] or [`Input::Refused`].
    Start,
    Send(Request),
    /// Ask privd to restart the machine.
    Restart,
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepState {
    Pending,
    Running,
    Done,
    Failed,
}

/// The How page's answers, for one disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answers {
    pub disk: String,
    pub kind: Kind,
    /// The free region, or the largest.
    pub region: Option<usize>,
    /// The partition a reuse takes as the root.
    pub root: Option<u32>,
    /// The root's size, or all the region gives.
    pub root_size: Option<u64>,
    pub remote: Option<String>,
    pub first: bool,
}

/// The engine's answer to the choices last proposed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proposed {
    pub choices: Choices,
    pub answer: Result<(u64, Proposal), Issue>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    /// The step under way, from 1; 0 before the first.
    pub step: u32,
    /// The copy under way: its bytes so far and its total.
    pub bytes: Option<(u64, u64)>,
    pub log: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Asked {
    Hello,
    Probe,
    Open(String),
    Propose(Choices),
    Install,
}

pub struct Wizard {
    page: Page,
    note: Option<String>,
    starting: bool,
    running: bool,
    asked: VecDeque<Asked>,
    system: Option<System>,
    disk: Option<String>,
    answers: Option<Answers>,
    proposed: Option<Proposed>,
    /// What the engine refused on the Disk page.
    refused: Option<Issue>,
    confirmed: bool,
    progress: Progress,
    summary: Option<Summary>,
    failure: Option<Failure>,
    /// Error's *Back to review* under way.
    resume: bool,
}

impl Default for Wizard {
    fn default() -> Self {
        Self::new()
    }
}

impl Wizard {
    pub fn new() -> Wizard {
        Wizard {
            page: Page::Welcome,
            note: None,
            starting: false,
            running: false,
            asked: VecDeque::new(),
            system: None,
            disk: None,
            answers: None,
            proposed: None,
            refused: None,
            confirmed: false,
            progress: Progress::default(),
            summary: None,
            failure: None,
            resume: false,
        }
    }

    pub fn on(&mut self, input: Input) -> Vec<Out> {
        match input {
            Input::Started => self.started(),
            Input::Refused(why) => {
                self.starting = false;
                self.resume = false;
                self.note = Some(why);
                Vec::new()
            }
            Input::Ended => {
                self.ended();
                Vec::new()
            }
            Input::Reply(reply) => self.reply(reply),
            Input::Disk(node) => {
                let listed = self
                    .system
                    .as_ref()
                    .and_then(|s| s.disk(&node))
                    .is_some_and(|d| d.listing().is_some());
                if self.page == Page::Disk && self.idle() && listed {
                    self.disk = Some(node);
                    self.refused = None;
                }
                Vec::new()
            }
            Input::Rescan if self.page == Page::Disk && self.idle() => self.ask(Asked::Probe),
            Input::Rescan => Vec::new(),
            Input::Kind(kind) => self.answer(|w, a| {
                if w.tile(kind).is_some_and(|fit| fit.is_ok()) {
                    a.kind = kind;
                    if kind == Kind::Reuse && a.root.is_none() {
                        a.root = w.fitting_roots().first().copied();
                    }
                }
            }),
            Input::Region(region) => self.answer(|w, a| {
                if a.kind == Kind::Free && w.fitting_regions().contains(&region) {
                    a.region = Some(region);
                }
            }),
            Input::Root(number) => self.answer(|w, a| {
                if a.kind == Kind::Reuse && w.fitting_roots().contains(&number) {
                    a.root = Some(number);
                }
            }),
            Input::RootSize(bytes) => self.answer(|_, a| a.root_size = bytes),
            Input::Remote(url) => self.answer(|_, a| a.remote = (!url.is_empty()).then_some(url)),
            Input::First(first) => self.answer(|_, a| a.first = first),
            Input::Confirm(on) => {
                if self.page == Page::Review {
                    self.confirmed = on;
                }
                Vec::new()
            }
            Input::Next => self.next(),
            Input::Back => {
                match self.page {
                    Page::Disk if self.idle() => self.page = Page::Welcome,
                    Page::How => self.page = Page::Disk,
                    Page::Review => {
                        self.page = Page::How;
                        self.confirmed = false;
                    }
                    _ => {}
                }
                Vec::new()
            }
            Input::Restart if matches!(self.page, Page::Done | Page::Error) => {
                alloc::vec![Out::Restart]
            }
            Input::Restart => Vec::new(),
            Input::Close if self.can_close() => alloc::vec![Out::Close],
            Input::Close => Vec::new(),
        }
    }

    fn start(&mut self) -> Vec<Out> {
        if self.starting {
            return Vec::new();
        }
        self.note = None;
        if self.running {
            return self.ask(Asked::Probe);
        }
        self.starting = true;
        alloc::vec![Out::Start]
    }

    fn started(&mut self) -> Vec<Out> {
        if !self.starting {
            return Vec::new();
        }
        self.starting = false;
        self.running = true;
        self.asked.clear();
        let mut out = self.ask(Asked::Hello);
        out.extend(self.ask(Asked::Probe));
        out
    }

    fn ended(&mut self) {
        self.running = false;
        self.starting = false;
        self.asked.clear();
        match self.page {
            Page::Install => {
                self.failure = Some(Failure {
                    step: self.progress.step,
                    message: "the installer stopped before it said how the install ended".into(),
                });
                self.page = Page::Error;
            }
            Page::Error if self.resume => {
                self.resume = false;
                self.note = Some("the installer stopped before the review".into());
            }
            Page::Done | Page::Error => {}
            _ => {
                self.page = Page::Welcome;
                self.note
                    .get_or_insert_with(|| "the installer stopped; it starts again".into());
            }
        }
    }

    fn ask(&mut self, asked: Asked) -> Vec<Out> {
        let request = match &asked {
            Asked::Hello => Request::Hello { version: VERSION },
            Asked::Probe => Request::Probe,
            Asked::Open(disk) => Request::Open { disk: disk.clone() },
            Asked::Propose(choices) => Request::Propose(choices.clone()),
            Asked::Install => {
                let Some((choices, id)) = self.current().map(|(c, id, _)| (c.clone(), id)) else {
                    return Vec::new();
                };
                Request::Install {
                    choices,
                    proposal: id,
                }
            }
        };
        self.asked.push_back(asked);
        alloc::vec![Out::Send(request)]
    }

    /// Propose the current choices, unless a proposal is already in flight,
    /// whose answer proposes them once it comes.
    fn propose(&mut self) -> Vec<Out> {
        let in_flight = self.asked.iter().any(|a| matches!(a, Asked::Propose(_)));
        match self.choices() {
            Some(choices) if !in_flight && self.running => self.ask(Asked::Propose(choices)),
            _ => Vec::new(),
        }
    }

    fn answer(&mut self, change: impl FnOnce(&Wizard, &mut Answers)) -> Vec<Out> {
        if self.page != Page::How {
            return Vec::new();
        }
        let Some(mut answers) = self.answers.clone() else {
            return Vec::new();
        };
        change(self, &mut answers);
        let before = self.choices();
        self.answers = Some(answers);
        if self.choices() == before {
            Vec::new()
        } else {
            self.propose()
        }
    }

    fn reply(&mut self, reply: Reply) -> Vec<Out> {
        if let Reply::Event(event) = reply {
            if self.page == Page::Install {
                match event {
                    Event::Step { index, .. } => {
                        self.progress.step = index;
                        self.progress.bytes = None;
                    }
                    Event::Progress { done, total } => self.progress.bytes = Some((done, total)),
                    Event::Log { line } => self.progress.log.push(line),
                }
            }
            return Vec::new();
        }
        let Some(asked) = self.asked.pop_front() else {
            return Vec::new();
        };
        match (asked, reply) {
            (Asked::Hello, Reply::Hello { version }) if version != VERSION => {
                self.note = Some(format!(
                    "the installer speaks version {version} of its protocol, and this window version {VERSION}"
                ));
                self.asked.clear();
                self.page = Page::Welcome;
                Vec::new()
            }
            (Asked::Hello, Reply::Hello { .. }) => Vec::new(),
            (Asked::Probe, Reply::System(system)) => self.probed(system),
            (Asked::Open(node), Reply::System(system)) => self.opened_disk(node, system),
            (Asked::Propose(choices), Reply::Proposal { id, proposal }) => {
                self.proposed(choices, Ok((id, proposal)))
            }
            (Asked::Propose(choices), Reply::Issue(issue)) => self.proposed(choices, Err(issue)),
            (Asked::Probe | Asked::Open(_), Reply::Issue(issue)) => {
                if self.resume {
                    self.resume = false;
                    self.note = Some(issue.to_string());
                } else if self.page == Page::Disk {
                    self.refused = Some(issue);
                } else {
                    self.note = Some(issue.to_string());
                    self.page = Page::Welcome;
                }
                Vec::new()
            }
            (Asked::Install, Reply::Done(summary)) => {
                self.summary = Some(summary);
                self.page = Page::Done;
                Vec::new()
            }
            (Asked::Install, Reply::Failed(failure)) => {
                self.failure = Some(failure);
                self.page = Page::Error;
                Vec::new()
            }
            (Asked::Install, _) => {
                self.failure = Some(Failure {
                    step: self.progress.step,
                    message: "the installer answered the install out of turn".into(),
                });
                self.page = Page::Error;
                Vec::new()
            }
            _ => {
                self.note = Some("the installer answered out of turn".into());
                Vec::new()
            }
        }
    }

    fn probed(&mut self, system: System) -> Vec<Out> {
        let offered = |node: &str| system.disk(node).is_some_and(|d| d.listing().is_some());
        if !self.disk.as_deref().is_some_and(offered) {
            self.disk = system
                .disks
                .iter()
                .find(|d| d.listing().is_some())
                .map(|d| d.node.clone());
        }
        self.system = Some(system);
        self.refused = None;
        if self.resume {
            match self.disk.clone() {
                Some(node) if self.answers.as_ref().is_some_and(|a| a.disk == node) => {
                    return self.ask(Asked::Open(node));
                }
                _ => self.resume = false,
            }
        }
        self.failure = None;
        self.page = Page::Disk;
        Vec::new()
    }

    fn opened_disk(&mut self, node: String, system: System) -> Vec<Out> {
        self.system = Some(system);
        let Some(opened) = self.opened_of(&node).cloned() else {
            self.resume = false;
            self.failure = None;
            self.page = Page::Disk;
            return Vec::new();
        };
        let previous = self.answers.take();
        let mut answers = match previous {
            Some(kept) if kept.disk == node => kept,
            other => Answers {
                disk: node,
                kind: default_kind(&opened),
                region: None,
                root: None,
                root_size: None,
                remote: other.as_ref().and_then(|a| a.remote.clone()),
                first: other.is_some_and(|a| a.first),
            },
        };
        let fits = |fit: Option<&Fit>| fit.is_some_and(|fit| fit.is_ok());
        let moved = !fits(tile_of(&opened, answers.kind));
        if moved {
            answers.kind = default_kind(&opened);
        }
        let regions = fitting_regions(&opened);
        if answers.region.is_some_and(|r| !regions.contains(&r)) {
            answers.region = None;
        }
        let roots = fitting_roots(&opened);
        if !answers.root.is_some_and(|r| roots.contains(&r)) {
            answers.root = roots.first().copied();
        }
        self.answers = Some(answers);
        self.proposed = None;
        if !self.resume || moved {
            self.resume = false;
            self.failure = None;
            self.page = Page::How;
        }
        self.propose()
    }

    fn proposed(&mut self, choices: Choices, answer: Result<(u64, Proposal), Issue>) -> Vec<Out> {
        let ok = answer.is_ok();
        let current = self.choices().as_ref() == Some(&choices);
        self.proposed = Some(Proposed { choices, answer });
        if self.resume && current {
            self.resume = false;
            self.failure = None;
            self.page = if ok { Page::Review } else { Page::How };
            self.confirmed = false;
        }
        if current { Vec::new() } else { self.propose() }
    }

    fn next(&mut self) -> Vec<Out> {
        if !self.can_advance() {
            return Vec::new();
        }
        match self.page {
            Page::Welcome => self.start(),
            Page::Disk => match self.disk.clone() {
                Some(node) => self.ask(Asked::Open(node)),
                None => Vec::new(),
            },
            Page::How => {
                self.page = Page::Review;
                self.confirmed = false;
                Vec::new()
            }
            Page::Review => {
                let out = self.ask(Asked::Install);
                if !out.is_empty() {
                    self.page = Page::Install;
                    self.progress = Progress::default();
                    self.failure = None;
                }
                out
            }
            Page::Error => {
                self.resume = true;
                self.start()
            }
            Page::Done => alloc::vec![Out::Restart],
            Page::Install => Vec::new(),
        }
    }

    fn idle(&self) -> bool {
        self.asked.is_empty() && !self.starting
    }

    fn opened_of(&self, node: &str) -> Option<&Opened> {
        self.system.as_ref()?.disk(node)?.opened()
    }

    fn opened(&self) -> Option<&Opened> {
        self.opened_of(&self.answers.as_ref()?.disk)
    }

    /// The choices the answers make: the engine's defaults for the person
    /// and the command line, and a size only for a root an install makes.
    pub fn choices(&self) -> Option<Choices> {
        let a = self.answers.as_ref()?;
        let opened = self.opened()?;
        let mode = match a.kind {
            Kind::Erase => Mode::Erase,
            Kind::Free => Mode::Free { region: a.region },
            Kind::Reuse => Mode::Reuse {
                root: a.root?,
                region: None,
                keep: false,
            },
            Kind::Reinstall => {
                let r = opened.reinstall.as_ref()?;
                Mode::Reuse {
                    root: r.root,
                    region: None,
                    keep: r.keep,
                }
            }
        };
        let source = self.system.as_ref().is_some_and(|s| s.medium.source);
        let mut choices = Choices::new(&a.disk, mode);
        choices.root_size = a
            .root_size
            .filter(|_| matches!(a.kind, Kind::Erase | Kind::Free));
        choices.remote = a.remote.clone().filter(|_| source);
        choices.first = a.first;
        Some(choices)
    }

    /// The proposal the current choices were answered with.
    fn current(&self) -> Option<(&Choices, u64, &Proposal)> {
        let proposed = self.proposed.as_ref()?;
        let (id, proposal) = proposed.answer.as_ref().ok()?;
        (Some(&proposed.choices) == self.choices().as_ref()).then_some((
            &proposed.choices,
            *id,
            proposal,
        ))
    }

    pub fn page(&self) -> Page {
        self.page
    }

    /// What the window says beside the page: why privd or the engine would
    /// not go on, or what stopped *Back to review*.
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    /// Whether the window waits on privd or the engine.
    pub fn waiting(&self) -> bool {
        !self.idle()
    }

    pub fn system(&self) -> Option<&System> {
        self.system.as_ref()
    }

    pub fn disk(&self) -> Option<&str> {
        self.disk.as_deref()
    }

    pub fn answers(&self) -> Option<&Answers> {
        self.answers.as_ref()
    }

    fn tile(&self, kind: Kind) -> Option<&Fit> {
        tile_of(self.opened()?, kind)
    }

    /// How's tiles, each with whether it may be chosen or why not; the
    /// reinstall only where the disk holds SlopOS's root.
    pub fn tiles(&self) -> Vec<(Kind, &Fit)> {
        [Kind::Erase, Kind::Free, Kind::Reuse, Kind::Reinstall]
            .into_iter()
            .filter_map(|kind| Some((kind, self.tile(kind)?)))
            .collect()
    }

    fn fitting_regions(&self) -> Vec<usize> {
        self.opened().map(fitting_regions).unwrap_or_default()
    }

    fn fitting_roots(&self) -> Vec<u32> {
        self.opened().map(fitting_roots).unwrap_or_default()
    }

    /// The region the free-space install takes.
    pub fn region(&self) -> Option<usize> {
        let a = self.answers.as_ref()?;
        (a.kind == Kind::Free)
            .then(|| a.region.or(self.opened()?.largest))
            .flatten()
    }

    /// The free regions a free-space install may choose between, when there
    /// are two or more.
    pub fn regions(&self) -> Vec<usize> {
        let regions = self.fitting_regions();
        match regions.len() {
            0 | 1 => Vec::new(),
            _ => regions,
        }
    }

    /// The partitions a reuse may take as the root, each with whether it
    /// fits.
    pub fn roots(&self) -> &[(u32, Fit)] {
        self.opened().map_or(&[], |o| o.roots.as_slice())
    }

    /// The proposal for the current choices, which Review and Install need.
    pub fn proposal(&self) -> Option<&Proposal> {
        self.current().map(|(_, _, p)| p)
    }

    /// The engine's last answer, which the How page shows while the answer
    /// to a change is on its way.
    pub fn last_answer(&self) -> Option<&Proposed> {
        self.proposed.as_ref()
    }

    /// What stops the page in front: the engine's refusal of the disk, or of
    /// the current choices.
    pub fn issue(&self) -> Option<&Issue> {
        match self.page {
            Page::Disk => self.refused.as_ref(),
            Page::How | Page::Review => {
                let proposed = self.proposed.as_ref()?;
                (Some(&proposed.choices) == self.choices().as_ref())
                    .then(|| proposed.answer.as_ref().err())
                    .flatten()
            }
            _ => None,
        }
    }

    pub fn confirmed(&self) -> bool {
        self.confirmed
    }

    pub fn progress(&self) -> &Progress {
        &self.progress
    }

    pub fn steps(&self) -> Vec<(&'static str, StepState)> {
        let at = self.progress.step;
        let failed = self.failure.as_ref().map(|f| f.step);
        STEPS
            .iter()
            .zip(1u32..)
            .map(|(&title, index)| {
                let state = match self.page {
                    Page::Done => StepState::Done,
                    _ if failed == Some(index) => StepState::Failed,
                    _ if index < at => StepState::Done,
                    Page::Install if index == at => StepState::Running,
                    _ => StepState::Pending,
                };
                (title, state)
            })
            .collect()
    }

    pub fn summary(&self) -> Option<&Summary> {
        self.summary.as_ref()
    }

    pub fn failure(&self) -> Option<&Failure> {
        self.failure.as_ref()
    }

    /// Whether the primary button acts.
    pub fn can_advance(&self) -> bool {
        match self.page {
            Page::Welcome | Page::Error => !self.starting && self.asked.is_empty(),
            Page::Disk => {
                self.idle()
                    && self
                        .disk
                        .as_deref()
                        .and_then(|node| self.system.as_ref()?.disk(node))
                        .is_some_and(|d| d.listing().is_some())
            }
            Page::How => self.idle() && self.current().is_some(),
            Page::Review => self.idle() && self.confirmed && self.current().is_some(),
            Page::Install => false,
            Page::Done => true,
        }
    }

    pub fn can_go_back(&self) -> bool {
        match self.page {
            Page::Disk => self.idle(),
            Page::How | Page::Review => true,
            _ => false,
        }
    }

    /// Whether the window may close: never while the engine installs.
    pub fn can_close(&self) -> bool {
        self.page != Page::Install
    }

    pub fn primary_label(&self) -> String {
        match self.page {
            Page::Welcome => "Install SlopOS".into(),
            Page::Disk => "Next".into(),
            Page::How => "Review".into(),
            Page::Review => self.scenario(),
            Page::Install => String::new(),
            Page::Done => "Restart now".into(),
            Page::Error => "Back to review".into(),
        }
    }

    fn kind(&self) -> Option<Kind> {
        self.answers.as_ref().map(|a| a.kind)
    }

    fn root_node(&self) -> Option<&str> {
        match self.proposal()?.role(Role::Root)?.1 {
            Part::Existing { node, .. } | Part::New { node, .. } => Some(short(node)),
        }
    }

    /// Whether the install formats a partition the disk holds.
    fn formats(&self) -> bool {
        self.proposal()
            .is_some_and(|p| p.actions.iter().any(|a| a.tag == Tag::Format))
    }

    fn keeps_root(&self) -> bool {
        self.proposal().is_some_and(Proposal::keeps_root)
    }

    /// The Review page's primary button, named by what it does.
    fn scenario(&self) -> String {
        let disk = self.answers.as_ref().map_or("", |a| short(&a.disk));
        let root = self.root_node().unwrap_or("");
        match self.kind() {
            Some(Kind::Erase) | None => format!("Erase {disk} and install"),
            Some(Kind::Free) => match self.proposal().and_then(|p| p.setup.beside.first()) {
                Some(other) => format!("Install beside {other}"),
                None => format!("Install in the free space on {disk}"),
            },
            Some(Kind::Reuse) => format!("Format {root} and install"),
            Some(Kind::Reinstall) if self.keeps_root() => "Reinstall SlopOS".into(),
            Some(Kind::Reinstall) => format!("Format {root} and reinstall SlopOS"),
        }
    }

    /// The Review page's checkbox, worded by the damage the install does.
    pub fn confirmation(&self) -> String {
        let disk = self.answers.as_ref().map_or("", |a| short(&a.disk));
        let root = self.root_node().unwrap_or("");
        match self.kind() {
            Some(Kind::Erase) | None => {
                format!("I understand that everything on {disk} will be erased")
            }
            Some(Kind::Free) if self.formats() => format!(
                "I understand that new partitions will be created in free space on {disk}, and what the review says is formatted"
            ),
            Some(Kind::Free) => format!(
                "I understand that new partitions will be created in free space on {disk}, and nothing else on it changes"
            ),
            Some(Kind::Reinstall) if self.keeps_root() => format!(
                "I understand that SlopOS on {disk} will be replaced, keeping what {root} holds"
            ),
            Some(Kind::Reuse | Kind::Reinstall) => {
                format!("I understand that {root} will be formatted")
            }
        }
    }
}

fn tile_of(opened: &Opened, kind: Kind) -> Option<&Fit> {
    match kind {
        Kind::Erase => Some(&opened.erase),
        Kind::Free => Some(&opened.free),
        Kind::Reuse => Some(&opened.reuse),
        Kind::Reinstall => opened.reinstall.as_ref().map(|r| &r.fit),
    }
}

/// A reinstall where SlopOS is, then free space, then an erase.
fn default_kind(opened: &Opened) -> Kind {
    [Kind::Reinstall, Kind::Free, Kind::Erase]
        .into_iter()
        .find(|&kind| tile_of(opened, kind).is_some_and(|fit| fit.is_ok()))
        .unwrap_or(Kind::Erase)
}

fn fitting_regions(opened: &Opened) -> Vec<usize> {
    (0..opened.regions.len())
        .filter(|&i| opened.regions[i].is_ok())
        .collect()
}

fn fitting_roots(opened: &Opened) -> Vec<u32> {
    opened
        .roots
        .iter()
        .filter(|(_, fit)| fit.is_ok())
        .map(|(number, _)| *number)
        .collect()
}

#[cfg(test)]
mod tests;
