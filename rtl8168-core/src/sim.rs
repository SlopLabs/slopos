//! A simulated RTL8168h behind [`RegisterBus`]: a register file, the
//! indirect address spaces, self-clearing flags, and a log of every access.

use crate::bus::RegisterBus;
use crate::regs::*;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Read(usize, u32),
    Write(usize, u32),
    Delay(u32),
}

/// A condition the simulated chip never reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stuck {
    TxFifo,
    RxTxFifo,
    LinkList,
    Reset,
    Eri,
    PhyOcp,
    PhyReset,
}

pub struct Chip {
    regs: [u8; 256],
    pub log: Vec<Op>,
    pub eri: BTreeMap<u16, u32>,
    pub mac_ocp: BTreeMap<u16, u16>,
    pub phy_ocp: BTreeMap<u16, u16>,
    pub ephy: BTreeMap<u8, u16>,
    pub csi: BTreeMap<u16, u32>,
    pub stuck: Option<Stuck>,
    /// Interrupt causes raised just after the next read of INTR_STATUS.
    pub raise_after_status_read: u16,
    /// The log's length, readable from outside a borrow of the chip.
    pub clock: Rc<Cell<usize>>,
}

impl Chip {
    pub fn new() -> Self {
        let mut chip = Self {
            regs: [0; 256],
            log: Vec::new(),
            eri: BTreeMap::new(),
            mac_ocp: BTreeMap::new(),
            phy_ocp: BTreeMap::new(),
            ephy: BTreeMap::new(),
            csi: BTreeMap::new(),
            stuck: None,
            clock: Rc::new(Cell::new(0)),
            raise_after_status_read: 0,
        };
        chip.set32(TX_CONFIG, 0x541 << 20);
        chip.set8(MCU, MCU_NOW_IS_OOB);
        chip.mac_ocp.insert(0xe8de, 1 << 14);
        chip.eri.insert(0xe0, 0x3322_1100);
        chip.eri.insert(0xe4, 0x0000_5544);
        chip.phy_ocp
            .insert(crate::bus::mii_reg(0), 0x1140 | 1 << 11);
        chip.phy_ocp.insert(crate::bus::mii_reg(1), 0x7949);
        chip.phy_ocp.insert(crate::bus::mii_reg(4), 0x01e1);
        chip.phy_ocp.insert(crate::bus::mii_reg(15), 0x3000);
        chip.phy_ocp.insert(0xa5d0, 0x0006);
        chip
    }

    pub fn set8(&mut self, offset: usize, value: u8) {
        self.regs[offset] = value;
    }

    pub fn get8(&self, offset: usize) -> u8 {
        self.regs[offset]
    }

