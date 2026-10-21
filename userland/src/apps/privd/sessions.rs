//! The sessions as init tells privd of them, which decide where a person
//! asking can be asked and who is logged in.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use slopos_abi::fs::SESSION_UID;
use slopos_abi::syscall::{POLLIN, UserPollFd};
use slopos_priv_core::Reach;
use slopos_priv_core::sessions::{LINKS, Table, kind};
use slopos_remote_core::Deframer;

use crate::init_control::{DESKTOP, Tasks};
use crate::syscall::{OwnedFd, SyscallError, fs};

/// How often the thread hearing the table looks for a drain.
const TICK_MS: i64 = 250;

enum Heard {
    /// Started by an init from before the table, whose one desktop, the
    /// first person's, carries the desktop's login tag and is always shown.
    Legacy,
    Table(Mutex<Table>),
    /// Told to hear the table and handed no pair init made: nobody is
    /// reached, and who is logged in is unknown.
    Deaf,
}

pub struct Seen {
    heard: Heard,
    /// The table can no longer be heard: what was heard last is stale.
    deaf: AtomicBool,
    /// Whether each link's desktop is in front and unlocked, which its
    /// broker reads before it asks.
    pub shown: [Arc<AtomicBool>; LINKS],
}

impl Seen {
    pub fn legacy() -> &'static Seen {
        Seen::leak(Heard::Legacy)
    }

    /// The table init writes on `fd`, read by a thread of its own until the
    /// pair ends or privd drains.
    pub fn hear(fd: Option<OwnedFd>) -> &'static Seen {
        let Some(fd) = fd else {
            log!("privd: no session table to hear; nobody will be asked");
            return Seen::leak(Heard::Deaf);
        };
        let seen = Seen::leak(Heard::Table(Mutex::new(Table::default())));
        let _ = crate::syscall::net::set_nonblocking(fd.raw());
        let mut frames = slopos_priv_core::wire::deframer();
        // Init writes the table before privd starts.
        seen.read(&fd, &mut frames);
        let spawned = thread::Builder::new()
            .name("privd-sessions".into())
            .spawn(move || seen.serve(fd, frames));
        if spawned.is_err() {
            log!("privd: no thread to hear the sessions on; nobody will be asked");
            seen.go_deaf();
        }
        seen
    }

    fn go_deaf(&self) {
        self.deaf.store(true, Ordering::SeqCst);
        for shown in &self.shown {
            shown.store(false, Ordering::SeqCst);
        }
    }

    fn heard(&self) -> &Heard {
        match self.deaf.load(Ordering::SeqCst) {
            true => &Heard::Deaf,
            false => &self.heard,
        }
    }

    fn leak(heard: Heard) -> &'static Seen {
        let legacy = matches!(heard, Heard::Legacy);
        Box::leak(Box::new(Seen {
            heard,
            deaf: AtomicBool::new(false),
            shown: std::array::from_fn(|link| Arc::new(AtomicBool::new(legacy && link == 0))),
        }))
    }

    fn serve(&self, fd: OwnedFd, mut frames: Deframer) {
        while !super::DRAINING.load(Ordering::SeqCst) {
            let mut fds = [UserPollFd {
                fd: fd.raw(),
                events: POLLIN,
                revents: 0,
            }];
            if matches!(fs::poll(&mut fds, TICK_MS), Ok(n) if n > 0) && !self.read(&fd, &mut frames)
            {
                log!("privd: init's sessions channel ended; nobody will be asked");
                self.go_deaf();
                return;
            }
        }
    }

    /// Take every table init has written, the last standing; `false` once
    /// the channel has ended.
    fn read(&self, fd: &OwnedFd, frames: &mut Deframer) -> bool {
        let mut buf = [0u8; 4096];
        let open = loop {
            match fs::read_slice(fd.raw(), &mut buf) {
                Ok(n) if n > 0 => frames.push(&buf[..n]),
                Err(SyscallError::EAGAIN) => break true,
                Err(SyscallError::EINTR) => {}
                _ => break false,
            }
        };
        let mut last = None;
        loop {
            match frames.next_frame() {
                Ok(Some((kind::SESSIONS, payload))) => match Table::decode(&payload) {
                    Ok(table) => last = Some(table),
                    Err(_) => log!("privd: init sent a session table that does not parse"),
                },
                Ok(Some((other, _))) => log!("privd: frame {other:#04x} is no session table"),
                Ok(None) => break,
                Err(_) => {
                    *frames = slopos_priv_core::wire::deframer();
                    break;
                }
            }
        }
        if let (Some(table), Heard::Table(held)) = (last, &self.heard) {
            for (link, shown) in self.shown.iter().enumerate() {
                let on = table
                    .sessions
                    .iter()
                    .any(|s| s.link == Some(link as u8) && s.front && !s.locked);
                shown.store(on, Ordering::SeqCst);
            }
            *held
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = table;
        }
        open
    }

    /// Where a requester of `uid` carrying `login` is asked, and the link
    /// that asks it on screen.
    pub fn reach(&self, login: u32, uid: u32, terminal: bool) -> (Reach, Option<u8>) {
        match self.heard() {
            Heard::Table(table) => table
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .reach(login, uid, terminal),
            Heard::Legacy if login == DESKTOP => (
                Reach::Screen {
                    theirs: uid == SESSION_UID,
                },
                Some(0),
            ),
            Heard::Legacy | Heard::Deaf => (Reach::Nowhere, None),
        }
    }

    /// Everyone logged in at a desktop or a terminal.
    pub fn logged_in(&self, tasks: Option<&Tasks>) -> Result<Vec<u32>, String> {
        let unknown = || "who is logged in is unknown".to_owned();
        match self.heard() {
            Heard::Table(table) => Ok(table
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .logged_in()),
            Heard::Legacy => tasks
                .map(|tasks| tasks.uids_of(&[DESKTOP]))
                .ok_or_else(|| format!("{}: the tasks could not be listed", unknown())),
            Heard::Deaf => Err(format!("{}: init told privd no sessions", unknown())),
        }
    }
}
