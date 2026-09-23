mod chain;
mod handshake;
mod interop;
mod vectors;

use alloc::vec::Vec;

use crate::hash::{Hash, Hmac, Sha256, Sha384, Sha512, hkdf_expand, hkdf_extract};

pub(crate) fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

fn hex(b: &[u8]) -> alloc::string::String {
    use core::fmt::Write;
    let mut s = alloc::string::String::new();
    for x in b {
        write!(s, "{x:02x}").expect("string");
    }
    s
}

#[test]
fn sha2_known_answers() {
    assert_eq!(
        hex(Sha256::digest(b"abc").as_ref()),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    for row in vectors::HASHES {
        let m = unhex(row[0]);
        assert_eq!(
            hex(Sha256::digest(&m).as_ref()),
            row[1],
            "sha256 of {} bytes",
            m.len()
        );
        assert_eq!(
            hex(Sha384::digest(&m).as_ref()),
            row[2],
            "sha384 of {} bytes",
            m.len()
        );
        assert_eq!(
            hex(Sha512::digest(&m).as_ref()),
            row[3],
            "sha512 of {} bytes",
            m.len()
        );
    }
}

#[test]
fn sha2_incremental_matches_one_shot() {
    let data: Vec<u8> = (0..1000u32).map(|i| (i * 7 + 3) as u8).collect();
    for split in [0, 1, 63, 64, 65, 127, 128, 129, 500] {
        let mut h = Sha512::new();
        h.update(&data[..split]);
        h.update(&data[split..]);
        assert_eq!(h.finish(), Sha512::digest(&data), "split at {split}");
        let mut h = Sha256::new();
        h.update(&data[..split]);
        h.update(&data[split..]);
        assert_eq!(h.finish(), Sha256::digest(&data), "split at {split}");
    }
}

#[test]
fn hmac_and_hkdf() {
    for row in vectors::HMACS {
        let (k, m) = (unhex(row[0]), unhex(row[1]));
        assert_eq!(hex(Hmac::<Sha256>::mac(&k, &m).as_ref()), row[2]);
        assert_eq!(hex(Hmac::<Sha384>::mac(&k, &m).as_ref()), row[3]);
    }
    fn check<H: Hash>(rows: &[&[&str]]) {
        for row in rows {
            let (salt, ikm, info) = (unhex(row[0]), unhex(row[1]), unhex(row[2]));
            let len: usize = row[3].parse().expect("len");
            let prk = hkdf_extract::<H>(&salt, &ikm);
            let mut okm = alloc::vec![0u8; len];
            hkdf_expand::<H>(prk.as_ref(), &[&info], &mut okm);
            assert_eq!(hex(&okm), row[4]);
        }
    }
    check::<Sha256>(vectors::HKDF_SHA256);
    check::<Sha384>(vectors::HKDF_SHA384);
}

/// The S-box from its definition: inversion in GF(2^8), then the affine map.
fn reference_sbox(x: u8) -> u8 {
    fn gmul(mut a: u8, mut b: u8) -> u8 {
        let mut p = 0u8;
        while b != 0 {
            if b & 1 != 0 {
                p ^= a;
            }
            let hi = a & 0x80;
            a <<= 1;
            if hi != 0 {
                a ^= 0x1b;
            }
            b >>= 1;
        }
        p
    }
    let inv = if x == 0 {
        0
    } else {
        (1..=255u8).find(|&y| gmul(x, y) == 1).expect("inverse")
    };
    inv ^ inv.rotate_left(1) ^ inv.rotate_left(2) ^ inv.rotate_left(3) ^ inv.rotate_left(4) ^ 0x63
}

#[test]
fn aes_sbox_circuit_matches_definition() {
    for x in 0..=255u8 {
        assert_eq!(crate::aes::sbox(x), reference_sbox(x), "S({x:#04x})");
    }
}

#[test]
fn aes_known_answers() {
    let mut block = [0u8; 16];
    crate::aes::Aes::new(&[0u8; 16])
        .expect("key")
        .encrypt_block(&mut block);
    assert_eq!(hex(&block), "66e94bd4ef8a2c3b884cfa59ca342b2e");
    for row in vectors::AES_ECB {
        let aes = crate::aes::Aes::new(&unhex(row[0])).expect("key");
        let mut block: [u8; 16] = unhex(row[1]).try_into().expect("block");
        aes.encrypt_block(&mut block);
        assert_eq!(hex(&block), row[2]);
    }
}

#[test]
fn clmul_matches_schoolbook() {
    fn naive(x: u64, y: u64) -> u128 {
        (0..64)
            .filter(|i| (y >> i) & 1 == 1)
            .fold(0u128, |acc, i| acc ^ ((x as u128) << i))
    }
    let mut s = 0x0123_4567_89ab_cdefu64;
    for _ in 0..500 {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let x = s;
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let y = s;
        assert_eq!(crate::gcm::clmul64_for_test(x, y), naive(x, y));
    }
    assert_eq!(
        crate::gcm::clmul64_for_test(u64::MAX, u64::MAX),
        naive(u64::MAX, u64::MAX)
    );
}

#[test]
fn gf128_multiplicative_identity() {
    let one = 1u128 << 127;
    let x = 0x66e9_4bd4_ef8a_2c3b_884c_fa59_ca34_2b2e_u128;
    assert_eq!(crate::gcm::gf_mul_for_test(x, one), x);
    assert_eq!(crate::gcm::gf_mul_for_test(one, x), x);
}

fn aead_rows(
    rows: &[&[&str]],
    seal: impl Fn(&[u8], &[u8; 12], &[u8], &mut [u8]) -> [u8; 16],
    open: impl Fn(&[u8], &[u8; 12], &[u8], &mut [u8], &[u8]) -> bool,
) {
    for row in rows {
        let (k, n, a, p) = (unhex(row[0]), unhex(row[1]), unhex(row[2]), unhex(row[3]));
        let n: [u8; 12] = n.try_into().expect("nonce");
        let mut data = p.clone();
        let tag = seal(&k, &n, &a, &mut data);
        data.extend_from_slice(&tag);
        assert_eq!(hex(&data), row[4], "seal of {} bytes", p.len());

        let (ct, tag) = data.split_at_mut(p.len());
        assert!(open(&k, &n, &a, ct, tag), "open of {} bytes", p.len());
        assert_eq!(ct, &p[..]);

        let mut ct = unhex(row[4]);
        let len = ct.len();
        ct[len - 1] ^= 1;
        let (body, tag) = ct.split_at_mut(len - 16);
        let before = body.to_vec();
        assert!(!open(&k, &n, &a, body, tag), "a flipped tag bit is refused");
        assert_eq!(body, &before[..], "a refused record is not decrypted");
    }
}

#[test]
fn aes_gcm_known_answers() {
    use crate::gcm::AesGcm;
    aead_rows(
        vectors::AES_GCM,
        |k, n, a, d| AesGcm::new(k).expect("key").seal(n, a, d),
        |k, n, a, d, t| AesGcm::new(k).expect("key").open(n, a, d, t),
    );
}

#[test]
fn chacha20_poly1305_known_answers() {
    use crate::chacha::ChaCha20Poly1305;
    for row in vectors::POLY1305 {
        let k: [u8; 32] = unhex(row[0]).try_into().expect("key");
        assert_eq!(
            hex(&crate::chacha::poly1305_for_test(&k, &unhex(row[1]))),
            row[2]
        );
    }
    aead_rows(
        vectors::CHACHA20_POLY1305,
        |k, n, a, d| ChaCha20Poly1305::new(k).expect("key").seal(n, a, d),
        |k, n, a, d, t| ChaCha20Poly1305::new(k).expect("key").open(n, a, d, t),
    );
}

#[test]
fn x25519_known_answers() {
    let rfc = crate::x25519::x25519(
        &unhex("a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4")
            .try_into()
            .expect("scalar"),
        &unhex("e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c")
            .try_into()
            .expect("u"),
    );
    assert_eq!(
        hex(&rfc),
        "c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552"
    );
    for row in vectors::X25519 {
        let k: [u8; 32] = unhex(row[0]).try_into().expect("scalar");
        let u: [u8; 32] = unhex(row[1]).try_into().expect("u");
        assert_eq!(hex(&crate::x25519::x25519(&k, &u)), row[2]);
    }
    assert_eq!(
        crate::x25519::shared_secret(&[7u8; 32], &[0u8; 32]),
        None,
        "a small-order point is refused"
    );
}

fn hash_alg(name: &str) -> crate::rsa::HashAlg {
    match name {
        "sha256" => crate::rsa::HashAlg::Sha256,
        "sha384" => crate::rsa::HashAlg::Sha384,
        _ => crate::rsa::HashAlg::Sha512,
    }
}

#[test]
fn ecdsa_verifies_and_refuses() {
    use crate::ec::Curve;
    for row in vectors::ECDSA {
        let curve = match row[0] {
            "P256" => Curve::P256,
            "P384" => Curve::P384,
            _ => Curve::P521,
        };
        let digest = hash_alg(row[2]).digest(&unhex(row[3]));
        let ok = crate::ec::verify(
            curve,
            &unhex(row[1]),
            digest.as_ref(),
            &unhex(row[4]),
            &unhex(row[5]),
        );
        assert_eq!(ok, row[6] == "1", "{} {} valid={}", row[0], row[2], row[6]);
    }
}

#[test]
fn rsa_verifies_and_refuses() {
    for row in vectors::RSA {
        let (n, e) = (unhex(row[0]), unhex(row[1]));
        let key = crate::rsa::PublicKey {
            modulus: &n,
            exponent: &e,
        };
        let (m, sig) = (unhex(row[4]), unhex(row[5]));
        let alg = hash_alg(row[3]);
        let ok = if row[2] == "pkcs1" {
            key.verify_pkcs1(alg, &m, &sig)
        } else {
            key.verify_pss(alg, &m, &sig)
        };
        assert_eq!(
            ok,
            row[6] == "1",
            "{}-bit {} {} valid={}",
            8 * n.len(),
            row[2],
            row[3],
            row[6]
        );
    }
}

#[test]
fn scalar_multiplication_of_the_generator() {
    use crate::ec::Curve;
    for curve in [Curve::P256, Curve::P384, Curve::P521] {
        let g = curve.group();
        let k = g.n.limbs();
        let one = crate::bignum::from_be(&[1], k).expect("one");
        let encoded_g = g.encode_point(&g.mul_g(&one)).expect("finite");
        assert!(g.decode_point(&encoded_g).is_some(), "G is on the curve");

        let mut minus_one = g.n.value().to_vec();
        minus_one[0] -= 1;
        let neg = g.encode_point(&g.mul_g(&minus_one)).expect("finite");
        let len = curve.scalar_bytes();
        assert_eq!(
            neg[1..1 + len],
            encoded_g[1..1 + len],
            "(n-1)G shares G's x"
        );
        assert_ne!(neg[1 + len..], encoded_g[1 + len..], "and has the other y");

        let mut n = g.n.value().to_vec();
        let order = g.mul_g(&n);
        assert!(
            g.encode_point(&order).is_none(),
            "nG is the point at infinity"
        );
        n[0] += 1;
        assert_eq!(
            g.encode_point(&g.mul_g(&n)).expect("finite"),
            encoded_g,
            "(n+1)G = G"
        );
    }
}

/// A bundle update adding a key type the verifier lacks fails here, not on a
/// user's first connection to a server under that root.
#[test]
fn every_shipped_root_is_usable() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../assets/certs/ca-certificates.crt");
    let pem = std::fs::read(&path).expect("the shipped bundle");
    let blocks = crate::pem::certificates(&pem).len();
    let mut store = crate::x509::TrustStore::new();
    assert_eq!(
        store.add_pem_bundle(&pem),
        0,
        "unusable roots in {}",
        path.display()
    );
    assert_eq!(store.len(), blocks);
    assert!(blocks > 100, "{blocks} roots is not a Web PKI bundle");
}

