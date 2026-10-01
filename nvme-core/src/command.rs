//! Submission queue entries (Base Specification 2.0, §4.2) and the admin and
//! NVM command set opcodes this driver issues.

pub const SQE_BYTES: usize = 64;

pub mod admin {
    pub const DELETE_IO_SQ: u8 = 0x00;
    pub const CREATE_IO_SQ: u8 = 0x01;
    pub const DELETE_IO_CQ: u8 = 0x04;
    pub const CREATE_IO_CQ: u8 = 0x05;
    pub const IDENTIFY: u8 = 0x06;
    pub const ABORT: u8 = 0x08;
    pub const SET_FEATURES: u8 = 0x09;
}

pub mod nvm {
    pub const FLUSH: u8 = 0x00;
    pub const WRITE: u8 = 0x01;
    pub const READ: u8 = 0x02;
}

pub mod feature {
    pub const VOLATILE_WRITE_CACHE: u8 = 0x06;
    pub const NUMBER_OF_QUEUES: u8 = 0x07;
    pub const HOST_MEMORY_BUFFER: u8 = 0x0D;
}

/// Identify's Controller or Namespace Structure selector.
pub mod cns {
    pub const NAMESPACE: u8 = 0x00;
    pub const CONTROLLER: u8 = 0x01;
    pub const ACTIVE_NAMESPACES: u8 = 0x02;
}

/// One submission queue entry, as the sixteen dwords the controller reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Command {
    pub dw: [u32; 16],
}

impl Command {
    fn new(opcode: u8, nsid: u32) -> Self {
        let mut dw = [0; 16];
        dw[0] = u32::from(opcode);
        dw[1] = nsid;
        Self { dw }
    }

    pub fn opcode(&self) -> u8 {
        self.dw[0] as u8
    }

    pub fn cid(&self) -> u16 {
        (self.dw[0] >> 16) as u16
    }

    pub fn with_cid(mut self, cid: u16) -> Self {
        self.dw[0] = (self.dw[0] & 0xFFFF) | (u32::from(cid) << 16);
        self
    }

    /// PRP entries: the first data page, and the second page or the list of
    /// the rest.
    pub fn with_prp(mut self, prp1: u64, prp2: u64) -> Self {
        self.dw[6] = prp1 as u32;
        self.dw[7] = (prp1 >> 32) as u32;
        self.dw[8] = prp2 as u32;
        self.dw[9] = (prp2 >> 32) as u32;
        self
    }

    pub fn identify(selector: u8, nsid: u32) -> Self {
        let mut cmd = Self::new(admin::IDENTIFY, nsid);
        cmd.dw[10] = u32::from(selector);
        cmd
    }

    /// A physically contiguous I/O completion queue of `entries`, raising
    /// MSI-X entry `vector`, or no interrupt at all when `None`.
    pub fn create_io_cq(qid: u16, entries: u16, vector: Option<u16>) -> Self {
        let mut cmd = Self::new(admin::CREATE_IO_CQ, 0);
        cmd.dw[10] = u32::from(qid) | (u32::from(entries - 1) << 16);
        cmd.dw[11] = match vector {
            Some(v) => 0b11 | (u32::from(v) << 16),
            None => 0b01,
        };
        cmd
    }

    /// A physically contiguous I/O submission queue of `entries` completing
    /// into `cqid`.
    pub fn create_io_sq(qid: u16, entries: u16, cqid: u16) -> Self {
        let mut cmd = Self::new(admin::CREATE_IO_SQ, 0);
        cmd.dw[10] = u32::from(qid) | (u32::from(entries - 1) << 16);
        cmd.dw[11] = 0b01 | (u32::from(cqid) << 16);
        cmd
    }

    pub fn delete_io_sq(qid: u16) -> Self {
        let mut cmd = Self::new(admin::DELETE_IO_SQ, 0);
        cmd.dw[10] = u32::from(qid);
        cmd
    }

    pub fn delete_io_cq(qid: u16) -> Self {
        let mut cmd = Self::new(admin::DELETE_IO_CQ, 0);
        cmd.dw[10] = u32::from(qid);
        cmd
    }

    pub fn abort(sqid: u16, cid: u16) -> Self {
        let mut cmd = Self::new(admin::ABORT, 0);
        cmd.dw[10] = u32::from(sqid) | (u32::from(cid) << 16);
        cmd
    }

    fn set_features(fid: u8, value: u32) -> Self {
        let mut cmd = Self::new(admin::SET_FEATURES, 0);
        cmd.dw[10] = u32::from(fid);
        cmd.dw[11] = value;
        cmd
    }

    /// Ask for `sq` submission and `cq` completion queues beside the admin
    /// pair. The completion's result says how many were allocated.
    pub fn set_queue_count(sq: u16, cq: u16) -> Self {
        Self::set_features(
            feature::NUMBER_OF_QUEUES,
            u32::from(sq - 1) | (u32::from(cq - 1) << 16),
        )
    }

