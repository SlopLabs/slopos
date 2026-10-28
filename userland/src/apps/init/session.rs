//! The sessions init keeps: each person's desktop, the login screen, a shell
//! a console login started, each under a login tag of its own that every
//! process of it carries, by which it is ended. One desktop or the login
//! screen is in front, holding the screen and the input sink; the others run
//! behind, locked. Init writes the table to `/run/sessions` and tells privd
//! of it.

use std::path::Path;

use slopos_abi::signal::{SIGCONT, SIGKILL, SIGTERM};
use slopos_abi::syscall::{O_CLOEXEC, O_NONBLOCK};
use slopos_abi::task::SESSION_LOGINS;
use slopos_accounts_core::Person;
use slopos_priv_core::sessions::{Class, LINKS, Session as Seen, Table};

use super::service::{
    Budget, COMPOSITOR, Child, GREETER, Identity, SHELL, TERMINAL, WELCOME, runtime_dir, start,
    start_ready,
};
use crate::init_control::{self, Tasks, fds};
use crate::sessions::{self as listed, Entry};
use crate::syscall::{OwnedFd, RawFd, fs, process, window};

/// How long what is ended has on `SIGTERM` before `SIGKILL`, and how long
/// after that init stops waiting for it.
const KILL_AFTER_MS: u64 = 5000;
const GIVE_UP_AFTER_MS: u64 = 10_000;
/// How long a desktop has to say it is ready.
const STARTING_MS: u64 = 60_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Then {
    Start,
    Stay,
}

/// How an end went.
pub enum Ended {
    All,
    /// This many processes outlived the wait.
    GaveUp(usize),
}

pub enum State {
    /// Not running, starting again when `due`.
    Down {
        due: Option<u64>,
    },
    /// The compositor, until it says it is ready; a desktop's first window is
    /// started then.
    Starting {
        leader: Child,
        ready: OwnedFd,
        since: u64,
    },
    Running {
        leader: Child,
        /// The readiness pipe of a compositor that was not ready in time,
        /// which is given the seats only once it says it is.
        unready: Option<OwnedFd>,
    },
    Ending(Ending),
}

pub struct Session {
    pub tag: u32,
    pub who: Identity,
    pub class: Class,
    pub state: State,
    pub link: Option<u8>,
    pub locked: bool,
    /// The boot's desktop on a live boot, which opens the installer's welcome
    /// rather than a terminal once its compositor is ready.
    welcome: bool,
    pub budget: Budget,
    /// Where init tells its compositor whether it is in front.
    pipe: Option<OwnedFd>,
    /// The terminal a console session's shell runs on, until it starts.
    tty: Option<OwnedFd>,
    tty_name: Option<String>,
    /// Requests to answer once it has ended.
    pub waiting: Vec<OwnedFd>,
}

impl Session {
    pub fn leader(&self) -> Option<&Child> {
        match &self.state {
            State::Starting { leader, .. } | State::Running { leader, .. } => Some(leader),
            _ => None,
        }
    }

    fn leader_mut(&mut self) -> Option<&mut Child> {
        match &mut self.state {
            State::Starting { leader, .. } | State::Running { leader, .. } => Some(leader),
            _ => None,
        }
    }

    pub fn running(&self) -> bool {
        matches!(self.state, State::Running { .. })
    }

    /// Whether it runs and its compositor, if it has one, said it is ready.
    fn shows(&self) -> bool {
        matches!(self.state, State::Running { unready: None, .. })
    }

    /// Whether `pid` leads it, or did until it began to end.
    pub fn led_by(&self, pid: u32) -> bool {
        self.leader().is_some_and(|c| c.pid == pid)
            || matches!(&self.state, State::Ending(e) if e.leader == Some(pid))
    }

    /// Tell its compositor `word`; one that does not read it loses it.
    fn tell(&self, word: &str) {
        if let Some(pipe) = &self.pipe {
            let _ = fs::write_slice(pipe.raw(), format!("{word}\n").as_bytes());
        }
    }
}

