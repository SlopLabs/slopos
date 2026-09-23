//! Montgomery arithmetic modulo an odd number, for signature verification.
//! Not constant-time: every operand on that path is public.

use alloc::vec;
use alloc::vec::Vec;
use core::cmp::Ordering;

pub type Limbs = Vec<u64>;

/// `bytes` as `k` little-endian limbs, or `None` if it does not fit.
pub fn from_be(bytes: &[u8], k: usize) -> Option<Limbs> {
    let bytes = &bytes[bytes.iter().take_while(|&&b| b == 0).count()..];
    if bytes.len() > 8 * k {
        return None;
    }
    let mut out = vec![0u64; k];
    for (i, chunk) in bytes.rchunks(8).enumerate() {
        let mut word = [0u8; 8];
        word[8 - chunk.len()..].copy_from_slice(chunk);
        out[i] = u64::from_be_bytes(word);
    }
    Some(out)
}

/// `a` as exactly `len` big-endian bytes; the caller knows it fits.
pub fn to_be(a: &[u64], len: usize) -> Vec<u8> {
    let mut full = Vec::with_capacity(a.len() * 8);
    for limb in a.iter().rev() {
        full.extend_from_slice(&limb.to_be_bytes());
    }
    let start = full.len().saturating_sub(len);
    let mut out = vec![0u8; len.saturating_sub(full.len())];
    out.extend_from_slice(&full[start..]);
    out
}