#[test]
fn x25519_refuses_every_low_order_point() {
    const LOW_ORDER: [&str; 7] = [
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0100000000000000000000000000000000000000000000000000000000000000",
        "e0eb7a7c3b41b8ae1656e3faf19fc46ada098deb9c32b1fd866205165f49b800",
        "5f9c95bca3508c24b1d0b1559c83ef5b04445cc4581c8e86d8224eddd09f1157",
        "ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
        "edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
        "eeffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
    ];
    for point in LOW_ORDER {
        let u: [u8; 32] = unhex(point).try_into().expect("u");
        for scalar in [[7u8; 32], [0xa5; 32]] {
            assert_eq!(crate::x25519::shared_secret(&scalar, &u), None, "{point}");
        }
    }
}

#[test]
fn ecdsa_refuses_r_and_s_outside_the_group_order() {
    use crate::ec::Curve;
    let row = vectors::ECDSA
        .iter()
        .find(|row| row[0] == "P256" && row[6] == "1")
        .expect("a valid P-256 row");
    let digest = hash_alg(row[2]).digest(&unhex(row[3]));
    let (key, r, s) = (unhex(row[1]), unhex(row[4]), unhex(row[5]));
    let verify = |r: &[u8], s: &[u8]| crate::ec::verify(Curve::P256, &key, digest.as_ref(), r, s);
    assert!(verify(&r, &s));
    let order = unhex("ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551");
    for (what, r, s) in [
        ("r = 0", &[0u8][..], &s[..]),
        ("s = 0", &r[..], &[0u8][..]),
        ("r = n", &order[..], &s[..]),
        ("s = n", &r[..], &order[..]),
    ] {
        assert!(!verify(r, s), "{what}");
    }
}

