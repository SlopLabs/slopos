use alloc::vec::Vec;

use crate::client::{Client, ClientConfig};
use crate::conn::{Alert, Error};
use crate::server::{Server, ServerConfig};
use crate::suite::CipherSuite;
use crate::testpki::{self, AltName, Key, Profile};
use crate::x509::{CertError, TrustStore};

const NOW: i64 = 1_790_000_000;

struct Pki {
    root: Vec<u8>,
    chain: Vec<Vec<u8>>,
    leaf_key: Key,
}

fn pki_with(leaf: &Profile<'_>, intermediate: &Profile<'_>) -> Pki {
    let root_key = Key::from_seed(b"root");
    let mid_key = Key::from_seed(b"intermediate");
    let leaf_key = Key::from_seed(b"leaf");
    let root = testpki::issue(
        &Profile::ca("Test Root"),
        &root_key,
        "Test Root",
        &root_key,
        1,
    );
    let mid = testpki::issue(intermediate, &mid_key, "Test Root", &root_key, 2);
    let leaf_cert = testpki::issue(leaf, &leaf_key, intermediate.subject, &mid_key, 3);
    Pki {
        root,
        chain: alloc::vec![leaf_cert, mid],
        leaf_key,
    }
}

const NAMES: &[AltName<'static>] = &[
    AltName::Dns("server.test"),
    AltName::Dns("*.wild.test"),
    AltName::Ip([127, 0, 0, 1]),
];

fn pki() -> Pki {
    pki_with(
        &Profile::leaf("server.test", NAMES),
        &Profile::ca("Test Intermediate"),
    )
}

fn counter_rng(seed: u8) -> impl FnMut(&mut [u8]) {
    let mut state = u64::from(seed).wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    move |buf: &mut [u8]| {
        for b in buf {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *b = state as u8;
        }
    }
}

fn entropy() -> [u8; 64] {
    let mut e = [0u8; 64];
    counter_rng(99)(&mut e);
    e
}

fn pump(client: &mut Client<'_>, server: &mut Server<'_>, chunk: usize) -> Result<(), Error> {
    loop {
        let to_server = client.take_output();
        let to_client = server.take_output();
        if to_server.is_empty() && to_client.is_empty() {
            return Ok(());
        }
        for piece in to_server.chunks(chunk) {
            server.read_tls(piece)?;
        }
        for piece in to_client.chunks(chunk) {
            client.read_tls(piece)?;
        }
    }
}

fn server_cfg<'a>(pki: &'a Pki, suites: &'a [CipherSuite]) -> ServerConfig<'a> {
    ServerConfig {
        chain: &pki.chain,
        key: &pki.leaf_key.private,
        suites,
        alpn: &[],
        request_certificate: false,
        entropy: entropy(),
    }
}

fn client_cfg<'a>(
    name: &'a str,
    trust: &'a TrustStore,
    suites: &'a [CipherSuite],
) -> ClientConfig<'a> {
    ClientConfig {
        server_name: name,
        trust,
        now: NOW,
        alpn: &[],
        suites,
    }
}

fn trust(pki: &Pki) -> TrustStore {
    let mut t = TrustStore::new();
    t.add_der(&pki.root).expect("root parses");
    t
}

fn exchange(client: &mut Client<'_>, server: &mut Server<'_>, len: usize) {
    let data: Vec<u8> = (0..len).map(|i| (i * 31 + 7) as u8).collect();
    client.write(&data).expect("client write");
    pump(client, server, 1 << 20).expect("pump");
    let mut got = alloc::vec![0u8; len + 1];
    let mut n = 0;
    while n < len {
        let k = server.read(&mut got[n..]);
        assert!(k > 0, "server read stalled at {n}");
        n += k;
    }
    assert_eq!(&got[..len], &data[..]);

    server.write(&data).expect("server write");
    pump(client, server, 1 << 20).expect("pump");
    let mut n = 0;
    while n < len {
        let k = client.read(&mut got[n..]);
        assert!(k > 0, "client read stalled at {n}");
        n += k;
    }
    assert_eq!(&got[..len], &data[..]);
}

