//! The SCSI commands a Bulk-Only disk is driven with, and what comes back:
//! INQUIRY data, both READ CAPACITY answers, the mode parameter header's
//! write protection and sense data in either format. Layouts follow
//! Seagate's SCSI Commands Reference Manual.

use super::bot::{Cdb, Direction};

const TEST_UNIT_READY: u8 = 0x00;
const REQUEST_SENSE: u8 = 0x03;
const INQUIRY: u8 = 0x12;
const MODE_SENSE_6: u8 = 0x1a;
const READ_CAPACITY_10: u8 = 0x25;
const READ_10: u8 = 0x28;
const WRITE_10: u8 = 0x2a;
const SYNCHRONIZE_CACHE_10: u8 = 0x35;
const READ_16: u8 = 0x88;
const WRITE_16: u8 = 0x8a;
const SERVICE_ACTION_IN_16: u8 = 0x9e;
const READ_CAPACITY_16: u8 = 0x10;

pub const SENSE_LEN: u8 = 18;
pub const INQUIRY_LEN: u8 = 36;
pub const CAPACITY_10_LEN: u32 = 8;
pub const CAPACITY_16_LEN: u32 = 32;
/// All pages, as much as a device sends in one MODE SENSE (6).
pub const MODE_SENSE_LEN: u8 = 192;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Command {
    pub cdb: Cdb,
    pub direction: Direction,
    pub length: u32,
}

impl Command {
    fn new(cdb: &[u8], direction: Direction, length: u32) -> Self {
        Self {
            cdb: Cdb::new(cdb),
            direction,
            length,
        }
    }

    pub fn test_unit_ready() -> Self {
        Self::new(&[TEST_UNIT_READY, 0, 0, 0, 0, 0], Direction::None, 0)
    }

    pub fn request_sense() -> Self {
        Self::new(
            &[REQUEST_SENSE, 0, 0, 0, SENSE_LEN, 0],
            Direction::In,
            SENSE_LEN.into(),
        )
    }

    pub fn inquiry() -> Self {
        Self::new(
            &[INQUIRY, 0, 0, 0, INQUIRY_LEN, 0],
            Direction::In,
            INQUIRY_LEN.into(),
        )
    }

