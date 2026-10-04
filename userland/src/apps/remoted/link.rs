//! One TLS connection to the broker, written and read at once.
//!
//! `TlsStream` is one object for both directions, so a thread blocked reading
//! it would hold off every writer. A link splits the socket instead: a reader
//! thread feeds what arrives to the TLS client and hands whole frames over a
//! bounded channel, answering PINGs itself, and every writer encrypts and
//! writes under one lock. A pinger thread keeps the broker able to tell a
//! dead machine from a quiet one; the reader declares the broker dead when
//! nothing has arrived for [`DEAD_AFTER`].

use std::io::{self, ErrorKind, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddrV4, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use slopos_remote_core::{CHUNK, Deframer, frame, kind, split_host_port};

use super::Settings;
use crate::tls::{self, CipherSuite, Client, ClientConfig};

pub type Frame = (u8, Vec<u8>);

const PING_EVERY: Duration = Duration::from_secs(15);
const DEAD_AFTER: Duration = Duration::from_secs(45);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// How often a blocked read wakes to look at the clock.
const READ_TICK: Duration = Duration::from_secs(1);
/// A write stuck this long means the broker stopped reading.
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
/// After `finish`, how long the reader waits for the broker to close.
const LINGER: Duration = Duration::from_secs(10);
/// Frames the reader may queue ahead of the request; past that it stops
/// reading, and TCP pushes back on the broker.
const QUEUE: usize = 32;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct Shared {
    tls: Mutex<Client<'static>>,
    /// The writing half, held across encrypt-and-write so records leave in
    /// the order they were sealed.
    out: Mutex<TcpStream>,
    /// For `shutdown` without waiting on a writer stuck holding `out`.
    ctl: TcpStream,
    dead: AtomicBool,
    closing: AtomicBool,
    why: Mutex<Option<String>>,
}

impl Shared {
    fn send(&self, kind: u8, payload: &[u8]) -> io::Result<()> {
        if self.dead.load(Ordering::Acquire) {
            return Err(io::Error::new(ErrorKind::BrokenPipe, self.reason()));
        }
        let mut out = lock(&self.out);
        let sealed = {
            let mut tls = lock(&self.tls);
            tls.write(&frame(kind, payload))
                .map_err(|e| io::Error::other(e.to_string()))?;
            tls.take_output()
        };
        let written = out.write_all(&sealed);
        drop(out);
        if let Err(e) = &written {
            self.kill(format!("writing to the broker: {e}"));
        }
        written
    }

    /// What the TLS client queued by itself while reading: alerts, key updates.
    fn flush(&self) -> io::Result<()> {
        let mut out = lock(&self.out);
        let sealed = lock(&self.tls).take_output();
        if sealed.is_empty() {
            return Ok(());
        }
        out.write_all(&sealed)
    }

    fn kill(&self, why: String) {
        let mut reason = lock(&self.why);
        if reason.is_none() {
            *reason = Some(why);
        }
        drop(reason);
        if !self.dead.swap(true, Ordering::AcqRel) {
            let _ = self.ctl.shutdown(Shutdown::Both);
        }
    }

    fn reason(&self) -> String {
        lock(&self.why)
            .clone()
            .unwrap_or_else(|| "the connection closed".into())
    }
}

/// A connected, authenticated-server link. Requests are served on the thread
/// that owns it; helpers borrow it from scoped threads.
pub struct Link {
    shared: Arc<Shared>,
    frames: Mutex<Receiver<Frame>>,
}

impl Link {
    /// Dial the broker and complete the TLS handshake against the pinned CA.
    pub fn open(s: &'static Settings) -> Result<Link, String> {
        let (host, port) =
            split_host_port(&s.broker).ok_or("the broker address is not host:port")?;
        let ip = crate::net::resolve_host(host).map_err(|e| format!("resolving {host}: {e}"))?;
        let addr = SocketAddrV4::new(Ipv4Addr::from(ip.octets()), port);
        let mut sock =
            TcpStream::connect(addr).map_err(|e| format!("connecting to {}: {e}", s.broker))?;
        let _ = sock.set_nodelay(true);
        let _ = sock.set_write_timeout(Some(WRITE_TIMEOUT));
        sock.set_read_timeout(Some(HANDSHAKE_TIMEOUT))
            .map_err(|e| format!("socket timeout: {e}"))?;

        let cfg = ClientConfig {
            server_name: &s.server_name,
            trust: &s.trust,
            now: tls::unix_now(),
            alpn: &[],
            suites: &CipherSuite::ALL,
        };
        let mut client = Client::new(cfg, &mut tls::random).map_err(|e| format!("TLS: {e}"))?;
        let mut buf = vec![0u8; CHUNK];
        while client.is_handshaking() {
            sock.write_all(&client.take_output())
                .map_err(|e| format!("TLS handshake with {}: {e}", s.broker))?;
            let n = sock.read(&mut buf).map_err(|e| match e.kind() {
                ErrorKind::WouldBlock | ErrorKind::TimedOut => format!(
                    "TLS handshake with {}: no answer in {} s",
                    s.broker,
                    HANDSHAKE_TIMEOUT.as_secs()
                ),
                _ => format!("TLS handshake with {}: {e}", s.broker),
            })?;
            if n == 0 {
                return Err(format!(
                    "{} closed the connection during the TLS handshake",
                    s.broker
                ));
            }
            let fed = client.read_tls(&buf[..n]);
            let _ = sock.write_all(&client.take_output());
            fed.map_err(|e| format!("TLS handshake with {}: {e}", s.broker))?;
        }
        sock.write_all(&client.take_output())
            .map_err(|e| format!("TLS handshake with {}: {e}", s.broker))?;
        sock.set_read_timeout(Some(READ_TICK))
            .map_err(|e| format!("socket timeout: {e}"))?;

        let reader_sock = sock.try_clone().map_err(|e| format!("socket dup: {e}"))?;
        let ctl = sock.try_clone().map_err(|e| format!("socket dup: {e}"))?;
        let shared = Arc::new(Shared {
            tls: Mutex::new(client),
            out: Mutex::new(sock),
            ctl,
            dead: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            why: Mutex::new(None),
        });
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let reader_shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("remoted-rx".into())
            .spawn(move || reader(reader_shared, reader_sock, tx))
            .map_err(|e| format!("spawning the reader: {e}"))?;
        let pinger_shared = Arc::clone(&shared);
        if let Err(e) = thread::Builder::new()
            .name("remoted-ping".into())
            .spawn(move || pinger(pinger_shared))
        {
            shared.kill(format!("spawning the pinger: {e}"));
            return Err(shared.reason());
        }
        Ok(Link {
            shared,
            frames: Mutex::new(rx),
        })
    }

    pub fn send(&self, kind: u8, payload: &[u8]) -> io::Result<()> {
        self.shared.send(kind, payload)
    }

    /// The next frame the broker sent; `Err` with why once the link is over.
    pub fn recv(&self) -> Result<Frame, String> {
        lock(&self.frames).recv().map_err(|_| self.shared.reason())
    }

    pub fn recv_timeout(&self, wait: Duration) -> Result<Frame, String> {
        match lock(&self.frames).recv_timeout(wait) {
            Ok(frame) => Ok(frame),
            Err(RecvTimeoutError::Timeout) => {
                Err(format!("no answer from the broker in {} s", wait.as_secs()))
            }
            Err(RecvTimeoutError::Disconnected) => Err(self.shared.reason()),
        }
    }

    /// Say `close_notify` and stop writing; the reader lingers until the
    /// broker closes too, so nothing it still sends is cut off by a reset.
    pub fn finish(&self) {
        let shared = &self.shared;
        if shared.closing.swap(true, Ordering::AcqRel) || shared.dead.load(Ordering::Acquire) {
            return;
        }
        let mut out = lock(&shared.out);
        let sealed = {
            let mut tls = lock(&shared.tls);
            tls.close();
            tls.take_output()
        };
        let _ = out.write_all(&sealed);
        let _ = out.shutdown(Shutdown::Write);
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.finish();
    }
}

fn pinger(shared: Arc<Shared>) {
    let mut last = Instant::now();
    while !shared.dead.load(Ordering::Acquire) && !shared.closing.load(Ordering::Acquire) {
        thread::sleep(READ_TICK);
        if last.elapsed() >= PING_EVERY {
            last = Instant::now();
            if shared.send(kind::PING, b"").is_err() {
                return;
            }
        }
    }
}

fn reader(shared: Arc<Shared>, mut sock: TcpStream, tx: SyncSender<Frame>) {
    let why = read_frames(&shared, &mut sock, &tx);
    shared.kill(why);
}

fn read_frames(shared: &Shared, sock: &mut TcpStream, tx: &SyncSender<Frame>) -> String {
    let mut buf = vec![0u8; CHUNK];
    let mut plain = vec![0u8; CHUNK];
    let mut frames = Deframer::new();
    let mut heard = Instant::now();
    let mut closing_since: Option<Instant> = None;
    // `tx` is dropped when the request has no more use for frames; what
    // arrives after that is read and discarded until the broker closes.
    let mut consumer = true;
    loop {
        let peer_closed = {
            let mut tls = lock(&shared.tls);
            while tls.plaintext_available() > 0 {
                let n = tls.read(&mut plain);
                frames.push(&plain[..n]);
            }
            tls.peer_closed()
        };
        loop {
            match frames.next_frame() {
                Ok(Some((kind::PING, payload))) => {
                    if shared.send(kind::PONG, &payload).is_err() {
                        return shared.reason();
                    }
                }
                Ok(Some((kind::PONG, _))) => {}
                Ok(Some(frame)) => {
                    if consumer && tx.send(frame).is_err() {
                        consumer = false;
                    }
                    // A full queue blocked the send: nothing was read meanwhile.
                    heard = Instant::now();
                }
                Ok(None) => break,
                Err(e) => return format!("the broker sent {e}"),
            }
        }
        if peer_closed {
            return "the broker closed the connection".into();
        }
        if shared.dead.load(Ordering::Acquire) {
            return shared.reason();
        }
        if shared.closing.load(Ordering::Acquire) {
            let since = *closing_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= LINGER {
                return "closed".into();
            }
        }
        match sock.read(&mut buf) {
            Ok(0) => return "the broker closed the connection".into(),
            Ok(n) => {
                heard = Instant::now();
                let fed = lock(&shared.tls).read_tls(&buf[..n]);
                let _ = shared.flush();
                if let Err(e) = fed {
                    return format!("TLS: {e}");
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if heard.elapsed() >= DEAD_AFTER {
                    return format!("the broker went silent for {} s", DEAD_AFTER.as_secs());
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return format!("reading from the broker: {e}"),
        }
    }
}
