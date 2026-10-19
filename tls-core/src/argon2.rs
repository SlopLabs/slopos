//! Argon2id (RFC 9106), version 0x13, its lanes filled one after another.

use alloc::vec::Vec;

use crate::blake2b::{self, Blake2b};

const WORDS: usize = 128;
const BLOCK_BYTES: usize = WORDS * 8;
const SLICES: u32 = 4;
const VERSION: u32 = 0x13;

type Block = [u64; WORDS];

/// The cost of a hash: memory in KiB, passes over it and lanes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Params {
    pub memory_kib: u32,
    pub passes: u32,
    pub lanes: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// A cost, length or input outside what RFC 9106 allows.
    Invalid,
    /// The memory could not be had.
    NoMemory,
}

/// Argon2d and Argon2i are here for RFC 9106's vectors, which hold each of
/// the two ways Argon2id addresses memory apart.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Variant {
    #[cfg(test)]
    D = 0,
    #[cfg(test)]
    I = 1,
    Id = 2,
}

/// Argon2id of `password` under `salt`, with an optional `secret` key and
/// associated `data`, into `tag`.
pub fn argon2id(
    params: Params,
    password: &[u8],
    salt: &[u8],
    secret: &[u8],
    data: &[u8],
    tag: &mut [u8],
) -> Result<(), Error> {
    hash(Variant::Id, params, password, salt, secret, data, tag)
}

