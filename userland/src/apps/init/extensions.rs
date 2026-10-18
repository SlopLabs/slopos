//! Extensions' services: each enabled extension's, as the activation shown at
//! `/opt/<name>` declares them and its record grants them. Each runs as its
//! user, or a session service as the person while the desktop session runs,
//! holding what it declares within the grant and the base's ceiling, on the
//! sockets init binds and keeps for it, its output kept by a copy of init
//! that runs as the extension's first user. Each carries a login tag of its
//! own (`init_control::service_tag`), which ends what it leaves when it
//! exits and is how init finds it again after re-executing.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, fchown};

use slopos_abi::fs::inode_flags::FS_IMMUTABLE_FL;
use slopos_abi::fs::{O_DIRECTORY, O_NOFOLLOW, O_RDWR, SESSION_GID, SESSION_HOME, SESSION_UID};
use slopos_abi::net::{AF_INET, AF_UNIX, SOCK_CLOEXEC, SOCK_DGRAM, SOCK_STREAM};
use slopos_abi::syscall::{BOOT_FLAG_EXTENSIONS_OFF, O_CLOEXEC, O_NONBLOCK, POLLIN, UserPollFd};
use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};
use slopos_extension_core::ceiling::NEVER;
use slopos_extension_core::config;
use slopos_extension_core::declare::{Listen, Restart, Service, mount_point};
use slopos_extension_core::record::Record;
use slopos_extension_core::{hex, sha256};
use slopos_slibc::pal::{Pal, Sys};

use super::control::answer;
use super::readable;
use super::service::{Budget, Child, STARTS, session_env};
use super::session::{Ended, Termination};
use crate::activation;
use crate::apps::extctl::store::Store;
use crate::init_control::{LISTEN_FDS_START, LOGGER, Tasks, service_tag};
use crate::readiness::NOTIFIER_FD;
use crate::syscall::process::{self, Endowment, WaitStatus};
use crate::syscall::{OwnedFd, RawFd, core as sys_core, fs as sys_fs, net};

/// `argv[1]` of the copy of init that keeps a service's log.
pub const LOG: &str = "--log";
/// How large a service's log grows before it becomes `<service>.log.1`, the
/// one kept before it.
const LOG_MAX: u64 = 256 * 1024;
/// How long a service that says nothing on its notifier is waited for
/// before what starts after it starts anyway.
const READY_MS: u64 = 30_000;
/// How often init looks again while a service's processes end.
const STOPPING_TICK_MS: u64 = 50;
/// How long after a start that failed the next is tried, and after a read
/// of the store that failed the first time it is read again.
const RETRY_MS: u64 = 1000;
/// The longest a store that cannot be read waits to be read again.
const RECHECK_MAX_MS: u64 = 60_000;
/// The descriptors init gives its services, out of the 256 it may hold:
/// each takes its process's pidfd, its notifier, its log pipe and its
/// logger's pidfd, and one for each socket.
const SERVICE_FDS: usize = 128;
const FDS_PER_SERVICE: usize = 4;
const BACKLOG: u32 = 64;
/// A service's unix socket is anyone's to connect to: the service judges
/// who did.
const SOCKET_UMASK: u32 = 0o111;

/// A service as init runs it.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Spec {
    extension: String,
    service: Service,
    tag: u32,
    /// The user it runs as: its own, or the person for a session service.
    user: String,
    ids: (u32, u32),
    caps: u64,
    /// The ids of the extension's first user, who owns its log and
    /// directories.
    owner: (u32, u32),
    state: bool,
    runtime: bool,
    /// The activation its declaration was read from, which its program
    /// must be a file of.
    generation: u64,
    /// A digest of what it reads of `/etc/<name>`.
    config: String,
}

impl Spec {
    fn name(&self) -> String {
        format!("{}/{}", self.extension, self.service.name)
    }

    /// What of the image it runs on: its program, what it reads there, and
    /// the image's libraries, as the image's manifest names them.
    fn image_paths(&self) -> Vec<String> {
        let mount = mount_point(&self.extension);
        std::iter::once(&self.service.program)
            .chain(&self.service.reads)
            .filter_map(|path| path.strip_prefix(&mount))
            .map(str::to_owned)
            .chain(std::iter::once("/lib/".to_owned()))
            .collect()
    }

    fn same(&self, manifest: &str, other: &Spec, other_manifest: &str) -> bool {
        let paths = self.image_paths();
        let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
        let unshown = Spec {
            generation: other.generation,
            ..self.clone()
        };
        unshown == *other && !slopos_boot_core::base::changed(manifest, other_manifest, &paths)
    }

    /// What it runs as and on, which a re-executed init holds a running
    /// service to: two specs [`same`](Self::same) have one, whichever image
    /// of init computed it.
    fn digest(&self, manifest: &str) -> String {
        let service = &self.service;
        let listen: Vec<String> = service.listen.iter().map(carried_name).collect();
        let restart = match service.restart {
            Restart::Always => "always",
            Restart::OnFailure => "on-failure",
            Restart::Never => "never",
        };
        let mut text = format!(
            "extension {}\nservice {}\nprogram {}\nargs {}\nuser {} {} {}\n\
             caps {:#x}\nowner {} {}\nafter {}\nreads {}\nlisten {}\n\
             restart {restart}\nsession {}\ntag {}\nstate {}\nruntime {}\nconfig {}\n",
            self.extension,
            service.name,
            service.program,
            service.args.join(" "),
            self.user,
            self.ids.0,
            self.ids.1,
            self.caps,
            self.owner.0,
            self.owner.1,
            service.after.join(" "),
            service.reads.join(" "),
            listen.join(" "),
            service.session,
            self.tag,
            self.state,
            self.runtime,
            self.config,
        );
        let paths = self.image_paths();
        let mut lines: Vec<String> = slopos_boot_core::base::manifest_lines(manifest)
            .flatten()
            .filter(|line| {
                paths.iter().any(|path| match path.strip_suffix('/') {
                    Some(_) => line.path.starts_with(path.as_str()),
                    None => line.path == path,
                })
            })
            .map(|line| format!("{} {:o} {}\n", hex(&line.digest), line.mode, line.path))
            .collect();
        lines.sort_unstable();
        text.extend(lines);
        hex(&sha256(text.as_bytes()))
    }
}