/// The desktop or login screen to bring in front once it is ready, and
/// whether to tell it the person at the machine has proved themselves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Want {
    pub tag: u32,
    pub unlock: bool,
}

pub struct Sessions {
    pub list: Vec<Session>,
    pub front: Option<u32>,
    pub want: Option<Want>,
    next_tag: u32,
    /// Uid 0 ended the sessions; nothing starts one until uid 0 asks.
    pub held: bool,
    /// Whether the screen is to show a session: not on a test boot until
    /// one is asked for.
    pub wanted: bool,
    /// The next session the screen needs is the boot's: the person the
    /// record logs in by itself, else the login screen.
    pub fresh: bool,
    pub default_budget: Budget,
    default_due: Option<u64>,
    /// Requests to answer once every session has ended, and how many
    /// processes outlived the wait on the way.
    all_waiting: Vec<OwnedFd>,
    outlived: usize,
    dirty: bool,
}

/// What a request to end sessions answers once they have.
pub struct Answer {
    pub reply: OwnedFd,
    pub said: String,
}

impl Sessions {
    pub fn new(wanted: bool, next_tag: u32) -> Sessions {
        Sessions {
            list: Vec::new(),
            front: None,
            want: None,
            next_tag: next_tag.max(SESSION_LOGINS.start),
            held: false,
            wanted,
            fresh: true,
            default_budget: Budget::default(),
            default_due: None,
            all_waiting: Vec::new(),
            outlived: 0,
            dirty: true,
        }
    }

    pub fn next_tag(&self) -> u32 {
        self.next_tag
    }

    /// A tag no session of this boot had: past what init gave and what any
    /// process still carries.
    fn tag(&mut self) -> u32 {
        let carried = init_control::Tasks::read().tags_in_range(SESSION_LOGINS);
        let tag = carried
            .into_iter()
            .map(|tag| tag + 1)
            .chain([self.next_tag])
            .max()
            .unwrap_or(self.next_tag);
        self.next_tag = tag + 1;
        tag
    }

    pub fn find(&self, tag: u32) -> Option<&Session> {
        self.list.iter().find(|s| s.tag == tag)
    }

    fn find_mut(&mut self, tag: u32) -> Option<&mut Session> {
        self.list.iter_mut().find(|s| s.tag == tag)
    }

    /// The desktop of `uid`, if one is logged in.
    pub fn desktop_of(&self, uid: u32) -> Option<u32> {
        self.list
            .iter()
            .find(|s| s.class == Class::User && s.who.uid == uid)
            .map(|s| s.tag)
    }

    fn greeter(&self) -> Option<u32> {
        self.list
            .iter()
            .find(|s| s.class == Class::Greeter)
            .map(|s| s.tag)
    }

    /// Whether the first person's desktop runs, which an extension's session
    /// services run beside.
    pub fn first_person_runs(&self) -> bool {
        self.list.iter().any(|s| {
            s.class == Class::User && s.who.uid == slopos_abi::fs::SESSION_UID && s.running()
        })
    }

    fn free_link(&self) -> Option<u8> {
        (0..LINKS as u8).find(|link| self.list.iter().all(|s| s.link != Some(*link)))
    }

    /// Bring a desktop of `person` in front: theirs if one is logged in,
    /// told the person proved themselves when `unlock`, else a new one.
    pub fn log_in(&mut self, person: &Person, unlock: bool, now: u64) -> Result<u32, String> {
        if let Some(tag) = self.desktop_of(person.uid) {
            if let Some(State::Ending(ending)) = self.find_mut(tag).map(|s| &mut s.state) {
                ending.then = Then::Start;
            }
            self.want = Some(Want { tag, unlock });
            return Ok(tag);
        }
        let tag = self.add(Identity::person(person, None), Class::User, None, now)?;
        self.want = Some(Want { tag, unlock: false });
        Ok(tag)
    }

