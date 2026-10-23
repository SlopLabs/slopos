//! `installer --serve`: the engine as privd runs it for a driver in another
//! process, speaking `slopos_installer_core::protocol` on standard input and
//! output. It holds one probed [`System`] from `Probe` to `Install`, and
//! answers each request before it reads the next.

use slopos_abi::signal::SIGPIPE;
use slopos_installer_core::protocol::{self, Reply, Request, VERSION};
use slopos_installer_core::{Choices, Event, Failure, Issue};
use slopos_priv_core::ENV_ACTION;
use slopos_priv_core::wire::RELAYED_SIGNALS;
use slopos_remote_core::{Deframer, MAX_PAYLOAD, frame};

use super::engine::{Events, Settled, System};
use crate::syscall::{SyscallError, fs, net, process};

const ACTION: &str = "org.slopos.install";

/// The driver's requests, read off standard input.
struct Requests {
    frames: Deframer,
}

impl Requests {
    /// The next request; `None` at the end of the input.
    fn next(&mut self) -> Result<Option<Request>, String> {
        let mut buf = [0u8; 4096];
        loop {
            match self.frames.next_frame() {
                Ok(Some((kind, payload))) => {
                    return Request::decode(kind, &payload)
                        .map(Some)
                        .map_err(|e| e.to_string());
                }
                Ok(None) => {}
                Err(e) => return Err(e.to_string()),
            }
            match fs::read_slice(0, &mut buf) {
                Ok(0) => return Ok(None),
                Ok(n) => self.frames.push(&buf[..n]),
                Err(SyscallError::EINTR) => {}
                Err(e) => return Err(format!("reading requests: {e}")),
            }
        }
    }
}

/// Replies on standard output. Once the driver has gone, nothing more is
/// written and the engine carries on.
struct Replies {
    gone: bool,
    /// The last progress sent: its bytes copied and its copy's total.
    reported: (u64, u64),
}

impl Replies {
    fn send(&mut self, reply: &Reply) {
        let (kind, payload) = reply.encode();
        if payload.len() > MAX_PAYLOAD {
            let issue = Issue::Probe(format!(
                "the answer would be {} bytes, past what a frame holds",
                payload.len()
            ));
            return self.send(&Reply::Issue(issue));
        }
        let frame = frame(kind, &payload);
        let mut sent = 0;
        while !self.gone && sent < frame.len() {
            match fs::write_slice(1, &frame[sent..]) {
                Ok(n) if n > 0 => sent += n,
                Err(SyscallError::EINTR) => {}
                _ => self.gone = true,
            }
        }
    }
}

/// The most of a line a log event carries: a program that prints no line
/// break would otherwise grow one past what a frame holds.
const LOG_LINE_MAX: usize = 16 * 1024;
/// How far a copy moves between the progress it reports.
const PROGRESS_EVERY: u64 = 16 << 20;

impl Events for Replies {
    fn event(&mut self, mut event: Event) {
        match &mut event {
            Event::Log { line } if line.len() > LOG_LINE_MAX => {
                let mut cut = LOG_LINE_MAX;
                while !line.is_char_boundary(cut) {
                    cut -= 1;
                }
                line.truncate(cut);
            }
            Event::Progress { done, total } => {
                let (reported, of) = self.reported;
                let same_copy = *total == of && *done >= reported;
                if same_copy && *done - reported < PROGRESS_EVERY && done != total {
                    return;
                }
                self.reported = (*done, *total);
            }
            _ => {}
        }
        self.send(&Reply::Event(event));
    }
}

/// The proposal last made, which alone may be installed.
struct Last {
    id: u64,
    choices: Choices,
    settled: Settled,
}

/// Serve until the input ends or an install succeeds; after an install
/// that fails, or is refused, the engine serves on unprobed.
pub fn run() -> Result<(), String> {
    if std::env::var_os(ENV_ACTION).is_none_or(|action| action != ACTION) {
        return Err(format!(
            "--serve answers only the program privd starts it for, as {ACTION}"
        ));
    }
    process::ignore_signal(SIGPIPE);
    for fd in [0, 1] {
        net::set_blocking(fd).map_err(|e| format!("fd {fd}: {e}"))?;
    }
    let mut requests = Requests {
        frames: protocol::requests(),
    };
    let mut replies = Replies {
        gone: false,
        reported: (0, 0),
    };
    match requests.next()? {
        Some(Request::Hello { version }) => {
            replies.send(&Reply::Hello { version: VERSION });
            if version != VERSION {
                return Err(format!(
                    "the driver speaks version {version} of the protocol, this engine {VERSION}"
                ));
            }
        }
        Some(_) => return Err("the driver's first request was not Hello".into()),
        None => return Ok(()),
    }
    let mut system: Option<System> = None;
    let mut last: Option<Last> = None;
    let mut made = 0u64;
    let unprobed = || Reply::Issue(Issue::Probe("the machine has not been probed".into()));
    while let Some(request) = requests.next()? {
        let reply = match request {
            Request::Hello { .. } => return Err("the driver said Hello twice".into()),
            Request::Probe => {
                last = None;
                system = None;
                match System::probe() {
                    Ok(probed) => Reply::System(system.insert(probed).view()),
                    Err(issue) => Reply::Issue(issue),
                }
            }
            Request::Open { disk } => {
                last = None;
                match system.as_mut() {
                    None => unprobed(),
                    Some(system) => match system.open(&disk) {
                        Ok(()) => Reply::System(system.view()),
                        Err(issue) => Reply::Issue(issue),
                    },
                }
            }
            Request::Propose(choices) => match system.as_ref() {
                None => unprobed(),
                Some(system) => match system.propose(&choices) {
                    Ok(settled) => {
                        made += 1;
                        let proposal = settled.view.clone();
                        last = Some(Last {
                            id: made,
                            choices,
                            settled,
                        });
                        Reply::Proposal { id: made, proposal }
                    }
                    Err(issue) => Reply::Issue(issue),
                },
            },
            Request::Install { choices, proposal } => {
                let result = match (system.take(), last.take()) {
                    (Some(system), Some(last)) if last.id == proposal && last.choices == choices => {
                        let held = RELAYED_SIGNALS.map(|signal| {
                            (signal, process::ignore_signal_saving(signal))
                        });
                        let result = system.install(&choices, &last.settled, &mut replies);
                        for (signal, action) in &held {
                            process::restore_signal(*signal, action);
                        }
                        result
                    }
                    _ => Err(Failure {
                        step: 0,
                        message: "that is not the proposal last made for these choices; nothing was written".into(),
                    }),
                };
                match result {
                    Ok(summary) => {
                        replies.send(&Reply::Done(summary));
                        return Ok(());
                    }
                    Err(failure) => Reply::Failed(failure),
                }
            }
        };
        replies.send(&reply);
    }
    Ok(())
}