/// Whether two services' `listen`s are one socket: a port, or a unix socket
/// of one extension's runtime directory.
fn one_socket((one, a): (&str, &Listen), (other, b): (&str, &Listen)) -> bool {
    a == b && (one == other || !matches!(a, Listen::Unix(_)))
}

/// The name a listen socket is carried across a re-execution by.
fn carried_name(listen: &Listen) -> String {
    match listen {
        Listen::Tcp(port) => format!("tcp:{port}"),
        Listen::Udp(port) => format!("udp:{port}"),
        Listen::Unix(name) => format!("unix:{name}"),
    }
}

struct Logger {
    /// The pipe's write end: held while the service is, so the logger
    /// outlives every start of it.
    pipe: OwnedFd,
    child: Child,
}

impl Logger {
    fn alive(&self) -> bool {
        self.child.exited.as_ref().is_some_and(|fd| !readable(fd))
    }
}

enum Run {
    /// Started at `due`, or not until something changes.
    Down {
        due: Option<u64>,
    },
    Up {
        child: Child,
        ready: Option<OwnedFd>,
        since: u64,
        up: bool,
    },
    Stopping {
        ending: Termination,
        then: Then,
    },
    /// Ended and no longer wanted.
    Gone,
}

enum Then {
    Restart,
    Idle,
    Remove,
    Replace(Box<(Spec, String)>),
}

struct Unit {
    spec: Spec,
    manifest: String,
    sockets: Vec<(Listen, OwnedFd)>,
    log: Option<Logger>,
    budget: Budget,
    /// Starts that failed since one succeeded.
    failed: usize,
    run: Run,
}

/// Every extension's services.
pub struct Units {
    units: Vec<Unit>,
    off: bool,
    /// Whether the desktop session ran as of the last step.
    session: bool,
    /// `extensions` requests, answered once no service is ending.
    waiting: Vec<OwnedFd>,
    /// Tags no unit or running extension claims, whose processes are ended,
    /// each batch on a clock of its own.
    strays: Vec<(Vec<u32>, Termination)>,
    /// Ended for an image of init that keeps no services.
    retired: bool,
    /// When the store, part of which could not be read, is read again, and
    /// how long the wait was.
    recheck: Option<(u64, u64)>,
    /// The store was read since [`take_reread`](Self::take_reread) last
    /// asked.
    reread: bool,
}

/// What a re-execution hands the next image of each service beside the
/// records: how it stood, its starts, a digest of what it was started as,
/// and the descriptors init held for it. The records alone cannot say that a
/// service is meant to be down, nor what a running one holds.
#[derive(Default)]
pub struct Carried {
    units: Vec<CarriedUnit>,
}

#[derive(Default)]
struct CarriedUnit {
    tag: u32,
    state: String,
    digest: String,
    /// The process it was started as, while that runs.
    pid: Option<u32>,
    starts: Vec<u64>,
    sockets: Vec<(String, RawFd)>,
    ready: Option<RawFd>,
    log: Option<(RawFd, u32)>,
}

/// The header line of what [`Carried::encode`] writes, which an image of
/// init that reads it carries in its text, its newline included so no later
/// version's header contains it.
pub const CARRIED: &str = "slopos-init-units 1\n";

impl Carried {
    pub fn descriptors(&self) -> Vec<RawFd> {
        self.units
            .iter()
            .flat_map(|unit| {
                unit.sockets
                    .iter()
                    .map(|&(_, fd)| fd)
                    .chain(unit.ready)
                    .chain(unit.log.map(|(fd, _)| fd))
            })
            .collect()
    }

    /// As [`super::reexec::State`] is: the first line names every descriptor
    /// kept, so an image that cannot read the rest still closes them.
    pub fn encode(&self) -> String {
        let kept: Vec<String> = self.descriptors().iter().map(|fd| fd.to_string()).collect();
        let mut out = format!("kept {}\n{CARRIED}", kept.join(" "));
        for unit in &self.units {
            let starts: Vec<String> = unit.starts.iter().map(u64::to_string).collect();
            let pid = unit.pid.map_or("-".to_owned(), |pid| pid.to_string());
            out += &format!(
                "unit {} {} {} {pid} {}\n",
                unit.tag,
                unit.state,
                unit.digest,
                starts.join(" ")
            );
            for (name, fd) in &unit.sockets {
                out += &format!("socket {} {name} {fd}\n", unit.tag);
            }
            if let Some(fd) = unit.ready {
                out += &format!("ready {} {fd}\n", unit.tag);
            }
            if let Some((fd, pid)) = unit.log {
                out += &format!("log {} {fd} {pid}\n", unit.tag);
            }
        }
        out
    }

    /// `text` read, or the descriptors its first line names when the rest
    /// cannot be.
    pub fn decode(text: &str) -> Result<Carried, Vec<RawFd>> {
        Self::decode_rest(text.lines().skip(1)).ok_or_else(|| Self::kept(text))
    }

    /// The descriptors `text`'s first line names.
    pub fn kept(text: &str) -> Vec<RawFd> {
        text.lines()
            .next()
            .and_then(|line| line.strip_prefix("kept"))
            .map(|fds| fds.split(' ').filter_map(|fd| fd.parse().ok()).collect())
            .unwrap_or_default()
    }

    fn decode_rest<'a>(mut lines: impl Iterator<Item = &'a str>) -> Option<Carried> {
        if lines.next() != Some(CARRIED.trim_end()) {
            return None;
        }
        let mut carried = Carried::default();
        for line in lines {
            let words: Vec<&str> = line.split(' ').collect();
            let tag: u32 = words.get(1)?.parse().ok()?;
            match words[0] {
                "unit" => {
                    let pid = match *words.get(4)? {
                        "-" => None,
                        pid => Some(pid.parse().ok()?),
                    };
                    let starts: Option<Vec<u64>> = words
                        .get(5..)?
                        .iter()
                        .filter(|w| !w.is_empty())
                        .map(|w| w.parse().ok())
                        .collect();
                    carried.units.push(CarriedUnit {
                        tag,
                        state: (*words.get(2)?).to_owned(),
                        digest: (*words.get(3)?).to_owned(),
                        pid,
                        starts: starts?,
                        ..CarriedUnit::default()
                    });
                }
                kind => {
                    let unit = carried.units.iter_mut().find(|unit| unit.tag == tag)?;
                    match (kind, &words[2..]) {
                        ("socket", [name, fd]) => {
                            unit.sockets.push(((*name).into(), fd.parse().ok()?))
                        }
                        ("ready", [fd]) => unit.ready = Some(fd.parse().ok()?),
                        ("log", [fd, pid]) => {
                            unit.log = Some((fd.parse().ok()?, pid.parse().ok()?))
                        }
                        _ => return None,
                    }
                }
            }
        }
        Some(carried)
    }

    fn take(&mut self, tag: u32) -> Option<CarriedUnit> {
        let at = self.units.iter().position(|unit| unit.tag == tag)?;
        Some(self.units.swap_remove(at))
    }
}