#[test]
fn handshake_and_data_under_every_suite() {
    let pki = pki();
    let trust = trust(&pki);
    for suite in CipherSuite::ALL {
        let suites = [suite];
        let mut server = Server::new(server_cfg(&pki, &suites));
        let mut client = Client::new(
            client_cfg("server.test", &trust, &suites),
            &mut counter_rng(1),
        )
        .expect("client");
        pump(&mut client, &mut server, 1 << 20).expect("handshake");
        assert!(
            client.is_connected() && server.is_connected(),
            "{suite:?} connects"
        );
        assert_eq!(client.suite(), Some(suite));
        exchange(&mut client, &mut server, 70_000);
    }
}

#[test]
fn handshake_survives_one_byte_segments() {
    let pki = pki();
    let trust = trust(&pki);
    let suites = CipherSuite::ALL;
    let mut server = Server::new(server_cfg(&pki, &suites));
    let mut client = Client::new(
        client_cfg("server.test", &trust, &suites),
        &mut counter_rng(2),
    )
    .expect("client");
    pump(&mut client, &mut server, 1).expect("handshake");
    assert!(client.is_connected());
    exchange(&mut client, &mut server, 100);
}

#[test]
fn server_names_by_wildcard_and_address() {
    let pki = pki();
    let trust = trust(&pki);
    let suites = CipherSuite::ALL;
    for name in ["a.wild.test", "A.WILD.TEST", "127.0.0.1", "server.test."] {
        let mut server = Server::new(server_cfg(&pki, &suites));
        let mut client =
            Client::new(client_cfg(name, &trust, &suites), &mut counter_rng(3)).expect("client");
        pump(&mut client, &mut server, 1 << 20).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert!(client.is_connected(), "{name}");
    }
}

fn expect_rejection(pki: &Pki, name: &str, now: i64, want: CertError) {
    let trust = trust(pki);
    let suites = CipherSuite::ALL;
    let mut server = Server::new(server_cfg(pki, &suites));
    let mut cfg = client_cfg(name, &trust, &suites);
    cfg.now = now;
    let mut client = Client::new(cfg, &mut counter_rng(4)).expect("client");
    let got = pump(&mut client, &mut server, 1 << 20);
    assert_eq!(got, Err(Error::Certificate(want)), "{name}");
    assert!(!client.is_connected());
    let alert = client.take_output();
    assert!(!alert.is_empty(), "the client tells the server why");
    assert!(
        server.read_tls(&alert).is_err(),
        "and the server hears an alert"
    );
}

#[test]
fn certificates_are_refused_for_cause() {
    let good = pki();
    expect_rejection(&good, "other.test", NOW, CertError::NameMismatch);
    expect_rejection(&good, "wild.test", NOW, CertError::NameMismatch);
    expect_rejection(&good, "a.b.wild.test", NOW, CertError::NameMismatch);
    expect_rejection(&good, "127.0.0.2", NOW, CertError::NameMismatch);
    expect_rejection(&good, "server.test", 4_200_000_000, CertError::Expired);
    expect_rejection(&good, "server.test", 1_000_000_000, CertError::NotYetValid);

    let mut client_only = Profile::leaf("server.test", NAMES);
    client_only.server_auth = Some(false);
    expect_rejection(
        &pki_with(&client_only, &Profile::ca("Test Intermediate")),
        "server.test",
        NOW,
        CertError::WrongUsage,
    );

    let mut critical = Profile::leaf("server.test", NAMES);
    critical.unknown_critical = true;
    expect_rejection(
        &pki_with(&critical, &Profile::ca("Test Intermediate")),
        "server.test",
        NOW,
        CertError::UnknownCriticalExtension,
    );

    let mut ca_leaf = Profile::leaf("server.test", NAMES);
    ca_leaf.ca = Some(None);
    expect_rejection(
        &pki_with(&ca_leaf, &Profile::ca("Test Intermediate")),
        "server.test",
        NOW,
        CertError::CaUsedAsLeaf,
    );

    let mut not_ca = Profile::leaf("Test Intermediate", &[]);
    not_ca.server_auth = None;
    expect_rejection(
        &pki_with(&Profile::leaf("server.test", NAMES), &not_ca),
        "server.test",
        NOW,
        CertError::NotACa,
    );

    let mut constrained = Profile::ca("Test Intermediate");
    constrained.permitted_dns = &["elsewhere.test"];
    expect_rejection(
        &pki_with(&Profile::leaf("server.test", NAMES), &constrained),
        "server.test",
        NOW,
        CertError::NameConstraintViolation,
    );
}