    pub fn read_capacity_10() -> Self {
        Self::new(
            &[READ_CAPACITY_10, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            Direction::In,
            CAPACITY_10_LEN,
        )
    }

    pub fn read_capacity_16() -> Self {
        let mut cdb = [0u8; 16];
        cdb[0] = SERVICE_ACTION_IN_16;
        cdb[1] = READ_CAPACITY_16;
        cdb[10..14].copy_from_slice(&CAPACITY_16_LEN.to_be_bytes());
        Self::new(&cdb, Direction::In, CAPACITY_16_LEN)
    }

    /// Every page, current values.
    pub fn mode_sense() -> Self {
        Self::new(
            &[MODE_SENSE_6, 0, 0x3f, 0, MODE_SENSE_LEN, 0],
            Direction::In,
            MODE_SENSE_LEN.into(),
        )
    }

    /// The whole cache, however long.
    pub fn synchronize_cache() -> Self {
        Self::new(
            &[SYNCHRONIZE_CACHE_10, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            Direction::None,
            0,
        )
    }

    /// READ or WRITE of `blocks` blocks of `block_size` bytes from `lba`: the
    /// ten-byte form while both fit it, else the sixteen-byte one. `None`
    /// when no form carries the count.
    pub fn transfer(write: bool, lba: u64, blocks: u32, block_size: u32) -> Option<Self> {
        let length = blocks.checked_mul(block_size)?;
        let direction = if write { Direction::Out } else { Direction::In };
        if let (Ok(lba), Ok(count)) = (u32::try_from(lba), u16::try_from(blocks)) {
            let opcode = if write { WRITE_10 } else { READ_10 };
            let mut cdb = [0u8; 10];
            cdb[0] = opcode;
            cdb[2..6].copy_from_slice(&lba.to_be_bytes());
            cdb[7..9].copy_from_slice(&count.to_be_bytes());
            return Some(Self::new(&cdb, direction, length));
        }
        let opcode = if write { WRITE_16 } else { READ_16 };
        let mut cdb = [0u8; 16];
        cdb[0] = opcode;
        cdb[2..10].copy_from_slice(&lba.to_be_bytes());
        cdb[10..14].copy_from_slice(&blocks.to_be_bytes());
        Some(Self::new(&cdb, direction, length))
    }

    pub fn is_synchronize_cache(&self) -> bool {
        self.cdb.opcode() == SYNCHRONIZE_CACHE_10
    }
}

pub mod device_type {
    pub const DIRECT_ACCESS: u8 = 0x00;
    pub const SIMPLIFIED_DIRECT: u8 = 0x0e;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inquiry {
    /// The peripheral qualifier says a device is attached at this LUN.
    pub present: bool,
    pub device_type: u8,
}

impl Inquiry {
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let &[peripheral, ..] = bytes else {
            return None;
        };
        Some(Self {
            present: peripheral >> 5 == 0,
            device_type: peripheral & 0x1f,
        })
    }

    pub fn is_disk(&self) -> bool {
        self.present
            && matches!(
                self.device_type,
                device_type::DIRECT_ACCESS | device_type::SIMPLIFIED_DIRECT
            )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capacity {
    pub blocks: u64,
    pub block_size: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapacityAnswer {
    Known(Capacity),
    /// The ten-byte answer's LBA is all ones: READ CAPACITY (16) has it.
    TooLarge,
}

impl Capacity {
    pub fn parse_10(bytes: &[u8]) -> Option<CapacityAnswer> {
        let bytes = bytes.get(..8)?;
        let last = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let block_size = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        if last == u32::MAX {
            return Some(CapacityAnswer::TooLarge);
        }
        Some(CapacityAnswer::Known(Self {
            blocks: u64::from(last) + 1,
            block_size,
        }))
    }

    pub fn parse_16(bytes: &[u8]) -> Option<Self> {
        let bytes = bytes.get(..12)?;
        let mut last = [0u8; 8];
        last.copy_from_slice(&bytes[..8]);
        Some(Self {
            blocks: u64::from_be_bytes(last).checked_add(1)?,
            block_size: u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
        })
    }

    /// Bytes, unless the block size is not one a disk here is read in or the
    /// total does not fit.
    pub fn bytes(&self) -> Option<u64> {
        let usable = matches!(self.block_size, 512 | 1024 | 2048 | 4096);
        usable.then(|| self.blocks.checked_mul(u64::from(self.block_size)))?
    }
}

/// Bit 7 of the device-specific parameter in a MODE SENSE (6) header.
pub fn write_protected(mode_header: &[u8]) -> Option<bool> {
    Some(mode_header.get(2)? & 0x80 != 0)
}

pub mod sense_key {
    pub const NO_SENSE: u8 = 0x0;
    pub const RECOVERED_ERROR: u8 = 0x1;
    pub const NOT_READY: u8 = 0x2;
    pub const MEDIUM_ERROR: u8 = 0x3;
    pub const HARDWARE_ERROR: u8 = 0x4;
    pub const ILLEGAL_REQUEST: u8 = 0x5;
    pub const UNIT_ATTENTION: u8 = 0x6;
    pub const DATA_PROTECT: u8 = 0x7;
    pub const ABORTED_COMMAND: u8 = 0xb;
}

/// Additional sense codes the driver acts on.
pub mod asc {
    /// NOT READY's "medium not present": a card reader's empty slot.
    pub const MEDIUM_NOT_PRESENT: u8 = 0x3a;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sense {
    pub key: u8,
    pub asc: u8,
    pub ascq: u8,
    /// It reports an earlier command's error: the one it answers did not run.
    pub deferred: bool,
}

/// What a failed command's sense says to do with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The command did its work.
    Done,
    /// It may succeed if sent again.
    Retry,
    Fail,
}

impl Sense {
    /// Fixed or descriptor format; `None` for any other response code or
    /// bytes short of the key.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let code = bytes.first()? & 0x7f;
        let deferred = matches!(code, 0x71 | 0x73);
        match code {
            0x70 | 0x71 => Some(Self {
                key: bytes.get(2)? & 0x0f,
                asc: bytes.get(12).copied().unwrap_or(0),
                ascq: bytes.get(13).copied().unwrap_or(0),
                deferred,
            }),
            0x72 | 0x73 => Some(Self {
                key: bytes.get(1)? & 0x0f,
                asc: bytes.get(2).copied().unwrap_or(0),
                ascq: bytes.get(3).copied().unwrap_or(0),
                deferred,
            }),
            _ => None,
        }
    }

    pub fn verdict(&self) -> Verdict {
        if self.deferred {
            return Verdict::Retry;
        }
        match self.key {
            sense_key::RECOVERED_ERROR => Verdict::Done,
            sense_key::NO_SENSE
            | sense_key::NOT_READY
            | sense_key::UNIT_ATTENTION
            | sense_key::ABORTED_COMMAND => Verdict::Retry,
            _ => Verdict::Fail,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;

    #[test]
    fn transfers_take_the_short_form_while_it_fits() {
        let read = Command::transfer(false, 0x0102_0304, 8, 512).unwrap();
        assert_eq!(read.cdb.bytes(), &[0x28, 0, 1, 2, 3, 4, 0, 0, 8, 0]);
        assert_eq!((read.direction, read.length), (Direction::In, 4096));
        let write = Command::transfer(true, 1 << 32, 240, 512).unwrap();
        assert_eq!(
            write.cdb.bytes(),
            &[0x8a, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 240, 0, 0]
        );
        assert_eq!((write.direction, write.length), (Direction::Out, 240 * 512));
        let many = Command::transfer(false, 0, 0x1_0000, 512);
        assert_eq!(many.map(|c| c.cdb.bytes().len()), Some(16));
        assert_eq!(Command::transfer(false, 0, u32::MAX, 4096), None);
    }

    #[test]
    fn fixed_commands_lay_out_as_specified() {
        assert_eq!(Command::test_unit_ready().cdb.bytes(), &[0; 6]);
        assert_eq!(Command::request_sense().cdb.bytes(), &[3, 0, 0, 0, 18, 0]);
        assert_eq!(Command::inquiry().cdb.bytes(), &[0x12, 0, 0, 0, 36, 0]);
        assert_eq!(
            Command::mode_sense().cdb.bytes(),
            &[0x1a, 0, 0x3f, 0, 192, 0]
        );
        let sixteen = Command::read_capacity_16();
        assert_eq!(&sixteen.cdb.bytes()[..2], &[0x9e, 0x10]);
        assert_eq!(&sixteen.cdb.bytes()[10..14], &[0, 0, 0, 32]);
        let sync = Command::synchronize_cache();
        assert!(sync.is_synchronize_cache() && sync.direction == Direction::None);
        assert!(!Command::test_unit_ready().is_synchronize_cache());
    }

    #[test]
    fn answers_parse_or_say_why_not() {
        assert_eq!(
            Capacity::parse_10(&[0, 0, 0x1f, 0xff, 0, 0, 2, 0]),
            Some(CapacityAnswer::Known(Capacity {
                blocks: 0x2000,
                block_size: 512
            }))
        );
        assert_eq!(
            Capacity::parse_10(&[0xff, 0xff, 0xff, 0xff, 0, 0, 2, 0]),
            Some(CapacityAnswer::TooLarge)
        );
        assert_eq!(Capacity::parse_10(&[0; 7]), None);
        let mut sixteen = vec![0u8; 32];
        sixteen[3] = 1;
        sixteen[7] = 0xff;
        sixteen[10] = 0x10;
        let capacity = Capacity::parse_16(&sixteen).unwrap();
        assert_eq!(capacity.blocks, (1 << 32) + 0x100);
        assert_eq!(capacity.bytes(), Some(((1 << 32) + 0x100) * 4096));
        assert_eq!(Capacity::parse_16(&[0xff; 12]), None);
        let odd = Capacity {
            blocks: 8,
            block_size: 520,
        };
        assert_eq!(odd.bytes(), None);

        let inquiry = Inquiry::parse(&[0x00, 0x80]).unwrap();
        assert!(inquiry.is_disk());
        assert!(!Inquiry::parse(&[0x05, 0]).unwrap().is_disk());
        assert!(
            !Inquiry::parse(&[0x60, 0]).unwrap().is_disk(),
            "no device at the LUN"
        );
        assert_eq!(Inquiry::parse(&[]), None);

        assert_eq!(write_protected(&[3, 0, 0x80, 0]), Some(true));
        assert_eq!(write_protected(&[3, 0, 0x10, 0]), Some(false));
        assert_eq!(write_protected(&[3, 0]), None);
    }

    #[test]
    fn sense_decides_whether_to_try_again() {
        let fixed = |key: u8| {
            let mut bytes = [0u8; 18];
            bytes[0] = 0x70;
            bytes[2] = key;
            bytes[12] = 0x3a;
            bytes
        };
        let not_ready = Sense::parse(&fixed(sense_key::NOT_READY)).unwrap();
        assert_eq!((not_ready.asc, not_ready.verdict()), (0x3a, Verdict::Retry));
        for key in [sense_key::UNIT_ATTENTION, sense_key::ABORTED_COMMAND] {
            assert_eq!(Sense::parse(&fixed(key)).unwrap().verdict(), Verdict::Retry);
        }
        for key in [
            sense_key::MEDIUM_ERROR,
            sense_key::DATA_PROTECT,
            sense_key::ILLEGAL_REQUEST,
            sense_key::HARDWARE_ERROR,
        ] {
            assert_eq!(Sense::parse(&fixed(key)).unwrap().verdict(), Verdict::Fail);
        }
        assert_eq!(
            Sense::parse(&fixed(sense_key::RECOVERED_ERROR))
                .unwrap()
                .verdict(),
            Verdict::Done
        );
        let descriptor = Sense::parse(&[0x72, 0x06, 0x29, 0x00]).unwrap();
        assert_eq!(
            descriptor,
            Sense {
                key: sense_key::UNIT_ATTENTION,
                asc: 0x29,
                ascq: 0,
                deferred: false,
            }
        );
        for deferred in [
            [
                0x71,
                0,
                sense_key::MEDIUM_ERROR,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0x11,
                0,
            ],
            [
                0x73,
                sense_key::RECOVERED_ERROR,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
            ],
        ] {
            let sense = Sense::parse(&deferred).unwrap();
            assert!(sense.deferred);
            assert_eq!(sense.verdict(), Verdict::Retry, "{deferred:x?}");
        }
        assert_eq!(Sense::parse(&[0x70, 0]), None);
        assert_eq!(Sense::parse(&[0x7f, 0, 6]), None);
        assert_eq!(Sense::parse(&[]), None);
    }

    #[test]
    fn mutated_answers_never_read_past_their_bytes() {
        let mut seed = 0x2545_f491u32;
        let mut bytes = [0u8; 40];
        for _ in 0..50_000 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            for (i, b) in bytes.iter_mut().enumerate() {
                *b = (seed.rotate_left(i as u32) >> 3) as u8;
            }
            bytes[0] = [0x70, 0x71, 0x72, 0x73, 0xf0, 0][(seed % 6) as usize];
            let len = (seed >> 7) as usize % bytes.len();
            let b = &bytes[..len];
            let _ = Sense::parse(b).map(|s| s.verdict());
            let _ = Capacity::parse_10(b);
            let _ = Capacity::parse_16(b).and_then(|c| c.bytes());
            let _ = Inquiry::parse(b).map(|i| i.is_disk());
            let _ = write_protected(b);
        }
    }
}