fn hash(
    variant: Variant,
    params: Params,
    password: &[u8],
    salt: &[u8],
    secret: &[u8],
    data: &[u8],
    tag: &mut [u8],
) -> Result<(), Error> {
    let Params {
        memory_kib,
        passes,
        lanes,
    } = params;
    let fits = |bytes: &[u8]| u32::try_from(bytes.len()).is_ok();
    if !(1..1 << 24).contains(&lanes)
        || passes == 0
        || u64::from(memory_kib) < 8 * u64::from(lanes)
        || tag.len() < 4
        || salt.len() < 8
        || ![password, salt, secret, data, tag].into_iter().all(fits)
    {
        return Err(Error::Invalid);
    }
    let columns = memory_kib / (SLICES * lanes) * SLICES;
    let blocks = (columns * lanes) as usize;
    let mut memory: Vec<Block> = Vec::new();
    memory
        .try_reserve_exact(blocks)
        .map_err(|_| Error::NoMemory)?;
    memory.resize(blocks, [0; WORDS]);

    let mut seed = [0u8; 72];
    let mut h0 = Blake2b::new(64);
    for word in [
        lanes,
        tag.len() as u32,
        memory_kib,
        passes,
        VERSION,
        variant as u32,
    ] {
        h0.update(&word.to_le_bytes());
    }
    for part in [password, salt, secret, data] {
        h0.update(&(part.len() as u32).to_le_bytes());
        h0.update(part);
    }
    h0.finish(&mut seed[..64]);
    let mut bytes = [0u8; BLOCK_BYTES];
    for lane in 0..lanes {
        for column in 0..2u32 {
            seed[64..68].copy_from_slice(&column.to_le_bytes());
            seed[68..].copy_from_slice(&lane.to_le_bytes());
            h_prime(&seed, &mut bytes);
            memory[(lane * columns + column) as usize] = words(&bytes);
        }
    }
    crate::ct::wipe(&mut seed);

    let fill = Fill {
        variant,
        lanes,
        columns,
        passes,
        blocks: blocks as u64,
    };
    for pass in 0..passes {
        for slice in 0..SLICES {
            for lane in 0..lanes {
                fill.segment(&mut memory, pass, slice, lane);
            }
        }
    }

    let mut last = [0u64; WORDS];
    for lane in 0..lanes {
        let block = &memory[(lane * columns + columns - 1) as usize];
        for (acc, word) in last.iter_mut().zip(block) {
            *acc ^= word;
        }
    }
    for (chunk, word) in bytes.chunks_exact_mut(8).zip(last) {
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    h_prime(&bytes, tag);
    crate::ct::wipe(&mut bytes);
    crate::ct::wipe(&mut last);
    for block in &mut memory {
        crate::ct::wipe(block);
    }
    Ok(())
}

fn words(bytes: &[u8; BLOCK_BYTES]) -> Block {
    let mut block = [0u64; WORDS];
    for (word, chunk) in block.iter_mut().zip(bytes.chunks_exact(8)) {
        *word = u64::from_le_bytes(chunk.try_into().expect("8 bytes"));
    }
    block
}

/// The variable-length hash H' of RFC 9106 §3.3, as long as `out`.
fn h_prime(input: &[u8], out: &mut [u8]) {
    let len = (out.len() as u32).to_le_bytes();
    if out.len() <= blake2b::MAX_OUTPUT {
        blake2b::digest(&[&len, input], out);
        return;
    }
    let whole = out.len().div_ceil(32) - 2;
    let mut v = [0u8; 64];
    blake2b::digest(&[&len, input], &mut v);
    out[..32].copy_from_slice(&v[..32]);
    for i in 1..whole {
        let previous = v;
        blake2b::digest(&[&previous], &mut v);
        out[32 * i..32 * (i + 1)].copy_from_slice(&v[..32]);
    }
    blake2b::digest(&[&v], &mut out[32 * whole..]);
    crate::ct::wipe(&mut v);
}

struct Fill {
    variant: Variant,
    lanes: u32,
    columns: u32,
    passes: u32,
    blocks: u64,
}

impl Fill {
    fn segment(&self, memory: &mut [Block], pass: u32, slice: u32, lane: u32) {
        let length = self.columns / SLICES;
        let independent = match self.variant {
            #[cfg(test)]
            Variant::D => false,
            #[cfg(test)]
            Variant::I => true,
            Variant::Id => pass == 0 && slice < SLICES / 2,
        };
        let mut input = [0u64; WORDS];
        let mut addresses = [0u64; WORDS];
        input[..6].copy_from_slice(&[
            u64::from(pass),
            u64::from(lane),
            u64::from(slice),
            self.blocks,
            u64::from(self.passes),
            self.variant as u64,
        ]);
        let first = if pass == 0 && slice == 0 { 2 } else { 0 };
        if independent && first != 0 {
            next_addresses(&mut input, &mut addresses);
        }
        for index in first..length {
            let column = slice * length + index;
            let at = (lane * self.columns + column) as usize;
            let previous = if column == 0 {
                at + self.columns as usize - 1
            } else {
                at - 1
            };
            let random = if independent {
                if index as usize % WORDS == 0 {
                    next_addresses(&mut input, &mut addresses);
                }
                addresses[index as usize % WORDS]
            } else {
                memory[previous][0]
            };
            let reference = self.reference(random, pass, slice, lane, index);
            let mut r = [0u64; WORDS];
            for (word, (x, y)) in r
                .iter_mut()
                .zip(memory[previous].iter().zip(&memory[reference]))
            {
                *word = x ^ y;
            }
            compress(&r, &mut memory[at], pass > 0);
        }
    }

    /// Where block `index` of the segment takes its second input, from the
    /// pseudo-random word `random` (RFC 9106 §3.4.1.2).
    fn reference(&self, random: u64, pass: u32, slice: u32, lane: u32, index: u32) -> usize {
        let length = self.columns / SLICES;
        let ref_lane = if pass == 0 && slice == 0 {
            lane
        } else {
            ((random >> 32) % u64::from(self.lanes)) as u32
        };
        let behind = u32::from(index == 0);
        let area = match (pass, ref_lane == lane) {
            (0, true) => slice * length + index - 1,
            (0, false) => slice * length - behind,
            (_, true) => self.columns - length + index - 1,
            (_, false) => self.columns - length - behind,
        };
        let j1 = random & 0xffff_ffff;
        let x = (j1 * j1) >> 32;
        let y = (u64::from(area) * x) >> 32;
        let relative = u64::from(area) - 1 - y;
        let start = if pass == 0 || slice == SLICES - 1 {
            0
        } else {
            (slice + 1) * length
        };
        let column = (u64::from(start) + relative) % u64::from(self.columns);
        (ref_lane * self.columns) as usize + column as usize
    }
}

/// The next block of Argon2i addresses: its counter moved on, compressed
/// twice against zero.
fn next_addresses(input: &mut Block, addresses: &mut Block) {
    input[6] += 1;
    let mut once = [0u64; WORDS];
    compress(input, &mut once, false);
    compress(&once, addresses, false);
}

/// G of RFC 9106 §3.5 over `r`, the two inputs already XORed: into `out`,
/// or XORed into it after the first pass.
fn compress(r: &Block, out: &mut Block, xor: bool) {
    let mut q = *r;
    for row in 0..8 {
        let mut v = [0u64; 16];
        v.copy_from_slice(&q[16 * row..16 * row + 16]);
        permute(&mut v);
        q[16 * row..16 * row + 16].copy_from_slice(&v);
    }
    for column in 0..8 {
        let mut v = [0u64; 16];
        for row in 0..8 {
            v[2 * row] = q[16 * row + 2 * column];
            v[2 * row + 1] = q[16 * row + 2 * column + 1];
        }
        permute(&mut v);
        for row in 0..8 {
            q[16 * row + 2 * column] = v[2 * row];
            q[16 * row + 2 * column + 1] = v[2 * row + 1];
        }
    }
    for (out, (q, r)) in out.iter_mut().zip(q.iter().zip(r)) {
        *out = if xor { *out ^ q ^ r } else { q ^ r };
    }
}

fn permute(v: &mut [u64; 16]) {
    gb(v, [0, 4, 8, 12]);
    gb(v, [1, 5, 9, 13]);
    gb(v, [2, 6, 10, 14]);
    gb(v, [3, 7, 11, 15]);
    gb(v, [0, 5, 10, 15]);
    gb(v, [1, 6, 11, 12]);
    gb(v, [2, 7, 8, 13]);
    gb(v, [3, 4, 9, 14]);
}

fn gb(v: &mut [u64; 16], [a, b, c, d]: [usize; 4]) {
    v[a] = blamka(v[a], v[b]);
    v[d] = (v[d] ^ v[a]).rotate_right(32);
    v[c] = blamka(v[c], v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(24);
    v[a] = blamka(v[a], v[b]);
    v[d] = (v[d] ^ v[a]).rotate_right(16);
    v[c] = blamka(v[c], v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(63);
}

/// `x + y + 2·lo(x)·lo(y)`, BLAKE2b's addition hardened by a multiplication.
fn blamka(x: u64, y: u64) -> u64 {
    let product = (x & 0xffff_ffff) * (y & 0xffff_ffff);
    x.wrapping_add(y).wrapping_add(product.wrapping_mul(2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::unhex;

    fn rfc9106(variant: Variant) -> [u8; 32] {
        let mut tag = [0u8; 32];
        let params = Params {
            memory_kib: 32,
            passes: 3,
            lanes: 4,
        };
        hash(
            variant, params, &[1; 32], &[2; 16], &[3; 8], &[4; 12], &mut tag,
        )
        .expect("valid");
        tag
    }

    #[test]
    fn rfc9106_test_vectors() {
        for (variant, want) in [
            (
                Variant::D,
                "512b391b6f1162975371d30919734294f868e3be3984f3c1a13a4db9fabe4acb",
            ),
            (
                Variant::I,
                "c814d9d1dc7f37aa13f0d77f2494bda1c8de6b016dd388d29952a4c4672b6ce8",
            ),
            (
                Variant::Id,
                "0d640df58d78766c08c037a34a8b53c9d01ef0452d75b65eb52520e96b01e659",
            ),
        ] {
            assert_eq!(rfc9106(variant).to_vec(), unhex(want), "{}", variant as u32);
        }
    }

    /// The reference implementation's own tests, which take no secret.
    #[test]
    fn the_reference_implementation_agrees() {
        for (lanes, want) in [
            (
                1,
                "9dfeb910e80bad0311fee20f9c0e2b12c17987b4cac90c2ef54d5b3021c68bfe",
            ),
            (
                2,
                "6d093c501fd5999645e0ea3bf620d7b8be7fd2db59c20d9fff9539da2bf57037",
            ),
        ] {
            let mut tag = [0u8; 32];
            let params = Params {
                memory_kib: 256,
                passes: 2,
                lanes,
            };
            argon2id(params, b"password", b"somesalt", &[], &[], &mut tag).expect("valid");
            assert_eq!(tag.to_vec(), unhex(want), "{lanes} lanes");
        }
    }

    /// argon2-cffi 25.1 on shapes the vectors above leave out: memory short
    /// of a multiple of four lanes, tags too short and too long for one
    /// BLAKE2b digest, and eight lanes.
    #[test]
    fn another_implementation_agrees_on_odd_shapes() {
        let long = [b'x'; 200];
        let cases: [(u32, u32, u32, &[u8], &[u8], &str); 5] = [
            (1, 8, 1, b"", b"saltsalt", "477bbdbc"),
            (
                4,
                100,
                3,
                b"pass",
                b"somesaltsome",
                "ed08af87ba5cf75cf1ff926a127f9b0b44827690ae370b0362a4da8881017d8eb3a4cca79ec39b08",
            ),
            (
                2,
                33,
                1,
                &long,
                &[0; 16],
                "22352e8657d29e0e8a46ed3212a738c23676e94b07a7b702335bad87d7ebde972d7149ebec57e58dddf2976502369e2c68c42b4cb50edb4053d53efa66b98c28ae",
            ),
            (
                3,
                64,
                2,
                b"hunter2",
                b"0123456789abcdef",
                "52bf1c93781b9bd9963961e8d448bc46c7029c4ba43008d5862ad55012bffac352769f4852dbd85a090d5934dc1528d780ccc3a47fafd10e8caefc7d0b34e9db9bfe871734d6e4ed78e4d3515fee43867fe32a87e4caa9303f1aed110956bff08cce358209cc204ca336c848c8d2545664002a640667f51c0f518e4aa44d10c1",
            ),
            (
                1,
                1024,
                8,
                b"p",
                b"saltsaltsalt",
                "87e9acba367c110371070b7dd4aea8c0cb5170be83974747008335505050bada",
            ),
        ];
        for (passes, memory_kib, lanes, password, salt, want) in cases {
            let want = unhex(want);
            let mut tag = alloc::vec![0u8; want.len()];
            let params = Params {
                memory_kib,
                passes,
                lanes,
            };
            argon2id(params, password, salt, &[], &[], &mut tag).expect("valid");
            assert_eq!(tag, want, "{params:?}, a {}-byte tag", want.len());
        }
    }

    #[test]
    fn costs_outside_the_rfc_are_refused() {
        let ok = Params {
            memory_kib: 32,
            passes: 1,
            lanes: 4,
        };
        let mut tag = [0u8; 32];
        assert_eq!(argon2id(ok, b"p", b"saltsalt", &[], &[], &mut tag), Ok(()));
        for params in [
            Params { lanes: 0, ..ok },
            Params { passes: 0, ..ok },
            Params {
                memory_kib: 31,
                ..ok
            },
            Params {
                lanes: 1 << 24,
                memory_kib: u32::MAX,
                ..ok
            },
        ] {
            assert_eq!(
                argon2id(params, b"p", b"saltsalt", &[], &[], &mut tag),
                Err(Error::Invalid),
                "{params:?}"
            );
        }
        assert_eq!(
            argon2id(ok, b"p", b"short", &[], &[], &mut tag),
            Err(Error::Invalid)
        );
        assert_eq!(
            argon2id(ok, b"p", b"saltsalt", &[], &[], &mut tag[..3]),
            Err(Error::Invalid)
        );
    }
}