    /// A shell for `person` on the terminal `tty`, a session of its own.
    pub fn log_in_console(
        &mut self,
        person: &Person,
        tty: OwnedFd,
        now: u64,
    ) -> Result<u32, String> {
        let tty_name = crate::syscall::tty::name_of(tty.raw());
        let tag = self.add(
            Identity::person(person, None),
            Class::Console,
            Some(tty),
            now,
        )?;
        if let Some(session) = self.find_mut(tag) {
            session.tty_name = tty_name;
        }
        Ok(tag)
    }

    /// Bring the login screen in front, starting it if it is not there.
    pub fn show_greeter(&mut self, now: u64) -> Result<u32, String> {
        let tag = match self.greeter() {
            Some(tag) => tag,
            None => self.add(Identity::greeter(None), Class::Greeter, None, now)?,
        };
        self.want = Some(Want { tag, unlock: false });
        Ok(tag)
    }

    fn add(
        &mut self,
        mut who: Identity,
        class: Class,
        tty: Option<OwnedFd>,
        now: u64,
    ) -> Result<u32, String> {
        let link = match class.graphical() {
            true => Some(
                self.free_link()
                    .ok_or("every desktop init can ask is logged in already")?,
            ),
            false => None,
        };
        let tag = self.tag();
        who.login = Some(tag);
        self.list.push(Session {
            tag,
            who,
            class,
            state: State::Down { due: Some(now) },
            link,
            locked: false,
            welcome: false,
            budget: Budget::default(),
            pipe: None,
            tty,
            tty_name: None,
            waiting: Vec::new(),
        });
        self.dirty = true;
        Ok(tag)
    }

    /// A session init's earlier image ran, taken over at resume: its leader
    /// running as `leader`, or due to start.
    #[allow(clippy::too_many_arguments)]
    pub fn adopt(
        &mut self,
        tag: u32,
        who: Identity,
        class: Class,
        leader: Option<u32>,
        link: Option<u8>,
        locked: bool,
        pipe: Option<OwnedFd>,
        tty_name: Option<String>,
        starts: Vec<u64>,
        now: u64,
    ) {
        let state = match leader {
            Some(pid) => State::Running {
                leader: Child::watch(pid),
                unready: None,
            },
            None => State::Down { due: Some(now) },
        };
        let mut who = who;
        who.login = Some(tag);
        self.list.push(Session {
            tag,
            who,
            class,
            state,
            link,
            locked,
            welcome: false,
            budget: Budget::of(starts),
            pipe,
            tty: None,
            tty_name,
            waiting: Vec::new(),
        });
        self.next_tag = self.next_tag.max(tag + 1);
        self.dirty = true;
    }

    /// Start what is due: a session's leader, and, when nothing is or is to
    /// be in front, the boot's session.
    pub fn start_due(&mut self, now: u64, agents: &[RawFd]) {
        let shown = [self.front, self.want.map(|want| want.tag)];
        for session in &mut self.list {
            if let State::Down { due: Some(at) } = session.state
                && at <= now
            {
                session.budget.record(now);
                let behind = !shown.contains(&Some(session.tag));
                session.state = launch(session, now, agents, behind);
                if let State::Down { due } = &mut session.state {
                    *due = Some(session.budget.next(now));
                }
            }
        }
        let shown = self.front.is_some()
            || self
                .want
                .is_some_and(|want| self.find(want.tag).is_some_and(|s| s.class.graphical()));
        if shown || self.held || !self.wanted {
            self.default_due = None;
            return;
        }
        let due = *self
            .default_due
            .get_or_insert(self.default_budget.next(now));
        if due > now {
            return;
        }
        self.default_due = None;
        self.default_budget.record(now);
        let started = match self.fresh.then(autologin).flatten() {
            Some(person) => {
                let started = self.log_in(&person, false, now);
                // A desktop already running has opened its first window.
                if let Ok(tag) = started
                    && let Some(session) = self.find_mut(tag)
                    && session.leader().is_none()
                {
                    session.welcome = super::live();
                }
                started
            }
            None => self.show_greeter(now),
        };
        if let Err(why) = started {
            eprintln!("init: the screen shows nothing: {why}");
        }
        self.fresh = false;
    }

