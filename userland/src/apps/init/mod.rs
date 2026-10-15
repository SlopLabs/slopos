//! `/sbin/init`: lays out the root, starts the services and starts them again
//! when they crash, ends and restarts the desktop session, and re-executes
//! itself keeping all of that.

mod control;
mod reexec;
mod service;
mod session;

use slopos_abi::caps::{POWER, RAW_BLOCK};
use slopos_abi::fs::{SESSION_GID, SESSION_UID};
use slopos_abi::net::{SOCK_CLOEXEC, SOCK_NONBLOCK};
use slopos_abi::syscall::{
    BOOT_FLAG_ROULETTE_SKIP, BOOT_FLAG_TESTS_ENABLED, MSG_NOSIGNAL, POLLIN, UserPollFd,
};
use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};
use slopos_font::atlas::GlyphAtlas;

use self::control::{Control, Request, answer};
use self::reexec::{RESUME, State};
use self::service::{
    Budget, COMPOSITOR, Child, PRIVD, REMOTED, ROULETTE, RUN_TESTS, Supervised, TERMINAL, TESTS,
    runtime_dir, start, start_ready,
};
use self::session::{Ended, Ending, Session, Then};
use crate::apps::privd::CONSENT_FD;
use crate::init_control::{self, Command, SOCKET};
use crate::syscall::process::{self, Endowment, WaitStatus};
use crate::syscall::{OwnedFd, RawFd, UserSysInfo, core as sys_core, fs, net, tty};

fn upgrade_console_font() {
    let font_data = match crate::gfx::font_loader::load_font("mono") {
        Some(data) => data,
        None => return,
    };
    let atlas = match GlyphAtlas::new(font_data, 16) {
        Some(a) => a,
        None => return,
    };
    let mut payload = Vec::with_capacity(atlas.coverage_len() + atlas.replacement().len());
    for chunk in atlas.coverage_chunks() {
        payload.extend_from_slice(chunk);
    }
    payload.extend_from_slice(atlas.replacement());
    tty::font_set_coverage(
        &payload,
        atlas.cell_width() as u16,
        atlas.cell_height() as u16,
    );
}

/// What a crashed boot left goes to `/var/log/crash` before the tests and the
/// services run.
fn collect_crash_records() {
    let argv = [b"bootctl\0".as_ptr(), b"collect\0".as_ptr()];
    let actions = [process::clone_fd(1, 1), process::clone_fd(2, 2)];
    let tid = process::spawn_endowed(
        b"/bin/bootctl",
        &argv,
        &[],
        &[],
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &actions,
        0,
        Endowment {
            caps: RAW_BLOCK | POWER,
            ..Endowment::NONE
        },
    );
    if tid > 0 {
        let _ = process::waitpid(tid as u32);
    }
}

/// Requests answered once the session has ended, each holding a descriptor
/// of init's.
const MAX_WAITING: usize = 8;
/// How long init sleeps with nothing due, and while a session ends.
const TICK_MS: u64 = 1000;
const ENDING_TICK_MS: u64 = 50;
/// How long the terminal waits for the compositor to say it is ready.
const STARTING_MS: u64 = 60_000;

/// In the sealed base of a machine the host built paired; the daemon runs
/// only there.
const REMOTE_CONF: &str = "/usr/share/slopos/remote/remote.conf";

/// On a live boot, the medium's apps at the RAM root's `/usr/local`, where an
/// install puts them on a disk.
fn install_live_apps() {
    use slopos_boot_core::layout::{MEDIUM_APPS, MEDIUM_DIR};
    let live = crate::syscall::fs::statfs_path(c"/".as_ptr())
        .is_ok_and(|stats| stats.f_type == slopos_abi::fs::RAMFS_MAGIC);
    let apps = std::path::Path::new(MEDIUM_DIR).join(MEDIUM_APPS);
    if !live || !apps.is_dir() {
        return;
    }
    let install = crate::tree::Install {
        root: "",
        name: slopos_tree_core::APPS,
        guest: "/usr/local",
        source: &apps,
        owner: Some((SESSION_UID, SESSION_GID)),
        keeper: Some((SESSION_UID, SESSION_GID)),
        remedy: "move it aside",
    };
    match crate::tree::scan(&apps).and_then(|new| install.run(&new, &mut |_| {})) {
        Ok(Some(_)) => println!("init: installed the medium's apps at /usr/local"),
        Ok(None) => {}
        Err(e) => eprintln!("init: the medium's apps: {e}"),
    }
}