/// A descriptor an earlier image of init kept for this one alone.
fn adopt(fd: RawFd) -> OwnedFd {
    // SAFETY: the earlier image named it in the state it handed over, and
    // nothing else in this image owns it.
    unsafe { OwnedFd::from_raw(fd) }
}

impl Units {
    /// What the store and the activations say runs. A service an earlier
    /// image of init ran is taken over as `carried` says it stood when its
    /// process still carries its tag and runs what the records now say;
    /// one that does not is started again, and what carries a service's tag
    /// that nothing claims is ended.
    pub fn load(now: u64, session: bool, mut carried: Carried) -> Units {
        let mut units = Units {
            units: Vec::new(),
            off: sys_core::boot_flag(BOOT_FLAG_EXTENSIONS_OFF),
            session,
            waiting: Vec::new(),
            strays: Vec::new(),
            retired: false,
            recheck: None,
            reread: false,
        };
        let tasks = Tasks::read();
        if !units.off {
            let wanted = wanted(|_| 0).unwrap_or_else(|why| {
                eprintln!("init: {why}");
                Wanted {
                    unread: true,
                    ..Wanted::default()
                }
            });
            units.recheck_after(now, wanted.unread || !wanted.unknown.is_empty());
            for (spec, manifest) in wanted.specs {
                let earlier = carried.take(spec.tag);
                let unit = Unit::take_over_from(spec, manifest, earlier, &tasks, now, session);
                units.units.push(unit);
            }
        }
        for fd in carried.descriptors() {
            drop(adopt(fd));
        }
        let claimed: Vec<u32> = units.units.iter().map(|unit| unit.spec.tag).collect();
        let strays: Vec<u32> = tasks
            .service_tags()
            .into_iter()
            .filter(|tag| !claimed.contains(tag))
            .collect();
        if !strays.is_empty() {
            eprintln!(
                "init: ending the processes of {} services nothing runs",
                strays.len()
            );
            units.strays.push((strays, Termination::new(now)));
        }
        units
    }

    /// What a re-execution hands the next image.
    pub fn carry(&self) -> Carried {
        let units = self
            .units
            .iter()
            .filter_map(|unit| {
                let state = match &unit.run {
                    Run::Up { up: true, .. } => "up",
                    Run::Up { up: false, .. } => "starting",
                    Run::Down { due: Some(_) } => "due",
                    Run::Down { due: None } if unit.gave_up() => "failed",
                    Run::Down { due: None } => "idle",
                    Run::Stopping { .. } | Run::Gone => return None,
                };
                let (pid, ready) = match &unit.run {
                    Run::Up { child, ready, .. } => {
                        (Some(child.pid), ready.as_ref().map(OwnedFd::raw))
                    }
                    _ => (None, None),
                };
                Some(CarriedUnit {
                    tag: unit.spec.tag,
                    state: state.into(),
                    digest: unit.spec.digest(&unit.manifest),
                    pid,
                    starts: unit.budget.starts().collect(),
                    sockets: unit
                        .sockets
                        .iter()
                        .map(|(listen, fd)| (carried_name(listen), fd.raw()))
                        .collect(),
                    ready,
                    log: unit.log.as_ref().map(|log| (log.pipe.raw(), log.child.pid)),
                })
            })
            .collect();
        Carried { units }
    }

    /// Run what the store and the activations now say runs, answering
    /// `reply` once that is done. An extension whose record or activation
    /// cannot be read keeps its services as they stand.
    pub fn reload(&mut self, reply: Option<OwnedFd>, now: u64, session: bool) {
        let requested = reply.is_some();
        let refuse = |reply: Option<OwnedFd>, why: &str| match reply {
            Some(reply) => answer(reply, &format!("refused: {why}")),
            None => eprintln!("init: {why}"),
        };
        if self.off || self.retired {
            self.recheck = None;
        }
        if self.off {
            return refuse(reply, "extensions are off this boot");
        }
        if self.retired {
            return refuse(
                reply,
                "init re-executes into an image that keeps no services",
            );
        }
        self.reread = true;
        let Wanted {
            specs: mut wanted,
            unknown,
            ..
        } = match wanted(|extension| self.fds_of(extension)) {
            Ok(wanted) => wanted,
            Err(why) => {
                self.recheck_after(now, true);
                return refuse(reply, &why);
            }
        };
        self.recheck_after(now, !unknown.is_empty());
        for unit in &mut self.units {
            if unknown.contains(&unit.spec.extension) {
                continue;
            }
            let key = (&unit.spec.extension, &unit.spec.service.name);
            let found = wanted
                .iter()
                .position(|(spec, _)| (&spec.extension, &spec.service.name) == key);
            match found.map(|at| wanted.swap_remove(at)) {
                Some((spec, manifest)) if unit.spec.same(&unit.manifest, &spec, &manifest) => {
                    if let Run::Stopping { then, .. } = &mut unit.run
                        && matches!(then, Then::Remove | Then::Replace(_))
                    {
                        *then = Then::Replace(Box::new((spec, manifest)));
                    } else {
                        unit.spec.generation = spec.generation;
                        unit.manifest = manifest;
                        if requested && unit.gave_up() {
                            unit.failed = 0;
                            unit.run = Run::Down {
                                due: due_at(&unit.spec, now, session)
                                    .map(|now| unit.budget.next(now)),
                            };
                        }
                    }
                }
                Some(next) => unit.end(now, Then::Replace(Box::new(next))),
                None => unit.end(now, Then::Remove),
            }
        }
        for (spec, manifest) in wanted {
            let due = due_at(&spec, now, session);
            self.units
                .push(Unit::new(spec, manifest, Run::Down { due }));
        }
        self.waiting.extend(reply);
        self.step(now, session);
    }

