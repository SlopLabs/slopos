//! Against OpenSSL, in both directions: a client and server that share code
//! can agree on a mistake, and only another implementation can tell.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::string::{String, ToString};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::vec::Vec;
use std::{format, fs, thread, vec};

use crate::client::{Client, ClientConfig};
use crate::conn::Error;
use crate::pem;
use crate::server::{Server, ServerConfig};
use crate::suite::CipherSuite;
use crate::testpki::{self, AltName, Key, Profile};
use crate::x509::TrustStore;

fn openssl(args: &[&str], dir: &Path) {
    let out = Command::new("openssl")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("the interop tests need the openssl command");
    assert!(
        out.status.success(),
        "openssl {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn scratch(tag: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("slopos-tls-{tag}-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// An OpenSSL-made root and `localhost` certificate, the server key per `key_args`.
fn openssl_pki(dir: &Path, key_args: &[&str]) {
    openssl(
        &[
            "req",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:P-384",
            "-nodes",
            "-keyout",
            "root.key",
            "-out",
            "root.pem",
            "-days",
            "2",
            "-subj",
            "/CN=Interop Root",
            "-addext",
            "basicConstraints=critical,CA:TRUE",
            "-addext",
            "keyUsage=critical,keyCertSign",
        ],
        dir,
    );
    let mut req = vec![
        "req",
        "-new",
        "-nodes",
        "-keyout",
        "server.key",
        "-out",
        "server.csr",
        "-subj",
        "/CN=localhost",
    ];
    req.extend_from_slice(key_args);
    openssl(&req, dir);
    fs::write(
        dir.join("ext.cnf"),
        "subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\nkeyUsage=critical,digitalSignature\n",
    )
    .expect("ext");
    openssl(
        &[
            "x509",
            "-req",
            "-in",
            "server.csr",
            "-CA",
            "root.pem",
            "-CAkey",
            "root.key",
            "-CAcreateserial",
            "-out",
            "server.pem",
            "-days",
            "2",
            "-sha256",
            "-extfile",
            "ext.cnf",
        ],
        dir,
    );
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

struct Kill(Child);

impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn connect_retrying(port: u16) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => return s,
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Err(e) => panic!("openssl s_server never listened: {e}"),
        }
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
}

fn os_random(buf: &mut [u8]) {
    let mut f = fs::File::open("/dev/urandom").expect("urandom");
    f.read_exact(buf).expect("random");
}

/// Handshake on `stream`, send `request`, and return the plaintext read to the end.
fn client_session(
    stream: &mut TcpStream,
    cfg: ClientConfig<'_>,
    request: &[u8],
) -> Result<Vec<u8>, Error> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let mut client = Client::new(cfg, &mut os_random)?;
    let mut buf = vec![0u8; 1 << 16];
    let mut sent = false;
    let mut plain = Vec::new();
    loop {
        let out = client.take_output();
        if !out.is_empty() {
            stream.write_all(&out).expect("send");
        }
        if client.is_connected() && !sent {
            client.write(request)?;
            sent = true;
            continue;
        }
        let n = stream.read(&mut buf).expect("the server's bytes");
        if n == 0 {
            break;
        }
        client.read_tls(&buf[..n])?;
        let mut chunk = [0u8; 4096];
        loop {
            let k = client.read(&mut chunk);
            if k == 0 {
                break;
            }
            plain.extend_from_slice(&chunk[..k]);
        }
        if client.peer_closed() {
            break;
        }
    }
    if !client.is_connected() || !client.peer_closed() {
        return Err(Error::Closed);
    }
    Ok(plain)
}