pub fn init_user_main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |i: usize| args.get(i).map(String::as_str);
    match arg(1) {
        Some(RUN_TESTS) => {
            let rc = sys_core::run_userland_tests();
            sys_core::exit_with_code(i32::from(rc != 0));
        }
        Some(RESUME) => match reexec::resume(arg(2).unwrap_or_default()) {
            Ok(state) => Init::resume(state).run(),
            Err(kept) => {
                eprintln!(
                    "init: re-executed with no state it can read; starting the session again \
                     without consent"
                );
                // So privd reads end of file once the session has gone, and a
                // requester hears at once.
                for fd in kept {
                    crate::syscall::memory::close(fd);
                }
                let mut init = Init::serving(None);
                if tests_enabled() {
                    init.tests = init_control::children(0)
                        .first()
                        .map(|&pid| Child::watch(pid));
                }
                init.session =
                    Session::Ending(Ending::new(sys_core::get_time_ms(), Then::Start, None));
                init.run()
            }
        },
        _ => boot(),
    }
}

fn boot() -> ! {
    upgrade_console_font();
    collect_crash_records();
    crate::root_layout::lay_out(std::path::Path::new("/"));
    if let Err(e) = crate::tree::hand_over(slopos_tree_core::APPS, (SESSION_UID, SESSION_GID)) {
        eprintln!("init: the apps' manifest: {e}");
    }
    install_live_apps();
    crate::root_layout::make_runtime_dir(&runtime_dir());

    let consent = net::socketpair(SOCK_CLOEXEC | SOCK_NONBLOCK).ok();
    let mut init = Init::serving(consent);
    // Must precede anything interactive; a missing or invalid /etc/keymap
    // leaves the built-in US default active.
    crate::keymap::apply_persisted_layout();

    if tests_enabled() {
        init.tests = start(&TESTS, &[]);
        if init.tests.is_none() && sys_core::run_userland_tests() == 0 {
            sys_core::exit_with_code(0);
        }
    } else {
        if !boot_flag(BOOT_FLAG_ROULETTE_SKIP)
            && let Some(roulette) = start(&ROULETTE, &[])
        {
            let _ = process::waitpid(roulette.pid);
        }
        init.session_due = Some(sys_core::get_time_ms());
    }
    init.run()
}

fn boot_flag(flag: u32) -> bool {
    let mut info = UserSysInfo::default();
    sys_core::sys_info(&mut info) == 0 && info.boot_flags & flag != 0
}

fn tests_enabled() -> bool {
    boot_flag(BOOT_FLAG_TESTS_ENABLED)
}

/// The two ends of the consent channel, kept for the boot: one for each
/// privd, the other for each compositor. `SCM_RIGHTS` carries no socket, so
/// a running side could not be handed a new pair.
struct Consent {
    privd: OwnedFd,
    agent: OwnedFd,
}

struct Init {
    consent: Option<Consent>,
    control: Option<Control>,
    privd: Supervised,
    remoted: Option<Supervised>,
    session: Session,
    session_budget: Budget,
    /// When the session is to start, while it is down.
    session_due: Option<u64>,
    /// Uid 0 ended the session; only uid 0 starts it again.
    session_held: bool,
    tests: Option<Child>,
}

#[derive(Clone, Copy)]
enum Side {
    Privd,
    Agent,
}