    pub fn step(&mut self, now: u64, session: bool) {
        if self.recheck.is_some_and(|(at, _)| at <= now) {
            return self.reload(None, now, session);
        }
        if session != self.session {
            self.session = session;
            for unit in self.units.iter_mut().filter(|u| u.spec.service.session) {
                unit.session_changed(now, session);
            }
        }
        let ending = !self.strays.is_empty() || self.units.iter().any(Unit::stopping);
        let tasks = ending.then(Tasks::read);
        if let Some(tasks) = &tasks {
            self.strays.retain_mut(|(tags, ending)| {
                ending.step_seen(now, &tasks.processes_of(tags)).is_none()
            });
        }
        for unit in &mut self.units {
            if let Some(tasks) = &tasks {
                unit.step_stopping(now, session, tasks);
            }
            unit.step_ready(now);
        }
        self.units.retain(|unit| !matches!(unit.run, Run::Gone));
        for at in 0..self.units.len() {
            let unit = &self.units[at];
            let due = matches!(unit.run, Run::Down { due: Some(due) } if due <= now);
            let allowed = session || !unit.spec.service.session;
            if due && allowed && self.free(at) && self.startable(at, session) {
                self.units[at].start(now);
            }
        }
        if !self.units.iter().any(Unit::stopping) {
            for reply in self.waiting.drain(..) {
                answer(reply, "ok");
            }
        }
    }

    /// Whether nothing still being ended carries the tag `at` would start
    /// under, which the ending would take with it, or holds a socket it
    /// listens on.
    fn free(&self, at: usize) -> bool {
        let unit = &self.units[at];
        let stray = self
            .strays
            .iter()
            .any(|(tags, _)| tags.contains(&unit.spec.tag));
        !stray
            && !self.units.iter().enumerate().any(|(i, other)| {
                i != at
                    && other.stopping()
                    && (other.spec.tag == unit.spec.tag
                        || other.sockets.iter().any(|(held, _)| {
                            unit.spec.service.listen.iter().any(|listen| {
                                one_socket(
                                    (&other.spec.extension, held),
                                    (&unit.spec.extension, listen),
                                )
                            })
                        }))
            })
    }

    /// Whether every service `at` starts after is up, or will not start.
    fn startable(&self, at: usize, session: bool) -> bool {
        let unit = &self.units[at];
        unit.spec.service.after.iter().all(|after| {
            self.units
                .iter()
                .find(|other| {
                    other.spec.extension == unit.spec.extension && other.spec.service.name == *after
                })
                .is_none_or(|other| other.settled(session))
        })
    }

    /// A service whose process `pid` was ends what it left, then starts
    /// again as its declaration says.
    pub fn exited(&mut self, pid: u32, status: WaitStatus, now: u64, session: bool) {
        let Some(unit) = self
            .units
            .iter_mut()
            .find(|unit| matches!(&unit.run, Run::Up { child, .. } if child.pid == pid))
        else {
            return;
        };
        let clean = status == WaitStatus::Exited(0);
        let restart = match unit.spec.service.restart {
            Restart::Always => true,
            Restart::OnFailure => !clean,
            Restart::Never => false,
        } && (session || !unit.spec.service.session);
        eprintln!("init: {} exited ({status:?})", unit.spec.name());
        unit.run = Run::Stopping {
            ending: Termination::new(now),
            then: if restart { Then::Restart } else { Then::Idle },
        };
    }

    /// The login tags of the session's services.
    pub fn session_tags(&self) -> Vec<u32> {
        self.units
            .iter()
            .filter(|unit| unit.spec.service.session)
            .map(|unit| unit.spec.tag)
            .collect()
    }

    pub fn poll_fds(&self, fds: &mut Vec<UserPollFd>) {
        for unit in &self.units {
            if let Run::Up { child, ready, .. } = &unit.run {
                for fd in child.exited.iter().chain(ready) {
                    fds.push(UserPollFd {
                        fd: fd.raw(),
                        events: POLLIN,
                        revents: 0,
                    });
                }
            }
        }
    }

    /// When init next has something to do here.
    pub fn deadline(&self, now: u64) -> Option<u64> {
        let strays = (!self.strays.is_empty()).then_some(now + STOPPING_TICK_MS);
        let recheck = self.recheck.map(|(at, _)| at);
        self.units
            .iter()
            .filter_map(|unit| match &unit.run {
                Run::Down { due: Some(at) } if *at > now => Some(*at),
                Run::Up {
                    since, up: false, ..
                } => Some(since + READY_MS),
                Run::Stopping { .. } => Some(now + STOPPING_TICK_MS),
                _ => None,
            })
            .chain(strays)
            .chain(recheck)
            .min()
    }

    pub fn children_mut(&mut self) -> impl Iterator<Item = &mut Child> {
        self.units
            .iter_mut()
            .filter_map(|unit| match &mut unit.run {
                Run::Up { child, .. } => Some(child),
                _ => None,
            })
    }

    /// End every service for good, ahead of a re-execution into an image of
    /// init that would leave them running unsupervised.
    pub fn retire(&mut self, now: u64) {
        self.retired = true;
        self.recheck = None;
        for unit in &mut self.units {
            unit.end(now, Then::Remove);
        }
    }

    /// Run the services again after a re-execution [`retire`](Self::retire)
    /// made way for failed; whether they had been retired, when trying again
    /// would only end them again.
    pub fn restore(&mut self, now: u64, session: bool) -> bool {
        let retired = std::mem::take(&mut self.retired);
        if retired {
            self.reload(None, now, session);
        }
        retired
    }

    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }

    /// The descriptors `extension`'s services hold of init's.
    fn fds_of(&self, extension: &str) -> usize {
        let units = self
            .units
            .iter()
            .filter(|unit| unit.spec.extension == extension);
        units.map(|unit| fds(&unit.spec)).sum()
    }

    pub fn take_reread(&mut self) -> bool {
        std::mem::take(&mut self.reread)
    }

    /// Read the store again later, as when part of it could not be read,
    /// unless that is due already.
    pub fn read_again(&mut self, now: u64) {
        if self.recheck.is_none() {
            self.recheck_after(now, true);
        }
    }

    /// Stop ending what carries any of `tags`, which run again.
    pub fn spare(&mut self, tags: &[u32]) {
        self.strays.retain_mut(|(ending, _)| {
            ending.retain(|tag| !tags.contains(tag));
            !ending.is_empty()
        });
    }

    /// End what carries any of `tags` not already being ended.
    pub fn end_strays(&mut self, now: u64, mut tags: Vec<u32>) {
        tags.retain(|tag| !self.strays.iter().any(|(ending, _)| ending.contains(tag)));
        if !tags.is_empty() {
            self.strays.push((tags, Termination::new(now)));
        }
    }

    /// Read the store again later when part of it could not be read: a
    /// second on, then twice as long each time, up to [`RECHECK_MAX_MS`].
    fn recheck_after(&mut self, now: u64, unread: bool) {
        self.recheck = unread.then(|| {
            let wait = self
                .recheck
                .map_or(RETRY_MS, |(_, wait)| (wait * 2).min(RECHECK_MAX_MS));
            (now + wait, wait)
        });
    }

    /// Whether a re-execution would lose what is under way: a service or a
    /// stray ending, or a request waiting on that.
    pub fn busy(&self) -> bool {
        !self.waiting.is_empty() || !self.strays.is_empty() || self.units.iter().any(Unit::stopping)
    }
}

