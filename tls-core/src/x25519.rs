//! X25519 (RFC 7748): a Montgomery ladder whose swaps are masks rather than
//! branches.

use crate::field25519::Fe;

pub const BASEPOINT: [u8; 32] = {
    let mut b = [0u8; 32];
    b[0] = 9;
    b
};

pub fn x25519(scalar: &[u8; 32], u: &[u8; 32]) -> [u8; 32] {
    let mut k = *scalar;
    k[0] &= 248;
    k[31] &= 127;
    k[31] |= 64;

    let x1 = Fe::from_bytes(u);
    let (mut x2, mut z2) = (Fe::ONE, Fe::ZERO);
    let (mut x3, mut z3) = (x1, Fe::ONE);
    let mut swap = 0u64;
    for t in (0..255).rev() {
        let bit = u64::from((k[t / 8] >> (t % 8)) & 1);
        swap ^= bit;
        Fe::cswap(&mut x2, &mut x3, swap);
        Fe::cswap(&mut z2, &mut z3, swap);
        swap = bit;

        let a = x2.add(z2);
        let aa = a.square();
        let b = x2.sub(z2);
        let bb = b.square();
        let e = aa.sub(bb);
        let c = x3.add(z3);
        let d = x3.sub(z3);
        let da = d.mul(a);
        let cb = c.mul(b);
        x3 = da.add(cb).square();
        z3 = x1.mul(da.sub(cb).square());
        x2 = aa.mul(bb);
        z2 = e.mul(aa.add(e.mul_small(121_665)));
    }
    Fe::cswap(&mut x2, &mut x3, swap);
    Fe::cswap(&mut z2, &mut z3, swap);
    let out = x2.mul(z2.invert()).to_bytes();
    crate::ct::wipe(&mut k);
    for fe in [&mut x2, &mut z2, &mut x3, &mut z3] {
        crate::ct::wipe(&mut fe.0);
    }
    out
}

/// The shared secret, or `None` when it is all zeros: the peer sent a
/// small-order point, and RFC 7748 §6.1 has the caller abort.
pub fn shared_secret(scalar: &[u8; 32], peer: &[u8; 32]) -> Option<[u8; 32]> {
    let s = x25519(scalar, peer);
    let zero = s.iter().fold(0u8, |acc, b| acc | b);
    (core::hint::black_box(zero) != 0).then_some(s)
}