impl Init {
    /// Init with privd started and waited for, and remoted started where the
    /// base is paired; the session down.
    fn serving(consent: Option<(OwnedFd, OwnedFd)>) -> Init {
        let control = Control::bind()
            .map_err(|why| eprintln!("init: {SOCKET}: {why}"))
            .ok();
        let mut init = Init {
            consent: consent.map(|(privd, agent)| Consent { privd, agent }),
            control,
            privd: Supervised::new(&PRIVD),
            remoted: std::path::Path::new(REMOTE_CONF)
                .exists()
                .then(|| Supervised::new(&REMOTED)),
            session: Session::Down,
            session_budget: Budget::default(),
            session_due: None,
            session_held: false,
            tests: None,
        };
        // Waited for before anything that may ask it. An image that could not
        // resume adopts what its predecessor started.
        let now = sys_core::get_time_ms();
        if let Some(&pid) = init_control::children(init_control::PRIVD).first() {
            init.privd.child = Some(Child::watch(pid));
        } else {
            init.privd.budget.record(now);
            match start_ready(&PRIVD, &init.privd_fds()) {
                Some((child, ready)) => {
                    let _ = fs::read_slice(ready.raw(), &mut [0u8; 1]);
                    init.privd.child = Some(child);
                }
                None => init.privd.due = Some(init.privd.budget.next(now)),
            }
        }
        // Early, so a machine whose desktop never comes up can still be
        // reached.
        if let Some(remoted) = &mut init.remoted {
            match init_control::children(init_control::REMOTE).first() {
                Some(&pid) => remoted.child = Some(Child::watch(pid)),
                None => remoted.start(now, &[]),
            }
        }
        init
    }

    fn resume(state: State) -> Init {
        // SAFETY: the earlier image kept these descriptors for this one alone.
        let adopt = |fd: RawFd| unsafe { OwnedFd::from_raw(fd) };
        let control = match state.control {
            Some(fd) => Some(Control::adopt(adopt(fd))),
            None => Control::bind()
                .map_err(|why| eprintln!("init: {SOCKET}: {why}"))
                .ok(),
        };
        let now = sys_core::get_time_ms();
        let supervised = |service, carried: reexec::Service| {
            let budget = Budget::of(carried.starts);
            Supervised {
                service,
                child: carried.pid.map(Child::watch),
                due: carried.pid.is_none().then(|| budget.next(now)),
                budget,
            }
        };
        let init = Init {
            consent: state.consent.map(|(privd, agent)| Consent {
                privd: adopt(privd),
                agent: adopt(agent),
            }),
            control,
            privd: supervised(&PRIVD, state.privd),
            remoted: state.remoted.map(|remoted| supervised(&REMOTED, remoted)),
            session: match state.session {
                Some(pid) => Session::Running {
                    compositor: Child::watch(pid),
                },
                None => Session::Down,
            },
            session_budget: Budget::of(state.session_starts),
            session_due: None,
            session_held: state.session_held,
            tests: state.tests.map(Child::watch),
        };
        if let Some(reply) = state.reply {
            answer(adopt(reply), "ok");
        }
        println!("init: re-executed");
        init
    }

    fn privd_fds(&self) -> Vec<(RawFd, RawFd)> {
        self.consent
            .iter()
            .map(|consent| (consent.privd.raw(), CONSENT_FD))
            .collect()
    }

    fn run(mut self) -> ! {
        let mut fds = Vec::new();
        loop {
            let now = sys_core::get_time_ms();
            self.reap(now);
            self.step_session(now);
            self.drain_consent();
            self.start_due(now);
            let mut requests = self
                .control
                .as_mut()
                .map(|control| control.serve(now))
                .unwrap_or_default();
            // A re-exec closes every request after it unanswered.
            requests.sort_by_key(|request| request.command == Command::Reexec);
            for request in requests {
                self.request(request, now);
            }
            fds.clear();
            let timeout = self.wait_set(&mut fds, now);
            let _ = fs::poll(&mut fds, timeout);
        }
    }

