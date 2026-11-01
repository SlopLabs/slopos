//! The network settings `/bin/netctl` keeps, carried out at every start and
//! for each adapter as it appears, as the kernel publishes it: `netctl
//! apply`, holding `NetAdmin`, one at a time.

use std::ffi::CString;

use slopos_abi::caps::NET_ADMIN;
use slopos_abi::net::{NET_EVENT_LEN, NET_MON_IFACE};
use slopos_abi::syscall::O_CLOEXEC;
use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};
use slopos_settings_core::network;

use super::service::Child;
use crate::net_query::Ifaces;
use crate::syscall::process::{self, Endowment};
use crate::syscall::{OwnedFd, RawFd, fs};

const NETCTL: &str = "/bin/netctl";

/// How long one `netctl apply` may run before it is ended, so one that
/// never finishes holds up no adapter after it.
const APPLY_MS: u64 = 60_000;

pub struct Adapters {
    /// Gone when the kernel would not watch, which leaves the adapters that
    /// appear later as the kernel gives them.
    monitor: Option<OwnedFd>,
    known: Vec<u32>,
    /// Adapters that appeared, by name, whose settings wait to be applied.
    due: Vec<String>,
    applying: Option<Child>,
    /// When the `netctl` in hand started, and whether it was ended since.
    since: u64,
    ended: bool,
}

fn kept() -> bool {
    std::path::Path::new(network::PATH).exists()
}

fn present() -> Vec<(u32, String)> {
    Ifaces::fetch()
        .map(|ifaces| {
            ifaces
                .rows
                .iter()
                .map(|row| (row.ifindex, crate::net_query::name_of(row).to_string()))
                .collect()
        })
        .unwrap_or_default()
}

impl Adapters {
    /// Watching for adapters from now, with every setting kept applied to
    /// those already here.
    pub fn start(now: u64) -> Adapters {
        let monitor = crate::syscall::net::net_monitor(NET_MON_IFACE, 0)
            .ok()
            .filter(|fd| fs::set_fd_cloexec(fd.raw()).is_ok());
        if monitor.is_none() {
            eprintln!("init: the kernel will not say when an adapter appears");
        }
        let known = present().into_iter().map(|(ifindex, _)| ifindex).collect();
        Adapters {
            monitor,
            known,
            due: Vec::new(),
            applying: kept().then(|| apply(None)).flatten(),
            since: now,
            ended: false,
        }
    }

    pub fn fd(&self) -> Option<RawFd> {
        self.monitor.as_ref().map(OwnedFd::raw)
    }

    /// Note the adapters published since, once the monitor says the table
    /// changed: read whole rather than from the events, so a lost event
    /// costs nothing.
    pub fn read(&mut self) {
        let Some(monitor) = self.fd() else {
            return;
        };
        let mut buf = [0u8; NET_EVENT_LEN * 16];
        let mut heard = false;
        while fs::read_slice(monitor, &mut buf).unwrap_or(0) > 0 {
            heard = true;
        }
        if !heard {
            return;
        }
        let now = present();
        for (ifindex, name) in &now {
            if !self.known.contains(ifindex) && kept() && !self.due.contains(name) {
                self.due.push(name.clone());
            }
        }
        self.known = now.into_iter().map(|(ifindex, _)| ifindex).collect();
    }

    /// Start applying the next adapter's settings, when none is in hand, and
    /// end the one in hand once it has run too long.
    pub fn step(&mut self, now: u64) {
        match &self.applying {
            Some(child) if !self.ended && now.saturating_sub(self.since) > APPLY_MS => {
                eprintln!("init: netctl apply ran past {APPLY_MS} ms and is ended");
                let _ = process::kill(child.pid, slopos_abi::signal::SIGKILL);
                self.ended = true;
            }
            Some(_) => {}
            None if !self.due.is_empty() => {
                let name = self.due.remove(0);
                self.applying = apply(Some(&name));
                self.since = now;
                self.ended = false;
            }
            None => {}
        }
    }

    pub fn child(&self) -> Option<&Child> {
        self.applying.as_ref()
    }

    pub fn child_mut(&mut self) -> Option<&mut Child> {
        self.applying.as_mut()
    }

    /// Whether `pid` was the `netctl` in hand, which it no longer is.
    pub fn exited(&mut self, pid: u32) -> bool {
        let ours = self.applying.as_ref().is_some_and(|c| c.pid == pid);
        if ours {
            self.applying = None;
        }
        ours
    }
}

fn apply(adapter: Option<&str>) -> Option<Child> {
    let null = fs::open_cstr(c"/dev/null", slopos_abi::fs::O_RDONLY | O_CLOEXEC as u32).ok()?;
    let actions = [
        process::clone_fd(null.raw(), 0),
        process::clone_fd(1, 1),
        process::clone_fd(2, 2),
    ];
    let argv: Vec<CString> = ["netctl", "apply"]
        .into_iter()
        .chain(adapter)
        .map(|arg| CString::new(arg).ok())
        .collect::<Option<_>>()?;
    let argv: Vec<*const u8> = argv.iter().map(|arg| arg.as_ptr().cast()).collect();
    let tid = process::spawn_endowed(
        NETCTL.as_bytes(),
        &argv,
        &[],
        b"/",
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &actions,
        0,
        Endowment {
            caps: NET_ADMIN,
            ..Endowment::NONE
        },
    );
    (tid > 0).then(|| Child::watch(tid as u32))
}
