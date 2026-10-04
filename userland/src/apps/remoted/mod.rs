//! `/bin/remoted` — lets the developer's host drive this machine over the
//! LAN: run programs, move files, read the kernel log, install a system.
//!
//! It dials out rather than listening, so the host opens one port and this
//! machine none: `scripts/remote.py serve` is the broker, and remoted keeps an
//! idle TLS connection to it, the broker's certificate checked against a
//! pinned CA and the agent authenticated by a token it sends. A request
//! arriving on the idle connection is served there while another is dialled,
//! so requests run concurrently. The wire protocol is `slopos_remote_core`'s.
//!
//! What it runs, it runs with remoted's authority, `Launch` by program
//! identity, so `bootctl` and `halt` get their grants as from a shell. The
//! broker it dials therefore holds this machine, so where to dial is read
//! from the boot slot's base alone, which no process can rewrite: the host
//! pairs a machine by building the base it boots (`REMOTE_PAIRING_DIR`). A
//! configuration anywhere a program could write would hand any program the
//! authority of whatever broker it named.

mod link;
mod serve;

use std::sync::OnceLock;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use slopos_remote_core::{Backoff, Conf, Fields, PROTOCOL, hex, kind};

use self::link::Link;
use crate::syscall::{UserUtsname, core as sys_core};
use crate::tls::{self, TrustStore};

/// In the sealed base: `remote.conf`, `ca.pem`, `token`.
const CONFIG_DIR: &str = "/usr/share/slopos/remote";

/// What every connection reads for the life of the process; a connection's
/// TLS client borrows the trust store and server name from it.
static SETTINGS: OnceLock<Settings> = OnceLock::new();

const USAGE: &str = "usage: remoted
dial the broker the base names in /usr/share/slopos/remote and serve it";

/// A connection that lived less than this before it was lost counts as a
/// failed dial, so a broker that drops every connection is not redialled in
/// a loop.
const STABLE: Duration = Duration::from_secs(5);
const HELLO_TIMEOUT: Duration = Duration::from_secs(15);

pub(crate) struct Settings {
    pub broker: String,
    pub server_name: String,
    pub trust: TrustStore,
    token: String,
    boot: String,
    host: String,
    version: String,
    base_tag: String,
}

impl Settings {
    fn hello(&self) -> Vec<u8> {
        let tag = self.version.splitn(4, ' ').nth(3).unwrap_or("");
        Fields::new()
            .with("token", &self.token)
            .with("proto", PROTOCOL)
            .with("host", &self.host)
            .with("version", &self.version)
            .with("tag", tag)
            .with("base_tag", &self.base_tag)
            .with("boot", &self.boot)
            .with("uptime_ms", sys_core::get_time_ms().to_string())
            .with("pid", std::process::id().to_string())
            .encode()
    }
}

fn uts_field(field: &[u8]) -> String {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

fn load(dir: &str) -> Result<Settings, String> {
    let read = |name: &str| {
        let path = format!("{dir}/{name}");
        std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))
    };
    let conf = Conf::parse(&read("remote.conf")?).map_err(|e| format!("{dir}/remote.conf: {e}"))?;
    let token = read("token")?.trim().to_string();
    if token.is_empty() {
        return Err(format!("{dir}/token is empty"));
    }
    let ca = format!("{dir}/ca.pem");
    let trust = tls::load_trust(&ca).map_err(|e| format!("{ca}: {e}"))?;
    let mut uts = UserUtsname::new();
    sys_core::uname(&mut uts);
    let mut boot = [0u8; 16];
    tls::random(&mut boot);
    Ok(Settings {
        broker: conf.broker,
        server_name: conf.server_name,
        trust,
        token,
        boot: hex(&boot),
        host: uts_field(&uts.nodename),
        version: uts_field(&uts.version),
        base_tag: std::fs::read_to_string("/usr/share/slopos/build-tag")
            .map(|t| t.trim().to_string())
            .unwrap_or_default(),
    })
}

enum Event {
    Ready,
    Failed(String),
    /// The idle connection took a request, or was lost, after `lived`.
    Claimed(Duration),
    Lost(String, Duration),
}