    /// What init waits on, and for how long at most.
    fn wait_set(&self, fds: &mut Vec<UserPollFd>, now: u64) -> i64 {
        let poll = |fd: &OwnedFd| UserPollFd {
            fd: fd.raw(),
            events: POLLIN,
            revents: 0,
        };
        if let Some(control) = &self.control {
            control.poll_fds(fds);
        }
        let children = [
            self.privd.child.as_ref(),
            self.remoted.as_ref().and_then(|r| r.child.as_ref()),
            self.tests.as_ref(),
            match &self.session {
                Session::Starting { compositor, .. } | Session::Running { compositor } => {
                    Some(compositor)
                }
                _ => None,
            },
        ];
        fds.extend(
            children
                .into_iter()
                .flatten()
                .filter_map(|child| child.exited.as_ref())
                .map(poll),
        );
        if let Session::Starting { ready, .. } = &self.session {
            fds.push(poll(ready));
        }
        fds.extend(self.idle_consent_ends().map(poll));
        let tick: u64 = if matches!(self.session, Session::Ending(_)) {
            ENDING_TICK_MS
        } else {
            TICK_MS
        };
        [
            self.control.as_ref().and_then(Control::deadline),
            self.privd.due,
            self.remoted.as_ref().and_then(|r| r.due),
            self.session_due,
        ]
        .into_iter()
        .flatten()
        .map(|at| at.saturating_sub(now))
        .fold(tick, u64::min) as i64
    }

    fn reap(&mut self, now: u64) {
        self.reap_exited(now);
        // A pidfd turns readable when the leader exits, `wait` only once the
        // last thread has: one left readable is reaped on a later tick.
        for child in self.children_mut() {
            if child.exited.as_ref().is_some_and(readable) {
                child.exited = None;
            }
        }
        self.reap_exited(now);
    }

    fn reap_exited(&mut self, now: u64) {
        while let Some((pid, status)) = process::wait_any_nohang() {
            self.exited(pid, process::wait_status(status), now);
        }
    }

    fn children_mut(&mut self) -> impl Iterator<Item = &mut Child> {
        let compositor = match &mut self.session {
            Session::Starting { compositor, .. } | Session::Running { compositor } => {
                Some(compositor)
            }
            _ => None,
        };
        [
            self.privd.child.as_mut(),
            self.remoted.as_mut().and_then(|r| r.child.as_mut()),
            self.tests.as_mut(),
            compositor,
        ]
        .into_iter()
        .flatten()
    }

    fn exited(&mut self, pid: u32, status: WaitStatus, now: u64) {
        if self.privd.is(pid) {
            self.reset_consent(Side::Agent);
            self.privd.exited(status, now);
        } else if let Some(remoted) = self.remoted.as_mut().filter(|r| r.is(pid)) {
            remoted.exited(status, now);
        } else if self.tests.as_ref().is_some_and(|tests| tests.pid == pid) {
            self.tests = None;
            // Exiting on success stops the boot waiting for input; a failure
            // brings the desktop up, whoever ended it, so a shell is reachable.
            if status == WaitStatus::Exited(0) {
                sys_core::exit_with_code(0);
            }
            self.session_held = false;
            match &mut self.session {
                Session::Down => self.session_due = Some(now),
                Session::Ending(ending) => ending.then = Then::Start,
                _ => {}
            }
        } else if self.is_compositor(pid) {
            eprintln!("init: the compositor exited ({status:?})");
            self.reset_consent(Side::Privd);
            self.session = Session::Ending(Ending::new(now, Then::Start, None));
        } else if let Session::Ending(ending) = &mut self.session
            && ending.compositor == Some(pid)
        {
            ending.compositor = None;
            self.reset_consent(Side::Privd);
        }
    }

    fn is_compositor(&self, pid: u32) -> bool {
        matches!(
            &self.session,
            Session::Starting { compositor, .. } | Session::Running { compositor }
                if compositor.pid == pid
        )
    }