fn due_at(spec: &Spec, now: u64, session: bool) -> Option<u64> {
    (session || !spec.service.session).then_some(now)
}

impl Unit {
    fn new(spec: Spec, manifest: String, run: Run) -> Unit {
        Unit {
            spec,
            manifest,
            sockets: Vec::new(),
            log: None,
            budget: Budget::default(),
            failed: 0,
            run,
        }
    }

    /// `spec` as an earlier image of init left it: running on, down, or
    /// started again when what runs is not what the records now say.
    fn take_over_from(
        spec: Spec,
        manifest: String,
        carried: Option<CarriedUnit>,
        tasks: &Tasks,
        now: u64,
        session: bool,
    ) -> Unit {
        let tagged = tasks.children(spec.tag);
        let due = due_at(&spec, now, session);
        let mut unit = Unit::new(spec, manifest, Run::Down { due });
        let Some(carried) = carried else {
            if let Some(&pid) = tagged.first() {
                unit.run = up(pid, None, now, true);
                let again = (unit.spec.clone(), unit.manifest.clone());
                unit.end(now, Then::Replace(Box::new(again)));
            }
            return unit;
        };
        unit.budget = Budget::of(carried.starts);
        for (name, fd) in carried.sockets {
            let fd = adopt(fd);
            if let Some(listen) = unit
                .spec
                .service
                .listen
                .iter()
                .find(|listen| carried_name(listen) == name)
            {
                unit.sockets.push((listen.clone(), fd));
            }
        }
        let loggers = tasks.children(LOGGER);
        unit.log = carried
            .log
            .map(|(fd, pid)| Logger {
                pipe: adopt(fd),
                child: Child::watch(pid),
            })
            .filter(|log| loggers.contains(&log.child.pid));
        let ready = carried.ready.map(adopt);
        let same = carried.digest == unit.spec.digest(&unit.manifest);
        let running = carried
            .pid
            .filter(|&pid| tagged.contains(&pid) || tasks.unreaped(pid));
        unit.run = match (carried.state.as_str(), running) {
            ("up" | "starting", Some(pid)) => up(pid, ready, now, carried.state == "up"),
            ("up" | "starting", None) => Run::Stopping {
                ending: Termination::new(now),
                then: Then::Restart,
            },
            ("idle", _) if same => Run::Down { due: None },
            ("failed", _) if same => {
                unit.failed = STARTS;
                Run::Down { due: None }
            }
            ("due", _) if same => Run::Down {
                due: due.map(|now| unit.budget.next(now)),
            },
            _ => Run::Down { due },
        };
        if !same && matches!(unit.run, Run::Up { .. }) {
            let again = (unit.spec.clone(), unit.manifest.clone());
            unit.end(now, Then::Replace(Box::new(again)));
        }
        unit
    }

    fn stopping(&self) -> bool {
        matches!(self.run, Run::Stopping { .. })
    }

    /// Down after [`STARTS`] starts in a row failed.
    fn gave_up(&self) -> bool {
        matches!(self.run, Run::Down { due: None }) && self.failed >= STARTS
    }

    /// Up, or not going to start until something changes.
    fn settled(&self, session: bool) -> bool {
        match &self.run {
            Run::Up { up, .. } => *up,
            Run::Down { due } => due.is_none() || (self.spec.service.session && !session),
            Run::Stopping { .. } | Run::Gone => false,
        }
    }

    /// End what runs of it, then do `then`.
    fn end(&mut self, now: u64, then: Then) {
        self.run = match std::mem::replace(&mut self.run, Run::Gone) {
            Run::Stopping { ending, .. } => Run::Stopping { ending, then },
            _ => Run::Stopping {
                ending: Termination::new(now),
                then,
            },
        };
    }

    /// A session service starts with the session and ends with it, which
    /// ends its processes anyway.
    fn session_changed(&mut self, now: u64, session: bool) {
        match &mut self.run {
            Run::Down { due } => {
                *due = session.then_some(now);
                self.budget.clear();
                self.failed = 0;
            }
            Run::Up { .. } if !session => self.end(now, Then::Idle),
            Run::Stopping { then, .. } if !session && matches!(then, Then::Restart) => {
                *then = Then::Idle
            }
            Run::Stopping { then, .. } if session && matches!(then, Then::Idle) => {
                self.budget.clear();
                *then = Then::Restart;
            }
            _ => {}
        }
    }

    /// Become `spec`, keeping the sockets both listen on, and the log while
    /// its owner stays.
    fn take_over(&mut self, (spec, manifest): (Spec, String)) {
        self.sockets
            .retain(|(listen, _)| spec.service.listen.contains(listen));
        if spec.owner != self.spec.owner {
            self.log = None;
        }
        self.spec = spec;
        self.manifest = manifest;
        self.budget.clear();
        self.failed = 0;
    }

    fn step_stopping(&mut self, now: u64, session: bool, tasks: &Tasks) {
        let Run::Stopping { ending, .. } = &mut self.run else {
            return;
        };
        let Some(ended) = ending.step_seen(now, &tasks.processes_of(&[self.spec.tag])) else {
            return;
        };
        if let Ended::GaveUp(left) = ended {
            eprintln!(
                "init: {left} processes of {} would not end",
                self.spec.name()
            );
        }
        let Run::Stopping { then, .. } = std::mem::replace(&mut self.run, Run::Down { due: None })
        else {
            unreachable!()
        };
        match then {
            Then::Restart => {
                let due = self.budget.next(now);
                if due > now {
                    eprintln!(
                        "init: {} exited its {STARTS}th time in a minute; it starts again in {} s",
                        self.spec.name(),
                        (due - now).div_ceil(1000)
                    );
                }
                self.run = Run::Down { due: Some(due) };
            }
            Then::Idle => {
                self.sockets.clear();
                self.log = None;
            }
            Then::Remove => self.run = Run::Gone,
            Then::Replace(next) => {
                self.take_over(*next);
                self.run = Run::Down {
                    due: due_at(&self.spec, now, session),
                };
            }
        }
    }