    /// Hand the controller `pages` pages of host memory described by the
    /// `entries` descriptors at `list` (16-byte aligned).
    pub fn enable_host_memory(pages: u32, list: u64, entries: u32) -> Self {
        let mut cmd = Self::set_features(feature::HOST_MEMORY_BUFFER, 1);
        cmd.dw[12] = pages;
        cmd.dw[13] = list as u32;
        cmd.dw[14] = (list >> 32) as u32;
        cmd.dw[15] = entries;
        cmd
    }

    /// Take the host memory buffer back. The memory stays the controller's
    /// until this completes.
    pub fn disable_host_memory() -> Self {
        Self::set_features(feature::HOST_MEMORY_BUFFER, 0)
    }

    fn transfer(opcode: u8, nsid: u32, lba: u64, blocks: u16) -> Self {
        let mut cmd = Self::new(opcode, nsid);
        cmd.dw[10] = lba as u32;
        cmd.dw[11] = (lba >> 32) as u32;
        cmd.dw[12] = u32::from(blocks - 1);
        cmd
    }

    pub fn read(nsid: u32, lba: u64, blocks: u16) -> Self {
        Self::transfer(nvm::READ, nsid, lba, blocks)
    }

    pub fn write(nsid: u32, lba: u64, blocks: u16) -> Self {
        Self::transfer(nvm::WRITE, nsid, lba, blocks)
    }

    pub fn flush(nsid: u32) -> Self {
        Self::new(nvm::FLUSH, nsid)
    }

    pub fn to_bytes(&self) -> [u8; SQE_BYTES] {
        let mut out = [0u8; SQE_BYTES];
        for (chunk, dw) in out.chunks_exact_mut(4).zip(self.dw) {
            chunk.copy_from_slice(&dw.to_le_bytes());
        }
        out
    }
}

/// The PRP entries for a transfer through `pages`, each [`crate::regs::PAGE_SIZE`]
/// and page-aligned: PRP1 is the first page; PRP2 is the second page, or
/// `list` when a third is needed, whose entries [`prp_list`] writes.
pub fn prp_pair(pages: &[u64], list: u64) -> (u64, u64) {
    match pages {
        [] => (0, 0),
        [only] => (*only, 0),
        [first, second] => (*first, *second),
        [first, ..] => (*first, list),
    }
}

/// The list page's entries for `pages`, when [`prp_pair`] points PRP2 at it:
/// every page but the first, in order. A single list page holds 512.
pub fn prp_list(pages: &[u64]) -> &[u64] {
    if pages.len() > 2 { &pages[1..] } else { &[] }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_a_read() {
        let cmd = Command::read(1, 0x1_0000_0002, 8)
            .with_cid(0x1234)
            .with_prp(0xAAAA_0000, 0xBBBB_0000);
        assert_eq!(cmd.dw[0], 0x1234_0002);
        assert_eq!(cmd.dw[1], 1);
        assert_eq!((cmd.dw[10], cmd.dw[11], cmd.dw[12]), (2, 1, 7));
        assert_eq!((cmd.dw[6], cmd.dw[8]), (0xAAAA_0000, 0xBBBB_0000));
        assert_eq!(cmd.cid(), 0x1234);
        assert_eq!(cmd.opcode(), nvm::READ);
        let bytes = cmd.to_bytes();
        assert_eq!(&bytes[..4], &[0x02, 0x00, 0x34, 0x12]);
    }

    #[test]
    fn encodes_queue_creation() {
        let cq = Command::create_io_cq(1, 64, Some(1));
        assert_eq!((cq.dw[10], cq.dw[11]), (0x003f_0001, 0x0001_0003));
        let polled = Command::create_io_cq(2, 16, None);
        assert_eq!(polled.dw[11], 1);
        let sq = Command::create_io_sq(2, 16, 2);
        assert_eq!((sq.dw[10], sq.dw[11]), (0x000f_0002, 0x0002_0001));
        assert_eq!(Command::set_queue_count(2, 2).dw[11], 0x0001_0001);
    }

    #[test]
    fn encodes_host_memory() {
        let cmd = Command::enable_host_memory(0x4000, 0x1_2345_6780, 8);
        assert_eq!(cmd.dw[10], u32::from(feature::HOST_MEMORY_BUFFER));
        assert_eq!(
            (cmd.dw[11], cmd.dw[12], cmd.dw[13], cmd.dw[14], cmd.dw[15]),
            (1, 0x4000, 0x2345_6780, 1, 8)
        );
        assert_eq!(Command::disable_host_memory().dw[11], 0);
    }

    #[test]
    fn prp_entries_follow_the_page_count() {
        assert_eq!(prp_pair(&[0x1000], 0x9000), (0x1000, 0));
        assert_eq!(prp_pair(&[0x1000, 0x5000], 0x9000), (0x1000, 0x5000));
        let three = [0x1000, 0x5000, 0x3000];
        assert_eq!(prp_pair(&three, 0x9000), (0x1000, 0x9000));
        assert_eq!(prp_list(&three), &[0x5000, 0x3000]);
        assert!(prp_list(&three[..2]).is_empty());
    }
}
