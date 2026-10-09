//! NCM's transfer blocks, 16-bit form only: the NTB parameters a device
//! reports (NCM 1.0 §6.2.1), and the NTH16/NDP16 framing (§3.2.1, §3.3.1).

use super::ETHERNET_HEADER;

pub const NTH_LEN: usize = 12;
pub const MIN_IN_SIZE: u32 = 2048;
/// What a host asks a device's NTBs to stay within.
pub const IN_SIZE: u32 = 16384;

const NTH_SIGNATURE: [u8; 4] = *b"NCMH";
const NDP_SIGNATURE: [u8; 4] = *b"NCM0";
const NDP_HEADER: usize = 8;
const NDP_MIN_LEN: usize = 16;
const NTB16_MAX: usize = u16::MAX as usize;
const FORMAT_NTB16: u16 = 1 << 0;
const FORMAT_NTB32: u16 = 1 << 1;
const MAX_NDPS: usize = 8;
const MAX_DATAGRAMS: usize = 64;

fn word(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn dword(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// The response to GET_NTB_PARAMETERS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Parameters {
    /// `bmNtbFormatsSupported`.
    pub formats: u16,
    /// `dwNtbInMaxSize`.
    pub in_max: u32,
    pub out: Out,
    /// `wNtbOutMaxDatagrams`; 0 is no limit.
    pub out_max_datagrams: u16,
}

impl Parameters {
    pub const LEN: usize = 28;

    /// `None` if short, `wLength` is under 28, NTB-16 is not offered, or
    /// `dwNtbOutMaxSize` is under [`MIN_IN_SIZE`].
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::LEN || usize::from(word(bytes, 0)) < Self::LEN {
            return None;
        }
        let formats = word(bytes, 2);
        if formats & FORMAT_NTB16 == 0 {
            return None;
        }
        Some(Self {
            formats,
            in_max: dword(bytes, 4),
            out: Out::new(
                dword(bytes, 16),
                word(bytes, 20),
                word(bytes, 22),
                word(bytes, 24),
            )?,
            out_max_datagrams: word(bytes, 26),
        })
    }

    /// The device also offers NTB-32, so the host must send SET_NTB_FORMAT.
    pub fn ntb32(&self) -> bool {
        self.formats & FORMAT_NTB32 != 0
    }

    /// The NTB IN size to hold the device to: [`IN_SIZE`], or `in_max` when
    /// smaller; `None` when `in_max` is under [`MIN_IN_SIZE`].
    pub fn in_size(&self) -> Option<u32> {
        (self.in_max >= MIN_IN_SIZE).then(|| self.in_max.min(IN_SIZE))
    }
}

/// How an OUT NTB is laid out, sanitised: `max` is `dwNtbOutMaxSize` capped
/// to what NTB-16's `wBlockLength` holds, and at least [`MIN_IN_SIZE`] or
/// the parameters are refused; a zero divisor is taken as 4 and the
/// remainder modulo the divisor; an alignment that is not a power of two of
/// at least 4 is taken as 4.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Out {
    pub max: u32,
    divisor: u16,
    remainder: u16,
    alignment: u16,
}

impl Out {
    fn new(max: u32, divisor: u16, remainder: u16, alignment: u16) -> Option<Self> {
        if max < MIN_IN_SIZE {
            return None;
        }
        let divisor = if divisor == 0 { 4 } else { divisor };
        let alignment = if alignment.is_power_of_two() && alignment >= 4 {
            alignment
        } else {
            4
        };
        Some(Self {
            max: max.min(NTB16_MAX as u32),
            divisor,
            remainder: remainder % divisor,
            alignment,
        })
    }
}