    /// Up once the notifier says so, a byte or closed by a process still
    /// running, or [`READY_MS`] after the start.
    fn step_ready(&mut self, now: u64) {
        let Run::Up {
            child,
            ready,
            since,
            up,
        } = &mut self.run
        else {
            return;
        };
        if let Some(fd) = ready.as_ref().filter(|fd| readable(fd)) {
            let _ = sys_fs::read_slice(fd.raw(), &mut [0u8; 1]);
            let exited = child.exited.as_ref().is_some_and(readable);
            *up |= !exited;
            *ready = None;
        }
        if now >= *since + READY_MS {
            *up = true;
        }
    }

    fn start(&mut self, now: u64) {
        self.budget.record(now);
        match self.spawn() {
            Ok((child, ready)) => {
                self.failed = 0;
                self.run = Run::Up {
                    child,
                    ready: Some(ready),
                    since: now,
                    up: false,
                };
            }
            Err(why) => {
                self.failed += 1;
                let due = (self.failed < STARTS).then(|| self.budget.next(now + RETRY_MS));
                let then = if due.is_some() {
                    "it is tried again"
                } else {
                    "it is not tried again until its extension changes"
                };
                eprintln!("init: cannot start {}: {why}; {then}", self.spec.name());
                if due.is_none() {
                    self.sockets.clear();
                    self.log = None;
                }
                self.run = Run::Down { due };
            }
        }
    }

    fn spawn(&mut self) -> Result<(Child, OwnedFd), String> {
        let spec = self.spec.clone();
        lay_out(&spec)?;
        shown(&spec)?;
        for listen in &spec.service.listen {
            if !self.sockets.iter().any(|(held, _)| held == listen) {
                let fd = bind(listen, &spec.extension)?;
                self.sockets.push((listen.clone(), fd));
            }
        }
        let logger = match self.log.take().filter(Logger::alive) {
            Some(logger) => logger,
            None => start_logger(&spec)?,
        };
        let log = logger.pipe.raw();
        self.log = Some(logger);
        let null =
            sys_fs::open_cstr(c"/dev/null", O_RDWR).map_err(|e| format!("/dev/null: {e}"))?;
        let (ready, notifier) =
            sys_fs::pipe2(O_CLOEXEC as u32).map_err(|e| format!("a pipe: {e}"))?;
        let mut actions = vec![
            process::clone_fd(null.raw(), 0),
            process::clone_fd(log, 1),
            process::clone_fd(log, 2),
            process::clone_fd(notifier.raw(), NOTIFIER_FD),
        ];
        let mut names = Vec::new();
        for (at, listen) in spec.service.listen.iter().enumerate() {
            let fd = self
                .sockets
                .iter()
                .find(|(held, _)| held == listen)
                .map(|(_, fd)| fd.raw())
                .ok_or("a socket was not bound")?;
            actions.push(process::clone_fd(fd, LISTEN_FDS_START + at as i32));
            names.push(match listen {
                Listen::Tcp(port) => format!("tcp-{port}"),
                Listen::Udp(port) => format!("udp-{port}"),
                Listen::Unix(name) => name.clone(),
            });
        }
        let argv: Vec<CString> = std::iter::once(spec.service.program.as_str())
            .chain(spec.service.args.iter().map(String::as_str))
            .map(|arg| CString::new(arg).map_err(|_| "a NUL in an argument"))
            .collect::<Result<_, _>>()?;
        let argv: Vec<*const u8> = argv.iter().map(|arg| arg.as_ptr().cast()).collect();
        let state = format!("/var/lib/{}", spec.extension);
        let mut env: Vec<String> = if spec.service.session {
            session_env().into()
        } else {
            vec![
                format!("HOME={}", if spec.state { state.as_str() } else { "/" }),
                format!("USER={}", spec.user),
                format!("LOGNAME={}", spec.user),
            ]
        };
        env.push(format!(
            "PATH={}:{}/bin",
            slopos_priv_core::MECHANISM_PATH,
            mount_point(&spec.extension)
        ));
        env.push(format!("LANG={}", slopos_priv_core::MECHANISM_LANG));
        if !names.is_empty() {
            env.push(format!("LISTEN_FDS={}", names.len()));
            env.push(format!("LISTEN_FDNAMES={}", names.join(":")));
        }
        let env: Vec<CString> = env
            .into_iter()
            .map(|var| CString::new(var).map_err(|_| "a NUL in the environment"))
            .collect::<Result<_, _>>()?;
        let envp: Vec<*const u8> = env.iter().map(|var| var.as_ptr().cast()).collect();
        let cwd = if spec.service.session {
            if std::path::Path::new(SESSION_HOME).is_dir() {
                SESSION_HOME
            } else {
                "/"
            }
        } else if spec.state {
            state.as_str()
        } else {
            "/"
        };
        let tid = process::spawn_endowed(
            spec.service.program.as_bytes(),
            &argv,
            &envp,
            cwd.as_bytes(),
            TaskPriority::Normal,
            TASK_FLAG_USER_MODE,
            &actions,
            0,
            Endowment {
                caps: spec.caps,
                ids: Some(spec.ids),
                groups: Some(&[]),
                login: Some(spec.tag),
            },
        );
        if tid <= 0 {
            return Err(crate::syscall::SyscallError::from_errno(-tid).to_string());
        }
        Ok((Child::watch(tid as u32), ready))
    }
}

fn up(pid: u32, ready: Option<OwnedFd>, now: u64, up: bool) -> Run {
    Run::Up {
        child: Child::watch(pid),
        ready,
        since: now,
        up,
    }
}