    pub fn set32(&mut self, offset: usize, value: u32) {
        self.regs[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    pub fn get32(&self, offset: usize) -> u32 {
        let b = &self.regs[offset..offset + 4];
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    }

    fn stuck(&self, s: Stuck) -> bool {
        self.stuck == Some(s)
    }

    fn record(&mut self, op: Op) {
        self.log.push(op);
        self.clock.set(self.log.len());
    }

    fn read(&mut self, offset: usize, width: usize) -> u32 {
        match offset {
            CHIP_CMD if !self.stuck(Stuck::Reset) => self.regs[CHIP_CMD] &= !CMD_RESET,
            MCU => {
                let mut v = self.regs[MCU] & !(MCU_TX_EMPTY | MCU_RX_EMPTY | MCU_LINK_LIST_READY);
                if !self.stuck(Stuck::RxTxFifo) {
                    v |= MCU_TX_EMPTY | MCU_RX_EMPTY;
                }
                if !self.stuck(Stuck::LinkList) {
                    v |= MCU_LINK_LIST_READY;
                }
                self.regs[MCU] = v;
            }
            TX_CONFIG => {
                let v = self.get32(TX_CONFIG) & !TX_CONFIG_EMPTY;
                let empty = if self.stuck(Stuck::TxFifo) {
                    0
                } else {
                    TX_CONFIG_EMPTY
                };
                self.set32(TX_CONFIG, v | empty);
            }
            _ => {}
        }
        let mut v = 0u32;
        for i in (0..width).rev() {
            v = v << 8 | u32::from(self.regs[offset + i]);
        }
        self.record(Op::Read(offset, v));
        if offset == INTR_STATUS {
            let raised = core::mem::take(&mut self.raise_after_status_read).to_le_bytes();
            self.regs[INTR_STATUS] |= raised[0];
            self.regs[INTR_STATUS + 1] |= raised[1];
        }
        v
    }

    fn write(&mut self, offset: usize, width: usize, value: u32) {
        self.record(Op::Write(offset, value));
        match (offset, width) {
            (INTR_STATUS, 2) => {
                let status = u16::from_le_bytes([self.regs[offset], self.regs[offset + 1]]);
                let left = status & !(value as u16);
                self.regs[offset..offset + 2].copy_from_slice(&left.to_le_bytes());
                return;
            }
            (ERI_ACCESS, 4) => return self.eri_access(value),
            (MAC_OCP, 4) => return self.mac_ocp_access(value),
            (PHY_OCP, 4) => return self.phy_ocp_access(value),
            (EPHY_ACCESS, 4) => return self.ephy_access(value),
            (CSI_ACCESS, 4) => return self.csi_access(value),
            _ => {}
        }
        for i in 0..width {
            self.regs[offset + i] = (value >> (8 * i)) as u8;
        }
    }

    fn eri_access(&mut self, cmd: u32) {
        let addr = (cmd & 0xfff) as u16;
        let mask = (cmd >> 12) & 0xf;
        if self.stuck(Stuck::Eri) {
            self.set32(ERI_ACCESS, cmd);
            return;
        }
        if cmd & ACCESS_FLAG != 0 {
            let data = self.get32(ERI_DATA);
            let old = self.eri.get(&addr).copied().unwrap_or(0);
            let bytes = (0..4).fold(0u32, |m, i| {
                if mask & 1 << i != 0 {
                    m | 0xff << (8 * i)
                } else {
                    m
                }
            });
            self.eri.insert(addr, (old & !bytes) | (data & bytes));
            self.set32(ERI_ACCESS, cmd & !ACCESS_FLAG);
        } else {
            let data = self.eri.get(&addr).copied().unwrap_or(0);
            self.set32(ERI_DATA, data);
            self.set32(ERI_ACCESS, cmd | ACCESS_FLAG);
        }
    }

    fn mac_ocp_access(&mut self, cmd: u32) {
        let reg = ((cmd >> 15) & 0xfffe) as u16;
        if cmd & ACCESS_FLAG != 0 {
            self.mac_ocp.insert(reg, cmd as u16);
        }
        let data = self.mac_ocp.get(&reg).copied().unwrap_or(0);
        self.set32(MAC_OCP, cmd & 0x7fff_0000 | u32::from(data));
    }

    fn phy_ocp_access(&mut self, cmd: u32) {
        let reg = ((cmd >> 15) & 0xfffe) as u16;
        if self.stuck(Stuck::PhyOcp) {
            self.set32(PHY_OCP, cmd);
            return;
        }
        if cmd & ACCESS_FLAG != 0 {
            self.phy_ocp.insert(reg, cmd as u16);
            self.set32(PHY_OCP, cmd & !ACCESS_FLAG);
        } else {
            let bmcr = crate::bus::mii_reg(0);
            let mut data = self.phy_ocp.get(&reg).copied().unwrap_or(0);
            if reg == bmcr && !self.stuck(Stuck::PhyReset) {
                let settled = data & !(crate::chip::mii::BMCR_RESET);
                self.phy_ocp.insert(bmcr, settled);
                data = settled;
            }
            self.set32(PHY_OCP, ACCESS_FLAG | cmd & 0x7fff_0000 | u32::from(data));
        }
    }

    fn ephy_access(&mut self, cmd: u32) {
        let reg = ((cmd >> 16) & 0x1f) as u8;
        if cmd & ACCESS_FLAG != 0 {
            self.ephy.insert(reg, cmd as u16);
            self.set32(EPHY_ACCESS, cmd & !ACCESS_FLAG);
        } else {
            let data = self.ephy.get(&reg).copied().unwrap_or(0);
            self.set32(
                EPHY_ACCESS,
                ACCESS_FLAG | cmd & 0x7fff_0000 | u32::from(data),
            );
        }
    }

    fn csi_access(&mut self, cmd: u32) {
        let addr = (cmd & 0xfff) as u16;
        if cmd & ACCESS_FLAG != 0 {
            let data = self.get32(CSI_DATA);
            self.csi.insert(addr, data);
            self.set32(CSI_ACCESS, cmd & !ACCESS_FLAG);
        } else {
            let data = self.csi.get(&addr).copied().unwrap_or(0);
            self.set32(CSI_DATA, data);
            self.set32(CSI_ACCESS, cmd | ACCESS_FLAG);
        }
    }

    /// Positions in the log of writes to `offset`, with the value written.
    pub fn writes_to(&self, offset: usize) -> Vec<(usize, u32)> {
        self.log
            .iter()
            .enumerate()
            .filter_map(|(i, op)| match *op {
                Op::Write(o, v) if o == offset => Some((i, v)),
                _ => None,
            })
            .collect()
    }

    /// Position of the first write to `offset` matching `pred`.
    pub fn first_write(&self, offset: usize, pred: impl Fn(u32) -> bool) -> Option<usize> {
        self.writes_to(offset)
            .into_iter()
            .find(|&(_, v)| pred(v))
            .map(|(i, _)| i)
    }

    pub fn last_write(&self, offset: usize, pred: impl Fn(u32) -> bool) -> Option<usize> {
        self.writes_to(offset)
            .into_iter()
            .rev()
            .find(|&(_, v)| pred(v))
            .map(|(i, _)| i)
    }
}

impl RegisterBus for Chip {
    fn read8(&mut self, offset: usize) -> u8 {
        self.read(offset, 1) as u8
    }
    fn read16(&mut self, offset: usize) -> u16 {
        self.read(offset, 2) as u16
    }
    fn read32(&mut self, offset: usize) -> u32 {
        self.read(offset, 4)
    }
    fn write8(&mut self, offset: usize, value: u8) {
        self.write(offset, 1, value.into())
    }
    fn write16(&mut self, offset: usize, value: u16) {
        self.write(offset, 2, value.into())
    }
    fn write32(&mut self, offset: usize, value: u32) {
        self.write(offset, 4, value)
    }
    fn delay_us(&mut self, us: u32) {
        self.record(Op::Delay(us));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{Error, Wait, mii_reg, paged_reg};
    use crate::chip::{self, ProbeError, RingAddresses, Stalls, mii};
    use crate::desc::{Descriptor, LENGTH_MASK};
    use crate::ring::{DescriptorMemory, RxRing};
    use std::cell::Cell;
    use std::vec;

    const OCP_REG: u32 = 0x7fff_0000;
    const MAC: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];

    fn mac_ocp_write_of(reg: u16) -> impl Fn(u32) -> bool {
        move |v| v & ACCESS_FLAG != 0 && v & OCP_REG == u32::from(reg) << 15
    }

    #[test]
    fn probe_takes_the_chip_from_management_firmware_before_resetting_it() {
        let mut chip = Chip::new();
        let (probed, stalls) = chip::probe(&mut chip).unwrap();
        assert_eq!(stalls, Stalls::default());
        assert_eq!(probed.mac, MAC);

        let gate = chip
            .first_write(MISC, |v| v & MISC_RXDV_GATED != 0)
            .unwrap();
        let rx_tx_off = chip
            .first_write(CHIP_CMD, |v| {
                v as u8 & (CMD_RX_ENABLE | CMD_TX_ENABLE | CMD_RESET) == 0
            })
            .unwrap();
        let oob_off = chip
            .first_write(MCU, |v| v as u8 & MCU_NOW_IS_OOB == 0)
            .unwrap();
        let ocp = chip.writes_to(MAC_OCP);
        let e8de: Vec<_> = ocp
            .iter()
            .filter(|(_, v)| mac_ocp_write_of(0xe8de)(*v))
            .collect();
        assert_eq!(e8de.len(), 2);
        let reset = chip
            .first_write(CHIP_CMD, |v| v as u8 == CMD_RESET)
            .unwrap();
        assert!(gate < rx_tx_off && rx_tx_off < oob_off);
        assert!(oob_off < e8de[0].0 && e8de[0].0 < e8de[1].0 && e8de[1].0 < reset);
        assert_eq!(e8de[0].1 as u16, 0);
        assert_eq!(e8de[1].1 as u16, 1 << 15);
        assert_eq!(chip.mac_ocp[&0xe8de], 1 << 15);

        let one_ms = chip.log[rx_tx_off..oob_off].contains(&Op::Delay(1000));
        assert!(one_ms, "1 ms between stopping RX/TX and clearing OOB");
        let ready_polls = chip.log[e8de[0].0..e8de[1].0]
            .iter()
            .any(|op| matches!(op, Op::Read(MCU, v) if *v as u8 & MCU_LINK_LIST_READY != 0));
        assert!(ready_polls);
    }

    #[test]
    fn interrupts_are_masked_before_the_handoff() {
        let mut chip = Chip::new();
        chip::probe(&mut chip).unwrap();
        let masked = chip.first_write(INTR_MASK, |v| v == 0).unwrap();
        let gate = chip
            .first_write(MISC, |v| v & MISC_RXDV_GATED != 0)
            .unwrap();
        assert!(masked < gate);
        assert!(chip.writes_to(INTR_MASK).iter().all(|&(_, v)| v == 0));
    }

    #[test]
    fn probe_programs_the_eri_address_while_config_is_unlocked() {
        let mut chip = Chip::new();
        chip::probe(&mut chip).unwrap();
        assert_eq!(chip.get32(MAC0), 0x3322_1100);
        assert_eq!(chip.get32(MAC4), 0x5544);
        let unlock = chip
            .first_write(CFG9346, |v| v as u8 == CFG9346_UNLOCK)
            .unwrap();
        let lock = chip
            .first_write(CFG9346, |v| v as u8 == CFG9346_LOCK)
            .unwrap();
        let mac4 = chip.first_write(MAC4, |_| true).unwrap();
        let mac0 = chip.first_write(MAC0, |_| true).unwrap();
        assert!(unlock < mac4 && mac4 < mac0 && mac0 < lock);
    }

    #[test]
    fn the_register_copy_of_the_address_backs_up_an_unusable_eri_copy() {
        for eri_low in [0, 0x3322_1101] {
            let mut chip = Chip::new();
            chip.eri.insert(0xe0, eri_low);
            chip.eri.insert(0xe4, 0);
            for (i, b) in [0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0xee].into_iter().enumerate() {
                chip.set8(MAC0 + i, b);
            }
            let (probed, _) = chip::probe(&mut chip).unwrap();
            assert_eq!(probed.mac, [0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0xee]);
        }
    }

    #[test]
    fn no_usable_address_anywhere_fails_the_probe() {
        let mut chip = Chip::new();
        chip.eri.clear();
        chip.set8(MAC0, 0x01);
        assert_eq!(chip::probe(&mut chip), Err(ProbeError::NoMacAddress));
    }

    #[test]
    fn a_reset_or_port_the_chip_never_completes_fails_the_probe() {
        for (stuck, wait) in [(Stuck::Reset, Wait::Reset), (Stuck::Eri, Wait::Eri)] {
            let mut chip = Chip::new();
            chip.stuck = Some(stuck);
            assert_eq!(
                chip::probe(&mut chip),
                Err(ProbeError::Chip(Error::Timeout(wait))),
                "{stuck:?}"
            );
        }
    }

    #[test]
    fn a_chip_whose_fifos_or_link_list_never_settle_is_reset_brought_up_and_reported() {
        for (stuck, wait, again_in_up) in [
            (Stuck::TxFifo, Wait::TxFifoEmpty, true),
            (Stuck::RxTxFifo, Wait::RxTxFifoEmpty, true),
            (Stuck::LinkList, Wait::LinkListReady, false),
        ] {
            let mut chip = Chip::new();
            chip.stuck = Some(stuck);
            let (probed, stalls) = chip::probe(&mut chip).expect("probe");
            assert_eq!(stalls.iter().collect::<Vec<_>>(), [wait], "{stuck:?}");
            assert_eq!(probed.mac, MAC);
            assert!(
                chip.first_write(CHIP_CMD, |v| v as u8 == CMD_RESET)
                    .is_some()
            );

            let start = chip.log.len();
            let stalls = chip::up(&mut chip, &probed, RINGS, 0, || {}).expect("up");
            let expected: &[Wait] = if again_in_up { &[wait] } else { &[] };
            assert_eq!(stalls.iter().collect::<Vec<_>>(), expected, "{stuck:?}");
            assert!(after(&chip, start, CHIP_CMD, |v| v as u8 == CMD_RESET).is_some());
            assert!(after(&chip, start, TX_DESC_LOW, |_| true).is_some());
            assert_eq!(chip.writes_to(INTR_MASK).last().unwrap().1, 0x002f);
        }
    }

    #[test]
    fn a_reset_that_never_completes_stops_the_bring_up() {
        let mut chip = Chip::new();
        let (probed, _) = chip::probe(&mut chip).unwrap();
        let start = chip.log.len();
        chip.stuck = Some(Stuck::Reset);
        let rearmed = Cell::new(false);
        let result = chip::up(&mut chip, &probed, RINGS, 0, || rearmed.set(true));
        assert_eq!(result, Err(Error::Timeout(Wait::Reset)));
        assert!(!rearmed.get());
        assert!(after(&chip, start, TX_DESC_LOW, |_| true).is_none());
    }

    const RINGS: RingAddresses = RingAddresses {
        tx: 0x0000_0001_2345_6000,
        rx: 0x0000_0002_8765_4000,
        rx_buffer_len: 2048,
    };

    /// Probe, then bring up; returns where in the log `up` began and where
    /// `rearm` ran.
    fn probe_and_up(chip: &mut Chip) -> (Result<Stalls, Error>, usize, Option<usize>) {
        let (probed, _) = chip::probe(chip).unwrap();
        let start = chip.log.len();
        let clock = chip.clock.clone();
        let rearmed = Cell::new(None);
        let result = chip::up(chip, &probed, RINGS, 0, || rearmed.set(Some(clock.get())));
        (result, start, rearmed.get())
    }

    fn after(
        chip: &Chip,
        start: usize,
        offset: usize,
        pred: impl Fn(u32) -> bool,
    ) -> Option<usize> {
        chip.writes_to(offset)
            .into_iter()
            .find(|&(i, v)| i >= start && pred(v))
            .map(|(i, _)| i)
    }

    #[test]
    fn up_rearms_the_rings_after_the_reset_and_before_the_chip_learns_them() {
        let mut chip = Chip::new();
        let (result, start, rearmed) = probe_and_up(&mut chip);
        assert_eq!(result, Ok(Stalls::default()));
        let rearmed = rearmed.unwrap();
        let reset = after(&chip, start, CHIP_CMD, |v| v as u8 == CMD_RESET).unwrap();
        let gate = after(&chip, start, MISC, |v| v & MISC_RXDV_GATED != 0).unwrap();
        let masked = after(&chip, start, INTR_MASK, |v| v == 0).unwrap();
        let ring = after(&chip, start, TX_DESC_HIGH, |_| true).unwrap();
        assert!(masked < gate && gate < reset && reset < rearmed && rearmed < ring);
    }

    #[test]
    fn ring_addresses_go_in_high_word_first() {
        let mut chip = Chip::new();
        let (_, start, _) = probe_and_up(&mut chip);
        let w = |o| after(&chip, start, o, |_| true).unwrap();
        assert!(w(TX_DESC_HIGH) < w(TX_DESC_LOW));
        assert!(w(RX_DESC_HIGH) < w(RX_DESC_LOW));
        assert_eq!(chip.get32(TX_DESC_HIGH), 0x1);
        assert_eq!(chip.get32(TX_DESC_LOW), 0x2345_6000);
        assert_eq!(chip.get32(RX_DESC_HIGH), 0x2);
        assert_eq!(chip.get32(RX_DESC_LOW), 0x8765_4000);
    }

    struct Slots(Vec<Descriptor>);

    impl DescriptorMemory for Slots {
        fn slots(&self) -> usize {
            self.0.len()
        }
        fn read(&self, index: usize) -> Descriptor {
            self.0[index]
        }
        fn write(&mut self, index: usize, desc: Descriptor) {
            self.0[index] = desc;
        }
    }

    #[test]
    fn no_frame_the_chip_accepts_outgrows_the_buffer_a_descriptor_names() {
        for buffer_len in [1536, 2048] {
            let ring = RxRing::new(Slots(vec![Descriptor::default(); 4]), buffer_len, |i| {
                0x1000 * i as u64
            })
            .unwrap();
            let named = (0..4)
                .map(|i| ring.memory().read(i).opts1 & LENGTH_MASK)
                .min()
                .unwrap();
            let mut chip = Chip::new();
            let (probed, _) = chip::probe(&mut chip).unwrap();
            let rings = RingAddresses {
                rx_buffer_len: ring.buffer_len(),
                ..RINGS
            };
            chip::up(&mut chip, &probed, rings, 0, || {}).unwrap();
            let limits = chip.writes_to(RX_MAX_SIZE);
            assert!(!limits.is_empty());
            assert!(
                limits.iter().all(|&(_, v)| v <= named),
                "{limits:?} > {named}"
            );
        }
    }

    #[test]
    fn config_registers_are_written_only_while_unlocked() {
        let mut chip = Chip::new();
        let (_, start, _) = probe_and_up(&mut chip);
        let guarded = [CONFIG2, CONFIG3, CONFIG5];
        let mut unlocked = false;
        let mut seen = 0;
        for op in &chip.log[start..] {
            if let Op::Write(offset, v) = *op {
                if offset == CFG9346 {
                    unlocked = v as u8 == CFG9346_UNLOCK;
                } else if guarded.contains(&offset) {
                    assert!(unlocked, "write {v:#x} to {offset:#x} while locked");
                    seen += 1;
                }
            }
        }
        assert!(seen >= guarded.len());
        assert!(!unlocked, "config left unlocked");
        assert_eq!(chip.get8(CFG9346), CFG9346_LOCK);
    }

    #[test]
    fn rx_and_tx_are_enabled_before_they_are_configured() {
        let mut chip = Chip::new();
        let (_, start, _) = probe_and_up(&mut chip);
        let enable = after(&chip, start, CHIP_CMD, |v| {
            v as u8 == CMD_RX_ENABLE | CMD_TX_ENABLE
        })
        .unwrap();
        let ring = after(&chip, start, RX_DESC_LOW, |_| true).unwrap();
        let rx_config = chip.last_write(RX_CONFIG, |_| true).unwrap();
        let tx_config = chip.last_write(TX_CONFIG, |_| true).unwrap();
        let irq = chip.last_write(INTR_MASK, |_| true).unwrap();
        assert!(ring < enable && enable < rx_config && enable < tx_config);
        assert!(rx_config < irq && tx_config < irq);
        assert!(after(&chip, enable, RX_CONFIG, |v| v & 0x3f != 0).is_some());
        assert_eq!(chip.get32(RX_CONFIG) & 0x3f, 0x0e);
        assert_eq!(chip.get32(RX_CONFIG) & 0xff00, 0xcf00);
        assert_eq!(chip.get32(TX_CONFIG) & 0x0fff_ffff, 0x0300_0780);
        assert_eq!(
            (chip.get32(MAR0), chip.get32(MAR0 + 4)),
            (u32::MAX, u32::MAX)
        );
        assert_eq!(chip.writes_to(INTR_MASK).last().unwrap().1, 0x002f);
    }

    #[test]
    fn the_phy_is_woken_configured_and_reset_before_the_mac() {
        let mut chip = Chip::new();
        let (_, start, _) = probe_and_up(&mut chip);
        let bmcr = u32::from(mii_reg(mii::BMCR)) << 15;
        let phy_write = |pred: &dyn Fn(u32) -> bool| {
            chip.writes_to(PHY_OCP)
                .into_iter()
                .find(|&(i, v)| i >= start && v & ACCESS_FLAG != 0 && pred(v))
                .map(|(i, _)| i)
                .unwrap()
        };
        let wake = phy_write(&|v| v & OCP_REG == bmcr && v as u16 & mii::BMCR_POWER_DOWN == 0);
        let first_param = phy_write(&|v| v & OCP_REG == u32::from(paged_reg(0x0a43, 0x13)) << 15);
        let soft_reset = phy_write(&|v| v & OCP_REG == bmcr && v as u16 & mii::BMCR_RESET != 0);
        let mac_reset = after(&chip, start, CHIP_CMD, |v| v as u8 == CMD_RESET).unwrap();
        assert!(wake < first_param && first_param < soft_reset && soft_reset < mac_reset);
        assert!(chip.log[wake..first_param].contains(&Op::Delay(20_000)));
    }

    #[test]
    fn autoneg_advertises_what_the_phy_reports_with_pause_and_without_eee() {
        let mut chip = Chip::new();
        chip.phy_ocp
            .insert(mii_reg(mii::ESTATUS), mii::ESTATUS_1000_FULL);
        let (result, _, _) = probe_and_up(&mut chip);
        assert_eq!(result, Ok(Stalls::default()));
        assert_eq!(chip.phy_ocp[&mii_reg(mii::ANAR)], 0x0de1);
        assert_eq!(chip.phy_ocp[&mii_reg(mii::CTRL1000)], mii::CTRL1000_FULL);
        assert_eq!(chip.phy_ocp[&0xa5d0], 0);
        let bmcr = chip.phy_ocp[&mii_reg(mii::BMCR)];
        assert_eq!(
            bmcr & (mii::BMCR_AUTONEG_ENABLE | mii::BMCR_AUTONEG_RESTART | mii::BMCR_ISOLATE),
            mii::BMCR_AUTONEG_ENABLE | mii::BMCR_AUTONEG_RESTART
        );
    }

    #[test]
    fn a_phy_without_extended_status_gets_no_gigabit_advertisement_written() {
        let mut chip = Chip::new();
        chip.phy_ocp
            .insert(mii_reg(mii::BMSR), 0x7849 & !mii::BMSR_EXTENDED_STATUS);
        chip.phy_ocp.insert(mii_reg(mii::CTRL1000), 0x0300);
        probe_and_up(&mut chip).0.unwrap();
        assert_eq!(chip.phy_ocp[&mii_reg(mii::CTRL1000)], 0x0300);
    }

    #[test]
    fn phy_settings_land_at_their_ocp_addresses() {
        let mut chip = Chip::new();
        chip.mac_ocp.insert(0xdd00, 0x0abc);
        chip.phy_ocp.insert(paged_reg(0x0bcd, 0x16), 0x0007);
        chip.phy_ocp.insert(paged_reg(0x0a44, 0x11), 1 << 7);
        chip.phy_ocp.insert(paged_reg(0x0a43, 0x10), 0b101);
        probe_and_up(&mut chip).0.unwrap();
        assert_eq!(chip.phy_ocp[&0xbcfc], 0x055c);
        assert_eq!(chip.phy_ocp[&0xbcde], 0x4444);
        assert_eq!(chip.phy_ocp[&0xa442], 1 << 11);
        assert_eq!(chip.phy_ocp[&0xa430], 0);
        assert_eq!(chip.phy_ocp[&0xa432], 1 << 4);
        assert_eq!(chip.phy_ocp[&0xa42c], 0x0002);
    }

    #[test]
    fn a_phy_that_never_leaves_reset_stops_the_bring_up() {
        let mut chip = Chip::new();
        chip.stuck = Some(Stuck::PhyReset);
        let (result, start, rearmed) = probe_and_up(&mut chip);
        assert_eq!(result, Err(Error::Timeout(Wait::PhyReset)));
        assert_eq!(rearmed, None);
        assert!(after(&chip, start, TX_DESC_LOW, |_| true).is_none());
    }

    #[test]
    fn a_phy_port_that_never_answers_is_a_timeout() {
        let mut chip = Chip::new();
        let (probed, _) = chip::probe(&mut chip).unwrap();
        chip.stuck = Some(Stuck::PhyOcp);
        let result = chip::up(&mut chip, &probed, RINGS, 0, || {});
        assert_eq!(result, Err(Error::Timeout(Wait::PhyOcp)));
    }

    #[test]
    fn indirect_ports_encode_their_address_words() {
        let mut chip = Chip::new();
        crate::bus::phy_ocp_write(&mut chip, 0xa438, 0x1234).unwrap();
        assert_eq!(
            chip.log[0],
            Op::Write(PHY_OCP, 0x8000_0000 | 0x521c_0000 | 0x1234)
        );
        chip.log.clear();
        crate::bus::phy_ocp_read(&mut chip, 0xa438).unwrap();
        assert_eq!(chip.log[0], Op::Write(PHY_OCP, 0x521c_0000));
        chip.log.clear();
        crate::bus::mac_ocp_write(&mut chip, 0xe8de, 0x8000);
        assert_eq!(
            chip.log[0],
            Op::Write(MAC_OCP, 0x8000_0000 | 0x746f_0000 | 0x8000)
        );
        chip.log.clear();
        crate::bus::eri_read(&mut chip, 0xe0).unwrap();
        assert_eq!(chip.log[0], Op::Write(ERI_ACCESS, 0x0000_f0e0));
        chip.log.clear();
        crate::bus::eri_write(&mut chip, 0xcc, crate::bus::EriMask::Byte0, 0x38).unwrap();
        assert_eq!(chip.log[0], Op::Write(ERI_DATA, 0x38));
        assert_eq!(chip.log[1], Op::Write(ERI_ACCESS, 0x8000_10cc));
        chip.log.clear();
        crate::bus::ephy_write(&mut chip, 0x1e, 0x0001).unwrap();
        assert_eq!(chip.log[0], Op::Write(EPHY_ACCESS, 0x801e_0001));
        chip.log.clear();
        crate::bus::csi_write(&mut chip, 0x70c, 3, 0x2700_0000).unwrap();
        assert_eq!(chip.log[1], Op::Write(CSI_ACCESS, 0x8003_f70c));
        assert_eq!(mii_reg(mii::CTRL1000), 0xa412);
        assert_eq!(paged_reg(0x0a43, 0x13), 0xa436);
    }

    #[test]
    fn mac_and_pcie_settings_reach_their_spaces() {
        let mut chip = Chip::new();
        chip.ephy.insert(0x1e, 0x0800);
        chip.ephy.insert(0x05, 0xffff);
        chip.csi.insert(0x70c, 0x11ab_cdef);
        chip.eri.insert(0xdc, 0x0000_0100);
        chip.eri.insert(0x1b0, 0x0000_1003);
        probe_and_up(&mut chip).0.unwrap();
        assert_eq!(chip.ephy[&0x1e], 0x0001);
        assert_eq!(chip.ephy[&0x05], 0x2089);
        assert_eq!(chip.csi[&0x70c], 0x27ab_cdef);
        assert_eq!(chip.eri[&0xdc], 0x0000_011d);
        assert_eq!(chip.eri[&0x1b0], 0);
        assert_eq!(chip.eri[&0xc8], 0x0008_0002);
        assert_eq!(chip.get32(MISC) & MISC_RXDV_GATED, 0);
    }

    #[test]
    fn acknowledging_clears_every_cause_including_one_raised_after_the_read() {
        let mut chip = Chip::new();
        chip.set8(INTR_STATUS, INTR_TX_OK as u8);
        chip.raise_after_status_read = INTR_RX_OK;
        assert_eq!(chip::ack_interrupts(&mut chip), INTR_TX_OK);
        assert_eq!(
            chip.log,
            [
                Op::Read(INTR_STATUS, u32::from(INTR_TX_OK)),
                Op::Write(INTR_STATUS, 0xffff)
            ]
        );
        assert_eq!((chip.get8(INTR_STATUS), chip.get8(INTR_STATUS + 1)), (0, 0));
    }

    #[test]
    fn stop_masks_interrupts_and_closes_rx_before_the_reset() {
        let mut chip = Chip::new();
        chip.set32(RX_CONFIG, 0xcf0e);
        assert_eq!(chip::stop(&mut chip), Ok(Stalls::default()));
        let masked = chip.first_write(INTR_MASK, |v| v == 0).unwrap();
        let closed = chip.first_write(RX_CONFIG, |v| v & 0x3f == 0).unwrap();
        let reset = chip
            .first_write(CHIP_CMD, |v| v as u8 == CMD_RESET)
            .unwrap();
        assert!(masked < closed && closed < reset);
        assert_eq!(chip.get32(RX_CONFIG), 0xcf00);
    }

    #[test]
    fn stop_resets_a_chip_whose_fifos_never_drain_and_reports_it() {
        for (stuck, wait) in [
            (Stuck::TxFifo, Wait::TxFifoEmpty),
            (Stuck::RxTxFifo, Wait::RxTxFifoEmpty),
        ] {
            let mut chip = Chip::new();
            chip.stuck = Some(stuck);
            let stalls = chip::stop(&mut chip).unwrap();
            assert_eq!(stalls.iter().collect::<Vec<_>>(), [wait]);
            assert!(
                chip.first_write(CHIP_CMD, |v| v as u8 == CMD_RESET)
                    .is_some()
            );
        }
    }

    #[test]
    fn aspm_counts_as_validated_only_when_the_vendor_flag_is_set() {
        for (flag, validated) in [
            (0x0000, false),
            (0x0010, false),
            (0x0001, true),
            (0x0008, true),
        ] {
            let mut chip = Chip::new();
            chip.mac_ocp.insert(0xc0b2, flag);
            assert_eq!(chip::aspm_validated(&mut chip), validated, "{flag:#x}");
        }
    }
}