/// One frame as an NTB-16: NTH16 at 0, the NDP16 right after it (aligned to
/// `alignment`), the datagram at the first offset at or past the NDP's end
/// with `offset % divisor == remainder`. `None` when it does not fit `into`
/// or `out.max`, or the frame is empty. Returns the NTB's length
/// (`wBlockLength`).
pub fn write(frame: &[u8], sequence: u16, out: &Out, into: &mut [u8]) -> Option<usize> {
    if frame.is_empty() {
        return None;
    }
    let ndp = NTH_LEN.next_multiple_of(usize::from(out.alignment));
    let ndp_end = ndp + NDP_MIN_LEN;
    let divisor = usize::from(out.divisor);
    let datagram = ndp_end + (usize::from(out.remainder) + divisor - ndp_end % divisor) % divisor;
    let end = datagram + frame.len();
    if end > into.len() || end > out.max as usize {
        return None;
    }
    let ntb = &mut into[..end];
    ntb.fill(0);
    ntb[0..4].copy_from_slice(&NTH_SIGNATURE);
    ntb[4..6].copy_from_slice(&(NTH_LEN as u16).to_le_bytes());
    ntb[6..8].copy_from_slice(&sequence.to_le_bytes());
    ntb[8..10].copy_from_slice(&(end as u16).to_le_bytes());
    ntb[10..12].copy_from_slice(&(ndp as u16).to_le_bytes());
    ntb[ndp..ndp + 4].copy_from_slice(&NDP_SIGNATURE);
    ntb[ndp + 4..ndp + 6].copy_from_slice(&(NDP_MIN_LEN as u16).to_le_bytes());
    ntb[ndp + 8..ndp + 10].copy_from_slice(&(datagram as u16).to_le_bytes());
    ntb[ndp + 10..ndp + 12].copy_from_slice(&(frame.len() as u16).to_le_bytes());
    ntb[datagram..].copy_from_slice(frame);
    Some(end)
}

/// The datagrams of a received NTB-16, each a slice of `ntb`. An NTB whose
/// NTH16 is wrong (signature, `wHeaderLength` not 12, `wBlockLength` past
/// `ntb` or inside the header) yields nothing; parsing stays within
/// `wBlockLength` (or `ntb` if it is 0). NDPs are followed through
/// `wNextNdpIndex`, at most 8, and a malformed one (signature, index under
/// 12 or unaligned, `wLength` under 16 or unaligned, past the block) ends
/// the walk; a (0, 0) pair or the NDP's end ends that NDP. A datagram shorter
/// than an Ethernet header or outside the block is skipped, and at most 64
/// are yielded.
pub fn datagrams(ntb: &[u8]) -> Datagrams<'_> {
    let mut walk = Datagrams {
        block: &[],
        ndp: 0,
        ndp_len: 0,
        pair: 0,
        ndps: 0,
        yielded: 0,
    };
    if ntb.len() < NTH_LEN || ntb[0..4] != NTH_SIGNATURE || usize::from(word(ntb, 4)) != NTH_LEN {
        return walk;
    }
    let block = match usize::from(word(ntb, 8)) {
        0 => ntb.len(),
        len if len < NTH_LEN || len > ntb.len() => return walk,
        len => len,
    };
    walk.block = &ntb[..block];
    walk.enter(usize::from(word(ntb, 10)));
    walk
}

pub struct Datagrams<'a> {
    block: &'a [u8],
    /// The current NDP's offset; 0 once the walk is over.
    ndp: usize,
    ndp_len: usize,
    pair: usize,
    ndps: usize,
    yielded: usize,
}

impl Datagrams<'_> {
    fn enter(&mut self, at: usize) {
        self.ndp = 0;
        if self.ndps == MAX_NDPS
            || at < NTH_LEN
            || !at.is_multiple_of(4)
            || at + NDP_HEADER > self.block.len()
            || self.block[at..at + 4] != NDP_SIGNATURE
        {
            return;
        }
        let len = usize::from(word(self.block, at + 4));
        if len < NDP_MIN_LEN || !len.is_multiple_of(4) || at + len > self.block.len() {
            return;
        }
        self.ndps += 1;
        self.ndp = at;
        self.ndp_len = len;
        self.pair = 0;
    }
}

impl<'a> Iterator for Datagrams<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        while self.ndp != 0 && self.yielded < MAX_DATAGRAMS {
            let at = self.ndp + NDP_HEADER + 4 * self.pair;
            let (index, len) = if at + 4 <= self.ndp + self.ndp_len {
                (
                    usize::from(word(self.block, at)),
                    usize::from(word(self.block, at + 2)),
                )
            } else {
                (0, 0)
            };
            if (index, len) == (0, 0) {
                let next = usize::from(word(self.block, self.ndp + 6));
                self.enter(next);
                continue;
            }
            self.pair += 1;
            if index >= NTH_LEN && len >= ETHERNET_HEADER && index + len <= self.block.len() {
                self.yielded += 1;
                return Some(&self.block[index..index + len]);
            }
        }
        None
    }
}