#[test]
fn rsa_moduli_out_of_range_are_refused_before_any_arithmetic() {
    let e = [1, 0, 1];
    for bytes in [128usize, 64 * 1024] {
        let n = alloc::vec![0xffu8; bytes];
        let sig = alloc::vec![1u8; bytes];
        let key = crate::rsa::PublicKey {
            modulus: &n,
            exponent: &e,
        };
        let start = std::time::Instant::now();
        assert!(!key.verify_pkcs1(crate::rsa::HashAlg::Sha256, b"m", &sig));
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "a {bytes}-byte modulus was refused only after {:?}",
            start.elapsed()
        );
    }
}

#[test]
fn aeads_refuse_any_tampered_byte() {
    use crate::chacha::ChaCha20Poly1305;
    use crate::gcm::AesGcm;
    let (key, nonce, aad) = ([3u8; 32], [9u8; 12], *b"header");
    let plain = *b"the quick brown fox jumps over the lazy dog";
    let gcm = AesGcm::new(&key).expect("key");
    let chacha = ChaCha20Poly1305::new(&key).expect("key");
    type Seal<'a> = &'a dyn Fn(&[u8], &mut [u8]) -> [u8; 16];
    type Open<'a> = &'a dyn Fn(&[u8], &mut [u8], &[u8]) -> bool;
    let aeads: [(&str, Seal<'_>, Open<'_>); 2] = [
        ("AES-GCM", &|a, d| gcm.seal(&nonce, a, d), &|a, d, t| {
            gcm.open(&nonce, a, d, t)
        }),
        (
            "ChaCha20-Poly1305",
            &|a, d| chacha.seal(&nonce, a, d),
            &|a, d, t| chacha.open(&nonce, a, d, t),
        ),
    ];
    for (name, seal, open) in aeads {
        let mut sealed = plain;
        let tag = seal(&aad, &mut sealed);
        for i in 0..sealed.len() {
            let mut data = sealed;
            data[i] ^= 1;
            assert!(!open(&aad, &mut data, &tag), "{name}: ciphertext byte {i}");
        }
        for i in 0..aad.len() {
            let mut bad = aad;
            bad[i] ^= 1;
            assert!(
                !open(&bad, &mut sealed.clone(), &tag),
                "{name}: AAD byte {i}"
            );
        }
        assert!(
            !open(&aad, &mut sealed.clone(), &tag[..15]),
            "{name}: short tag"
        );
        let mut data = sealed;
        assert!(open(&aad, &mut data, &tag));
        assert_eq!(data, plain);
    }
}

#[test]
fn padding_is_stripped_back_to_the_content_type() {
    use crate::suite::{CipherSuite, Protection};
    let secret = Sha256::digest(b"record secret");
    let cases: [(&[u8], (u8, usize)); 4] = [
        (b"hi\x17", (23, 2)),
        (b"hi\x17\0\0\0", (23, 2)),
        (b"a\0b\x16\0", (22, 3)),
        (b"\0\0\0", (0, 0)),
    ];
    for (payload, expected) in cases {
        let mut sealer = Protection::new(CipherSuite::Aes128GcmSha256, secret.clone());
        let mut opener = Protection::new(CipherSuite::Aes128GcmSha256, secret.clone());
        let mut record = Vec::new();
        sealer.seal(0, payload, &mut record);
        let (header, body) = record.split_at_mut(5);
        let header: [u8; 5] = header.try_into().expect("header");
        assert_eq!(opener.open(&header, body), Some(expected), "{payload:?}");
    }
}