    /// A session's compositor said it is ready, or was given up on: a
    /// desktop gets its first window.
    pub fn step_starting(&mut self, now: u64) {
        for session in &mut self.list {
            if let State::Running { unready, .. } = &mut session.state
                && unready
                    .as_ref()
                    .is_some_and(|ready| !matches!(readiness(ready), Readiness::Pending))
            {
                *unready = None;
                continue;
            }
            let State::Starting { ready, since, .. } = &session.state else {
                continue;
            };
            let (up, late) = match readiness(ready) {
                Readiness::Pending if now.saturating_sub(*since) < STARTING_MS => continue,
                Readiness::Pending => {
                    eprintln!("init: the compositor was not ready in time");
                    (true, true)
                }
                Readiness::Ready => (true, false),
                Readiness::Gone => (false, false),
            };
            let State::Starting { leader, ready, .. } =
                std::mem::replace(&mut session.state, State::Down { due: None })
            else {
                unreachable!()
            };
            if up && session.class == Class::User {
                let opens = match std::mem::take(&mut session.welcome) {
                    true => &WELCOME,
                    false => &TERMINAL,
                };
                let _ = start(opens, Some(&session.who), &[]);
            }
            if session.locked {
                session.tell("lock");
            }
            session.state = State::Running {
                leader,
                unready: late.then_some(ready),
            };
            self.dirty = true;
        }
    }

    /// Bring the session wanted in front once its compositor runs: the seats
    /// are its, the one behind is told and locks, and a login screen it
    /// replaces ends.
    pub fn step_front(&mut self, now: u64) {
        let Some(want) = self.want else {
            return;
        };
        let Some(session) = self.find(want.tag) else {
            self.want = None;
            return;
        };
        let Some(pid) = session.leader().filter(|_| session.shows()).map(|c| c.pid) else {
            return;
        };
        if window::seat_switch(pid) != 0 {
            return;
        }
        let before = self.front.replace(want.tag);
        self.want = None;
        self.dirty = true;
        if let Some(session) = self.find_mut(want.tag) {
            session.tell(if want.unlock { "front unlock" } else { "front" });
            if want.unlock {
                session.locked = false;
            }
        }
        let greeters: Vec<u32> = self
            .list
            .iter()
            .filter(|s| s.class == Class::Greeter && s.tag != want.tag)
            .map(|s| s.tag)
            .collect();
        for tag in greeters {
            self.end(tag, Then::Stay, None, now);
        }
        if let Some(behind) = before
            .filter(|&tag| tag != want.tag)
            .and_then(|tag| self.find_mut(tag))
            .filter(|s| s.class == Class::User)
        {
            behind.tell("behind");
            behind.locked = true;
        }
    }

    /// Lock the desktop `tag` names: its compositor shows its lock screen.
    pub fn lock(&mut self, tag: u32) {
        if let Some(session) = self.find(tag).filter(|s| s.class == Class::User) {
            session.tell("lock");
        }
    }

    pub fn set_locked(&mut self, tag: u32, locked: bool) {
        if let Some(session) = self.find_mut(tag)
            && session.locked != locked
        {
            session.locked = locked;
            self.dirty = true;
        }
    }

    /// Have the session `tag` end, to start again when `then` says, its
    /// requester answered once it has.
    pub fn end(&mut self, tag: u32, then: Then, reply: Option<OwnedFd>, now: u64) {
        let Some(at) = self.list.iter().position(|s| s.tag == tag) else {
            if let Some(reply) = reply {
                super::control::answer(reply, "refused: no such session");
            }
            return;
        };
        let session = &mut self.list[at];
        session.waiting.extend(reply);
        session.state = match std::mem::replace(&mut session.state, State::Down { due: None }) {
            State::Ending(mut ending) => {
                ending.then = then;
                State::Ending(ending)
            }
            State::Starting { leader, .. } | State::Running { leader, .. } => {
                State::Ending(Ending::new(now, then, Some(leader.pid)))
            }
            State::Down { .. } => State::Ending(Ending::new(now, then, None)),
        };
        if self.front == Some(tag) {
            self.front = None;
            if then == Then::Start {
                self.want = Some(Want { tag, unlock: false });
            }
        }
        if self.want.is_some_and(|want| want.tag == tag) && then == Then::Stay {
            self.want = None;
        }
        self.dirty = true;
    }

