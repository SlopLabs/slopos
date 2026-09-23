use alloc::string::String;
use alloc::vec::Vec;

use crate::testpki::{self, AltName, Key, Profile};
use crate::x509::{
    CertError, Certificate, ServerName, TrustStore, dns_matches, verify_server_chain,
};

const NOW: i64 = 1_790_000_000;

fn root_key() -> Key {
    Key::from_seed(b"chain root")
}

fn mid_key() -> Key {
    Key::from_seed(b"chain intermediate")
}

fn trusting(root: &Profile<'_>) -> TrustStore {
    let key = root_key();
    let mut trust = TrustStore::new();
    trust
        .add_der(&testpki::issue(root, &key, root.subject, &key, 1))
        .expect("the root parses");
    trust
}

fn verify(chain: &[Vec<u8>], trust: &TrustStore, host: &str) -> Result<(), CertError> {
    let chain: Vec<&[u8]> = chain.iter().map(Vec::as_slice).collect();
    let name = ServerName::parse(host).expect("host");
    verify_server_chain(&chain, trust, name, NOW).map(|_| ())
}

fn chain_under(mid: &Profile<'_>, names: &[AltName<'_>]) -> Vec<Vec<u8>> {
    let leaf_key = Key::from_seed(b"chain leaf");
    let leaf = testpki::issue(
        &Profile::leaf("leaf", names),
        &leaf_key,
        mid.subject,
        &mid_key(),
        3,
    );
    let mid = testpki::issue(mid, &mid_key(), "Chain Root", &root_key(), 2);
    alloc::vec![leaf, mid]
}

#[test]
fn an_excluded_name_is_excluded_in_every_form() {
    let trust = trusting(&Profile::ca("Chain Root"));
    let mut mid = Profile::ca("Chain Mid");
    mid.excluded_dns = &["bank.example.com"];

    let wildcard = chain_under(&mid, &[AltName::Dns("*.example.com")]);
    assert_eq!(
        verify(&wildcard, &trust, "www.example.com"),
        Err(CertError::NameConstraintViolation),
        "a wildcard that can stand for an excluded name is refused"
    );
    let dotted = chain_under(&mid, &[AltName::Dns("bank.example.com.")]);
    assert_eq!(
        verify(&dotted, &trust, "bank.example.com"),
        Err(CertError::NameMismatch),
        "a name with a trailing dot matches no host"
    );
    let upper = chain_under(&mid, &[AltName::Dns("BANK.Example.com")]);
    assert_eq!(
        verify(&upper, &trust, "bank.example.com"),
        Err(CertError::NameConstraintViolation)
    );
    let sibling = chain_under(&mid, &[AltName::Dns("*.shop.example.com")]);
    assert_eq!(verify(&sibling, &trust, "a.shop.example.com"), Ok(()));
}

#[test]
fn a_constraint_or_name_that_does_not_parse_refuses_the_certificate() {
    let trust = trusting(&Profile::ca("Chain Root"));
    for (permitted, excluded) in [
        (&["example.com"][..], &[][..]),
        (&[], &["bank.example.com"]),
    ] {
        let mut mid = Profile::ca("Chain Mid");
        mid.permitted_dns = permitted;
        mid.excluded_dns = excluded;
        mid.leading_subtree = &[0x30, 0x00];
        let der = testpki::issue(&mid, &mid_key(), "Chain Root", &root_key(), 2);
        assert_eq!(Certificate::parse(&der).err(), Some(CertError::Malformed));
        let chain = chain_under(&mid, &[AltName::Dns("bank.example.com")]);
        assert!(verify(&chain, &trust, "bank.example.com").is_err());
    }
    let key = Key::from_seed(b"chain leaf");
    for raw in [&[0x87, 3, 10, 0, 0][..], &[0x82, 9, b'a']] {
        let names = [AltName::Dns("example.com"), AltName::Raw(raw)];
        let der = testpki::issue(
            &Profile::leaf("leaf", &names),
            &key,
            "Chain Mid",
            &mid_key(),
            3,
        );
        assert_eq!(Certificate::parse(&der).err(), Some(CertError::Malformed));
    }
}

#[test]
fn a_leading_dot_permits_only_names_below() {
    let trust = trusting(&Profile::ca("Chain Root"));
    let mut mid = Profile::ca("Chain Mid");
    mid.permitted_dns = &[".example.com"];
    let apex = chain_under(&mid, &[AltName::Dns("example.com")]);
    assert_eq!(
        verify(&apex, &trust, "example.com"),
        Err(CertError::NameConstraintViolation)
    );
    let below = chain_under(&mid, &[AltName::Dns("*.example.com")]);
    assert_eq!(verify(&below, &trust, "www.example.com"), Ok(()));
}

