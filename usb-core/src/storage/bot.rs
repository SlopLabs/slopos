//! Bulk-Only Transport 1.0: the Command Block Wrapper a command goes out in,
//! the Command Status Wrapper it comes back in (§5.1, §5.2, §6.3), and the
//! class requests (§3.1, §3.2).

use crate::device::request::{DIRECTION_IN, RECIPIENT_INTERFACE, Setup, TYPE_CLASS};

pub const CBW_LEN: usize = 31;
pub const CSW_LEN: usize = 13;
const CBW_SIGNATURE: u32 = 0x4342_5355;
const CSW_SIGNATURE: u32 = 0x5342_5355;
/// The highest LUN a device may report.
pub const MAX_LUN: u8 = 15;

const GET_MAX_LUN: u8 = 0xfe;
const MASS_STORAGE_RESET: u8 = 0xff;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    None,
    In,
    Out,
}

/// A command block of 1 to 16 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cdb {
    bytes: [u8; 16],
    len: u8,
}

impl Cdb {
    /// `bytes` past 16 are dropped.
    pub fn new(bytes: &[u8]) -> Self {
        let len = bytes.len().min(16);
        let mut cdb = Self {
            bytes: [0; 16],
            len: len as u8,
        };
        cdb.bytes[..len].copy_from_slice(&bytes[..len]);
        cdb
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    pub fn opcode(&self) -> u8 {
        self.bytes[0]
    }
}

pub fn command_block_wrapper(
    tag: u32,
    length: u32,
    direction: Direction,
    lun: u8,
    cdb: &Cdb,
) -> [u8; CBW_LEN] {
    let mut cbw = [0u8; CBW_LEN];
    cbw[0..4].copy_from_slice(&CBW_SIGNATURE.to_le_bytes());
    cbw[4..8].copy_from_slice(&tag.to_le_bytes());
    cbw[8..12].copy_from_slice(&length.to_le_bytes());
    cbw[12] = if direction == Direction::In { 0x80 } else { 0 };
    cbw[13] = lun & 0x0f;
    cbw[14] = cdb.len;
    cbw[15..15 + usize::from(cdb.len)].copy_from_slice(cdb.bytes());
    cbw
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Passed,
    Failed,
    PhaseError,
}

/// A CSW that is valid and meaningful (§6.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandStatus {
    pub status: Status,
    /// Bytes of the data stage the device did not move.
    pub residue: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadStatus {
    /// Not thirteen bytes, the wrong signature, or another command's tag.
    Invalid,
    /// An unknown status, or a residue past the length asked for.
    Meaningless,
}

/// The CSW in `bytes`, `received` of which arrived, answering the CBW that
/// carried `tag` and asked for `length` bytes.
pub fn command_status(
    bytes: &[u8],
    received: usize,
    tag: u32,
    length: u32,
) -> Result<CommandStatus, BadStatus> {
    if received != CSW_LEN || bytes.len() < CSW_LEN {
        return Err(BadStatus::Invalid);
    }
    let word =
        |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    if word(0) != CSW_SIGNATURE || word(4) != tag {
        return Err(BadStatus::Invalid);
    }
    let residue = word(8);
    let status = match bytes[12] {
        0 => Status::Passed,
        1 => Status::Failed,
        2 => Status::PhaseError,
        _ => return Err(BadStatus::Meaningless),
    };
    if status != Status::PhaseError && residue > length {
        return Err(BadStatus::Meaningless);
    }
    Ok(CommandStatus { status, residue })
}

impl Setup {
    pub fn get_max_lun(interface: u8) -> Self {
        Self {
            request_type: DIRECTION_IN | TYPE_CLASS | RECIPIENT_INTERFACE,
            request: GET_MAX_LUN,
            value: 0,
            index: interface.into(),
            length: 1,
        }
    }