/// Every operand below has the modulus's width: callers pass `limbs()`
/// limbs, and a shorter one would be compared or carried at the wrong place.
pub fn cmp(a: &[u64], b: &[u64]) -> Ordering {
    debug_assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().rev().zip(b.iter().rev()) {
        match x.cmp(y) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    Ordering::Equal
}

pub fn is_zero(a: &[u64]) -> bool {
    a.iter().all(|&l| l == 0)
}

pub fn bits(a: &[u64]) -> usize {
    match a.iter().rposition(|&l| l != 0) {
        Some(i) => 64 * i + 64 - a[i].leading_zeros() as usize,
        None => 0,
    }
}

pub fn shr(a: &mut [u64], k: usize) {
    let (words, bits) = (k / 64, k % 64);
    for i in 0..a.len() {
        let lo = a.get(i + words).copied().unwrap_or(0);
        let hi = a.get(i + words + 1).copied().unwrap_or(0);
        a[i] = if bits == 0 {
            lo
        } else {
            (lo >> bits) | (hi << (64 - bits))
        };
    }
}

fn bit(a: &[u64], i: usize) -> bool {
    (a[i / 64] >> (i % 64)) & 1 == 1
}

/// `a += b`, returning the carry out.
fn add_in(a: &mut [u64], b: &[u64]) -> u64 {
    debug_assert_eq!(a.len(), b.len());
    let mut carry = 0u64;
    for (x, y) in a.iter_mut().zip(b) {
        let (s1, c1) = x.overflowing_add(*y);
        let (s2, c2) = s1.overflowing_add(carry);
        *x = s2;
        carry = u64::from(c1) + u64::from(c2);
    }
    carry
}

/// `a -= b`, returning the borrow out.
fn sub_in(a: &mut [u64], b: &[u64]) -> u64 {
    debug_assert_eq!(a.len(), b.len());
    let mut borrow = 0u64;
    for (x, y) in a.iter_mut().zip(b) {
        let (d1, b1) = x.overflowing_sub(*y);
        let (d2, b2) = d1.overflowing_sub(borrow);
        *x = d2;
        borrow = u64::from(b1) + u64::from(b2);
    }
    borrow
}

/// An odd modulus with its Montgomery constants, `R = 2^(64k)`.
#[derive(Clone)]
pub struct Modulus {
    n: Limbs,
    n0inv: u64,
    rr: Limbs,
}

impl Modulus {
    /// `None` unless `n` is odd and at least 3.
    pub fn new(n: Limbs) -> Option<Self> {
        if n.is_empty() || n[0] & 1 == 0 || bits(&n) < 2 {
            return None;
        }
        let mut inv = 1u64;
        for _ in 0..6 {
            inv = inv.wrapping_mul(2u64.wrapping_sub(n[0].wrapping_mul(inv)));
        }
        let k = n.len();
        let mut r = vec![0u64; k];
        r[0] = 1;
        for _ in 0..2 * 64 * k {
            let carry = shl1(&mut r);
            if carry != 0 || cmp(&r, &n) != Ordering::Less {
                sub_in(&mut r, &n);
            }
        }
        Some(Self {
            n0inv: inv.wrapping_neg(),
            rr: r,
            n,
        })
    }

    pub fn from_be(bytes: &[u8]) -> Option<Self> {
        let k = bytes.len().div_ceil(8).max(1);
        Self::new(from_be(bytes, k)?)
    }

    pub fn limbs(&self) -> usize {
        self.n.len()
    }

    pub fn value(&self) -> &[u64] {
        &self.n
    }

    pub fn bits(&self) -> usize {
        bits(&self.n)
    }

    /// `a` reduced below the modulus, for `a < 2n`.
    pub fn reduce_once(&self, a: &[u64]) -> Limbs {
        debug_assert!(a.len() <= self.limbs());
        let mut r = a.to_vec();
        r.resize(self.limbs(), 0);
        if cmp(&r, &self.n) != Ordering::Less {
            sub_in(&mut r, &self.n);
        }
        r
    }

    pub fn is_reduced(&self, a: &[u64]) -> bool {
        a.len() == self.limbs() && cmp(a, &self.n) == Ordering::Less
    }

    pub fn mul(&self, a: &[u64], b: &[u64]) -> Limbs {
        let k = self.limbs();
        debug_assert!(a.len() == k && b.len() == k);
        let n = &self.n;
        let mut t = vec![0u64; k + 2];
        for &bi in b.iter().take(k) {
            let mut carry = 0u128;
            for j in 0..k {
                let v = u128::from(t[j]) + u128::from(a[j]) * u128::from(bi) + carry;
                t[j] = v as u64;
                carry = v >> 64;
            }
            let v = u128::from(t[k]) + carry;
            t[k] = v as u64;
            t[k + 1] = (v >> 64) as u64;

            let m = t[0].wrapping_mul(self.n0inv);
            let mut carry = (u128::from(t[0]) + u128::from(m) * u128::from(n[0])) >> 64;
            for j in 1..k {
                let v = u128::from(t[j]) + u128::from(m) * u128::from(n[j]) + carry;
                t[j - 1] = v as u64;
                carry = v >> 64;
            }
            let v = u128::from(t[k]) + carry;
            t[k - 1] = v as u64;
            t[k] = t[k + 1] + (v >> 64) as u64;
        }
        let mut r = t[..k].to_vec();
        if t[k] != 0 || cmp(&r, n) != Ordering::Less {
            sub_in(&mut r, n);
        }
        r
    }

    pub fn to_mont(&self, a: &[u64]) -> Limbs {
        self.mul(a, &self.rr)
    }

    pub fn from_mont(&self, a: &[u64]) -> Limbs {
        let mut one = vec![0u64; self.limbs()];
        one[0] = 1;
        self.mul(a, &one)
    }

    pub fn one_mont(&self) -> Limbs {
        let mut one = vec![0u64; self.limbs()];
        one[0] = 1;
        self.to_mont(&one)
    }

    pub fn add(&self, a: &[u64], b: &[u64]) -> Limbs {
        let mut r = a.to_vec();
        let carry = add_in(&mut r, b);
        if carry != 0 || cmp(&r, &self.n) != Ordering::Less {
            sub_in(&mut r, &self.n);
        }
        r
    }

    pub fn sub(&self, a: &[u64], b: &[u64]) -> Limbs {
        let mut r = a.to_vec();
        if sub_in(&mut r, b) != 0 {
            add_in(&mut r, &self.n);
        }
        r
    }

    /// `base^exp` with `base` in Montgomery form; the result is too.
    pub fn pow(&self, base: &[u64], exp: &[u64]) -> Limbs {
        let mut acc = self.one_mont();
        for i in (0..bits(exp)).rev() {
            acc = self.mul(&acc, &acc);
            if bit(exp, i) {
                acc = self.mul(&acc, base);
            }
        }
        acc
    }

    /// The inverse of Montgomery-form `a` for a prime modulus, by Fermat.
    pub fn invert_prime(&self, a: &[u64]) -> Limbs {
        let mut e = self.n.clone();
        let mut two = vec![0u64; self.limbs()];
        two[0] = 2;
        sub_in(&mut e, &two);
        self.pow(a, &e)
    }
}

/// `a <<= 1`, returning the bit shifted out.
fn shl1(a: &mut [u64]) -> u64 {
    let mut carry = 0u64;
    for limb in a.iter_mut() {
        let next = *limb >> 63;
        *limb = (*limb << 1) | carry;
        carry = next;
    }
    carry
}