/// The directories a service's extension has: `/var/log/<name>` for its
/// log, `/var/lib/<name>` and `/run/<name>` when it declares them. Each is
/// made for the first user or held to being its already: one there that is
/// anyone else's is no extension's to take.
fn lay_out(spec: &Spec) -> Result<(), String> {
    let name = &spec.extension;
    let dirs = [
        (true, format!("/var/log/{name}")),
        (spec.state, format!("/var/lib/{name}")),
        (spec.runtime, format!("/run/{name}")),
    ];
    for (_, path) in dirs.iter().filter(|(wanted, _)| *wanted) {
        own_dir(path, spec.owner)?;
    }
    Ok(())
}

fn open_dir(path: &str) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags((O_DIRECTORY | O_NOFOLLOW) as i32 | O_CLOEXEC as i32)
        .open(path)
}

fn own_dir(path: &str, (uid, gid): (u32, u32)) -> Result<(), String> {
    let made = match fs::create_dir(path) {
        Ok(()) => true,
        Err(e) if e.kind() == ErrorKind::AlreadyExists => false,
        Err(e) => return Err(format!("{path}: {e}")),
    };
    let dir = open_dir(path).map_err(|e| format!("{path}: {e}"))?;
    let meta = dir.metadata().map_err(|e| format!("{path}: {e}"))?;
    if !made {
        if meta.uid() != uid {
            return Err(format!("{path} belongs to uid {}, not {uid}", meta.uid()));
        }
        return Ok(());
    }
    let owned = fchown(&dir, Some(uid), Some(gid))
        .and_then(|()| dir.set_permissions(fs::Permissions::from_mode(0o755)));
    owned.map_err(|e| {
        let _ = fs::remove_dir(path);
        format!("{path}: {e}")
    })
}

/// Whether `spec`'s program is a file of the activation its declaration
/// was read from, and that activation is still the one shown.
fn shown(spec: &Spec) -> Result<(), String> {
    activation::holds(&spec.extension, spec.generation, &spec.service.program)
}

fn bind(listen: &Listen, extension: &str) -> Result<OwnedFd, String> {
    let fail = |e: crate::syscall::SyscallError| format!("{listen:?}: {e}");
    match listen {
        Listen::Tcp(port) => {
            let fd =
                net::socket(AF_INET, u32::from(SOCK_STREAM) | SOCK_CLOEXEC, 0).map_err(fail)?;
            // Connections an earlier start served may sit in TIME_WAIT on it.
            net::set_reuse_addr(fd.raw()).map_err(fail)?;
            net::bind_any(fd.raw(), *port).map_err(fail)?;
            net::listen(fd.raw(), BACKLOG).map_err(fail)?;
            Ok(fd)
        }
        Listen::Udp(port) => {
            let fd = net::socket(AF_INET, u32::from(SOCK_DGRAM) | SOCK_CLOEXEC, 0).map_err(fail)?;
            net::bind_any(fd.raw(), *port).map_err(fail)?;
            Ok(fd)
        }
        Listen::Unix(name) => {
            let path = format!("/run/{extension}/{name}");
            if fs::symlink_metadata(&path).is_ok_and(|meta| {
                use std::os::unix::fs::FileTypeExt;
                meta.file_type().is_socket()
            }) {
                let _ = fs::remove_file(&path);
            }
            let fd =
                net::socket(AF_UNIX, u32::from(SOCK_STREAM) | SOCK_CLOEXEC, 0).map_err(fail)?;
            let mask = Sys::umask(SOCKET_UMASK);
            let bound = net::bind_unix(fd.raw(), path.as_bytes());
            Sys::umask(mask);
            bound.map_err(fail)?;
            net::listen(fd.raw(), BACKLOG).map_err(fail)?;
            Ok(fd)
        }
    }
}

fn start_logger(spec: &Spec) -> Result<Logger, String> {
    let (read, pipe) = sys_fs::pipe2(O_CLOEXEC as u32).map_err(|e| format!("a pipe: {e}"))?;
    let argv: Vec<CString> = ["/sbin/init", LOG, spec.service.name.as_str()]
        .into_iter()
        .map(|arg| CString::new(arg).expect("no NUL in a service's name"))
        .collect();
    let argv: Vec<*const u8> = argv.iter().map(|arg| arg.as_ptr().cast()).collect();
    let actions = [
        process::clone_fd(read.raw(), 0),
        process::clone_fd(2, 1),
        process::clone_fd(2, 2),
    ];
    let cwd = format!("/var/log/{}", spec.extension);
    let tid = process::spawn_endowed(
        b"/sbin/init",
        &argv,
        &[],
        cwd.as_bytes(),
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &actions,
        0,
        Endowment {
            caps: 0,
            ids: Some(spec.owner),
            groups: Some(&[]),
            login: Some(LOGGER),
        },
    );
    if tid <= 0 {
        return Err(format!(
            "its log: {}",
            crate::syscall::SyscallError::from_errno(-tid)
        ));
    }
    Ok(Logger {
        pipe,
        child: Child::watch(tid as u32),
    })
}

/// The copy of init that keeps a service's log: what reaches its standard
/// input goes to `<service>.log` in its working directory, which becomes
/// `<service>.log.1` at [`LOG_MAX`]. It ends once no service writes there.
pub fn keep_log(service: &str) -> ! {
    use slopos_abi::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
    for signum in [SIGHUP, SIGINT, SIGQUIT, SIGTERM] {
        let _ = process::ignore_signal(signum);
    }
    if !slopos_extension_core::declare::valid_user(service) {
        sys_core::exit_with_code(2);
    }
    let current = format!("{service}.log");
    let previous = format!("{service}.log.1");
    let open = || {
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o644)
            .custom_flags(O_NOFOLLOW as i32 | O_NONBLOCK as i32 | O_CLOEXEC as i32)
            .open(&current)
            .ok()?;
        let meta = file.metadata().ok()?;
        meta.is_file().then_some((file, meta.len()))
    };
    let (mut log, mut size) = open().map_or((None, 0), |(file, len)| (Some(file), len));
    let mut input = std::io::stdin().lock();
    let mut buf = [0u8; 4096];
    loop {
        let n = match input.read(&mut buf) {
            Ok(0) => sys_core::exit_with_code(0),
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => sys_core::exit_with_code(1),
        };
        if size >= LOG_MAX || log.is_none() {
            let _ = fs::rename(&current, &previous);
            (log, size) = open().map_or((None, 0), |(file, len)| (Some(file), len));
        }
        if size < LOG_MAX
            && let Some(file) = log.as_mut()
            && file.write_all(&buf[..n]).is_ok()
        {
            size += n as u64;
        }
    }
}

