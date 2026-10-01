//! Controller registers in BAR0 (Base Specification 2.0, §3.1.4).

pub const CAP: usize = 0x00;
pub const VS: usize = 0x08;
pub const INTMS: usize = 0x0C;
pub const INTMC: usize = 0x10;
pub const CC: usize = 0x14;
pub const CSTS: usize = 0x1C;
pub const AQA: usize = 0x24;
pub const ASQ: usize = 0x28;
pub const ACQ: usize = 0x30;
pub const CRTO: usize = 0x68;
pub const DOORBELLS: usize = 0x1000;

/// Bytes of BAR0 the driver touches for `queues` queue pairs, the admin pair
/// included.
pub fn register_span(cap: Cap, queues: u16) -> usize {
    DOORBELLS + 2 * usize::from(queues) * cap.doorbell_stride()
}

/// The page size every queue, PRP and host memory buffer is laid out in.
pub const PAGE_SHIFT: u32 = 12;
pub const PAGE_SIZE: usize = 1 << PAGE_SHIFT;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cap(pub u64);

impl Cap {
    fn field(self, shift: u32, bits: u32) -> u64 {
        (self.0 >> shift) & ((1 << bits) - 1)
    }

    /// Entries a queue may hold, admin queues included.
    pub fn max_queue_entries(self) -> u32 {
        self.field(0, 16) as u32 + 1
    }

    pub fn contiguous_queues_required(self) -> bool {
        self.field(16, 1) != 0
    }

    /// The longest the controller may take to become ready or disabled, at
    /// least one 500 ms unit.
    pub fn ready_timeout_ms(self) -> u32 {
        (self.field(24, 8) as u32).max(1) * 500
    }

    /// CAP.CRMS: the controller reports its ready timeouts in CRTO, which an
    /// NVMe 2.0 controller must.
    pub fn reports_ready_timeouts(self) -> bool {
        self.field(59, 2) != 0
    }

    pub fn doorbell_stride(self) -> usize {
        4 << self.field(32, 4)
    }

    pub fn supports_nvm_command_set(self) -> bool {
        self.field(37, 1) != 0
    }

    pub fn min_page_size(self) -> usize {
        PAGE_SIZE << self.field(48, 4)
    }

    pub fn max_page_size(self) -> usize {
        PAGE_SIZE << self.field(52, 4)
    }

    /// Whether the controller can address queues and data in [`PAGE_SIZE`]
    /// pages, the only size this driver lays memory out in.
    pub fn supports_page_size(self) -> bool {
        (self.min_page_size()..=self.max_page_size()).contains(&PAGE_SIZE)
    }
}

pub const CC_EN: u32 = 1;
const CC_IOSQES_SHIFT: u32 = 16;
const CC_IOCQES_SHIFT: u32 = 20;
const CC_SHN_SHIFT: u32 = 14;
const CC_SHN_NORMAL: u32 = 0b01 << CC_SHN_SHIFT;
const CC_SHN_MASK: u32 = 0b11 << CC_SHN_SHIFT;

/// CC for an enabled controller: the NVM command set, round-robin
/// arbitration, [`PAGE_SIZE`] pages and the entry sizes this driver lays
/// queues out in.
pub fn cc_enabled() -> u32 {
    let entry_shift = |bytes: usize| bytes.trailing_zeros();
    CC_EN
        | (entry_shift(crate::command::SQE_BYTES) << CC_IOSQES_SHIFT)
        | (entry_shift(crate::completion::CQE_BYTES) << CC_IOCQES_SHIFT)
}

