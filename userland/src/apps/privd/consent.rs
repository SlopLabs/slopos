//! Asking the person at the machine through the compositor at the other end
//! of the socket pair init made; `Questions` decides who is asked when.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use slopos_abi::syscall::{MSG_NOSIGNAL, POLLHUP, POLLIN, POLLOUT, UserPollFd};
use slopos_priv_core::consent::{
    self as protocol, Answer, Ask, EXPIRY_MS, Inbound, Link, Questions, Refusal, WAITING_MAX, kind,
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
    Answered(Result<Answer, Refusal>),
}

struct Asker {
    events: Sender<Event>,
    withdrawn: Arc<AtomicBool>,
}

impl Asker {
    fn settle(&self, result: Result<Answer, Refusal>) {
        let _ = self.events.send(Event::Answered(result));
    }
}

pub struct Broker {
    questions: Mutex<Sender<(Ask, Asker)>>,
}

impl Broker {
    /// With no channel, every question is refused at once, and while
    /// `shown` is false, as its desktop is behind or locked.
    pub fn start(channel: Option<OwnedFd>, shown: Arc<AtomicBool>) -> Broker {
        let (tx, rx) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("privd-consent".into())
            .spawn(move || serve(channel, &shown, rx));
        if spawned.is_err() {
            log!("privd: no consent thread; every question will be refused");
        }
        Broker {
            questions: Mutex::new(tx),
        }
    }

    /// The person's allowing answer, or why there is none.
    pub fn ask(&self, ask: Ask, waiting: &mut impl Waiting) -> Result<Answer, Refusal> {
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
                Err(RecvTimeoutError::Timeout) if super::DRAINING.load(Ordering::SeqCst) => {
                    return give_up(Refusal::Interrupted);
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
    link: Link,
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
                    match fs::poll(&mut fds, WRITE_MS) {
                        Ok(n) if n > 0 => {}
                        Err(SyscallError::EINTR) => {}
                        _ => return false,
                    }
                }
                Err(SyscallError::EINTR) => {}
                _ => return false,
            }
        }
        true
    }
}

fn serve(channel: Option<OwnedFd>, shown: &AtomicBool, incoming: Receiver<(Ask, Asker)>) {
    let epoch = Instant::now();
    let now = || epoch.elapsed().as_millis() as u64;
    let mut agent = channel
        .map(|fd| {
            let _ = net::set_nonblocking(fd.raw());
            Agent {
                fd,
                frames: slopos_priv_core::wire::deframer(),
                link: Link::new(crate::privd::request_id()),
            }
        })
        .filter(|agent| agent.write(&agent.link.hello()));
    let mut questions: Questions<Asker> = Questions::default();
    loop {
        // The pair is shared with the privd that starts after this one.
        if super::DRAINING.load(Ordering::SeqCst) {
            agent = None;
        }
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
        let on_screen = shown.load(Ordering::SeqCst);
        while let Ok((ask, asker)) = incoming.try_recv() {
            let offered = match on_screen {
                true => questions.offer(ask, asker, now()),
                false => Err((asker, Refusal::Interrupted)),
            };
            if let Err((asker, refusal)) = offered {
                asker.settle(Err(refusal));
            }
        }
        if !on_screen {
            let (showing, askers) = questions.hidden();
            for asker in askers {
                asker.settle(Err(Refusal::Interrupted));
            }
            if let Some(id) = showing
                && !live.write(&protocol::cancel_frame(id))
            {
                agent = None;
                continue;
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
        let version = questions.version();
        if let Some((ask, asker)) = questions.next(now()) {
            if !live.write(&ask.frame(version)) {
                agent = None;
                continue;
            }
            if !protocol::says_shown(version) {
                let _ = asker.events.send(Event::Shown);
            }
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
/// either of which ends it as the agent. A new agent, or init on behalf of
/// one that exited, interrupts every question.
fn read(agent: &mut Agent, questions: &mut Questions<Asker>, now: u64) -> bool {
    let mut buf = [0u8; 1024];
    loop {
        match fs::read_slice(agent.fd.raw(), &mut buf) {
            Ok(n) if n > 0 => agent.frames.push(&buf[..n]),
            Err(SyscallError::EAGAIN) => break,
            Err(SyscallError::EINTR) => {}
            _ => {
                protocol::wipe(&mut buf);
                return false;
            }
        }
    }
    protocol::wipe(&mut buf);
    loop {
        let inbound = match agent.frames.next_frame() {
            Ok(None) => return true,
            Ok(Some((kind, payload))) => agent.link.take(kind, payload),
            Err(_) => return false,
        };
        match inbound {
            Ok(Inbound::Stale) => {}
            Ok(Inbound::Reset(seen)) => {
                for asker in questions.lost() {
                    asker.settle(Err(Refusal::Interrupted));
                }
                if !agent.write(&seen) {
                    return false;
                }
            }
            Ok(Inbound::Again(seen)) => {
                if !agent.write(&seen) {
                    return false;
                }
            }
            Ok(Inbound::Frame(kind::READY, payload)) => {
                let Ok(version) = protocol::decode_ready(&payload) else {
                    return false;
                };
                questions.ready(version);
            }
            Ok(Inbound::Frame(kind::SHOWN, payload))
                if protocol::says_shown(questions.version()) =>
            {
                let Ok(id) = protocol::decode_shown(&payload) else {
                    return false;
                };
                if let Some(asker) = questions.shown(id) {
                    let _ = asker.events.send(Event::Shown);
                }
            }
            Ok(Inbound::Frame(kind::ANSWER, mut payload)) => {
                let settled = match Answer::decode(&payload) {
                    Ok(answer) => questions.answer(answer, now),
                    Err(_) => Err(()),
                };
                protocol::wipe(&mut payload);
                agent.frames.scrub();
                let Ok(settled) = settled else {
                    return false;
                };
                for (asker, result) in settled {
                    asker.settle(result);
                }
            }
            Ok(Inbound::Frame(..)) | Err(_) => return false,
        }
    }
}