    /// Start what is due: a service that exited, once its budget allows, and
    /// the session.
    fn start_due(&mut self, now: u64) {
        if self.privd.due_by(now) {
            let fds = self.privd_fds();
            self.privd.start(now, &fds);
        }
        if let Some(remoted) = self.remoted.as_mut().filter(|r| r.due_by(now)) {
            remoted.start(now, &[]);
        }
        if matches!(self.session, Session::Down) && self.session_due.is_some_and(|at| at <= now) {
            self.session_budget.record(now);
            self.start_session(now);
            self.session_due =
                matches!(self.session, Session::Down).then(|| self.session_budget.next(now));
        }
    }

    fn start_session(&mut self, now: u64) {
        let fds: Vec<(RawFd, RawFd)> = self
            .consent
            .iter()
            .map(|consent| (consent.agent.raw(), CONSENT_FD))
            .collect();
        self.session = match start_ready(&COMPOSITOR, &fds) {
            Some((compositor, ready)) => Session::Starting {
                compositor,
                ready,
                since: now,
            },
            None => Session::Down,
        };
    }

    /// Have the session end, and start again when `then` says and uid 0 has
    /// not ended it.
    fn want_session(&mut self, then: Then, now: u64) {
        let then = if self.session_held { Then::Stay } else { then };
        match &mut self.session {
            Session::Ending(ending) => ending.then = then,
            Session::Down if then == Then::Start => {
                self.session_due = Some(self.session_budget.next(now));
            }
            Session::Down => self.session_due = None,
            _ => self.session = Session::Ending(self.ending(now, then)),
        }
    }

    fn step_session(&mut self, now: u64) {
        match &mut self.session {
            Session::Starting { ready, since, .. } => {
                let up = match readiness(ready) {
                    Readiness::Pending if now.saturating_sub(*since) < STARTING_MS => return,
                    Readiness::Pending => {
                        eprintln!("init: the compositor was not ready in time");
                        true
                    }
                    Readiness::Ready => true,
                    Readiness::Gone => false,
                };
                let Session::Starting { compositor, .. } =
                    std::mem::replace(&mut self.session, Session::Down)
                else {
                    unreachable!()
                };
                if up {
                    let _ = start(&TERMINAL, &[]);
                }
                self.session = Session::Running { compositor };
            }
            Session::Ending(ending) => {
                let Some(ended) = ending.step(now) else {
                    return;
                };
                let Session::Ending(ending) = std::mem::replace(&mut self.session, Session::Down)
                else {
                    unreachable!()
                };
                // A session ends once nothing of it runs, which can be before
                // the compositor's exit is reaped.
                if ending.compositor.is_some() {
                    self.reset_consent(Side::Privd);
                }
                let reply = match ended {
                    Ended::All => "ok".to_owned(),
                    Ended::GaveUp(left) => {
                        format!("refused: {left} processes of the session would not end")
                    }
                };
                for waiting in ending.waiting {
                    answer(waiting, &reply);
                }
                self.want_session(ending.then, now);
            }
            _ => {}
        }
    }

    fn request(&mut self, request: Request, now: u64) {
        let Request {
            command,
            uid,
            reply,
        } = request;
        let then = match command {
            Command::Reexec => return self.reexec(reply),
            Command::RestartSession if self.session_held && uid != 0 => {
                return answer(reply, "refused: uid 0 ended the session");
            }
            Command::RestartSession => Then::Start,
            Command::EndSession => Then::Stay,
        };
        if let Session::Ending(ending) = &self.session
            && ending.waiting.len() >= MAX_WAITING
            && uid != 0
        {
            return answer(reply, "refused: too many requests wait on the session");
        }
        self.session_held = then == Then::Stay;
        if then == Then::Start {
            self.session_budget.clear();
        }
        // A session that is down may still have processes carrying its tag,
        // left by one given up on.
        let mut ending = self.ending(now, then);
        ending.then = then;
        ending.waiting.push(reply);
        self.session_due = None;
        self.session = Session::Ending(ending);
    }