    /// Every session, each to start again when `then` says, answering
    /// `reply` once none is left ending.
    pub fn end_all(&mut self, then: Then, reply: Option<OwnedFd>, now: u64) {
        let tags: Vec<u32> = self.list.iter().map(|s| s.tag).collect();
        for tag in tags {
            self.end(tag, then, None, now);
        }
        self.all_waiting.extend(reply);
    }

    /// Step what ends: once nothing of a session runs, its requesters are
    /// answered and it starts again or leaves the table. `extra` names what
    /// ends with the first person's desktop beside it.
    pub fn step_ending(&mut self, now: u64, extra: &[u32]) -> Vec<Answer> {
        let mut answers = Vec::new();
        let mut gone = Vec::new();
        for session in &mut self.list {
            let State::Ending(ending) = &mut session.state else {
                continue;
            };
            let first =
                session.class == Class::User && session.who.uid == slopos_abi::fs::SESSION_UID;
            let tags: Vec<u32> = std::iter::once(session.tag)
                .chain(extra.iter().copied().filter(|_| first))
                .collect();
            let Some(ended) = ending.termination.step(now, &tags) else {
                continue;
            };
            let said = match ended {
                Ended::All => "ok".to_owned(),
                Ended::GaveUp(left) => {
                    eprintln!(
                        "init: {left} processes of session {} would not end",
                        session.tag
                    );
                    self.outlived += left;
                    format!("refused: {left} processes of the session would not end")
                }
            };
            answers.extend(session.waiting.drain(..).map(|reply| Answer {
                reply,
                said: said.clone(),
            }));
            match ending.then {
                Then::Start if session.class != Class::Console => {
                    session.state = State::Down {
                        due: Some(session.budget.next(now)),
                    };
                }
                _ => gone.push(session.tag),
            }
            session.pipe = None;
            self.dirty = true;
        }
        for tag in gone {
            self.list.retain(|s| s.tag != tag);
            listed::remove(tag);
        }
        if !self.ending() {
            let said = match std::mem::take(&mut self.outlived) {
                0 => "ok".to_owned(),
                left => format!("refused: {left} processes of the sessions would not end"),
            };
            answers.extend(self.all_waiting.drain(..).map(|reply| Answer {
                reply,
                said: said.clone(),
            }));
        }
        answers
    }

    /// A process init waits on exited: a session's leader, whose session
    /// then ends and, unless it was ending already, starts again.
    pub fn exited(&mut self, pid: u32, now: u64) -> bool {
        let Some(session) = self.list.iter_mut().find(|s| s.led_by(pid)) else {
            return false;
        };
        let tag = session.tag;
        if let State::Ending(ending) = &mut session.state {
            ending.leader = None;
            return true;
        }
        let then = match session.class {
            Class::Console => Then::Stay,
            _ => {
                eprintln!("init: the compositor of session {tag} exited");
                Then::Start
            }
        };
        self.end(tag, then, None, now);
        true
    }

    pub fn children_mut(&mut self) -> impl Iterator<Item = &mut Child> {
        self.list.iter_mut().filter_map(Session::leader_mut)
    }

    pub fn poll_fds(&self) -> impl Iterator<Item = &OwnedFd> {
        self.list.iter().flat_map(|s| {
            let ready = match &s.state {
                State::Starting { ready, .. }
                | State::Running {
                    unready: Some(ready),
                    ..
                } => Some(ready),
                _ => None,
            };
            s.leader()
                .and_then(|c| c.exited.as_ref())
                .into_iter()
                .chain(ready)
        })
    }

