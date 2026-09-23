//! AES encryption (FIPS 197) without secret-indexed table lookups: SubBytes
//! runs the S-box circuit of Boyar and Peralta ("A new combinational logic
//! minimization technique with applications to cryptology", 2010) over the
//! bit planes of up to 64 bytes. Encryption only; GCM never decrypts a block.

const RCON: [u8; 10] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36];

/// Transpose an 8×8 bit matrix whose row `r` is byte `r` and column `c` is
/// bit `c`.
fn transpose8(mut x: u64) -> u64 {
    let t = (x ^ (x >> 7)) & 0x00AA_00AA_00AA_00AA;
    x ^= t ^ (t << 7);
    let t = (x ^ (x >> 14)) & 0x0000_CCCC_0000_CCCC;
    x ^= t ^ (t << 14);
    let t = (x ^ (x >> 28)) & 0x0000_0000_F0F0_F0F0;
    x ^= t ^ (t << 28);
    x
}

/// Plane `b` holds bit `b` of every byte, byte `j` at bit `j`.
fn to_planes(bytes: &[u8; 64]) -> [u64; 8] {
    let mut planes = [0u64; 8];
    for (group, chunk) in bytes.chunks_exact(8).enumerate() {
        let t = transpose8(u64::from_le_bytes(chunk.try_into().expect("8 bytes")));
        for (b, plane) in planes.iter_mut().enumerate() {
            *plane |= ((t >> (8 * b)) & 0xff) << (8 * group);
        }
    }
    planes
}

fn from_planes(planes: &[u64; 8], bytes: &mut [u8; 64]) {
    for (group, chunk) in bytes.chunks_exact_mut(8).enumerate() {
        let mut t = 0u64;
        for (b, plane) in planes.iter().enumerate() {
            t |= ((plane >> (8 * group)) & 0xff) << (8 * b);
        }
        chunk.copy_from_slice(&transpose8(t).to_le_bytes());
    }
}

/// The S-box over bit planes; `x0` is the most significant bit.
#[allow(clippy::many_single_char_names)]
fn sbox_planes(q: &mut [u64; 8]) {
    let (x0, x1, x2, x3, x4, x5, x6, x7) = (q[7], q[6], q[5], q[4], q[3], q[2], q[1], q[0]);

    let y14 = x3 ^ x5;
    let y13 = x0 ^ x6;
    let y9 = x0 ^ x3;
    let y8 = x0 ^ x5;
    let t0 = x1 ^ x2;
    let y1 = t0 ^ x7;
    let y4 = y1 ^ x3;
    let y12 = y13 ^ y14;
    let y2 = y1 ^ x0;
    let y5 = y1 ^ x6;
    let y3 = y5 ^ y8;
    let t1 = x4 ^ y12;
    let y15 = t1 ^ x5;
    let y20 = t1 ^ x1;
    let y6 = y15 ^ x7;
    let y10 = y15 ^ t0;
    let y11 = y20 ^ y9;
    let y7 = x7 ^ y11;
    let y17 = y10 ^ y11;
    let y19 = y10 ^ y8;
    let y16 = t0 ^ y11;
    let y21 = y13 ^ y16;
    let y18 = x0 ^ y16;

    let t2 = y12 & y15;
    let t3 = y3 & y6;
    let t4 = t3 ^ t2;
    let t5 = y4 & x7;
    let t6 = t5 ^ t2;
    let t7 = y13 & y16;
    let t8 = y5 & y1;
    let t9 = t8 ^ t7;
    let t10 = y2 & y7;
    let t11 = t10 ^ t7;
    let t12 = y9 & y11;
    let t13 = y14 & y17;
    let t14 = t13 ^ t12;
    let t15 = y8 & y10;
    let t16 = t15 ^ t12;
    let t17 = t4 ^ t14;
    let t18 = t6 ^ t16;
    let t19 = t9 ^ t14;
    let t20 = t11 ^ t16;
    let t21 = t17 ^ y20;
    let t22 = t18 ^ y19;
    let t23 = t19 ^ y21;
    let t24 = t20 ^ y18;

    let t25 = t21 ^ t22;
    let t26 = t21 & t23;
    let t27 = t24 ^ t26;
    let t28 = t25 & t27;
    let t29 = t28 ^ t22;
    let t30 = t23 ^ t24;
    let t31 = t22 ^ t26;
    let t32 = t31 & t30;
    let t33 = t32 ^ t24;
    let t34 = t23 ^ t33;
    let t35 = t27 ^ t33;
    let t36 = t24 & t35;
    let t37 = t36 ^ t34;
    let t38 = t27 ^ t36;
    let t39 = t29 & t38;
    let t40 = t25 ^ t39;

    let t41 = t40 ^ t37;
    let t42 = t29 ^ t33;
    let t43 = t29 ^ t40;
    let t44 = t33 ^ t37;
    let t45 = t42 ^ t41;
    let z0 = t44 & y15;
    let z1 = t37 & y6;
    let z2 = t33 & x7;
    let z3 = t43 & y16;
    let z4 = t40 & y1;
    let z5 = t29 & y7;
    let z6 = t42 & y11;
    let z7 = t45 & y17;
    let z8 = t41 & y10;
    let z9 = t44 & y12;
    let z10 = t37 & y3;
    let z11 = t33 & y4;
    let z12 = t43 & y13;
    let z13 = t40 & y5;
    let z14 = t29 & y2;
    let z15 = t42 & y9;
    let z16 = t45 & y14;
    let z17 = t41 & y8;

    let t46 = z15 ^ z16;
    let t47 = z10 ^ z11;
    let t48 = z5 ^ z13;
    let t49 = z9 ^ z10;
    let t50 = z2 ^ z12;
    let t51 = z2 ^ z5;
    let t52 = z7 ^ z8;
    let t53 = z0 ^ z3;
    let t54 = z6 ^ z7;
    let t55 = z16 ^ z17;
    let t56 = z12 ^ t48;
    let t57 = t50 ^ t53;
    let t58 = z4 ^ t46;
    let t59 = z3 ^ t54;
    let t60 = t46 ^ t57;
    let t61 = z14 ^ t57;
    let t62 = t52 ^ t58;
    let t63 = t49 ^ t58;
    let t64 = z4 ^ t59;
    let t65 = t61 ^ t62;
    let t66 = z1 ^ t63;
    let s0 = t59 ^ t63;
    let s6 = t56 ^ !t62;
    let s7 = t48 ^ !t60;
    let t67 = t64 ^ t65;
    let s3 = t53 ^ t66;
    let s4 = t51 ^ t66;
    let s5 = t47 ^ t65;
    let s1 = t64 ^ !s3;
    let s2 = t55 ^ !t67;

    *q = [s7, s6, s5, s4, s3, s2, s1, s0];
}