/// What the store and the activations say runs.
#[derive(Default)]
struct Wanted {
    /// Each service of every enabled extension the store keeps and the
    /// kernel shows, with the activation's manifest.
    specs: Vec<(Spec, String)>,
    /// The extensions whose records or activations could not be read.
    unknown: Vec<String>,
    /// The store could not be read at all.
    unread: bool,
    /// The descriptors the services of `unknown` hold.
    held: usize,
}

/// The descriptors of init's a service takes.
fn fds(spec: &Spec) -> usize {
    FDS_PER_SERVICE + spec.service.listen.len()
}

/// What does not fit is said and left out: an extension whose port an
/// extension of a lower number listens on, or whose services would take init
/// past [`SERVICE_FDS`] beside those of the extensions before it and of each
/// it cannot read, which `held` names. A store or ceiling that cannot be read
/// is an error.
fn wanted(held: impl Fn(&str) -> usize) -> Result<Wanted, String> {
    let store = Store::open(false).map_err(|why| format!("the extension store: {why}"))?;
    let names = store
        .names()
        .map_err(|why| format!("the extension store: {why}"))?;
    let mut out = Wanted::default();
    if names.is_empty() {
        return Ok(out);
    }
    let ceiling = activation::ceiling()?;
    let mut records = Vec::new();
    for name in names {
        match store.record(&name) {
            Ok(Some(record)) => records.push(record),
            Ok(None) => {}
            Err(why) => {
                eprintln!("init: {why}");
                out.held += held(&name);
                out.unknown.push(name);
            }
        }
    }
    records.sort_by_key(Record::number);
    let mut taken_uids: Vec<u32> = Vec::new();
    let mut runnable = Vec::new();
    for record in &records {
        let uids: Vec<u32> = record.uids.iter().map(|&(_, uid)| uid).collect();
        if uids.iter().any(|uid| taken_uids.contains(uid)) {
            eprintln!(
                "init: {} holds a uid another extension holds; its services do not run",
                record.name
            );
            continue;
        }
        taken_uids.extend(uids);
        if !record.runs() {
            continue;
        }
        match specs(record, ceiling) {
            Ok(specs) => runnable.push((record, specs)),
            Err(why) => {
                eprintln!("init: {}: {why}", record.name);
                out.held += held(&record.name);
                out.unknown.push(record.name.clone());
            }
        }
    }
    let fds = |specs: &[(Spec, String)]| -> usize { specs.iter().map(|(spec, _)| fds(spec)).sum() };
    for (record, specs) in runnable {
        let taken =
            specs.iter().find_map(|(spec, _)| {
                spec.service.listen.iter().find(|listen| {
                    out.specs.iter().any(|(other, _)| {
                        other.service.listen.iter().any(|held| {
                            one_socket((&other.extension, held), (&spec.extension, listen))
                        })
                    })
                })
            });
        if let Some(listen) = taken {
            eprintln!(
                "init: {} listens on {listen:?}, as another extension does; its services do not run",
                record.name
            );
        } else if out.held + fds(&out.specs) + fds(&specs) > SERVICE_FDS {
            eprintln!(
                "init: {}'s services would take init past the descriptors it gives services; they do not run",
                record.name
            );
        } else {
            out.specs.extend(specs);
        }
    }
    Ok(out)
}

/// `record`'s services as its activation declares them.
fn specs(record: &Record, ceiling: u64) -> Result<Vec<(Spec, String)>, String> {
    let name = &record.name;
    let current = record.current.as_ref().ok_or("nothing is installed")?;
    let activation::Shown {
        generation: shown,
        declaration,
        manifest,
    } = activation::shown(record)?;
    let number = record.number().ok_or("its record gives it no number")?;
    let uid = |user: &str| {
        record
            .uid(user)
            .ok_or_else(|| format!("its record gives {user} no uid"))
    };
    let first = declaration
        .users
        .first()
        .map(|user| uid(user))
        .transpose()?;
    let mut out = Vec::new();
    for (slot, service) in declaration.services.iter().enumerate() {
        let tag = service_tag(number, slot).ok_or("its number makes no login tag")?;
        let owner = first.ok_or("it declares no user")?;
        let (user, ids) = match (&service.user, service.session) {
            (Some(user), false) => {
                let id = uid(user)?;
                (user.clone(), (id, id))
            }
            (None, true) => ("user".to_owned(), (SESSION_UID, SESSION_GID)),
            _ => return Err(format!("{} runs as no user of its own", service.name)),
        };
        let spec = Spec {
            extension: name.clone(),
            service: service.clone(),
            tag,
            user,
            ids,
            caps: service.caps & current.grant.caps & ceiling & !NEVER,
            owner: (owner, owner),
            state: declaration.state,
            runtime: declaration.runtime,
            generation: shown,
            config: configured(name, &service.reads),
        };
        out.push((spec, manifest.clone()));
    }
    Ok(out)
}

/// A digest of what `reads` names of `extension`'s `/etc/<name>` as it
/// stands: each sealed file's path and SHA-256, a directory's among them.
/// What no `Seal` holder put there is no configuration of its.
fn configured(extension: &str, reads: &[String]) -> String {
    let dir = config::dir(extension);
    let mut text = String::new();
    for path in reads.iter().filter(|path| path.starts_with(&dir)) {
        let path = path.trim_end_matches('/');
        let mut files: Vec<String> = match fs::read_dir(path) {
            Ok(listing) => listing
                .flatten()
                .filter_map(|entry| entry.file_name().into_string().ok())
                .map(|file| format!("{path}/{file}"))
                .collect(),
            Err(_) => vec![path.to_owned()],
        };
        files.sort();
        for file in files.iter().take(config::FILES_MAX) {
            let digest = sealed_bytes(file).map_or_else(|| "-".to_owned(), |b| hex(&sha256(&b)));
            text.push_str(&format!("{file} {digest}\n"));
        }
    }
    hex(&sha256(text.as_bytes()))
}

fn sealed_bytes(path: &str) -> Option<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW as i32 | (O_NONBLOCK | O_CLOEXEC) as i32)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    let sealed = sys_fs::inode_flags(file.as_raw_fd()).is_ok_and(|f| f & FS_IMMUTABLE_FL != 0);
    if !meta.is_file() || !sealed || meta.len() > config::FILE_MAX {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(config::FILE_MAX + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(bytes)
}
