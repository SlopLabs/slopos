//! `/sbin/init`: lays out the root, starts the services and starts them again
//! when they crash, extensions' among them, keeps the sessions people log in
//! to — which desktop is in front, which are behind, the login screen and the
//! shells console logins start — and re-executes itself keeping all of that.
//! When a new base comes in live it starts again each service the switch
//! changed, itself included.

mod control;
mod extensions;
mod reexec;
mod service;
mod session;

use slopos_abi::caps::{EXTENSION_UPDATE, POWER, RAW_BLOCK, SEAL};
use slopos_abi::fs::{GREETER_UID, SESSION_UID};
use slopos_abi::net::{SOCK_CLOEXEC, SOCK_NONBLOCK};
use slopos_abi::syscall::{
    BOOT_FLAG_EXTENSIONS_OFF, BOOT_FLAG_ROULETTE_SKIP, BOOT_FLAG_TESTS_ENABLED, MSG_NOSIGNAL,
    POLLIN, UserPollFd,
};
use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};
use slopos_font::atlas::GlyphAtlas;
use slopos_priv_core::sessions::{Class, LINKS};

use self::control::{Control, Request, answer};
use self::extensions::Units;
use self::reexec::{RESUME, State};
use self::service::{
    Budget, Child, Identity, LOGIN, PRIVD, REMOTED, ROULETTE, RUN_TESTS, Supervised, TESTS,
    runtime_dir, start, start_ready,
};
use self::session::{Sessions, Then, Want};
use crate::apps::privd::{LINKS_FD, SESSIONS_FD};
use crate::base_watch::BaseWatch;
use crate::init_control::{self, At, Command, SOCKET};
use crate::root_layout::LOCAL;
use crate::syscall::process::{self, Endowment, WaitStatus};
use crate::syscall::{OwnedFd, RawFd, core as sys_core, fs, net, tty};

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

fn run_holding(program: &[u8], argv: &[&core::ffi::CStr], caps: u64) {
    let argv: Vec<*const u8> = argv.iter().map(|arg| arg.as_ptr().cast()).collect();
    let actions = [process::clone_fd(1, 1), process::clone_fd(2, 2)];
    let tid = process::spawn_endowed(
        program,
        &argv,
        &[],
        &[],
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &actions,
        0,
        Endowment {
            caps,
            ..Endowment::NONE
        },
    );
    if tid > 0 {
        let _ = process::waitpid(tid as u32);
    }
}

/// What a crashed boot left goes to `/var/log/crash` before the tests and the
/// services run.
fn collect_crash_records() {
    run_holding(
        b"/bin/bootctl",
        &[c"bootctl", c"collect"],
        RAW_BLOCK | POWER,
    );
}

/// The user database written from the accounts record, and a root without
/// one given the one person's, before anything reads either.
fn sync_accounts() {
    run_holding(b"/bin/users", &[c"users", c"sync"], SEAL);
}

/// The store's enabled extensions shown at `/opt`, each held again to its
/// record, its key and the base's ceiling, and the user database written
/// from the records, before privd and the services start.
fn activate_extensions() {
    use crate::apps::extctl::store::Store;
    let any = Store::open(false)
        .and_then(|store| store.names())
        .is_ok_and(|names| !names.is_empty());
    if !any || sys_core::boot_flag(BOOT_FLAG_EXTENSIONS_OFF) {
        return;
    }
    run_holding(
        b"/bin/extctl",
        &[c"extctl", c"activate-all"],
        EXTENSION_UPDATE | SEAL,
    );
}

/// Requests answered once a session has ended, each holding a descriptor
/// of init's, beside uid 0's.
const MAX_WAITING: usize = 8;
/// How long init sleeps with nothing due, and while a session ends.
const TICK_MS: u64 = 1000;
const ENDING_TICK_MS: u64 = 50;

/// In the sealed base of a machine the host built paired; the daemon runs
/// only there.
const REMOTE_CONF: &str = "/usr/share/slopos/remote/remote.conf";

/// On a live boot, the medium's apps at the RAM root's `/usr/local`, where an
/// install puts them on a disk, `holder`'s.
fn install_live_apps(holder: (u32, u32)) {
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
        guest: LOCAL,
        manifests: slopos_tree_core::MANIFEST_DIR,
        source: &apps,
        owner: Some(holder),
        keeper: Some(holder),
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
        Some(extensions::LOG) => extensions::keep_log(arg(2).unwrap_or_default()),
        Some(RESUME) => match reexec::resume(arg(2).unwrap_or_default()) {
            Ok(state) => Init::resume(state, reexec::resume_units(arg(3))).run(),
            Err(kept) => {
                eprintln!(
                    "init: re-executed with no state it can read; starting the sessions again \
                     without consent"
                );
                // So privd reads end of file once the sessions have gone, and
                // a requester hears at once.
                for fd in kept {
                    crate::syscall::memory::close(fd);
                }
                let mut init = Init::serving(None, reexec::resume_units(arg(3)), !tests_enabled());
                if tests_enabled() {
                    init.tests = init_control::children(0)
                        .first()
                        .map(|&pid| Child::watch(pid));
                }
                init.run()
            }
        },
        _ => boot(),
    }
}