fn connection(s: &'static Settings, events: Sender<Event>) {
    let link = match Link::open(s) {
        Ok(link) => link,
        Err(why) => {
            let _ = events.send(Event::Failed(why));
            return;
        }
    };
    if let Err(e) = link.send(kind::HELLO, &s.hello()) {
        let _ = events.send(Event::Failed(format!("sending hello: {e}")));
        return;
    }
    match link.recv_timeout(HELLO_TIMEOUT) {
        Ok((kind::WELCOME, _)) => {}
        Ok((kind::ERROR, payload)) => {
            let msg = Fields::decode(&payload)
                .ok()
                .and_then(|f| f.text("msg").map(str::to_string))
                .unwrap_or_default();
            let _ = events.send(Event::Failed(format!("the broker refused us: {msg}")));
            return;
        }
        Ok((other, _)) => {
            let _ = events.send(Event::Failed(format!(
                "the broker answered hello with frame {other:#04x}"
            )));
            return;
        }
        Err(why) => {
            let _ = events.send(Event::Failed(format!("hello: {why}")));
            return;
        }
    }
    let _ = events.send(Event::Ready);
    let born = Instant::now();
    match link.recv() {
        Ok(request) => {
            let _ = events.send(Event::Claimed(born.elapsed()));
            serve::serve(&link, request);
        }
        Err(why) => {
            let _ = events.send(Event::Lost(why, born.elapsed()));
        }
    }
}

/// Keep one idle connection open, forever: dial when there is none, at once
/// after one takes a request, and with backoff after a failure.
fn supervise(s: &'static Settings) -> ! {
    let (tx, rx) = mpsc::channel();
    let mut idle = 0usize;
    let mut dialing = false;
    let mut backoff = Backoff::new();
    let mut next = Instant::now();
    let mut complaint: Option<String> = None;
    let mut up = false;
    loop {
        if idle == 0 && !dialing && Instant::now() >= next {
            let events = tx.clone();
            match thread::Builder::new()
                .name("remoted-conn".into())
                .spawn(move || connection(s, events))
            {
                Ok(_) => dialing = true,
                Err(e) => {
                    let _ = tx.send(Event::Failed(format!("spawning a connection: {e}")));
                    dialing = true;
                }
            }
        }
        let wait = if idle == 0 && !dialing {
            next.saturating_duration_since(Instant::now())
        } else {
            Duration::from_secs(3600)
        };
        let event = match rx.recv_timeout(wait) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => unreachable!("supervise holds a sender"),
        };
        match event {
            Event::Ready => {
                dialing = false;
                idle += 1;
                if !up {
                    eprintln!("remoted: connected to {} as boot {}", s.broker, s.boot);
                    up = true;
                    complaint = None;
                }
            }
            Event::Failed(why) => {
                dialing = false;
                let secs = backoff.next_secs();
                next = Instant::now() + Duration::from_secs(secs.into());
                if complaint.as_deref() != Some(why.as_str()) {
                    eprintln!("remoted: {why}; retrying, backing off to 30 s");
                    complaint = Some(why);
                }
                up = false;
            }
            Event::Claimed(lived) => {
                idle -= 1;
                if lived >= STABLE {
                    backoff.reset();
                }
                next = Instant::now();
            }
            Event::Lost(why, lived) => {
                idle -= 1;
                if lived >= STABLE {
                    backoff.reset();
                    next = Instant::now();
                } else {
                    next = Instant::now() + Duration::from_secs(backoff.next_secs().into());
                }
                eprintln!("remoted: idle connection lost: {why}");
                up = false;
            }
        }
    }
}

fn daemon() -> Result<(), String> {
    let settings: &'static Settings = match SETTINGS.set(load(CONFIG_DIR)?) {
        Ok(()) => SETTINGS.get().expect("just set"),
        Err(_) => return Err("the daemon is already configured".into()),
    };
    eprintln!(
        "remoted: dialing {} (boot {})",
        settings.broker, settings.boot
    );
    supervise(settings)
}

pub fn remoted_main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.as_slice() {
        [] => daemon(),
        [flag] if flag == "-h" || flag == "--help" => {
            println!("{USAGE}");
            Ok(())
        }
        _ => Err(USAGE.into()),
    };
    if let Err(msg) = result {
        eprintln!("remoted: {msg}");
        sys_core::exit_with_code(1);
    }
}