    /// When the earliest restart is due.
    pub fn deadline(&self) -> Option<u64> {
        self.list
            .iter()
            .filter_map(|s| match s.state {
                State::Down { due } => due,
                _ => None,
            })
            .chain(self.default_due)
            .min()
    }

    pub fn ending(&self) -> bool {
        self.list
            .iter()
            .any(|s| matches!(s.state, State::Ending(_)))
    }

    /// Whether a re-execution can carry every session as it stands.
    pub fn settled(&self) -> bool {
        self.list.iter().all(|s| s.running()) && self.want.is_none()
    }

    /// The table as privd reads it.
    pub fn table(&self) -> Table {
        Table {
            sessions: self
                .list
                .iter()
                .map(|s| Seen {
                    tag: s.tag,
                    uid: s.who.uid,
                    class: s.class,
                    front: self.front == Some(s.tag),
                    locked: s.locked,
                    link: s.link,
                })
                .collect(),
        }
    }

    /// Whether the table changed since the last call.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// `/run/sessions` as the table stands, the background session beside.
    pub fn publish(&self, background: Option<Entry>) {
        for session in &self.list {
            listed::write(&Entry {
                tag: session.tag,
                uid: session.who.uid,
                user: session.who.name.clone(),
                class: session.class,
                active: self.front == Some(session.tag),
                locked: session.locked,
                leader: session.leader().map(|c| c.pid),
                tty: session.tty_name.clone(),
            });
        }
        if let Some(entry) = background {
            listed::write(&entry);
        }
    }

    /// What a re-execution carries of each session.
    pub fn carried(&self) -> Vec<Carried<'_>> {
        self.list
            .iter()
            .map(|s| Carried {
                session: s,
                leader: s.leader().map(|c| c.pid),
                pipe: s.pipe.as_ref().map(OwnedFd::raw),
                tty: s.tty_name.as_deref(),
            })
            .collect()
    }

    /// Every person who has a session, and whose runtime directory should
    /// therefore be there.
    pub fn people(&self) -> Vec<u32> {
        let mut uids: Vec<u32> = self
            .list
            .iter()
            .filter(|s| s.who.person)
            .map(|s| s.who.uid)
            .collect();
        uids.sort_unstable();
        uids.dedup();
        uids
    }
}

pub struct Carried<'a> {
    pub session: &'a Session,
    pub leader: Option<u32>,
    pub pipe: Option<RawFd>,
    pub tty: Option<&'a str>,
}

/// Start `session`'s leader: its compositor, with its consent link and its
/// pipe, which says first that it is behind when it is, or its shell on the
/// terminal it was handed.
fn launch(session: &mut Session, now: u64, agents: &[RawFd], behind: bool) -> State {
    if session.who.person {
        crate::root_layout::make_runtime_dir(&runtime_dir(session.who.uid), session.who.uid);
    }
    match session.class {
        Class::Console => {
            let Some(tty) = session.tty.take() else {
                return State::Down { due: None };
            };
            let fds = [(tty.raw(), 0), (tty.raw(), 1), (tty.raw(), 2)];
            match start(&SHELL, Some(&session.who), &fds) {
                Some(leader) => State::Running {
                    leader,
                    unready: None,
                },
                None => State::Down { due: None },
            }
        }
        _ => {
            let Ok((pipe, end)) = fs::pipe2((O_CLOEXEC | O_NONBLOCK) as u32) else {
                return State::Down { due: None };
            };
            let mut handed = vec![(pipe.raw(), fds::COMPOSITOR_SESSION)];
            if let Some(agent) = session.link.and_then(|link| agents.get(usize::from(link))) {
                handed.push((*agent, fds::COMPOSITOR_CONSENT));
            }
            let service = match session.class {
                Class::Greeter => &GREETER,
                _ => &COMPOSITOR,
            };
            if behind && session.class == Class::User {
                let _ = fs::write_slice(end.raw(), b"behind\n");
                session.locked = true;
            }
            match start_ready(service, Some(&session.who), &handed) {
                Some((leader, ready)) => {
                    session.pipe = Some(end);
                    State::Starting {
                        leader,
                        ready,
                        since: now,
                    }
                }
                None => State::Down { due: None },
            }
        }
    }
}

