//! Getting bytes in and out of the machine: TLS to a server, `curl https` to
//! a file, and `nc` as a byte pipe, all against peers this test runs on
//! loopback so no case depends on the host's network.
//!
//! The server is the TLS crate's test server under a root minted here, so the
//! client under test is the one `curl` ships, anchored on a root the shipped
//! bundle does not hold.

use slopos_userland as _;

use std::fs::{self, File};
use std::io::ErrorKind;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use slopos_slibc::test_harness::note;
use slopos_tls_core::pem;
use slopos_tls_core::server::{Server, ServerConfig};
use slopos_tls_core::testpki::{self, AltName, Key, Profile};
use slopos_userland::tls::{self, CipherSuite, ClientConfig, TlsStream, TrustStore};

const WORK: &str = "/tmp/transfer";
const IO_TIMEOUT: Duration = Duration::from_secs(60);

struct Pki {
    root: Vec<u8>,
    leaf: Vec<u8>,
    key: Key,
}

fn pki() -> Pki {
    let root_key = Key::from_seed(b"transfer test root");
    let key = Key::from_seed(b"transfer test leaf");
    let root = testpki::issue(
        &Profile::ca("Transfer Test Root"),
        &root_key,
        "Transfer Test Root",
        &root_key,
        1,
    );
    let names = [AltName::Ip([127, 0, 0, 1]), AltName::Dns("localhost")];
    let leaf = testpki::issue(
        &Profile::leaf("localhost", &names),
        &key,
        "Transfer Test Root",
        &root_key,
        2,
    );
    Pki { root, leaf, key }
}

/// Gives up after `IO_TIMEOUT`: a client that exits before it connects must
/// fail its case, not hang it.
fn accept(listener: &TcpListener) -> Result<TcpStream, String> {
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + IO_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((sock, _)) => {
                sock.set_nonblocking(false).map_err(|e| e.to_string())?;
                sock.set_read_timeout(Some(IO_TIMEOUT))
                    .map_err(|e| e.to_string())?;
                return Ok(sock);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(format!("accept: {e}")),
        }
    }
}

fn body(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 131 + i / 251) as u8).collect()
}

enum Framing {
    Length,
    Chunked,
}

fn serve_https(
    listener: TcpListener,
    suite: CipherSuite,
    payload: Vec<u8>,
    framing: Framing,
) -> JoinHandle<Result<(), String>> {
    let pki = pki();
    thread::spawn(move || {
        let mut sock = accept(&listener)?;
        let chain = vec![pki.leaf];
        let suites = [suite];
        let mut entropy = [0u8; 64];
        tls::random(&mut entropy);
        let mut server = Server::new(ServerConfig {
            chain: &chain,
            key: &pki.key.private,
            suites: &suites,
            alpn: &[b"http/1.1"],
            request_certificate: false,
            entropy,
        });
        let mut request = Vec::new();
        let mut buf = vec![0u8; 16 * 1024];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            sock.write_all(&server.take_output())
                .map_err(|e| e.to_string())?;
            let n = sock
                .read(&mut buf)
                .map_err(|e| format!("server read: {e}"))?;
            if n == 0 {
                return Err("the client hung up before its request".into());
            }
            if let Err(e) = server.read_tls(&buf[..n]) {
                let _ = sock.write_all(&server.take_output());
                return Err(format!("server TLS: {e}"));
            }
            let mut plain = [0u8; 1024];
            loop {
                let k = server.read(&mut plain);
                if k == 0 {
                    break;
                }
                request.extend_from_slice(&plain[..k]);
            }
        }
        let head = match framing {
            Framing::Length => format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                payload.len()
            ),
            Framing::Chunked => "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_string(),
        };
        // Sent as it is sealed: a body encrypted whole before its first byte
        // leaves can outlast the client's receive timeout under TCG.
        let mut send = |server: &mut Server, plain: &[u8]| -> Result<(), String> {
            server.write(plain).map_err(|e| e.to_string())?;
            sock.write_all(&server.take_output())
                .map_err(|e| format!("server write: {e}"))
        };
        send(&mut server, head.as_bytes())?;
        match framing {
            Framing::Length => {
                for piece in payload.chunks(64 * 1024) {
                    send(&mut server, piece)?;
                }
            }
            Framing::Chunked => {
                for chunk in payload.chunks(10_007) {
                    let mut framed = format!("{:x}\r\n", chunk.len()).into_bytes();
                    framed.extend_from_slice(chunk);
                    framed.extend_from_slice(b"\r\n");
                    send(&mut server, &framed)?;
                }
                send(&mut server, b"0\r\n\r\n")?;
            }
        }
        server.close();
        sock.write_all(&server.take_output())
            .map_err(|e| format!("server write: {e}"))?;
        let _ = sock.shutdown(std::net::Shutdown::Write);
        Ok(())
    })
}