#[test]
fn a_root_the_chain_does_not_reach_is_refused() {
    let pki = pki();
    let mut trust = TrustStore::new();
    let other_root = Key::from_seed(b"other root");
    let foreign = testpki::issue(
        &Profile::ca("Test Root"),
        &other_root,
        "Test Root",
        &other_root,
        9,
    );
    trust.add_der(&foreign).expect("parses");
    let suites = CipherSuite::ALL;
    let mut server = Server::new(server_cfg(&pki, &suites));
    let mut client = Client::new(
        client_cfg("server.test", &trust, &suites),
        &mut counter_rng(5),
    )
    .expect("client");
    assert_eq!(
        pump(&mut client, &mut server, 1 << 20),
        Err(Error::Certificate(CertError::BadSignature)),
        "a root with the right name and the wrong key does not verify"
    );

    let empty = TrustStore::new();
    let mut server = Server::new(server_cfg(&pki, &suites));
    let mut client = Client::new(
        client_cfg("server.test", &empty, &suites),
        &mut counter_rng(5),
    )
    .expect("client");
    assert_eq!(
        pump(&mut client, &mut server, 1 << 20),
        Err(Error::Certificate(CertError::UnknownIssuer))
    );
}

#[test]
fn a_tampered_record_is_refused() {
    let pki = pki();
    let trust = trust(&pki);
    let suites = CipherSuite::ALL;
    let mut server = Server::new(server_cfg(&pki, &suites));
    let mut client = Client::new(
        client_cfg("server.test", &trust, &suites),
        &mut counter_rng(6),
    )
    .expect("client");
    pump(&mut client, &mut server, 1 << 20).expect("handshake");
    server.write(b"hello").expect("write");
    let mut wire = server.take_output();
    let last = wire.len() - 1;
    wire[last] ^= 0x40;
    assert_eq!(
        client.read_tls(&wire).map_err(|e| e.alert()),
        Err(Some(Alert::BadRecordMac))
    );
    assert_eq!(
        client.write(b"more"),
        Err(client.read_tls(&[]).unwrap_err())
    );
}

#[test]
fn key_updates_in_both_directions() {
    let pki = pki();
    let trust = trust(&pki);
    let suites = CipherSuite::ALL;
    let mut server = Server::new(server_cfg(&pki, &suites));
    let mut client = Client::new(
        client_cfg("server.test", &trust, &suites),
        &mut counter_rng(7),
    )
    .expect("client");
    pump(&mut client, &mut server, 1 << 20).expect("handshake");
    server.key_update(true);
    pump(&mut client, &mut server, 1 << 20).expect("update");
    exchange(&mut client, &mut server, 1000);
    server.key_update(false);
    exchange(&mut client, &mut server, 1000);
}

