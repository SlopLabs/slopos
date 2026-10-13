//! Asking the person at the machine through the compositor at the other end
//! of the socket pair init made; `Questions` decides who is asked when.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use slopos_abi::syscall::{MSG_NOSIGNAL, POLLHUP, POLLIN, POLLOUT, UserPollFd};
use slopos_priv_core::consent::{
    self as protocol, Answer, Ask, EXPIRY_MS, Questions, Refusal, WAITING_MAX, kind,
};
use slopos_remote_core::Deframer;

use crate::syscall::{OwnedFd, SyscallError, fs, net};

const TICK_MS: i64 = 250;
const WRITE_MS: i64 = 2000;
/// A question unanswered this long past its expiry means the broker itself
/// is stuck; the asker gives up rather than hang with it.
const GRACE_MS: u64 = 10_000;

/// The requester's side of a question while it waits.
pub trait Waiting {
    /// The question is on screen now.
    fn shown(&mut self);
    /// Whether the requester has gone or withdrawn the request.
    fn withdrawn(&mut self) -> bool;
}

enum Event {
    Shown,
    Answered(Result<(), Refusal>),
}

struct Asker {
    events: Sender<Event>,
    withdrawn: Arc<AtomicBool>,
}

impl Asker {
    fn settle(&self, result: Result<(), Refusal>) {
        let _ = self.events.send(Event::Answered(result));
    }
}

pub struct Broker {
    questions: Mutex<Sender<(Ask, Asker)>>,
}

impl Broker {
    /// With no channel, every question is refused at once.
    pub fn start(channel: Option<OwnedFd>) -> Broker {
        let (tx, rx) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("privd-consent".into())
            .spawn(move || serve(channel, rx));
        if spawned.is_err() {
            log!("privd: no consent thread; every question will be refused");
        }
        Broker {
            questions: Mutex::new(tx),
        }
    }

    /// The person's answer, or why there is none.
    pub fn ask(&self, ask: Ask, waiting: &mut impl Waiting) -> Result<(), Refusal> {
        let (events, received) = mpsc::channel();
        let withdrawn = Arc::new(AtomicBool::new(false));
        let asker = Asker {
            events,
            withdrawn: withdrawn.clone(),
        };
        let sent = self
            .questions
            .lock()
            .map(|questions| questions.send((ask, asker)).is_ok())
            .unwrap_or(false);
        if !sent {
            return Err(Refusal::NoAgent);
        }
        let give_up = |refusal| {
            withdrawn.store(true, Ordering::Release);
            Err(refusal)
        };
        let started = Instant::now();
        let mut limit = Duration::from_millis(EXPIRY_MS * (WAITING_MAX as u64 + 1) + GRACE_MS);
        loop {
            match received.recv_timeout(Duration::from_millis(TICK_MS as u64)) {
                Ok(Event::Answered(result)) => return result,
                Ok(Event::Shown) => {
                    limit = started.elapsed() + Duration::from_millis(EXPIRY_MS + GRACE_MS);
                    waiting.shown();
                }
                Err(RecvTimeoutError::Disconnected) => return Err(Refusal::NoAgent),
                Err(RecvTimeoutError::Timeout) if started.elapsed() >= limit => {
                    return give_up(Refusal::Expired);
                }
                Err(RecvTimeoutError::Timeout) if waiting.withdrawn() => {
                    return give_up(Refusal::Withdrawn);
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }
}

struct Agent {
    fd: OwnedFd,
    frames: Deframer,
}

impl Agent {
    /// `false` when the agent would not take the frame in time.
    fn write(&self, frame: &[u8]) -> bool {
        let mut sent = 0;
        while sent < frame.len() {
            match net::send(self.fd.raw(), &frame[sent..], MSG_NOSIGNAL) {
                Ok(n) if n > 0 => sent += n,
                Err(SyscallError::EAGAIN) => {
                    let mut fds = [UserPollFd {
                        fd: self.fd.raw(),
                        events: POLLOUT,
                        revents: 0,
                    }];
                    if !matches!(fs::poll(&mut fds, WRITE_MS), Ok(n) if n > 0) {
                        return false;
                    }
                }
                Err(SyscallError::EINTR) => {}
                _ => return false,
            }
        }
        true
    }
}

fn serve(channel: Option<OwnedFd>, incoming: Receiver<(Ask, Asker)>) {
    let epoch = Instant::now();
    let now = || epoch.elapsed().as_millis() as u64;
    let mut agent = channel.map(|fd| {
        let _ = net::set_nonblocking(fd.raw());
        Agent {
            fd,
            frames: slopos_priv_core::wire::deframer(),
        }
    });
    let mut questions: Questions<Asker> = Questions::default();
    loop {
        let Some(live) = agent.as_mut() else {
            for asker in questions.lost() {
                asker.settle(Err(Refusal::NoAgent));
            }
            match incoming.recv() {
                Ok((_, asker)) => {
                    asker.settle(Err(Refusal::NoAgent));
                    continue;
                }
                Err(_) => return,
            }
        };
        while let Ok((ask, asker)) = incoming.try_recv() {
            if let Err((asker, refusal)) = questions.offer(ask, asker, now()) {
                asker.settle(Err(refusal));
            }
        }
        let gone = questions
            .withdraw(|asker| asker.withdrawn.load(Ordering::Acquire))
            .map(|(id, asker)| (id, asker, Refusal::Withdrawn))
            .or_else(|| {
                questions
                    .expired(now())
                    .map(|(id, asker)| (id, asker, Refusal::Expired))
            });
        if let Some((id, asker, refusal)) = gone {
            asker.settle(Err(refusal));
            if !live.write(&protocol::cancel_frame(id)) {
                agent = None;
                continue;
            }
        }
        if let Some((ask, asker)) = questions.next(now()) {
            if !live.write(&ask.frame()) {
                agent = None;
                continue;
            }
            let _ = asker.events.send(Event::Shown);
        }
        let mut fds = [UserPollFd {
            fd: live.fd.raw(),
            events: POLLIN,
            revents: 0,
        }];
        if fs::poll(&mut fds, TICK_MS).is_err() || fds[0].revents & (POLLIN | POLLHUP) == 0 {
            continue;
        }
        if !read(live, &mut questions, now()) {
            log!("privd: the consent agent has gone; no question can be asked");
            agent = None;
        }
    }
}

/// Take what the agent sent; `false` once it is gone or broke the protocol,
/// either of which ends it as the agent.
fn read(agent: &mut Agent, questions: &mut Questions<Asker>, now: u64) -> bool {
    let mut buf = [0u8; 1024];
    loop {
        match fs::read_slice(agent.fd.raw(), &mut buf) {
            Ok(n) if n > 0 => agent.frames.push(&buf[..n]),
            Err(SyscallError::EAGAIN) => break,
            Err(SyscallError::EINTR) => {}
            _ => return false,
        }
    }
    loop {
        match agent.frames.next_frame() {
            Ok(None) => return true,
            Ok(Some((kind::READY, payload))) if payload.is_empty() => questions.ready(),
            Ok(Some((kind::ANSWER, payload))) => {
                let Ok(answer) = Answer::decode(&payload) else {
                    return false;
                };
                for (asker, result) in questions.answer(answer, now) {
                    asker.settle(result);
                }
            }
            Ok(Some(_)) | Err(_) => return false,
        }
    }
}