fn listener() -> Option<(TcpListener, u16)> {
    let l = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| note(&format!("bind: {e}")))
        .ok()?;
    let port = l.local_addr().ok()?.port();
    Some((l, port))
}

fn joined(server: JoinHandle<Result<(), String>>) -> bool {
    match server.join() {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            note(&format!("server: {e}"));
            false
        }
        Err(_) => {
            note("server panicked");
            false
        }
    }
}

/// Through the library curl uses, a body larger than the 64 KiB an unscaled
/// TCP window could carry.
fn tls_client_reads_a_large_body() -> bool {
    let Some((l, port)) = listener() else {
        return false;
    };
    let payload = body(1 << 20);
    let server = serve_https(
        l,
        CipherSuite::Aes128GcmSha256,
        payload.clone(),
        Framing::Length,
    );

    let mut trust = TrustStore::new();
    if trust.add_der(&pki().root).is_err() {
        note("the test root does not parse");
        return false;
    }
    let sock = match TcpStream::connect(("127.0.0.1", port)) {
        Ok(s) => s,
        Err(e) => {
            note(&format!("connect: {e}"));
            return false;
        }
    };
    let _ = sock.set_read_timeout(Some(IO_TIMEOUT));
    let cfg = ClientConfig {
        server_name: "127.0.0.1",
        trust: &trust,
        now: tls::unix_now(),
        alpn: &[b"http/1.1"],
        suites: &CipherSuite::ALL,
    };
    let mut stream = match TlsStream::connect(sock, cfg) {
        Ok(s) => s,
        Err(e) => {
            note(&format!("handshake: {e}"));
            return false;
        }
    };
    if stream.alpn_protocol() != Some(b"http/1.1") {
        note("ALPN was not negotiated");
        return false;
    }
    if let Err(e) = stream.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n") {
        note(&format!("request: {e}"));
        return false;
    }
    let mut got = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => got.extend_from_slice(&chunk[..n]),
            Err(e) => {
                note(&format!("read failed after {} bytes: {e}", got.len()));
                return false;
            }
        }
    }
    let Some(start) = got.windows(4).position(|w| w == b"\r\n\r\n") else {
        note(&format!("no response head in {} bytes", got.len()));
        return false;
    };
    let ok = got[start + 4..] == payload[..];
    if !ok {
        note(&format!(
            "body of {} bytes, want {}",
            got.len() - start - 4,
            payload.len()
        ));
    }
    joined(server) && ok
}

fn write_root() -> Option<String> {
    let _ = fs::create_dir_all(WORK);
    let path = format!("{WORK}/root.pem");
    fs::write(&path, pem::encode_certificate(&pki().root))
        .map_err(|e| note(&format!("writing {path}: {e}")))
        .ok()?;
    Some(path)
}

/// A chunked body, which has to be decoded as it streams in.
fn curl_https_writes_the_body_to_a_file() -> bool {
    let (Some((l, port)), Some(root)) = (listener(), write_root()) else {
        return false;
    };
    let payload = body(700_000);
    let server = serve_https(
        l,
        CipherSuite::Chacha20Poly1305Sha256,
        payload.clone(),
        Framing::Chunked,
    );
    let out = format!("{WORK}/body");
    let _ = fs::remove_file(&out);
    let status = Command::new("/bin/curl")
        .args([
            "-sS",
            "--cacert",
            &root,
            "-o",
            &out,
            &format!("https://127.0.0.1:{port}/file"),
        ])
        .status();
    let ran = match status {
        Ok(s) if s.success() => true,
        other => {
            note(&format!("curl: {other:?}"));
            false
        }
    };
    let written = fs::read(&out).unwrap_or_default();
    let same = written == payload;
    if ran && !same {
        note(&format!(
            "curl wrote {} bytes, want {}",
            written.len(),
            payload.len()
        ));
    }
    joined(server) && ran && same
}