/// `cc` with a normal shutdown notification requested.
pub fn cc_shutdown(cc: u32) -> u32 {
    (cc & !CC_SHN_MASK) | CC_SHN_NORMAL
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Csts(pub u32);

impl Csts {
    pub fn ready(self) -> bool {
        self.0 & 1 != 0
    }

    pub fn fatal(self) -> bool {
        self.0 & 2 != 0
    }

    pub fn shutdown_complete(self) -> bool {
        (self.0 >> 2) & 0b11 == 0b10
    }

    /// All ones: the device fell off the bus.
    pub fn absent(self) -> bool {
        self.0 == u32::MAX
    }
}

/// CRTO.CRWMT, how long the controller may take to become ready with media:
/// at least one 500 ms unit, and no more than CAP.TO can say, since the field
/// reaches nine hours and a probe spins for it.
pub fn crto_ready_timeout_ms(crto: u32) -> u32 {
    (crto & 0xFFFF).clamp(1, 0xFF) * 500
}

/// The VS register's value for NVMe `major`.`minor`.
pub const fn version(major: u32, minor: u32) -> u32 {
    (major << 16) | (minor << 8)
}

/// AQA for admin queues of `sq` and `cq` entries.
pub fn aqa(sq: u16, cq: u16) -> u32 {
    u32::from(sq - 1) | (u32::from(cq - 1) << 16)
}

pub fn sq_tail_doorbell(cap: Cap, qid: u16) -> usize {
    DOORBELLS + 2 * usize::from(qid) * cap.doorbell_stride()
}

pub fn cq_head_doorbell(cap: Cap, qid: u16) -> usize {
    DOORBELLS + (2 * usize::from(qid) + 1) * cap.doorbell_stride()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CAP as QEMU's model reports it: MQES 2047, CQR, TO 7.5 s, DSTRD 0,
    /// the NVM and I/O command sets, MPSMIN 4 KiB, MPSMAX 64 KiB.
    const QEMU_CAP: Cap = Cap(0x0040_0820_0f01_07ff);

    #[test]
    fn decodes_cap() {
        assert_eq!(QEMU_CAP.max_queue_entries(), 2048);
        assert!(QEMU_CAP.contiguous_queues_required());
        assert_eq!(QEMU_CAP.ready_timeout_ms(), 7500);
        assert_eq!(QEMU_CAP.doorbell_stride(), 4);
        assert!(QEMU_CAP.supports_nvm_command_set());
        assert_eq!(QEMU_CAP.min_page_size(), 4096);
        assert_eq!(QEMU_CAP.max_page_size(), 65536);
        assert!(QEMU_CAP.supports_page_size());
        assert!(!QEMU_CAP.reports_ready_timeouts());
        assert_eq!(Cap(1 << 59).ready_timeout_ms(), 500);
        assert!(Cap(1 << 59).reports_ready_timeouts());
        assert_eq!(crto_ready_timeout_ms(0x0004_0010), 8000);
        assert_eq!(crto_ready_timeout_ms(0xFFFF), 127_500);
        assert_eq!(version(1, 4), 0x0001_0400);
    }

    #[test]
    fn refuses_a_minimum_page_above_4k() {
        let cap = Cap(QEMU_CAP.0 | (1 << 48));
        assert!(!cap.supports_page_size());
    }

    #[test]
    fn doorbells_follow_the_stride() {
        assert_eq!(sq_tail_doorbell(QEMU_CAP, 0), 0x1000);
        assert_eq!(cq_head_doorbell(QEMU_CAP, 0), 0x1004);
        assert_eq!(sq_tail_doorbell(QEMU_CAP, 2), 0x1010);
        let wide = Cap(QEMU_CAP.0 | (2 << 32));
        assert_eq!(cq_head_doorbell(wide, 1), 0x1000 + 3 * 16);
        assert_eq!(register_span(wide, 3), 0x1000 + 6 * 16);
    }

    #[test]
    fn composes_cc() {
        assert_eq!(cc_enabled(), 0x0046_0001);
        assert_eq!(cc_shutdown(cc_enabled()), 0x0046_4001);
        assert_eq!(cc_shutdown(0x0046_8001), 0x0046_4001);
    }

    #[test]
    fn decodes_csts() {
        assert!(Csts(1).ready());
        assert!(Csts(3).fatal());
        assert!(Csts(0b1001).shutdown_complete());
        assert!(!Csts(0b0101).shutdown_complete());
        assert!(Csts(u32::MAX).absent());
        assert_eq!(aqa(32, 64), 0x003f_001f);
    }
}