    pub fn mass_storage_reset(interface: u8) -> Self {
        Self {
            request_type: TYPE_CLASS | RECIPIENT_INTERFACE,
            request: MASS_STORAGE_RESET,
            value: 0,
            index: interface.into(),
            length: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn csw(tag: u32, residue: u32, status: u8) -> [u8; CSW_LEN] {
        let mut bytes = [0u8; CSW_LEN];
        bytes[0..4].copy_from_slice(b"USBS");
        bytes[4..8].copy_from_slice(&tag.to_le_bytes());
        bytes[8..12].copy_from_slice(&residue.to_le_bytes());
        bytes[12] = status;
        bytes
    }

    #[test]
    fn a_cbw_carries_its_command_in_wire_order() {
        let cdb = Cdb::new(&[0x28, 0, 0, 0, 0x10, 0, 0, 0, 8, 0]);
        let cbw = command_block_wrapper(0x1234_5678, 4096, Direction::In, 0x13, &cdb);
        assert_eq!(&cbw[0..4], b"USBC");
        assert_eq!(&cbw[4..8], &[0x78, 0x56, 0x34, 0x12]);
        assert_eq!(&cbw[8..12], &[0, 0x10, 0, 0]);
        assert_eq!(cbw[12], 0x80);
        assert_eq!(cbw[13], 3, "the LUN is four bits");
        assert_eq!(cbw[14], 10);
        assert_eq!(&cbw[15..25], cdb.bytes());
        assert!(cbw[25..].iter().all(|&b| b == 0));
        let out = command_block_wrapper(1, 0, Direction::None, 0, &Cdb::new(&[0; 6]));
        assert_eq!(out[12], 0);
        assert_eq!(Cdb::new(&[7; 20]).bytes().len(), 16);
    }

    #[test]
    fn a_csw_is_held_to_its_command() {
        let good = csw(7, 512, 0);
        assert_eq!(
            command_status(&good, CSW_LEN, 7, 4096),
            Ok(CommandStatus {
                status: Status::Passed,
                residue: 512
            })
        );
        assert_eq!(
            command_status(&csw(7, 0, 1), CSW_LEN, 7, 0).map(|s| s.status),
            Ok(Status::Failed)
        );
        assert_eq!(
            command_status(&good, CSW_LEN, 8, 4096),
            Err(BadStatus::Invalid),
            "another command's tag"
        );
        assert_eq!(command_status(&good, 12, 7, 4096), Err(BadStatus::Invalid));
        assert_eq!(
            command_status(&good[..12], 13, 7, 4096),
            Err(BadStatus::Invalid)
        );
        let mut signed = good;
        signed[0] = b'u';
        assert_eq!(
            command_status(&signed, CSW_LEN, 7, 4096),
            Err(BadStatus::Invalid)
        );
        assert_eq!(
            command_status(&csw(7, 4097, 0), CSW_LEN, 7, 4096),
            Err(BadStatus::Meaningless)
        );
        assert_eq!(
            command_status(&csw(7, 0, 3), CSW_LEN, 7, 4096),
            Err(BadStatus::Meaningless)
        );
        assert_eq!(
            command_status(&csw(7, u32::MAX, 2), CSW_LEN, 7, 0).map(|s| s.status),
            Ok(Status::PhaseError),
            "a phase error's residue means nothing"
        );
    }

    #[test]
    fn mutated_csws_never_read_past_their_bytes() {
        let mut seed = 0x9e37_79b9u32;
        for _ in 0..20_000 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let mut bytes = csw(seed, seed >> 3, (seed >> 29) as u8);
            bytes[(seed % CSW_LEN as u32) as usize] ^= (seed >> 8) as u8;
            let len = (seed >> 11) as usize % (CSW_LEN + 1);
            let _ = command_status(&bytes[..len], (seed >> 16) as usize % 20, seed, seed >> 1);
        }
    }

    #[test]
    fn class_requests_name_the_interface() {
        assert_eq!(
            Setup::get_max_lun(2).bytes(),
            [0xa1, 0xfe, 0, 0, 2, 0, 1, 0]
        );
        assert_eq!(
            Setup::mass_storage_reset(2).bytes(),
            [0x21, 0xff, 0, 0, 2, 0, 0, 0]
        );
    }
}
