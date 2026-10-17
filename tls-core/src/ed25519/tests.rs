use super::*;
use crate::tests::unhex;

/// RFC 8032 §7.1: secret, public key, message, signature.
const RFC: [[&str; 4]; 4] = [
    [
        "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
        "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
        "",
        "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
    ],
    [
        "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb",
        "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
        "72",
        "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
    ],
    [
        "c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7",
        "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025",
        "af82",
        "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a",
    ],
    [
        "833fe62409237b9d62ec77587520911e9a759cec1d19755b7da901b96dca3d42",
        "ec172b93ad5e563bf4932c70e1245034c35467ef2efd4d64ebf819683467e2bf",
        "ddaf35a193617abacc417349ae204131 12e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
        "dc2a4459e7369633a52b1bf277839a00201009a3efbf3ecb69bea2186c26b58909351fc9ac90b3ecfdfbc7c66431e0303dca179c138ac17ad9bef1177331a704",
    ],
];

struct Case {
    seed: [u8; 32],
    public: [u8; 32],
    message: alloc::vec::Vec<u8>,
    signature: [u8; 64],
}

fn cases() -> impl Iterator<Item = Case> {
    RFC.iter().map(|row| Case {
        seed: unhex(row[0]).try_into().expect("seed"),
        public: unhex(row[1]).try_into().expect("public key"),
        message: unhex(&row[2].replace(' ', "")),
        signature: unhex(row[3]).try_into().expect("signature"),
    })
}

#[test]
fn the_curve_constants_are_what_they_say() {
    assert!(d().mul(small(121_666)).equals(small(121_665).neg()));
    assert!(sqrt_m1().square().equals(Fe::ONE.neg()));
    let y = small(4).mul(small(5).invert());
    assert_eq!(y.to_bytes(), {
        let mut b = BASE;
        b[31] &= 0x7f;
        b
    });
}

#[test]
fn reduction_modulo_the_order_takes_away_every_multiple() {
    let mut x = 0x0123_4567_89ab_cdefu64;
    let mut next = || {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        x
    };
    for _ in 0..200 {
        let mut r = [next(), next(), next(), next() >> 4];
        if !below_l(&r) {
            r[3] = 0;
        }
        let k = [next(), next(), next(), next() >> 8];
        let mut wide = [0u64; 8];
        for (i, &ki) in k.iter().enumerate() {
            let mut carry = 0u128;
            for (j, &lj) in L.iter().enumerate() {
                let t = u128::from(wide[i + j]) + u128::from(ki) * u128::from(lj) + carry;
                wide[i + j] = t as u64;
                carry = t >> 64;
            }
            wide[i + 4] = carry as u64;
        }
        let mut carry = 0u128;
        for (i, limb) in wide.iter_mut().enumerate() {
            let t = u128::from(*limb) + u128::from(r.get(i).copied().unwrap_or(0)) + carry;
            *limb = t as u64;
            carry = t >> 64;
        }
        assert_eq!(reduce(wide), r);
    }
    let mut l = [0u64; 8];
    l[..4].copy_from_slice(&L);
    assert_eq!(reduce(l), [0; 4]);
    assert!(!below_l(&L));
}

#[test]
fn rfc_8032_keys_and_signatures() {
    for case in cases() {
        let key = SigningKey::from_seed(&case.seed);
        assert_eq!(key.public(), case.public);
        assert_eq!(key.sign(&case.message), case.signature);
        assert!(verify(&case.public, &case.message, &case.signature));
    }
}

#[test]
fn any_flipped_bit_is_refused() {
    let case = cases().nth(2).expect("TEST 3");
    for byte in 0..64 {
        for bit in [0, 3, 7] {
            let mut signature = case.signature;
            signature[byte] ^= 1 << bit;
            assert!(!verify(&case.public, &case.message, &signature));
        }
    }
    for byte in 0..32 {
        let mut public = case.public;
        public[byte] ^= 1;
        assert!(!verify(&public, &case.message, &case.signature));
    }
    let mut message = case.message.clone();
    message[0] ^= 0x80;
    assert!(!verify(&case.public, &message, &case.signature));
    assert!(!verify(&case.public, b"", &case.signature));
}

/// `S + L` verifies the same equation, so only a check of `S` refuses it.
#[test]
fn a_scalar_past_the_order_is_refused() {
    for case in cases() {
        let mut s: Scalar = limbs(&case.signature[32..]);
        let mut carry = 0u128;
        for (limb, l) in s.iter_mut().zip(L) {
            let t = u128::from(*limb) + u128::from(l) + carry;
            *limb = t as u64;
            carry = t >> 64;
        }
        assert_eq!(carry, 0);
        let mut signature = case.signature;
        for (chunk, limb) in signature[32..].chunks_exact_mut(8).zip(s) {
            chunk.copy_from_slice(&limb.to_le_bytes());
        }
        assert!(!verify(&case.public, &case.message, &signature));
    }
}

#[test]
fn keys_that_are_no_canonical_point_or_of_small_order_are_refused() {
    let case = cases().next().expect("TEST 1");
    let mut identity = [0u8; 32];
    identity[0] = 1;
    let mut past_p = [0xff; 32];
    past_p[31] = 0x7f;
    let mut negative_zero = identity;
    negative_zero[31] |= 0x80;
    let low_order: [u8; 32] =
        unhex("c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac037a")
            .try_into()
            .expect("32 bytes");
    let curve = Curve::new();
    let point = curve.decode(&low_order).expect("a point of order 8");
    assert!(curve.is_small_order(&point));
    for key in [identity, past_p, negative_zero, low_order] {
        assert!(!verify(&key, &case.message, &case.signature));
    }
    let mut p_plus_one = [0u8; 32];
    p_plus_one[0] = 0xee;
    p_plus_one[1..31].fill(0xff);
    p_plus_one[31] = 0x7f;
    assert!(
        curve.decode(&p_plus_one).is_none(),
        "y = p + 1 is y = 1 spelt twice"
    );
}

#[test]
fn signatures_made_here_verify_and_bind_their_message() {
    for seed in [[7u8; 32], [0xa5; 32], [0; 32]] {
        let key = SigningKey::from_seed(&seed);
        for message in [&b""[..], b"one", &[0x5a; 300][..]] {
            let signature = key.sign(message);
            assert!(verify(&key.public(), message, &signature));
            assert!(!verify(&key.public(), b"other", &signature));
        }
    }
}
