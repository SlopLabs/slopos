//! A simulated Bulk-Only disk behind a stick's bulk pipes: the device's side
//! of each wrapper and data stage, SCSI over media held in memory, and the
//! faults a test asks for. Its data toggles are tracked against the host's,
//! which QEMU's `usb-storage` does not model.

use super::devices::SimDevice;
use crate::device::Speed;
use crate::storage::bot::{CBW_LEN, CSW_LEN};
use crate::storage::scsi::sense_key;
use std::vec;
use std::vec::Vec;

#[derive(Clone, Debug)]
pub struct SimLun {
    pub media: Vec<u8>,
    pub block_size: u32,
    pub write_protected: bool,
    /// TEST UNIT READY answers NOT READY this many times.
    pub not_ready: u32,
    /// SYNCHRONIZE CACHE is refused with ILLEGAL REQUEST.
    pub no_cache: bool,
    pub syncs: u32,
}

impl SimLun {
    pub fn new(blocks: usize, block_size: u32) -> Self {
        Self {
            media: vec![0; blocks * block_size as usize],
            block_size,
            write_protected: false,
            not_ready: 0,
            no_cache: false,
            syncs: 0,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct StorageFaults {
    /// The next data stages stall, and their CSW reports failure with this
    /// sense key.
    pub stall_data: u32,
    pub stall_sense: u8,
    pub phase_error: u32,
    /// CSWs with the wrong signature.
    pub invalid_csw: u32,
    /// CBWs whose bulk-OUT stalls.
    pub stall_cbw: u32,
    /// CSW reads that stall once each.
    pub stall_csw: u32,
    pub zero_length_csw: u32,
    /// Commands the device takes and never answers until it is reset.
    pub silent: u32,
    /// Commands answered only once the simulated clock reaches this.
    pub busy_until_us: u64,
    pub stall_reset: bool,
    pub stall_max_lun: bool,
    /// The next commands fail with this sense key.
    pub fail_with: Option<(u32, u8)>,
    /// The next READ sends this many bytes and reports the rest as residue.
    pub short_read: Option<usize>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Wrapper,
    DataIn {
        data: Vec<u8>,
    },
    DataOut {
        lba: u64,
        lun: u8,
        expected: u32,
    },
    Status,
    /// An invalid CBW stalls both pipes until Reset Recovery.
    Stalled,
    /// Holds the command until the device is reset.
    Silent,
}

#[derive(Clone, Debug, Default)]
pub struct SimStorage {
    pub luns: Vec<SimLun>,
    pub faults: StorageFaults,
    /// Opcodes in the order commands arrived.
    pub commands: Vec<u8>,
    pub resets: u32,
    state: State,
    tag: u32,
    length: u32,
    residue: u32,
    status: u8,
    sense: [u8; 18],
    /// The next data stage stalls, its CSW then carrying this status.
    stall_next_data: Option<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Bulk {
    /// OUT: every byte taken; IN: these bytes sent.
    Moved(Vec<u8>),
    Stall,
    /// Nothing yet: the TD waits.
    Nak,
}

const IN_DCI: u8 = 3;
const OUT_DCI: u8 = 4;

impl SimStorage {
    fn fixed_sense(key: u8, asc: u8) -> [u8; 18] {
        let mut sense = [0u8; 18];
        sense[0] = 0x70;
        sense[2] = key;
        sense[7] = 10;
        sense[12] = asc;
        sense
    }

    pub fn max_lun(&self) -> Option<u8> {
        if self.faults.stall_max_lun {
            return None;
        }
        Some(self.luns.len().saturating_sub(1) as u8)
    }

    /// A Bulk-Only Mass Storage Reset: ready for a CBW, halts and toggles
    /// as they were (§3.1).
    pub fn reset(&mut self) -> bool {
        if self.faults.stall_reset {
            return false;
        }
        self.resets += 1;
        self.state = State::Wrapper;
        true
    }

    pub(super) fn bulk_out(&mut self, bytes: &[u8], halted: &mut u32) -> Bulk {
        match core::mem::take(&mut self.state) {
            State::Wrapper => self.wrapper(bytes, halted),
            State::DataOut { lba, lun, expected } if self.stall_next_data.is_none() => {
                let taken = (bytes.len() as u32).min(expected);
                let disk = &mut self.luns[usize::from(lun)];
                let at = lba as usize * disk.block_size as usize;
                if let Some(target) = disk.media.get_mut(at..at + taken as usize) {
                    target.copy_from_slice(&bytes[..taken as usize]);
                }
                self.residue = expected - taken;
                self.state = State::Status;
                Bulk::Moved(Vec::new())
            }
            State::DataOut { .. } => self.stall_data(OUT_DCI, halted),
            state => {
                self.state = state;
                Bulk::Nak
            }
        }
    }

    pub(super) fn bulk_in(&mut self, room: u32, now_us: u64, halted: &mut u32) -> Bulk {
        if self.state == State::Status && now_us < self.faults.busy_until_us {
            return Bulk::Nak;
        }
        match core::mem::take(&mut self.state) {
            State::DataIn { .. } if self.stall_next_data.is_some() => {
                self.stall_data(IN_DCI, halted)
            }
            State::DataIn { data } => {
                let sent = (data.len() as u32).min(room).min(self.length);
                self.residue = self.length - sent;
                self.state = State::Status;
                Bulk::Moved(data[..sent as usize].to_vec())
            }
            State::Status => self.status(room),
            state => {
                self.state = state;
                Bulk::Nak
            }
        }
    }

    fn stall_data(&mut self, dci: u8, halted: &mut u32) -> Bulk {
        self.status = self.stall_next_data.take().unwrap_or(1);
        *halted |= 1 << dci;
        self.residue = self.length;
        self.state = State::Status;
        Bulk::Stall
    }

    fn status(&mut self, room: u32) -> Bulk {
        if self.faults.stall_csw > 0 {
            self.faults.stall_csw -= 1;
            self.state = State::Status;
            return Bulk::Stall;
        }
        if self.faults.zero_length_csw > 0 {
            self.faults.zero_length_csw -= 1;
            self.state = State::Status;
            return Bulk::Moved(Vec::new());
        }
        let mut csw = [0u8; CSW_LEN];
        csw[0..4].copy_from_slice(b"USBS");
        if self.faults.invalid_csw > 0 {
            self.faults.invalid_csw -= 1;
            csw[0] = b'X';
        }
        csw[4..8].copy_from_slice(&self.tag.to_le_bytes());
        csw[8..12].copy_from_slice(&self.residue.to_le_bytes());
        csw[12] = self.status;
        self.state = State::Wrapper;
        let sent = (CSW_LEN as u32).min(room) as usize;
        Bulk::Moved(csw[..sent].to_vec())
    }

    fn wrapper(&mut self, bytes: &[u8], halted: &mut u32) -> Bulk {
        let valid = bytes.len() == CBW_LEN && bytes[0..4] == *b"USBC";
        if !valid {
            *halted |= 1 << IN_DCI | 1 << OUT_DCI;
            self.state = State::Stalled;
            return Bulk::Stall;
        }
        if self.faults.stall_cbw > 0 {
            self.faults.stall_cbw -= 1;
            *halted |= 1 << OUT_DCI;
            self.state = State::Stalled;
            return Bulk::Stall;
        }
        self.tag = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        self.length = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        let lun = bytes[13] & 0x0f;
        let cdb_len = usize::from(bytes[14] & 0x1f).min(16);
        let mut cdb = [0u8; 16];
        cdb[..cdb_len].copy_from_slice(&bytes[15..15 + cdb_len]);
        self.commands.push(cdb[0]);
        if self.faults.silent > 0 {
            self.faults.silent -= 1;
            self.state = State::Silent;
            return Bulk::Moved(Vec::new());
        }
        let direction_in = bytes[12] & 0x80 != 0;
        self.status = 0;
        self.residue = 0;
        self.stall_next_data = None;
        if self.faults.phase_error > 0 {
            self.faults.phase_error -= 1;
            self.status = 2;
            self.state = State::Status;
            if self.length > 0 {
                self.stall_next_data = Some(2);
                self.state = self.data_state(lun, direction_in);
            }
            return Bulk::Moved(Vec::new());
        }
        if self.faults.stall_data > 0 && self.length > 0 {
            self.faults.stall_data -= 1;
            self.fail(self.faults.stall_sense, 0, lun, direction_in);
            return Bulk::Moved(Vec::new());
        }
        self.execute(lun, &cdb, direction_in);
        Bulk::Moved(Vec::new())
    }

    /// A data stage the next TD meets, whatever it carries.
    fn data_state(&self, lun: u8, direction_in: bool) -> State {
        if direction_in {
            State::DataIn { data: Vec::new() }
        } else {
            State::DataOut {
                lba: 0,
                lun,
                expected: self.length,
            }
        }
    }

    /// A command that fails before its data stage stalls the stage, as
    /// §6.7.2 lets a device do, and its CSW says it failed.
    fn fail(&mut self, key: u8, asc: u8, lun: u8, direction_in: bool) {
        self.status = 1;
        self.residue = self.length;
        self.sense = Self::fixed_sense(key, asc);
        self.state = State::Status;
        if self.length > 0 {
            self.stall_next_data = Some(1);
            self.state = self.data_state(lun, direction_in);
        }
    }

    fn data_in(&mut self, data: Vec<u8>) {
        if self.length == 0 {
            self.state = State::Status;
        } else {
            self.state = State::DataIn { data };
        }
    }

    fn execute(&mut self, lun: u8, cdb: &[u8; 16], direction_in: bool) {
        if let Some((count, key)) = self.faults.fail_with
            && count > 0
            && cdb[0] != 0x03
        {
            self.faults.fail_with = Some((count - 1, key));
            self.fail(key, 0, lun, direction_in);
            return;
        }
        let Some(disk) = self.luns.get_mut(usize::from(lun)) else {
            self.fail(sense_key::ILLEGAL_REQUEST, 0x25, lun, direction_in);
            return;
        };
        let be32 = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        let blocks = (disk.media.len() / disk.block_size as usize) as u64;
        match cdb[0] {
            0x00 if disk.not_ready > 0 => {
                disk.not_ready -= 1;
                self.fail(sense_key::NOT_READY, 0x04, lun, direction_in);
            }
            0x00 => self.state = State::Status,
            0x03 => {
                let sense = self.sense.to_vec();
                self.sense = Self::fixed_sense(sense_key::NO_SENSE, 0);
                self.data_in(sense);
            }
            0x12 => {
                let mut inquiry = vec![0u8; 36];
                inquiry[1] = 0x80;
                inquiry[4] = 31;
                self.data_in(inquiry);
            }
            0x25 => {
                let last = (blocks - 1).min(u64::from(u32::MAX)) as u32;
                let mut answer = last.to_be_bytes().to_vec();
                answer.extend_from_slice(&disk.block_size.to_be_bytes());
                self.data_in(answer);
            }
            0x9e if cdb[1] == 0x10 => {
                let mut answer = (blocks - 1).to_be_bytes().to_vec();
                answer.extend_from_slice(&disk.block_size.to_be_bytes());
                answer.resize(32, 0);
                self.data_in(answer);
            }
            0x1a => {
                let wp = if disk.write_protected { 0x80 } else { 0 };
                self.data_in(vec![3, 0, wp, 0]);
            }
            0x35 if disk.no_cache => {
                self.fail(sense_key::ILLEGAL_REQUEST, 0x20, lun, direction_in);
            }
            0x35 => {
                disk.syncs += 1;
                self.state = State::Status;
            }
            0x28 | 0x88 | 0x2a | 0x8a => {
                let (lba, count) = if cdb[0] & 0x80 != 0 {
                    let mut lba = [0u8; 8];
                    lba.copy_from_slice(&cdb[2..10]);
                    (u64::from_be_bytes(lba), be32(&cdb[10..14]))
                } else {
                    (
                        u64::from(be32(&cdb[2..6])),
                        u32::from(u16::from_be_bytes([cdb[7], cdb[8]])),
                    )
                };
                let write = cdb[0] & 0x02 != 0;
                if write != !direction_in
                    || u64::from(count) * u64::from(disk.block_size) != u64::from(self.length)
                {
                    self.status = 2;
                    self.state = State::Status;
                    return;
                }
                if lba + u64::from(count) > blocks {
                    self.fail(sense_key::ILLEGAL_REQUEST, 0x21, lun, direction_in);
                    return;
                }
                if write && disk.write_protected {
                    self.fail(sense_key::DATA_PROTECT, 0x27, lun, direction_in);
                    return;
                }
                if write {
                    self.state = State::DataOut {
                        lba,
                        lun,
                        expected: self.length,
                    };
                } else {
                    let at = (lba * u64::from(disk.block_size)) as usize;
                    let mut data = disk.media[at..at + self.length as usize].to_vec();
                    if let Some(bytes) = self.faults.short_read.take() {
                        data.truncate(bytes);
                    }
                    self.data_in(data);
                }
            }
            _ => {
                self.fail(sense_key::ILLEGAL_REQUEST, 0x20, lun, direction_in);
            }
        }
    }
}

impl SimDevice {
    /// A Bulk-Only stick with one LUN of `blocks` 512-byte blocks.
    pub fn disk(speed: Speed, blocks: usize) -> Self {
        let mut stick = Self::storage(speed);
        stick.storage = Some(std::boxed::Box::new(SimStorage {
            luns: vec![SimLun::new(blocks, 512)],
            ..SimStorage::default()
        }));
        stick
    }

    pub fn disk_storage(&mut self) -> &mut SimStorage {
        self.storage.as_mut().expect("a disk")
    }
}