/// Exit 60, curl's status for an unverified peer, and nothing on stdout.
fn curl_refuses_an_untrusted_server() -> bool {
    let Some((l, port)) = listener() else {
        return false;
    };
    let server = serve_https(l, CipherSuite::Aes128GcmSha256, body(10), Framing::Length);
    let out = Command::new("/bin/curl")
        .arg(format!("https://127.0.0.1:{port}/"))
        .output();
    let _ = server.join();
    match out {
        Ok(o) if o.status.code() == Some(60) && o.stdout.is_empty() => true,
        Ok(o) => {
            note(&format!(
                "curl exited {:?}, stdout {} bytes, stderr {}",
                o.status.code(),
                o.stdout.len(),
                String::from_utf8_lossy(&o.stderr)
            ));
            false
        }
        Err(e) => {
            note(&format!("spawning curl: {e}"));
            false
        }
    }
}

/// Redirected, every byte value crosses both ways unaltered while both
/// directions are busy: no line editing, control keys or appended newline.
fn nc_is_a_byte_pipe() -> bool {
    let Some((l, port)) = listener() else {
        return false;
    };
    let echo = thread::spawn(move || -> Result<(), String> {
        let mut sock = accept(&l)?;
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let n = sock.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                return Ok(());
            }
            sock.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        }
    });
    let _ = fs::create_dir_all(WORK);
    let input_path = format!("{WORK}/nc-in");
    let input: Vec<u8> = (0..1024).flat_map(|_| 0..=255u8).collect();
    if let Err(e) = fs::write(&input_path, &input) {
        note(&format!("{e}"));
        return false;
    }
    let stdin = match File::open(&input_path) {
        Ok(f) => f,
        Err(e) => {
            note(&format!("{e}"));
            return false;
        }
    };
    let out = Command::new("/bin/nc")
        .args(["127.0.0.1", &port.to_string()])
        .stdin(Stdio::from(stdin))
        .output();
    let echoed = joined(echo);
    match out {
        Ok(o) if o.stdout == input => echoed,
        Ok(o) => {
            note(&format!(
                "nc returned {} bytes for {} sent (exit {:?})",
                o.stdout.len(),
                input.len(),
                o.status.code()
            ));
            false
        }
        Err(e) => {
            note(&format!("spawning nc: {e}"));
            false
        }
    }
}

/// The peer still gets all of `nc`'s redirected stdin, and `nc` then exits 0.
fn nc_keeps_sending_after_the_peer_half_closes() -> bool {
    let Some((l, port)) = listener() else {
        return false;
    };
    let sink = thread::spawn(move || -> Result<Vec<u8>, String> {
        let mut sock = accept(&l)?;
        sock.shutdown(std::net::Shutdown::Write)
            .map_err(|e| e.to_string())?;
        let mut got = Vec::new();
        sock.read_to_end(&mut got).map_err(|e| e.to_string())?;
        Ok(got)
    });
    let _ = fs::create_dir_all(WORK);
    let input_path = format!("{WORK}/nc-half-close");
    let input = body(256 * 1024);
    let stdin = match fs::write(&input_path, &input).and_then(|()| File::open(&input_path)) {
        Ok(f) => f,
        Err(e) => {
            note(&format!("{e}"));
            return false;
        }
    };
    let status = Command::new("/bin/nc")
        .args(["127.0.0.1", &port.to_string()])
        .stdin(Stdio::from(stdin))
        .status();
    let got = match sink.join() {
        Ok(Ok(got)) => got,
        Ok(Err(e)) => {
            note(&format!("peer: {e}"));
            return false;
        }
        Err(_) => {
            note("peer thread panicked");
            return false;
        }
    };
    match status {
        Ok(s) if s.success() && got == input => true,
        Ok(s) => {
            note(&format!(
                "the peer got {} of {} bytes; nc exited {:?}",
                got.len(),
                input.len(),
                s.code()
            ));
            false
        }
        Err(e) => {
            note(&format!("spawning nc: {e}"));
            false
        }
    }
}

fn main() {
    slopos_slibc::test_harness::run(&[
        (
            "tls_client_reads_a_large_body",
            tls_client_reads_a_large_body,
        ),
        (
            "curl_https_writes_the_body_to_a_file",
            curl_https_writes_the_body_to_a_file,
        ),
        (
            "curl_refuses_an_untrusted_server",
            curl_refuses_an_untrusted_server,
        ),
        ("nc_is_a_byte_pipe", nc_is_a_byte_pipe),
        (
            "nc_keeps_sending_after_the_peer_half_closes",
            nc_keeps_sending_after_the_peer_half_closes,
        ),
    ]);
}