    /// The session's end, begun now unless it already was.
    fn ending(&mut self, now: u64, then: Then) -> Ending {
        match std::mem::replace(&mut self.session, Session::Down) {
            Session::Ending(ending) => ending,
            Session::Starting { compositor, .. } | Session::Running { compositor } => {
                Ending::new(now, then, Some(compositor.pid))
            }
            Session::Down => Ending::new(now, then, None),
        }
    }

    /// The new image answers `reply`; this one only when the exec failed.
    fn reexec(&mut self, reply: OwnedFd) {
        let session = match &self.session {
            Session::Down if self.session_due.is_none() => None,
            Session::Running { compositor } => Some(compositor.pid),
            _ => return answer(reply, "refused: the session is starting or ending"),
        };
        let carried = |supervised: &Supervised| reexec::Service {
            pid: supervised.child.as_ref().map(|child| child.pid),
            starts: supervised.budget.starts().collect(),
        };
        let state = State {
            consent: self
                .consent
                .as_ref()
                .map(|consent| (consent.privd.raw(), consent.agent.raw())),
            control: self.control.as_ref().map(Control::listener),
            reply: Some(reply.raw()),
            privd: carried(&self.privd),
            remoted: self.remoted.as_ref().map(carried),
            session,
            session_held: self.session_held,
            session_starts: self.session_budget.starts().collect(),
            tests: self.tests.as_ref().map(|tests| tests.pid),
        };
        let why = reexec::reexec(&state);
        eprintln!("init: cannot re-execute: {why}");
        answer(reply, &format!("refused: {why}"));
    }

    fn runs(&self, side: Side) -> bool {
        match side {
            Side::Privd => self.privd.child.is_some(),
            Side::Agent => matches!(
                self.session,
                Session::Starting { .. } | Session::Running { .. }
            ),
        }
    }

    /// Tell `side`, while it runs, that the other exited, so it drops what it
    /// was waiting on at once. A frame written on one end reaches whoever
    /// holds the other.
    fn reset_consent(&self, side: Side) {
        let Some(consent) = self.consent.as_ref().filter(|_| self.runs(side)) else {
            return;
        };
        let end = match side {
            Side::Privd => &consent.agent,
            Side::Agent => &consent.privd,
        };
        let frame = slopos_priv_core::consent::reset_frame(crate::privd::request_id());
        let _ = net::send(end.raw(), &frame, MSG_NOSIGNAL);
    }

    /// The ends of sides not running, where nothing reads.
    fn idle_consent_ends(&self) -> impl Iterator<Item = &OwnedFd> {
        self.consent.iter().flat_map(|consent| {
            [
                (!self.runs(Side::Privd)).then_some(&consent.privd),
                (!self.runs(Side::Agent)).then_some(&consent.agent),
            ]
            .into_iter()
            .flatten()
        })
    }

    /// Discard what is written to a side that is not running, so it never
    /// piles up and its next instance reads only what follows its start; a
    /// peer whose hello is discarded says it again (`consent::Link`).
    fn drain_consent(&self) {
        let mut buf = [0u8; 512];
        for end in self.idle_consent_ends() {
            while matches!(fs::read_slice(end.raw(), &mut buf), Ok(n) if n > 0) {}
        }
    }
}

fn readable(fd: &OwnedFd) -> bool {
    let mut fds = [UserPollFd {
        fd: fd.raw(),
        events: POLLIN,
        revents: 0,
    }];
    matches!(fs::poll(&mut fds, 0), Ok(n) if n > 0)
}

enum Readiness {
    Pending,
    Ready,
    /// The compositor closed its notifier without a word: it has exited.
    Gone,
}

fn readiness(ready: &OwnedFd) -> Readiness {
    if !readable(ready) {
        return Readiness::Pending;
    }
    match fs::read_slice(ready.raw(), &mut [0u8; 1]) {
        Ok(1) => Readiness::Ready,
        _ => Readiness::Gone,
    }
}