#[test]
fn alpn_and_certificate_request() {
    let pki = pki();
    let trust = trust(&pki);
    let suites = CipherSuite::ALL;
    let mut cfg = server_cfg(&pki, &suites);
    cfg.alpn = &[b"http/1.1"];
    cfg.request_certificate = true;
    let mut server = Server::new(cfg);
    let mut ccfg = client_cfg("server.test", &trust, &suites);
    ccfg.alpn = &[b"h2", b"http/1.1"];
    let mut client = Client::new(ccfg, &mut counter_rng(8)).expect("client");
    pump(&mut client, &mut server, 1 << 20).expect("handshake");
    assert_eq!(client.alpn_protocol(), Some(&b"http/1.1"[..]));
    exchange(&mut client, &mut server, 10);
}

#[test]
fn close_notify_ends_the_stream() {
    let pki = pki();
    let trust = trust(&pki);
    let suites = CipherSuite::ALL;
    let mut server = Server::new(server_cfg(&pki, &suites));
    let mut client = Client::new(
        client_cfg("server.test", &trust, &suites),
        &mut counter_rng(9),
    )
    .expect("client");
    pump(&mut client, &mut server, 1 << 20).expect("handshake");
    server.write(b"last words").expect("write");
    server.close();
    pump(&mut client, &mut server, 1 << 20).expect("close");
    let mut buf = [0u8; 32];
    let n = client.read(&mut buf);
    assert_eq!(&buf[..n], b"last words");
    assert!(client.peer_closed());
}

#[test]
fn server_speaking_tls12_is_refused() {
    for legacy_version in [0x0303, 0x0000] {
        let pki = pki();
        let trust = trust(&pki);
        let suites = CipherSuite::ALL;
        let mut client = Client::new(
            client_cfg("server.test", &trust, &suites),
            &mut counter_rng(10),
        )
        .expect("client");
        let hello = client.take_output();
        let session_id = hello[5 + 4 + 2 + 32 + 1..][..32].to_vec();
        let reply = crate::conn::handshake_message(crate::conn::HS_SERVER_HELLO, |m| {
            crate::conn::put_u16(m, legacy_version);
            m.extend_from_slice(&[7u8; 32]);
            crate::conn::vec8(m, |m| m.extend_from_slice(&session_id));
            crate::conn::put_u16(m, 0x1301);
            m.push(0);
            crate::conn::vec16(m, |_| {});
        });
        let mut record = alloc::vec![22, 3, 3, 0, reply.len() as u8];
        record.extend(reply);
        assert_eq!(
            client.read_tls(&record).map_err(|e| e.alert()),
            Err(Some(Alert::ProtocolVersion)),
            "legacy version {legacy_version:#06x}"
        );
    }
}

#[test]
fn an_unprotected_record_past_the_plaintext_limit_is_refused() {
    let pki = pki();
    let trust = trust(&pki);
    let suites = CipherSuite::ALL;
    let mut client = Client::new(
        client_cfg("server.test", &trust, &suites),
        &mut counter_rng(12),
    )
    .expect("client");
    let len = crate::conn::MAX_FRAGMENT as u16 + 1;
    let [hi, lo] = len.to_be_bytes();
    assert_eq!(
        client.read_tls(&[22, 3, 3, hi, lo]).map_err(|e| e.alert()),
        Err(Some(Alert::RecordOverflow))
    );
}

#[test]
fn a_compatibility_change_cipher_spec_is_dropped() {
    let pki = pki();
    let trust = trust(&pki);
    let suites = CipherSuite::ALL;
    let mut server = Server::new(server_cfg(&pki, &suites));
    let mut client = Client::new(
        client_cfg("server.test", &trust, &suites),
        &mut counter_rng(11),
    )
    .expect("client");
    client
        .read_tls(&[20, 3, 3, 0, 1, 1])
        .expect("a CCS before the ServerHello is dropped");
    pump(&mut client, &mut server, 1 << 20).expect("handshake");
    assert!(client.is_connected());
    assert!(
        client.read_tls(&[20, 3, 3, 0, 1, 1]).is_err(),
        "but not once the handshake is over"
    );
}