fn against_s_server(key_args: &[&str], suite: CipherSuite, openssl_suite: &str) {
    let dir = scratch("s_server");
    openssl_pki(&dir, key_args);
    let port = free_port();
    let _server = Kill(
        Command::new("openssl")
            .args([
                "s_server",
                "-accept",
                &port.to_string(),
                "-cert",
                "server.pem",
                "-key",
                "server.key",
                "-tls1_3",
                "-ciphersuites",
                openssl_suite,
                "-www",
                "-naccept",
                "1",
                "-quiet",
            ])
            .current_dir(&dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("openssl s_server"),
    );
    let mut trust = TrustStore::new();
    let root = fs::read(dir.join("root.pem")).expect("root");
    assert_eq!(trust.add_pem_bundle(&root), 0);
    assert_eq!(trust.len(), 1);

    let mut stream = connect_retrying(port);
    let suites = [suite];
    let cfg = ClientConfig {
        server_name: "localhost",
        trust: &trust,
        now: now(),
        alpn: &[],
        suites: &suites,
    };
    let reply = client_session(&mut stream, cfg, b"GET / HTTP/1.0\r\n\r\n")
        .unwrap_or_else(|e| panic!("{key_args:?} {openssl_suite}: {e}"));
    let text = String::from_utf8_lossy(&reply);
    assert!(text.starts_with("HTTP/1.0 200 ok"), "{text}");
    assert!(
        text.contains(openssl_suite),
        "s_server reports {openssl_suite}: {text}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn client_against_openssl_with_ecdsa_p256() {
    let args = ["-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256"];
    against_s_server(
        &args,
        CipherSuite::Aes128GcmSha256,
        "TLS_AES_128_GCM_SHA256",
    );
    against_s_server(
        &args,
        CipherSuite::Chacha20Poly1305Sha256,
        "TLS_CHACHA20_POLY1305_SHA256",
    );
    against_s_server(
        &args,
        CipherSuite::Aes256GcmSha384,
        "TLS_AES_256_GCM_SHA384",
    );
}

#[test]
fn client_against_openssl_with_ecdsa_p384_and_p521() {
    let args = ["-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-384"];
    against_s_server(
        &args,
        CipherSuite::Aes256GcmSha384,
        "TLS_AES_256_GCM_SHA384",
    );
    let args = ["-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-521"];
    against_s_server(
        &args,
        CipherSuite::Aes128GcmSha256,
        "TLS_AES_128_GCM_SHA256",
    );
}

#[test]
fn client_against_openssl_with_rsa_pss() {
    let args = ["-newkey", "rsa:2048"];
    against_s_server(
        &args,
        CipherSuite::Aes128GcmSha256,
        "TLS_AES_128_GCM_SHA256",
    );
    let args = ["-newkey", "rsa:4096"];
    against_s_server(
        &args,
        CipherSuite::Aes256GcmSha384,
        "TLS_AES_256_GCM_SHA384",
    );
}

/// OpenSSL's client, with verification on, against the test server.
fn s_client_against_server(suite: CipherSuite, openssl_suite: &str) {
    let dir = scratch("s_client");
    let root_key = Key::from_seed(b"interop root");
    let leaf_key = Key::from_seed(b"interop leaf");
    let root = testpki::issue(
        &Profile::ca("Interop Test Root"),
        &root_key,
        "Interop Test Root",
        &root_key,
        1,
    );
    let names = [AltName::Dns("server.test")];
    let leaf = testpki::issue(
        &Profile::leaf("server.test", &names),
        &leaf_key,
        "Interop Test Root",
        &root_key,
        2,
    );
    fs::write(dir.join("root.pem"), pem::encode_certificate(&root)).expect("root");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let server_thread = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        let chain = vec![leaf];
        let suites = [suite];
        let mut entropy = [0u8; 64];
        os_random(&mut entropy);
        let mut server = Server::new(ServerConfig {
            chain: &chain,
            key: &leaf_key.private,
            suites: &suites,
            alpn: &[],
            request_certificate: false,
            entropy,
        });
        let mut buf = vec![0u8; 1 << 16];
        let mut got = Vec::new();
        loop {
            let out = server.take_output();
            if !out.is_empty() {
                stream.write_all(&out).expect("send");
            }
            if got.ends_with(b"\n") {
                server.write(b"pong from slopos\n").expect("write");
                server.close();
                stream.write_all(&server.take_output()).expect("send");
                break;
            }
            let n = stream.read(&mut buf).expect("read");
            assert!(n > 0, "s_client hung up");
            server.read_tls(&buf[..n]).expect("server");
            let mut chunk = [0u8; 256];
            let k = server.read(&mut chunk);
            got.extend_from_slice(&chunk[..k]);
        }
        got
    });

    let mut child = Command::new("openssl")
        .args([
            "s_client",
            "-connect",
            &format!("127.0.0.1:{port}"),
            "-servername",
            "server.test",
            "-verify_hostname",
            "server.test",
            "-CAfile",
            "root.pem",
            "-verify_return_error",
            "-tls1_3",
            "-ciphersuites",
            openssl_suite,
            "-ign_eof",
        ])
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("openssl s_client");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"ping\n")
        .expect("write");
    let out = child.wait_with_output().expect("s_client");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{openssl_suite}: {stdout}\n{stderr}");
    assert!(stdout.contains("Verify return code: 0 (ok)"), "{stdout}");
    assert!(stdout.contains(openssl_suite), "{stdout}");
    assert!(stdout.contains("pong from slopos"), "{stdout}");
    assert_eq!(server_thread.join().expect("server"), b"ping\n");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn openssl_client_against_server() {
    s_client_against_server(CipherSuite::Aes128GcmSha256, "TLS_AES_128_GCM_SHA256");
    s_client_against_server(CipherSuite::Aes256GcmSha384, "TLS_AES_256_GCM_SHA384");
    s_client_against_server(
        CipherSuite::Chacha20Poly1305Sha256,
        "TLS_CHACHA20_POLY1305_SHA256",
    );
}