fn boot() -> ! {
    upgrade_console_font();
    collect_crash_records();
    let root = std::path::Path::new("/");
    let holder = crate::root_layout::lay_out(root).owner(LOCAL);
    if holder == (0, 0)
        && let Err(e) = crate::root_layout::settle()
    {
        eprintln!("init: {LOCAL}: {e}");
    }
    if let Err(e) = crate::tree::hand_over(slopos_tree_core::APPS, holder) {
        eprintln!("init: the apps' manifest: {e}");
    }
    install_live_apps(holder);
    sync_accounts();
    activate_extensions();

    let consent = Consent::make();
    let tests = tests_enabled();
    let mut init = Init::serving(consent, extensions::Carried::default(), !tests);
    // Must precede anything interactive; a missing or invalid /etc/keymap
    // leaves the built-in US default active.
    crate::keymap::apply_persisted_layout();

    if tests {
        init.tests = start(&TESTS, None, &[]);
        if init.tests.is_none() && sys_core::run_userland_tests() == 0 {
            sys_core::exit_with_code(0);
        }
    } else {
        if !sys_core::boot_flag(BOOT_FLAG_ROULETTE_SKIP)
            && let Some(roulette) = start(&ROULETTE, Some(&Identity::first_person()), &[])
        {
            let _ = process::waitpid(roulette.pid);
        }
        init.login = Some(Supervised::new(&LOGIN, Some(Identity::greeter(None))));
        if let Some(login) = &mut init.login {
            login.due = Some(sys_core::get_time_ms());
        }
    }
    init.run()
}

fn watch_base() -> Option<BaseWatch> {
    let watch = BaseWatch::open();
    if watch.is_none() {
        eprintln!("init: no base monitor: a base switched to live restarts nothing");
    }
    watch
}

fn tests_enabled() -> bool {
    sys_core::boot_flag(BOOT_FLAG_TESTS_ENABLED)
}

/// The consent links and the pair privd hears the sessions on, kept for the
/// boot: `SCM_RIGHTS` carries no socket, so a running side could not be
/// handed a new pair. Each link joins privd to one desktop or login screen;
/// either end of a pair names its maker, so neither side takes one from
/// anyone but init.
struct Consent {
    /// privd's end, then the compositor's.
    links: Vec<(OwnedFd, OwnedFd)>,
    /// Init's end, then privd's.
    sessions: Option<(OwnedFd, OwnedFd)>,
}

impl Consent {
    fn make() -> Option<Consent> {
        let pair = || net::socketpair(SOCK_CLOEXEC | SOCK_NONBLOCK).ok();
        let links = (0..LINKS).map(|_| pair()).collect::<Option<Vec<_>>>()?;
        Some(Consent {
            links,
            sessions: pair(),
        })
    }

    fn agents(&self) -> Vec<RawFd> {
        self.links.iter().map(|(_, agent)| agent.raw()).collect()
    }

    /// A pair of its own for each privd started, so one still draining reads
    /// end of file and takes no table the new one is owed.
    fn renew_sessions(&mut self) {
        self.sessions = net::socketpair(SOCK_CLOEXEC | SOCK_NONBLOCK).ok();
    }
}

struct Init {
    consent: Option<Consent>,
    control: Option<Control>,
    base: Option<BaseWatch>,
    /// When init next tries to re-execute for a switch that changed
    /// `/sbin/init`.
    reexec_due: Option<u64>,
    privd: Supervised,
    /// The extensions' actions privd read last, as `activation::Actions::from`
    /// names them, unknown while a privd started since reads its own.
    privd_actions: Option<String>,
    /// Readable once a privd started since serving began is ready, before
    /// which a `SIGHUP` would end it rather than have it read its actions.
    privd_ready: Option<OwnedFd>,
    /// What a service left draining past [`service::DRAIN_MS`]: no longer
    /// supervised, only reaped.
    retired: Vec<Child>,
    /// A switch changed the pairing: what the old broker ran has until then
    /// to end before `SIGKILL`, and remoted starts after.
    remote_ending: Option<u64>,
    remoted: Option<Supervised>,
    /// The console's login prompt, on every boot but a test boot.
    login: Option<Supervised>,
    sessions: Sessions,
    /// The people whose runtime directories init made.
    runtime: Vec<u32>,
    /// The sessions ended for an image that keeps none, to be wanted again
    /// should the re-execution fail.
    wanted_after_reexec: bool,
    tests: Option<Child>,
    units: Units,
}

#[derive(Clone, Copy)]
enum Side {
    Privd,
    Agent(u8),
}