#[test]
fn an_anchor_keeps_its_constraints_whatever_signs_it() {
    let mut root = Profile::ca("Chain Root");
    root.permitted_dns = &["example.com"];
    root.sha1_signature = true;
    let trust = trusting(&root);
    let outside = chain_under(&Profile::ca("Chain Mid"), &[AltName::Dns("server.test")]);
    assert_eq!(
        verify(&outside, &trust, "server.test"),
        Err(CertError::NameConstraintViolation)
    );
    let inside = chain_under(
        &Profile::ca("Chain Mid"),
        &[AltName::Dns("www.example.com")],
    );
    assert_eq!(verify(&inside, &trust, "www.example.com"), Ok(()));
}

#[test]
fn a_dead_end_at_the_depth_limit_is_backed_out_of() {
    let trust = trusting(&Profile::ca("Chain Root"));
    let mid = Profile::ca("Chain Mid");
    let mut chain = chain_under(&mid, &[AltName::Dns("server.test")]);
    let real_mid = chain.pop().expect("intermediate");
    for serial in 10..17 {
        chain.push(testpki::issue(
            &mid,
            &mid_key(),
            "Chain Mid",
            &mid_key(),
            serial,
        ));
    }
    chain.push(real_mid);
    assert_eq!(verify(&chain, &trust, "server.test"), Ok(()));
}

#[test]
fn a_path_past_six_intermediates_is_too_long() {
    let trust = trusting(&Profile::ca("Chain Root"));
    let names: Vec<String> = (0..7).map(|i| alloc::format!("Mid {i}")).collect();
    let keys: Vec<Key> = names.iter().map(|n| Key::from_seed(n.as_bytes())).collect();
    let leaf_key = Key::from_seed(b"chain leaf");
    let sans = [AltName::Dns("server.test")];
    let mut chain = alloc::vec![testpki::issue(
        &Profile::leaf("leaf", &sans),
        &leaf_key,
        &names[0],
        &keys[0],
        3
    )];
    for i in 0..7 {
        let (issuer, issuer_key) = match names.get(i + 1) {
            Some(name) => (name.as_str(), &keys[i + 1]),
            None => ("Chain Root", &root_key()),
        };
        chain.push(testpki::issue(
            &Profile::ca(&names[i]),
            &keys[i],
            issuer,
            issuer_key,
            10 + i as u8,
        ));
    }
    assert_eq!(
        verify(&chain, &trust, "server.test"),
        Err(CertError::ChainTooLong)
    );
}

#[test]
fn a_search_is_bounded_in_signatures() {
    let trust = trusting(&Profile::ca("Chain Root"));
    let mid = Profile::ca("Chain Mid");
    let mut chain = chain_under(&mid, &[AltName::Dns("server.test")]);
    chain.pop();
    for serial in 10..22 {
        chain.push(testpki::issue(
            &mid,
            &mid_key(),
            "Chain Mid",
            &mid_key(),
            serial,
        ));
    }
    assert_eq!(
        verify(&chain, &trust, "server.test"),
        Err(CertError::TooComplex)
    );
}

#[test]
fn a_signature_this_code_does_not_do_is_refused() {
    let trust = trusting(&Profile::ca("Chain Root"));
    let sans = [AltName::Dns("server.test")];
    let mut leaf = Profile::leaf("leaf", &sans);
    leaf.sha1_signature = true;
    let chain = alloc::vec![
        testpki::issue(
            &leaf,
            &Key::from_seed(b"chain leaf"),
            "Chain Mid",
            &mid_key(),
            3
        ),
        testpki::issue(
            &Profile::ca("Chain Mid"),
            &mid_key(),
            "Chain Root",
            &root_key(),
            2
        ),
    ];
    assert_eq!(
        verify(&chain, &trust, "server.test"),
        Err(CertError::UnsupportedAlgorithm)
    );
}

#[test]
fn name_constraint_work_is_bounded() {
    let trust = trusting(&Profile::ca("Chain Root"));
    let excluded: Vec<String> = (0..500).map(|i| alloc::format!("x{i}.test")).collect();
    let excluded: Vec<&str> = excluded.iter().map(String::as_str).collect();
    let mut mid = Profile::ca("Chain Mid");
    mid.excluded_dns = &excluded;
    let sans: Vec<String> = (0..600)
        .map(|i| alloc::format!("n{i}.example.com"))
        .collect();
    let sans: Vec<AltName<'_>> = sans.iter().map(|s| AltName::Dns(s)).collect();
    let chain = chain_under(&mid, &sans);
    assert_eq!(
        verify(&chain, &trust, "n0.example.com"),
        Err(CertError::TooComplex)
    );
}

#[test]
fn server_names_take_one_canonical_form() {
    assert!(matches!(
        ServerName::parse("1.2.3.4"),
        Some(ServerName::Ipv4([1, 2, 3, 4]))
    ));
    assert!(
        ServerName::parse("1.2.3.4.").is_none(),
        "the resolver would look this up as a name"
    );
    assert!(matches!(
        ServerName::parse("example.com."),
        Some(ServerName::Dns("example.com"))
    ));
    assert!(dns_matches(b"example.com", b"example.com"));
    assert!(!dns_matches(b"example.com.", b"example.com"));
}