fn sub_bytes(bytes: &mut [u8; 64]) {
    let mut planes = to_planes(bytes);
    sbox_planes(&mut planes);
    from_planes(&planes, bytes);
}

#[cfg(test)]
pub(crate) fn sbox(x: u8) -> u8 {
    let mut bytes = [0u8; 64];
    bytes[0] = x;
    sub_bytes(&mut bytes);
    bytes[0]
}

fn shift_rows(block: &mut [u8]) {
    let old: [u8; 16] = block.try_into().expect("16 bytes");
    for c in 0..4 {
        for r in 0..4 {
            block[4 * c + r] = old[4 * ((c + r) % 4) + r];
        }
    }
}

fn mix_columns(block: &mut [u8]) {
    for col in block.chunks_exact_mut(4) {
        let w = u32::from_le_bytes(col.try_into().expect("4 bytes"));
        let r1 = w.rotate_right(8);
        let d = w ^ r1;
        let xtime = ((d & 0x7f7f_7f7f) << 1) ^ (((d >> 7) & 0x0101_0101) * 0x1b);
        let out = xtime ^ r1 ^ w.rotate_right(16) ^ w.rotate_right(24);
        col.copy_from_slice(&out.to_le_bytes());
    }
}

/// An expanded AES-128 or AES-256 key.
#[derive(Clone)]
pub struct Aes {
    round_keys: [[u8; 16]; 15],
    rounds: usize,
}

impl Drop for Aes {
    fn drop(&mut self) {
        crate::ct::wipe(self.round_keys.as_flattened_mut());
    }
}

pub const BLOCK: usize = 16;
/// Blocks enciphered per S-box pass.
pub const LANES: usize = 4;

impl Aes {
    /// `None` unless `key` is 16 or 32 bytes.
    pub fn new(key: &[u8]) -> Option<Self> {
        let nk = match key.len() {
            16 => 4,
            32 => 8,
            _ => return None,
        };
        let rounds = nk + 6;
        let mut w = [[0u8; 4]; 60];
        let mut lanes = [0u8; 64];
        for (i, word) in key.chunks_exact(4).enumerate() {
            w[i].copy_from_slice(word);
        }
        for i in nk..4 * (rounds + 1) {
            let mut t = w[i - 1];
            if i % nk == 0 || (nk > 6 && i % nk == 4) {
                lanes[..4].copy_from_slice(&t);
                sub_bytes(&mut lanes);
                t.copy_from_slice(&lanes[..4]);
                if i % nk == 0 {
                    t.rotate_left(1);
                    t[0] ^= RCON[i / nk - 1];
                }
            }
            for (b, prev) in t.iter_mut().zip(w[i - nk]) {
                *b ^= prev;
            }
            w[i] = t;
        }
        let mut round_keys = [[0u8; 16]; 15];
        for (r, rk) in round_keys.iter_mut().enumerate().take(rounds + 1) {
            for c in 0..4 {
                rk[4 * c..4 * c + 4].copy_from_slice(&w[4 * r + c]);
            }
        }
        crate::ct::wipe(&mut w);
        crate::ct::wipe(&mut lanes);
        Some(Self { round_keys, rounds })
    }

    pub fn encrypt_lanes(&self, blocks: &mut [u8; BLOCK * LANES]) {
        let add_key = |blocks: &mut [u8; 64], rk: &[u8; 16]| {
            for block in blocks.chunks_exact_mut(BLOCK) {
                for (b, k) in block.iter_mut().zip(rk) {
                    *b ^= k;
                }
            }
        };
        add_key(blocks, &self.round_keys[0]);
        for round in 1..=self.rounds {
            sub_bytes(blocks);
            for block in blocks.chunks_exact_mut(BLOCK) {
                shift_rows(block);
                if round != self.rounds {
                    mix_columns(block);
                }
            }
            add_key(blocks, &self.round_keys[round]);
        }
    }

    pub fn encrypt_block(&self, block: &mut [u8; BLOCK]) {
        let mut lanes = [0u8; BLOCK * LANES];
        lanes[..BLOCK].copy_from_slice(block);
        self.encrypt_lanes(&mut lanes);
        block.copy_from_slice(&lanes[..BLOCK]);
    }
}