impl Init {
    /// Init with privd started and waited for, and remoted started where the
    /// base is paired; no session started yet, the screen to show one when
    /// `wanted`.
    fn serving(consent: Option<Consent>, units: extensions::Carried, wanted: bool) -> Init {
        let control = Control::bind()
            .map_err(|why| eprintln!("init: {SOCKET}: {why}"))
            .ok();
        crate::sessions::lay_out(&[]);
        let mut init = Init {
            consent,
            control,
            base: watch_base(),
            reexec_due: None,
            privd: Supervised::new(&PRIVD, None),
            privd_actions: extension_actions().ok(),
            privd_ready: None,
            retired: Vec::new(),
            remote_ending: None,
            remoted: std::path::Path::new(REMOTE_CONF)
                .exists()
                .then(|| Supervised::new(&REMOTED, remote_person())),
            login: None,
            sessions: Sessions::new(wanted, 0),
            runtime: Vec::new(),
            wanted_after_reexec: false,
            tests: None,
            units: Units::load(sys_core::get_time_ms(), false, units),
        };
        // Waited for before anything that may ask it. An image that could not
        // resume adopts what its predecessor started.
        let now = sys_core::get_time_ms();
        if let Some(&pid) = init_control::children(init_control::PRIVD).first() {
            init.privd.child = Some(Child::watch(pid));
        } else {
            init.privd.budget.record(now);
            init.publish_to_privd();
            let fds = init.privd_fds();
            match start_ready(init.privd.service, None, &fds) {
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

    fn resume(state: State, units: extensions::Carried) -> Init {
        // SAFETY: the earlier image kept these descriptors for this one alone.
        let adopt = |fd: RawFd| unsafe { OwnedFd::from_raw(fd) };
        let control = match state.control {
            Some(fd) => Some(Control::adopt(adopt(fd))),
            None => Control::bind()
                .map_err(|why| eprintln!("init: {SOCKET}: {why}"))
                .ok(),
        };
        let now = sys_core::get_time_ms();
        let supervised = |service, who, carried: reexec::Service| {
            let budget = Budget::of(carried.starts);
            Supervised {
                service,
                who,
                child: carried.pid.map(Child::watch),
                due: carried.pid.is_none().then(|| budget.next(now)),
                draining: None,
                budget,
            }
        };
        let first = state.legacy.is_some() || state.links.len() < LINKS;
        let mut links: Vec<(OwnedFd, OwnedFd)> = state
            .links
            .iter()
            .map(|&(privd, agent)| (adopt(privd), adopt(agent)))
            .collect();
        while links.len() < LINKS {
            match net::socketpair(SOCK_CLOEXEC | SOCK_NONBLOCK) {
                Ok(pair) => links.push(pair),
                Err(_) => break,
            }
        }
        let sessions_link = state
            .sessions_link
            .map(|(init, privd)| (adopt(init), adopt(privd)))
            .or_else(|| net::socketpair(SOCK_CLOEXEC | SOCK_NONBLOCK).ok());
        let mut sessions = Sessions::new(state.wanted, state.next_tag);
        sessions.held = state.held;
        sessions.fresh = state.fresh;
        sessions.default_budget = Budget::of(state.default_starts);
        let kept: Vec<u32> = state
            .sessions
            .iter()
            .map(|carried| carried.tag)
            .chain(state.remoted.is_some().then_some(init_control::REMOTE))
            .collect();
        crate::sessions::lay_out(&kept);
        for carried in state.sessions {
            let Some(who) = identity_of(carried.uid, carried.class) else {
                eprintln!(
                    "init: session {} names nobody the record knows",
                    carried.tag
                );
                if let Some(pipe) = carried.pipe {
                    drop(adopt(pipe));
                }
                continue;
            };
            sessions.adopt(
                carried.tag,
                who,
                carried.class,
                carried.leader,
                carried.link,
                carried.locked,
                carried.pipe.map(adopt),
                carried.tty,
                carried.starts,
                now,
            );
        }
        sessions.front = state.front.filter(|tag| sessions.find(*tag).is_some());
        if let Some(legacy) = state.legacy {
            sessions.wanted = true;
            if let Some(pid) = legacy.pid {
                let who = Identity::first_person();
                sessions.adopt(
                    init_control::DESKTOP,
                    who,
                    Class::User,
                    Some(pid),
                    Some(0),
                    false,
                    None,
                    None,
                    legacy.starts,
                    now,
                );
                sessions.front = Some(init_control::DESKTOP);
                sessions.fresh = false;
            }
        }
        let session_runs = sessions.first_person_runs();
        let mut init = Init {
            consent: Some(Consent {
                links,
                sessions: sessions_link,
            }),
            control,
            base: watch_base(),
            reexec_due: None,
            privd: supervised(&PRIVD, None, state.privd),
            privd_actions: extension_actions().ok(),
            privd_ready: None,
            retired: Vec::new(),
            remote_ending: None,
            remoted: state
                .remoted
                .map(|remoted| supervised(&REMOTED, remote_person(), remoted)),
            login: state
                .login
                .map(|login| supervised(&LOGIN, Some(Identity::greeter(None)), login)),
            runtime: sessions.people(),
            wanted_after_reexec: false,
            sessions,
            tests: state.tests.map(Child::watch),
            units: Units::load(now, session_runs, units),
        };
        if let Some(reply) = state.reply {
            answer(adopt(reply), "ok");
        }
        if first {
            println!("init: privd starts again to hear the sessions");
            init.privd.restart(now);
        }
        println!("init: re-executed");
        init
    }

    /// What privd is started with: a consent link per desktop, the first
    /// where a privd from before the session table finds its one, and the
    /// pair it hears the table on.
    fn privd_fds(&self) -> Vec<(RawFd, RawFd)> {
        let Some(consent) = &self.consent else {
            return Vec::new();
        };
        let links = consent
            .links
            .iter()
            .enumerate()
            .map(|(i, (privd, _))| (privd.raw(), LINKS_FD + i as RawFd));
        let sessions = consent
            .sessions
            .iter()
            .map(|(_, privd)| (privd.raw(), SESSIONS_FD));
        links.chain(sessions).collect()
    }

    fn run(mut self) -> ! {
        let mut fds = Vec::new();
        loop {
            let now = sys_core::get_time_ms();
            self.reap(now);
            self.step_base(now);
            self.step_remote_ending(now);
            self.step_sessions(now);
            let session = self.sessions.first_person_runs();
            self.units.step(now, session);
            self.step_privd_ready(now);
            self.follow_units(now);
            self.drain_consent();
            self.start_due(now);
            let sessions = &self.sessions;
            let desktop_leader = |pid| {
                sessions
                    .list
                    .iter()
                    .any(|s| s.class == Class::User && s.leader().is_some_and(|c| c.pid == pid))
            };
            let mut requests = self
                .control
                .as_mut()
                .map(|control| control.serve(now, desktop_leader))
                .unwrap_or_default();
            // A re-exec closes every request after it unanswered.
            requests.sort_by_key(|request| request.command == Command::Reexec);
            for request in requests {
                self.request(request, now);
            }
            self.sessions.step_front(now);
            self.publish();
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
        if let Some(base) = &self.base {
            fds.push(UserPollFd {
                fd: base.fd(),
                events: POLLIN,
                revents: 0,
            });
        }
        let children = [
            self.privd.child.as_ref(),
            self.remoted.as_ref().and_then(|r| r.child.as_ref()),
            self.login.as_ref().and_then(|l| l.child.as_ref()),
            self.tests.as_ref(),
        ];
        fds.extend(
            children
                .into_iter()
                .flatten()
                .chain(&self.retired)
                .filter_map(|child| child.exited.as_ref())
                .map(poll),
        );
        fds.extend(self.sessions.poll_fds().map(poll));
        fds.extend(self.privd_ready.iter().map(poll));
        fds.extend(self.idle_consent_ends().map(poll));
        self.units.poll_fds(fds);
        let tick: u64 = if self.sessions.ending() {
            ENDING_TICK_MS
        } else {
            TICK_MS
        };
        [
            self.control.as_ref().and_then(Control::deadline),
            self.privd.due,
            self.privd.draining,
            self.remoted.as_ref().and_then(|r| r.due),
            self.remoted.as_ref().and_then(|r| r.draining),
            self.login.as_ref().and_then(|l| l.due),
            self.sessions.deadline(),
            self.remote_ending,
            self.reexec_due.filter(|_| self.may_reexec()),
            self.units.deadline(now),
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
        [
            self.privd.child.as_mut(),
            self.remoted.as_mut().and_then(|r| r.child.as_mut()),
            self.login.as_mut().and_then(|l| l.child.as_mut()),
            self.tests.as_mut(),
        ]
        .into_iter()
        .flatten()
        .chain(self.retired.iter_mut())
        .chain(self.sessions.children_mut())
        .chain(self.units.children_mut())
    }

    fn exited(&mut self, pid: u32, status: WaitStatus, now: u64) {
        if let Some(at) = self.retired.iter().position(|child| child.pid == pid) {
            self.retired.swap_remove(at);
        } else if self.privd.is(pid) {
            self.privd.child = None;
            self.reset_consent(Side::Privd);
            self.privd.exited(status, now);
        } else if let Some(remoted) = self.remoted.as_mut().filter(|r| r.is(pid)) {
            remoted.exited(status, now);
        } else if let Some(login) = self.login.as_mut().filter(|l| l.is(pid)) {
            login.exited(status, now);
        } else if self.tests.as_ref().is_some_and(|tests| tests.pid == pid) {
            self.tests = None;
            // Exiting on success stops the boot waiting for input; a failure
            // brings the screen up, whoever ended the sessions, so a shell is
            // reachable.
            if status == WaitStatus::Exited(0) {
                sys_core::exit_with_code(0);
            }
            self.sessions.held = false;
            self.sessions.wanted = true;
        } else {
            let link = self.leader_link(pid);
            if !self.sessions.exited(pid, now) {
                let session = self.sessions.first_person_runs();
                self.units.exited(pid, status, now, session);
            } else if let Some(link) = link {
                self.reset_consent(Side::Agent(link));
            }
        }
    }

    /// The consent link of the session whose leader `pid` is, when it has
    /// one.
    fn leader_link(&self, pid: u32) -> Option<u8> {
        self.sessions
            .list
            .iter()
            .find(|s| s.led_by(pid))
            .and_then(|s| s.link)
    }

    /// Start again what a new base changed: privd, which drains first, and
    /// remoted, or start or stop remoted as the base is paired or not, and
    /// the extensions' services a new ceiling changes; and re-execute init
    /// once it may. The sessions stay, and each compositor offers its
    /// person the restart.
    fn step_base(&mut self, now: u64) {
        if let Some(switch) = self
            .base
            .as_mut()
            .filter(|base| base.switched())
            .and_then(BaseWatch::take)
        {
            if self.privd.changed_by(&switch) {
                println!("init: the new base changes privd; it drains and starts again");
                self.privd.restart(now);
            }
            if switch.changed(&[service::PAIRING]) {
                self.end_remote(now);
            } else if let Some(remoted) = self.remoted.as_mut().filter(|r| r.changed_by(&switch)) {
                remoted.restart(now);
            }
            if switch.changed(&[slopos_priv_core::INIT]) {
                self.reexec_due = Some(now);
            }
            if switch.changed(&[slopos_extension_core::CEILING]) {
                let session = self.sessions.first_person_runs();
                self.units.reload(None, now, session);
            }
        }
        if self.reexec_due.is_some_and(|at| at <= now) && self.may_reexec() {
            println!("init: the new base changes init; it re-executes");
            if !self.reexec(None, now) {
                self.reexec_due = Some(now + service::WINDOW_MS);
            }
        }
    }

    fn step_privd_ready(&mut self, now: u64) {
        if self.privd_ready.as_ref().is_some_and(readable) {
            self.privd_ready = None;
            self.follow_actions(now);
        }
    }

    /// [`follow_actions`](Self::follow_actions) once the extensions' services
    /// followed the store.
    fn follow_units(&mut self, now: u64) {
        if self.units.take_reread() {
            self.follow_actions(now);
        }
    }

    /// End the mechanisms of each extension read whose actions no longer run,
    /// and have privd read the actions again when those it read are not those
    /// that run. One draining, not running or not yet ready reads them as it
    /// starts or once it is.
    fn follow_actions(&mut self, now: u64) {
        let actions = match crate::activation::actions() {
            Ok(actions) => actions,
            Err(why) => {
                eprintln!("init: {why}");
                return self.units.read_again(now);
            }
        };
        if actions.unread {
            self.units.read_again(now);
        }
        let running: Vec<u32> = actions.loaded.iter().map(|loaded| loaded.login).collect();
        self.units.spare(&running);
        let stopped: Vec<u32> = init_control::Tasks::read()
            .action_tags()
            .into_iter()
            .filter(|tag| actions.stopped.contains(tag))
            .collect();
        self.units.end_strays(now, stopped);
        let ready = self.privd.draining.is_none() && self.privd_ready.is_none();
        let Some(child) = self.privd.child.as_ref().filter(|_| ready) else {
            return;
        };
        if self.privd_actions.as_ref() != Some(&actions.from) {
            println!("init: privd reads the extensions' actions again");
            let _ = process::kill(child.pid, slopos_abi::signal::SIGHUP);
            self.privd_actions = Some(actions.from);
        }
    }

    /// End remoted and what it ran for a broker the base no longer pairs
    /// with, as a session ends: remoted for a new pairing starts after.
    fn end_remote(&mut self, now: u64) {
        let paired = std::path::Path::new(REMOTE_CONF).exists();
        let mut remoted = self.remoted.take();
        if let Some(remoted) = remoted.as_mut() {
            self.retired.extend(remoted.child.take());
            remoted.due = None;
            remoted.draining = None;
        }
        self.remoted =
            paired.then(|| remoted.unwrap_or_else(|| Supervised::new(&REMOTED, remote_person())));
        for pid in init_control::processes(init_control::REMOTE) {
            let _ = process::kill(pid, slopos_abi::signal::SIGTERM);
            let _ = process::kill(pid, slopos_abi::signal::SIGCONT);
        }
        self.remote_ending = Some(now + service::DRAIN_MS);
    }

    fn step_remote_ending(&mut self, now: u64) {
        let Some(deadline) = self.remote_ending else {
            return;
        };
        let left = init_control::processes(init_control::REMOTE);
        if !left.is_empty() && deadline > now {
            return;
        }
        for pid in left {
            let _ = process::kill(pid, slopos_abi::signal::SIGKILL);
        }
        self.remote_ending = None;
        if let Some(remoted) = self.remoted.as_mut().filter(|r| r.child.is_none()) {
            remoted.due = Some(now);
        }
    }

    /// Start what is due: a service that exited, once its budget allows, and
    /// the sessions.
    fn start_due(&mut self, now: u64) {
        let retired = [
            Some(&mut self.privd),
            self.remoted.as_mut(),
            self.login.as_mut(),
        ]
        .into_iter()
        .flatten()
        .filter_map(|supervised| supervised.retire(now));
        self.retired.extend(retired);
        if self.privd.due_by(now) {
            if let Some(consent) = self.consent.as_mut() {
                consent.renew_sessions();
            }
            self.publish_to_privd();
            let fds = self.privd_fds();
            self.privd_actions = None;
            self.privd_ready = self.privd.start_ready(now, &fds);
        }
        if let Some(remoted) = self.remoted.as_mut().filter(|r| r.due_by(now)) {
            remoted.who = remote_person();
            remoted.start(now, &[]);
        }
        if let Some(login) = self.login.as_mut().filter(|l| l.due_by(now)) {
            login.start(now, &[]);
        }
        let agents = self
            .consent
            .as_ref()
            .map(Consent::agents)
            .unwrap_or_default();
        self.sessions.start_due(now, &agents);
    }

    fn step_sessions(&mut self, now: u64) {
        self.sessions.step_starting(now);
        self.sessions.step_front(now);
        let extra = self.units.session_tags();
        for done in self.sessions.step_ending(now, &extra) {
            answer(done.reply, &done.said);
        }
        self.follow_runtime_dirs();
    }

    /// Each person's runtime directory lives while a session of theirs does,
    /// remoted's among them; the first person's, whom the boot's own programs
    /// run as, lives for the boot, as a lingering user's does under logind.
    fn follow_runtime_dirs(&mut self) {
        let mut people = self.sessions.people();
        people.extend(self.background().map(|entry| entry.uid));
        people.push(SESSION_UID);
        people.sort_unstable();
        people.dedup();
        for uid in people.iter().filter(|uid| !self.runtime.contains(uid)) {
            crate::root_layout::make_runtime_dir(&runtime_dir(*uid), *uid);
        }
        for uid in self.runtime.iter().filter(|uid| !people.contains(uid)) {
            let _ = std::fs::remove_dir_all(runtime_dir(*uid));
        }
        self.runtime = people;
    }

    fn request(&mut self, request: Request, now: u64) {
        let Request {
            command,
            uid,
            pid,
            login,
            handed,
            reply,
        } = request;
        let own = self
            .sessions
            .find(login)
            .filter(|s| uid == 0 || s.who.uid == uid)
            .map(|s| (s.tag, s.class));
        let refuse = |reply, why: &str| answer(reply, &format!("refused: {why}"));
        match command {
            Command::Reexec => {
                self.reexec(Some(reply), now);
            }
            Command::Extensions => {
                let session = self.sessions.first_person_runs();
                self.units.reload(Some(reply), now, session);
                self.follow_units(now);
            }
            Command::RestartSession | Command::EndSession if uid == 0 && own.is_none() => {
                let restart = command == Command::RestartSession;
                self.sessions.held = !restart;
                self.sessions.wanted |= restart;
                self.sessions.fresh = restart;
                self.sessions.want = None;
                if restart {
                    self.sessions.default_budget.clear();
                }
                self.end_every(reply, now);
            }
            Command::RestartSession | Command::EndSession => {
                let Some((tag, class)) = own else {
                    return refuse(reply, "the requester is in no session of theirs");
                };
                if self.waiting_full(uid) {
                    return refuse(reply, "too many requests wait on the sessions");
                }
                let then = match command {
                    Command::RestartSession => Then::Start,
                    _ => Then::Stay,
                };
                // Whether the desktop is locked is its compositor's word, which
                // may not have reached init; anything else restarting it gets
                // it back locked.
                if then == Then::Start && class == Class::User && !self.leads(tag, pid) {
                    self.sessions.set_locked(tag, true);
                }
                self.end_one(tag, then, reply, now);
            }
            Command::Restart(tag) | Command::End(tag) => {
                let then = match command {
                    Command::Restart(_) => Then::Start,
                    _ => Then::Stay,
                };
                self.end_one(tag, then, reply, now);
            }
            Command::Greeter => {
                let front = own.is_some_and(|(tag, _)| self.sessions.front == Some(tag));
                if uid != 0 && !front {
                    return refuse(reply, "only the desktop in front shows the login screen");
                }
                match self.sessions.show_greeter(now) {
                    Ok(_) => answer(reply, "ok"),
                    Err(why) => refuse(reply, &why),
                }
            }
            Command::Switch(tag) => match self.sessions.find(tag) {
                Some(s) if s.class.graphical() => {
                    self.sessions.want = Some(Want { tag, unlock: false });
                    answer(reply, "ok");
                }
                _ => refuse(reply, "no desktop has that tag"),
            },
            Command::Login(at, name) => {
                let Some(person) = session::person_named(name.as_str()) else {
                    return refuse(reply, "the record names no such person");
                };
                let logged_in = match (at, handed) {
                    (At::Desktop, None) => self.sessions.log_in(&person, true, now),
                    (At::Console, Some(tty)) if fs::isatty(tty.raw()) => {
                        self.sessions.log_in_console(&person, tty, now)
                    }
                    _ => Err("a console login hands a terminal, a desktop's nothing".into()),
                };
                match logged_in {
                    Ok(_) => answer(reply, "ok"),
                    Err(why) => refuse(reply, &why),
                }
            }
            Command::Lock => match own {
                Some((tag, Class::User)) => {
                    self.sessions.lock(tag);
                    answer(reply, "ok");
                }
                _ => refuse(reply, "the requester's session is no desktop"),
            },
            Command::Locked(locked) => match own {
                Some((tag, Class::User))
                    if self.leads(tag, pid) && !locked && self.sessions.front != Some(tag) =>
                {
                    refuse(reply, "a desktop behind stays locked")
                }
                Some((tag, Class::User)) if self.leads(tag, pid) => {
                    self.sessions.set_locked(tag, locked);
                    answer(reply, "ok");
                }
                _ => refuse(reply, "only a desktop's compositor says it locked"),
            },
        }
    }

    /// Whether `uid` may not leave another request waiting on an end.
    fn waiting_full(&self, uid: u32) -> bool {
        let waiting: usize = self.sessions.list.iter().map(|s| s.waiting.len()).sum();
        uid != 0 && waiting >= MAX_WAITING
    }

    fn end_one(&mut self, tag: u32, then: Then, reply: OwnedFd, now: u64) {
        self.sessions.end(tag, then, Some(reply), now);
    }

    /// Whether `pid` leads the session `tag`: its compositor.
    fn leads(&self, tag: u32, pid: u32) -> bool {
        self.sessions
            .find(tag)
            .and_then(|s| s.leader())
            .is_some_and(|leader| leader.pid == pid)
    }

    /// End every session, answering `reply` once the last has: an answer is
    /// owed to the request even when none runs.
    fn end_every(&mut self, reply: OwnedFd, now: u64) {
        self.sessions.end_all(Then::Stay, Some(reply), now);
    }

    /// A re-exec carries each service and session as it stands: one
    /// starting, ending, due, draining or left running has timers, a pipe or
    /// a child it does not.
    fn may_reexec(&self) -> bool {
        let draining = self.privd.draining.is_some()
            || self.remoted.as_ref().is_some_and(|r| r.draining.is_some())
            || self.remote_ending.is_some()
            || !self.retired.is_empty()
            || self.privd_ready.is_some()
            || self.units.busy();
        !draining && self.sessions.settled()
    }

    /// The new image answers `reply`; this one only when the exec failed, or
    /// when the new image keeps no services or sessions and they end first.
    /// True when this has decided when the re-execution is next due.
    fn reexec(&mut self, reply: Option<OwnedFd>, now: u64) -> bool {
        if !self.may_reexec() {
            if let Some(reply) = reply {
                answer(
                    reply,
                    "refused: a session is starting or ending, or a service drains",
                );
            }
            return false;
        }
        let reads = match reexec::reads() {
            Ok(reads) => reads,
            Err(why) => {
                eprintln!("init: cannot re-execute: {why}");
                if let Some(reply) = reply {
                    answer(reply, &format!("refused: {why}"));
                }
                return false;
            }
        };
        if !reads.sessions && !self.sessions.list.is_empty() {
            eprintln!("init: the new init keeps no session table; the sessions end first");
            self.wanted_after_reexec |= std::mem::take(&mut self.sessions.wanted);
            self.sessions.end_all(Then::Stay, None, now);
            self.reexec_due = Some(now);
            if let Some(reply) = reply {
                answer(
                    reply,
                    "refused: the sessions end first, then init re-executes",
                );
            }
            return true;
        }
        if !reads.units && !self.units.is_empty() {
            eprintln!("init: the new init keeps no extensions' services; they end first");
            self.units.retire(now);
            self.reexec_due = Some(now);
            if let Some(reply) = reply {
                answer(
                    reply,
                    "refused: extensions' services end first, then init re-executes",
                );
            }
            return true;
        }
        let carried = |supervised: &Supervised| reexec::Service {
            pid: supervised.child.as_ref().map(|child| child.pid),
            starts: supervised.budget.starts().collect(),
        };
        let state = State {
            links: self
                .consent
                .iter()
                .flat_map(|c| c.links.iter().map(|(p, a)| (p.raw(), a.raw())))
                .collect(),
            sessions_link: self
                .consent
                .as_ref()
                .and_then(|c| c.sessions.as_ref())
                .map(|(init, privd)| (init.raw(), privd.raw())),
            control: self.control.as_ref().map(Control::listener),
            reply: reply.as_ref().map(OwnedFd::raw),
            privd: carried(&self.privd),
            remoted: self.remoted.as_ref().map(carried),
            login: self.login.as_ref().map(carried),
            sessions: self
                .sessions
                .carried()
                .into_iter()
                .map(|c| reexec::Carried {
                    tag: c.session.tag,
                    uid: c.session.who.uid,
                    class: c.session.class,
                    leader: c.leader,
                    link: c.session.link,
                    locked: c.session.locked,
                    pipe: c.pipe,
                    tty: c.tty.map(str::to_owned),
                    starts: c.session.budget.starts().collect(),
                })
                .collect(),
            front: self.sessions.front,
            next_tag: self.sessions.next_tag(),
            held: self.sessions.held,
            wanted: self.sessions.wanted || self.wanted_after_reexec,
            fresh: self.sessions.fresh,
            default_starts: self.sessions.default_budget.starts().collect(),
            tests: self.tests.as_ref().map(|tests| tests.pid),
            legacy: None,
        };
        let units = reads.units.then(|| self.units.carry());
        let why = reexec::reexec(&state, reads.sessions, units.as_ref());
        eprintln!("init: cannot re-execute: {why}");
        if let Some(reply) = reply {
            answer(reply, &format!("refused: {why}"));
        }
        self.sessions.wanted |= std::mem::take(&mut self.wanted_after_reexec);
        let session = self.sessions.first_person_runs();
        if self.units.restore(now, session) {
            self.reexec_due = None;
            return true;
        }
        false
    }

    fn runs(&self, side: Side) -> bool {
        match side {
            Side::Privd => self.privd.child.is_some(),
            Side::Agent(link) => self
                .sessions
                .list
                .iter()
                .any(|s| s.link == Some(link) && s.leader().is_some()),
        }
    }

    /// Tell each side `exited` faced that it has gone, so it drops what it
    /// was waiting on at once: a frame written on one end of a link reaches
    /// whoever holds the other.
    fn reset_consent(&self, exited: Side) {
        let Some(consent) = self.consent.as_ref() else {
            return;
        };
        let frame = slopos_priv_core::consent::reset_frame(crate::privd::request_id());
        let ends: Vec<&OwnedFd> = match exited {
            Side::Privd => consent
                .links
                .iter()
                .enumerate()
                .filter(|(link, _)| self.runs(Side::Agent(*link as u8)))
                .map(|(_, (privd, _))| privd)
                .collect(),
            Side::Agent(link) if self.runs(Side::Privd) => consent
                .links
                .get(usize::from(link))
                .map(|(_, agent)| agent)
                .into_iter()
                .collect(),
            Side::Agent(_) => Vec::new(),
        };
        for end in ends {
            let _ = net::send(end.raw(), &frame, MSG_NOSIGNAL);
        }
    }

    /// The ends nothing reads: each link's on a side not running, and
    /// privd's of the sessions pair while no privd hears it.
    fn idle_consent_ends(&self) -> impl Iterator<Item = &OwnedFd> {
        let privd = self.runs(Side::Privd);
        self.consent.iter().flat_map(move |consent| {
            let links =
                consent
                    .links
                    .iter()
                    .enumerate()
                    .flat_map(move |(link, (privd_end, agent))| {
                        let agent_runs = self.runs(Side::Agent(link as u8));
                        [
                            (!privd).then_some(privd_end),
                            (!agent_runs).then_some(agent),
                        ]
                        .into_iter()
                        .flatten()
                    });
            let sessions = consent
                .sessions
                .iter()
                .filter(move |_| !privd)
                .map(|(_, privd_end)| privd_end);
            links.chain(sessions)
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

    /// The table as it stands, to `/run/sessions` and to privd, whenever it
    /// changed.
    fn publish(&mut self) {
        if !self.sessions.take_dirty() {
            return;
        }
        self.sessions.publish(self.background());
        if self.runs(Side::Privd) {
            self.publish_to_privd();
        }
    }

    /// Tell privd the table: the whole of it in one frame, which a privd
    /// about to start reads first.
    fn publish_to_privd(&self) {
        let Some((init_end, _)) = self.consent.as_ref().and_then(|c| c.sessions.as_ref()) else {
            return;
        };
        let frame = self.sessions.table().frame();
        let _ = net::send(init_end.raw(), &frame, MSG_NOSIGNAL);
    }

    /// remoted as the background session of the person it runs as.
    fn background(&self) -> Option<crate::sessions::Entry> {
        let remoted = self.remoted.as_ref()?;
        let who = remoted.who.as_ref()?;
        Some(crate::sessions::Entry {
            tag: init_control::REMOTE,
            uid: who.uid,
            user: who.name.clone(),
            class: Class::Background,
            active: false,
            locked: false,
            leader: remoted.child.as_ref().map(|c| c.pid),
            tty: None,
        })
    }
}

/// Whom a carried session runs as, as the record names them now.
fn identity_of(uid: u32, class: Class) -> Option<Identity> {
    if class == Class::Greeter && uid == GREETER_UID {
        return Some(Identity::greeter(None));
    }
    let accounts = crate::accounts::Store::read(std::path::Path::new("/"))
        .accounts()
        .ok()?;
    Some(Identity::person(accounts.by_uid(uid)?, None))
}

/// The person remoted runs as: whom the record names, else the first
/// administrator, under remoted's own login tag.
fn remote_person() -> Option<Identity> {
    let accounts = crate::accounts::Store::read(std::path::Path::new("/"))
        .accounts()
        .ok();
    let person = accounts
        .as_ref()
        .and_then(|accounts| accounts.remote().cloned());
    let mut who = match person {
        Some(person) => Identity::person(&person, None),
        None => Identity::first_person(),
    };
    who.login = Some(init_control::REMOTE);
    Some(who)
}

/// What the running extensions' actions were read from.
fn extension_actions() -> Result<String, String> {
    crate::activation::actions().map(|actions| actions.from)
}

fn readable(fd: &OwnedFd) -> bool {
    let mut fds = [UserPollFd {
        fd: fd.raw(),
        events: POLLIN,
        revents: 0,
    }];
    matches!(fs::poll(&mut fds, 0), Ok(n) if n > 0)
}
