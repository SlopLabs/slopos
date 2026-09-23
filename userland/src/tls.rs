//! TLS for userland programs: the system trust store, the kernel's CSPRNG,
//! the wall clock, and a blocking stream over any socket.

use std::fmt;
use std::io::{self, Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

pub use slopos_tls_core::{CertError, CipherSuite, Client, ClientConfig, Error, TrustStore};

/// The Mozilla root program's roots, installed by the image builder.
pub const SYSTEM_BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";

/// Fill `buf` from the kernel CSPRNG. There is no fallback: a handshake keyed
/// from anything weaker is worse than none, so a failure aborts.
pub fn random(buf: &mut [u8]) {
    let mut filled = 0;
    while filled < buf.len() {
        let n = crate::syscall::core::getrandom(&mut buf[filled..]);
        assert!(n > 0, "getrandom failed: {n}");
        filled += n as usize;
    }
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The roots in a PEM file; a file with none in it is an error rather than
/// an empty store that trusts nothing.
pub fn load_trust(path: &str) -> io::Result<TrustStore> {
    let pem = std::fs::read(path)?;
    let mut store = TrustStore::new();
    store.add_pem_bundle(&pem);
    if store.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{path} holds no usable certificate"),
        ));
    }
    Ok(store)
}

#[derive(Debug)]
pub enum ConnectError {
    Io(io::Error),
    Tls(Error),
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectError::Io(e) => write!(f, "{e}"),
            ConnectError::Tls(e) => write!(f, "{e}"),
        }
    }
}

/// A TLS client connection over `S`, read and written as plaintext.
pub struct TlsStream<'a, S> {
    sock: S,
    tls: Client<'a>,
    scratch: Vec<u8>,
    broken: Option<Error>,
    /// A socket write failed partway through the records it was sending, so
    /// nothing written after it could be framed.
    torn: Option<io::ErrorKind>,
}

fn tls_err(e: Error) -> io::Error {
    io::Error::other(e.to_string())
}

impl<'a, S: Read + Write> TlsStream<'a, S> {
    /// Handshake over `sock`; returns once the server is authenticated.
    pub fn connect(sock: S, cfg: ClientConfig<'a>) -> Result<Self, ConnectError> {
        let tls = Client::new(cfg, &mut random).map_err(ConnectError::Tls)?;
        let mut stream = Self {
            sock,
            tls,
            scratch: vec![0u8; 1 << 16],
            broken: None,
            torn: None,
        };
        while stream.tls.is_handshaking() {
            stream.flush_tls().map_err(ConnectError::Io)?;
            let n = stream
                .sock
                .read(&mut stream.scratch)
                .map_err(ConnectError::Io)?;
            if n == 0 {
                return Err(ConnectError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the server closed the connection during the TLS handshake",
                )));
            }
            let fed = stream.tls.read_tls(&stream.scratch[..n]);
            let flushed = stream.flush_tls();
            fed.map_err(ConnectError::Tls)?;
            flushed.map_err(ConnectError::Io)?;
        }
        stream.flush_tls().map_err(ConnectError::Io)?;
        Ok(stream)
    }

    pub fn suite(&self) -> Option<CipherSuite> {
        self.tls.suite()
    }

    pub fn alpn_protocol(&self) -> Option<&[u8]> {
        self.tls.alpn_protocol()
    }

    pub fn get_ref(&self) -> &S {
        &self.sock
    }

    fn flush_tls(&mut self) -> io::Result<()> {
        if let Some(kind) = self.torn {
            return Err(io::Error::new(
                kind,
                "an earlier socket write stopped partway through a TLS record",
            ));
        }
        if self.tls.wants_write() {
            let out = self.tls.take_output();
            if let Err(e) = self.sock.write_all(&out) {
                self.torn = Some(e.kind());
                return Err(e);
            }
        }
        Ok(())
    }

    pub fn close(&mut self) -> io::Result<()> {
        self.tls.close();
        self.flush_tls()
    }
}

impl<S: Read + Write> Read for TlsStream<'_, S> {
    /// `Ok(0)` only after the server's `close_notify`; a bare TCP close, which
    /// anyone on the path can forge to truncate a response, is `UnexpectedEof`.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.tls.plaintext_available() > 0 {
                return Ok(self.tls.read(buf));
            }
            if self.tls.peer_closed() {
                return Ok(0);
            }
            if let Some(e) = self.broken {
                return Err(tls_err(e));
            }
            let n = self.sock.read(&mut self.scratch)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the server closed the connection without a TLS close_notify",
                ));
            }
            let fed = self.tls.read_tls(&self.scratch[..n]);
            let flushed = self.flush_tls();
            match fed {
                Err(e) => self.broken = Some(e),
                // Plaintext first; `torn` makes the next flush report this one's failure.
                Ok(()) if self.tls.plaintext_available() > 0 => {}
                Ok(()) => flushed?,
            }
        }
    }
}

impl<S: Read + Write> Write for TlsStream<'_, S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.tls.write(buf).map_err(tls_err)?;
        self.flush_tls()?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_tls()?;
        self.sock.flush()
    }
}