pub struct Ending {
    termination: Termination,
    /// The leader until init has seen it exit.
    pub leader: Option<u32>,
    pub then: Then,
}

impl Ending {
    pub fn new(now: u64, then: Then, leader: Option<u32>) -> Ending {
        Ending {
            termination: Termination::new(now),
            leader,
            then,
        }
    }
}

/// Ending everything that carries some login tags: `SIGTERM` to each
/// process, then to each forked since, and `SIGCONT` to whichever is
/// stopped, `SIGKILL` to all after [`KILL_AFTER_MS`], given up on after
/// [`GIVE_UP_AFTER_MS`].
pub struct Termination {
    since: u64,
    signalled: Vec<u32>,
}

/// Signal passes in one step: a pass finds what a process forked during
/// the one before.
const PASSES: usize = 8;

impl Termination {
    pub fn new(now: u64) -> Termination {
        Termination {
            since: now,
            signalled: Vec::new(),
        }
    }

    /// `Some` once nothing carrying any of `tags` runs, or once init has
    /// waited long enough.
    pub fn step(&mut self, now: u64, tags: &[u32]) -> Option<Ended> {
        for _ in 0..PASSES {
            match self.pass(now, &Tasks::read(), tags) {
                Err(ended) => return Some(ended),
                Ok(false) => break,
                Ok(true) => {}
            }
        }
        None
    }

    /// [`step`](Self::step) once, as of one reading init takes for
    /// everything it ends.
    pub fn step_seen(&mut self, now: u64, tasks: &Tasks, tags: &[u32]) -> Option<Ended> {
        self.pass(now, tasks, tags).err()
    }

    /// Signal what carries `tags`; whether any of it was new.
    fn pass(&mut self, now: u64, tasks: &Tasks, tags: &[u32]) -> Result<bool, Ended> {
        let left = tasks.processes_of(tags);
        let waited = now.saturating_sub(self.since);
        if left.is_empty() {
            return Err(Ended::All);
        }
        if waited >= GIVE_UP_AFTER_MS {
            return Err(Ended::GaveUp(left.len()));
        }
        let fresh: Vec<u32> = left
            .iter()
            .copied()
            .filter(|pid| !self.signalled.contains(pid))
            .collect();
        if waited >= KILL_AFTER_MS {
            for &pid in &left {
                let _ = process::kill(pid, SIGKILL);
            }
        } else {
            for &pid in &fresh {
                let _ = process::kill(pid, SIGTERM);
            }
            // A stopped process acts on its SIGTERM only once continued; a
            // running one is sent nothing more, since every kill walks the
            // task table.
            for pid in tasks.stopped_of(tags) {
                let _ = process::kill(pid, SIGCONT);
            }
        }
        let signalled = !fresh.is_empty();
        self.signalled.extend(fresh);
        Ok(signalled)
    }
}

enum Readiness {
    Pending,
    Ready,
    /// The compositor closed its notifier without a word: it has exited.
    Gone,
}

fn readiness(ready: &OwnedFd) -> Readiness {
    if !super::readable(ready) {
        return Readiness::Pending;
    }
    match fs::read_slice(ready.raw(), &mut [0u8; 1]) {
        Ok(1) => Readiness::Ready,
        _ => Readiness::Gone,
    }
}

/// The person the accounts record names `name`, as init reads it.
pub fn person_named(name: &str) -> Option<Person> {
    crate::accounts::Store::read(Path::new("/"))
        .accounts()
        .ok()?
        .person(name)
        .cloned()
}

/// The person the record logs in by itself, as init reads it.
pub fn autologin() -> Option<Person> {
    crate::accounts::Store::read(Path::new("/"))
        .accounts()
        .ok()?
        .autologin()
        .cloned()
}
