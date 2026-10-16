//! The base as a long-running program sees it across a live switch: the
//! manifest it started under, and whether a switch changed what it runs.

use slopos_abi::syscall::{POLLIN, UserPollFd};
use slopos_boot_core::base::MANIFEST_PATH;

use crate::syscall::base::{base_generation, base_monitor};
use crate::syscall::{OwnedFd, RawFd, fs};

pub struct BaseWatch {
    monitor: OwnedFd,
    manifest: String,
    /// The monitor answered a switch whose manifest could not be read yet.
    unread: bool,
}

/// The manifests before and after a switch.
pub struct Switch {
    pub old: String,
    pub new: String,
}

impl Switch {
    /// Whether any of `paths` reads differently now: a path ending in `/`
    /// stands for everything beneath it.
    pub fn changed(&self, paths: &[&str]) -> bool {
        slopos_boot_core::base::changed(&self.old, &self.new, paths)
    }
}

impl BaseWatch {
    /// `None` on a kernel with no monitor to give, or a base whose manifest
    /// cannot be read.
    pub fn open() -> Option<BaseWatch> {
        let monitor = base_monitor().ok()?;
        let manifest = settled(&monitor)?;
        Some(BaseWatch {
            monitor,
            manifest,
            unread: false,
        })
    }

    pub fn fd(&self) -> RawFd {
        self.monitor.raw()
    }

    /// The manifest as of the last [`take`](Self::take), or the open.
    pub fn manifest(&self) -> &str {
        &self.manifest
    }

    pub fn switched(&self) -> bool {
        self.unread || readable(self.monitor.raw())
    }

    /// Once [`switched`](Self::switched): the manifests either side, or
    /// `None` while the new one cannot be read, which a later call retries.
    pub fn take(&mut self) -> Option<Switch> {
        let Some(new) = settled(&self.monitor) else {
            self.unread = true;
            return None;
        };
        self.unread = false;
        Some(Switch {
            old: std::mem::replace(&mut self.manifest, new.clone()),
            new,
        })
    }
}

/// The manifest of the generation the monitor last answered: read again
/// while another switch lands during the read.
fn settled(monitor: &OwnedFd) -> Option<String> {
    loop {
        let _ = base_generation(monitor.raw());
        let manifest = std::fs::read_to_string(MANIFEST_PATH).ok()?;
        if !readable(monitor.raw()) {
            return Some(manifest);
        }
    }
}

fn readable(fd: RawFd) -> bool {
    let mut fds = [UserPollFd {
        fd,
        events: POLLIN,
        revents: 0,
    }];
    matches!(fs::poll(&mut fds, 0), Ok(n) if n > 0) && fds[0].revents & POLLIN != 0
}
