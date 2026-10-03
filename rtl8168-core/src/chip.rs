//! Bring-up of the RTL8168h MAC and its internal PHY, written once against
//! [`RegisterBus`]. The register writes and their order are facts of this
//! version's bring-up as Linux's `r8169` performs it, without the PHY patch
//! firmware.

use crate::bus::{
    EriMask, Error, RegisterBus, Result, Wait, eri_modify, eri_read, eri_write, mac_ocp_modify,
    mac_ocp_read, mac_ocp_write, mii_reg, paged_reg, phy_ocp_modify, phy_ocp_read, phy_ocp_write,
    poll,
};
use crate::regs::*;

/// IEEE 802.3 clause 22 registers and bits.
pub mod mii {
    pub const BMCR: u8 = 0;
    pub const BMSR: u8 = 1;
    pub const ANAR: u8 = 4;
    pub const CTRL1000: u8 = 9;
    pub const ESTATUS: u8 = 15;

    pub const BMCR_RESET: u16 = 1 << 15;
    pub const BMCR_AUTONEG_ENABLE: u16 = 1 << 12;
    pub const BMCR_POWER_DOWN: u16 = 1 << 11;
    pub const BMCR_ISOLATE: u16 = 1 << 10;
    pub const BMCR_AUTONEG_RESTART: u16 = 1 << 9;

    pub const BMSR_100_FULL: u16 = 1 << 14;
    pub const BMSR_100_HALF: u16 = 1 << 13;
    pub const BMSR_10_FULL: u16 = 1 << 12;
    pub const BMSR_10_HALF: u16 = 1 << 11;
    pub const BMSR_EXTENDED_STATUS: u16 = 1 << 8;

    pub const ESTATUS_1000_FULL: u16 = 1 << 13;
    pub const ESTATUS_1000_HALF: u16 = 1 << 12;

    pub const ANAR_ASYM_PAUSE: u16 = 1 << 11;
    pub const ANAR_PAUSE: u16 = 1 << 10;
    pub const ANAR_100_FULL: u16 = 1 << 8;
    pub const ANAR_100_HALF: u16 = 1 << 7;
    pub const ANAR_10_FULL: u16 = 1 << 6;
    pub const ANAR_10_HALF: u16 = 1 << 5;

    pub const CTRL1000_FULL: u16 = 1 << 9;
    pub const CTRL1000_HALF: u16 = 1 << 8;

    /// IEEE 802.3 clause 45.2.7.13 EEE advertisement: 100BASE-TX and
    /// 1000BASE-T.
    pub const EEE_ADV_100: u16 = 1 << 1;
    pub const EEE_ADV_1000: u16 = 1 << 2;
}

/// The internal PHY's copy of the EEE advertisement register.
const PHY_EEE_ADVERTISEMENT: u16 = 0xa5d0;

pub const INTERRUPTS: u16 =
    INTR_RX_OK | INTR_RX_ERROR | INTR_TX_OK | INTR_TX_ERROR | INTR_LINK_CHANGE;
const RX_CONFIG_BASE: u32 =
    RX_CONFIG_128_INT | RX_CONFIG_MULTI | RX_CONFIG_DMA_UNLIMITED | RX_CONFIG_EARLY_OFF;
const RX_ACCEPT: u32 = RX_ACCEPT_BROADCAST | RX_ACCEPT_MULTICAST | RX_ACCEPT_MY_PHYS;
const TX_CONFIG_VALUE: u32 = TX_CONFIG_DMA_UNLIMITED | TX_CONFIG_IFG_SHORTEST | TX_CONFIG_AUTO_FIFO;
/// MaxTxPacketSize in 128-byte units.
const MAX_TX_PACKET: u8 = 0x27;
/// Idle time before TX enters low-power idle, in byte times: an MTU-sized
/// frame, its header and 32 bytes.
const EEE_TX_IDLE: u16 = 1500 + 14 + 0x20;

pub type Mac = [u8; 6];

/// What the probe-time sequence leaves for [`up`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Probed {
    pub mac: Mac,
    cplus: u16,
}

/// Where the rings live, and the buffer length every RX descriptor names:
/// the chip's receive length limit is set to it, so no frame it accepts
/// outgrows a buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingAddresses {
    pub tx: u64,
    pub rx: u64,
    pub rx_buffer_len: u16,
}

fn modify8<B: RegisterBus>(bus: &mut B, offset: usize, clear: u8, set: u8) {
    let value = bus.read8(offset);
    bus.write8(offset, (value & !clear) | set);
}

fn modify8_if_changed<B: RegisterBus>(bus: &mut B, offset: usize, clear: u8, set: u8) {
    let value = bus.read8(offset);
    let new = (value & !clear) | set;
    if new != value {
        bus.write8(offset, new);
    }
}

fn modify32<B: RegisterBus>(bus: &mut B, offset: usize, clear: u32, set: u32) {
    let value = bus.read32(offset);
    bus.write32(offset, (value & !clear) | set);
}

fn flush<B: RegisterBus>(bus: &mut B) {
    bus.read8(CHIP_CMD);
}

/// Waits that timed out where the bring-up carries on: the chip works
/// without them, so they are reported rather than fatal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stalls(u16);

impl Stalls {
    const ADVISORY: [Wait; 3] = [Wait::TxFifoEmpty, Wait::RxTxFifoEmpty, Wait::LinkListReady];

    fn note(&mut self, waited: Result<()>) {
        if let Err(Error::Timeout(wait)) = waited {
            self.0 |= 1 << wait as u16;
        }
    }

    fn contains(self, wait: Wait) -> bool {
        self.0 & 1 << wait as u16 != 0
    }

    /// The waits that timed out, in bring-up order.
    pub fn iter(self) -> impl Iterator<Item = Wait> {
        Self::ADVISORY
            .into_iter()
            .filter(move |&w| self.contains(w))
    }
}

fn mask_and_ack_interrupts<B: RegisterBus>(bus: &mut B) {
    bus.write16(INTR_MASK, 0);
    bus.write16(INTR_STATUS, 0xffff);
    flush(bus);
}

/// Gate RXDV off and wait for both FIFOs to drain.
fn gate_rx<B: RegisterBus>(bus: &mut B, stalls: &mut Stalls) {
    modify32(bus, MISC, 0, MISC_RXDV_GATED);
    bus.delay_us(2000);
    stalls.note(poll(bus, Wait::TxFifoEmpty, 100, 42, |b| {
        b.read32(TX_CONFIG) & TX_CONFIG_EMPTY != 0
    }));
    stalls.note(poll(bus, Wait::RxTxFifoEmpty, 100, 42, |b| {
        b.read8(MCU) & (MCU_TX_EMPTY | MCU_RX_EMPTY) == MCU_TX_EMPTY | MCU_RX_EMPTY
    }));
}

fn wait_link_list_ready<B: RegisterBus>(bus: &mut B, stalls: &mut Stalls) {
    stalls.note(poll(bus, Wait::LinkListReady, 100, 42, |b| {
        b.read8(MCU) & MCU_LINK_LIST_READY != 0
    }));
}

/// Take the MAC from the out-of-band management firmware.
fn take_from_oob<B: RegisterBus>(bus: &mut B, stalls: &mut Stalls) {
    gate_rx(bus, stalls);
    modify8(bus, CHIP_CMD, CMD_TX_ENABLE | CMD_RX_ENABLE, 0);
    bus.delay_us(1000);
    modify8(bus, MCU, MCU_NOW_IS_OOB, 0);
    mac_ocp_modify(bus, 0xe8de, 1 << 14, 0);
    wait_link_list_ready(bus, stalls);
    mac_ocp_modify(bus, 0xe8de, 0, 1 << 15);
    wait_link_list_ready(bus, stalls);
}

fn reset<B: RegisterBus>(bus: &mut B) -> Result<()> {
    bus.write8(CHIP_CMD, CMD_RESET);
    poll(bus, Wait::Reset, 100, 100, |b| {
        b.read8(CHIP_CMD) & CMD_RESET == 0
    })
}

fn valid_mac(mac: &Mac) -> bool {
    mac[0] & 1 == 0 && mac.iter().any(|&b| b != 0)
}

/// The station address: the copy in ERI space, else MAC0..MAC5.
pub fn read_mac<B: RegisterBus>(bus: &mut B) -> Result<Option<Mac>> {
    let low = eri_read(bus, 0xe0)?.to_le_bytes();
    let high = eri_read(bus, 0xe4)?.to_le_bytes();
    let eri = [low[0], low[1], low[2], low[3], high[0], high[1]];
    if valid_mac(&eri) {
        return Ok(Some(eri));
    }
    let mut reg = [0; 6];
    for (i, b) in reg.iter_mut().enumerate() {
        *b = bus.read8(MAC0 + i);
    }
    Ok(valid_mac(&reg).then_some(reg))
}

fn program_mac<B: RegisterBus>(bus: &mut B, mac: &Mac) {
    bus.write8(CFG9346, CFG9346_UNLOCK);
    bus.write32(MAC4, u32::from(u16::from_le_bytes([mac[4], mac[5]])));
    flush(bus);
    bus.write32(MAC0, u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]));
    flush(bus);
    bus.write8(CFG9346, CFG9346_LOCK);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeError {
    Chip(Error),
    NoMacAddress,
}

impl From<Error> for ProbeError {
    fn from(e: Error) -> Self {
        Self::Chip(e)
    }
}

/// Probe-time sequence: quiesce the chip, take it from the management
/// firmware, reset it and fix its station address. RX checksum offload and
/// VLAN tag stripping stay off.
pub fn probe<B: RegisterBus>(bus: &mut B) -> Result<(Probed, Stalls), ProbeError> {
    let cplus = bus.read16(CPLUS_CMD) & (CPLUS_NORMAL_MODE | CPLUS_INTT);
    bus.write32(RX_CONFIG, RX_CONFIG_BASE);
    mask_and_ack_interrupts(bus);
    let mut stalls = Stalls::default();
    take_from_oob(bus, &mut stalls);
    reset(bus)?;
    let mac = read_mac(bus)?.ok_or(ProbeError::NoMacAddress)?;
    program_mac(bus, &mac);
    bus.write16(CPLUS_CMD, cplus);
    flush(bus);
    modify32(bus, RX_CONFIG, RX_ACCEPT_ERROR | RX_ACCEPT_RUNT, 0);
    modify8_if_changed(bus, PMCH, 0, PMCH_D3HOT_NO_PLL_DOWN);
    modify8_if_changed(bus, PMCH, PMCH_D3COLD_NO_PLL_DOWN, 0);
    Ok((Probed { mac, cplus }, stalls))
}

/// Stop DMA and interrupts and reset the chip: what both a restart and
/// shutdown begin with. FIFOs that never drain still get the reset.
pub fn stop<B: RegisterBus>(bus: &mut B) -> Result<Stalls> {
    mask_and_ack_interrupts(bus);
    modify32(bus, RX_CONFIG, 0x3f, 0);
    let mut stalls = Stalls::default();
    gate_rx(bus, &mut stalls);
    bus.delay_us(2000);
    reset(bus)?;
    Ok(stalls)
}

fn phy_param<B: RegisterBus>(bus: &mut B, param: u16, clear: u16, set: u16) -> Result<()> {
    phy_ocp_write(bus, paged_reg(0x0a43, 0x13), param)?;
    phy_ocp_modify(bus, paged_reg(0x0a43, 0x14), clear, set)
}

fn adc_bias_offset<B: RegisterBus>(bus: &mut B) -> u16 {
    mac_ocp_write(bus, 0xdd02, 0x807d);
    let control = mac_ocp_read(bus, 0xdd02);
    let data = mac_ocp_read(bus, 0xdd00);
    let mut offset = (data >> 1) & 0x7ff8 | data & 0x0007;
    if control & (1 << 7) != 0 {
        offset |= 1 << 15;
    }
    offset
}

/// Wake the PHY: clear power-down and give it 20 ms to come up.
fn phy_power_up<B: RegisterBus>(bus: &mut B) -> Result<()> {
    phy_ocp_modify(bus, mii_reg(mii::BMCR), mii::BMCR_POWER_DOWN, 0)?;
    bus.delay_us(20_000);
    Ok(())
}

/// The PHY's analog settings for this version, its 10 Mb/s PLL-off and
/// power-saving (PFM, ALDPS) modes off.
fn phy_configure<B: RegisterBus>(bus: &mut B) -> Result<()> {
    phy_param(bus, 0x808a, 0x003f, 0x000a)?;
    phy_param(bus, 0x0811, 0, 0x0800)?;
    phy_ocp_modify(bus, paged_reg(0x0a42, 0x16), 0, 0x0002)?;
    phy_ocp_modify(bus, paged_reg(0x0a44, 0x11), 0, 1 << 11)?;
    let offset = adc_bias_offset(bus);
    if offset != 0xffff {
        phy_ocp_write(bus, paged_reg(0x0bcf, 0x16), offset)?;
    }
    let level = phy_ocp_read(bus, paged_reg(0x0bcd, 0x16))? & 0x000f;
    let rlen = level.saturating_sub(3);
    phy_ocp_write(
        bus,
        paged_reg(0x0bcd, 0x17),
        rlen | rlen << 4 | rlen << 8 | rlen << 12,
    )?;
    phy_ocp_modify(bus, paged_reg(0x0a44, 0x11), 1 << 7, 0)?;
    phy_ocp_modify(bus, paged_reg(0x0a43, 0x10), 1 << 0, 0)?;
    phy_ocp_modify(bus, paged_reg(0x0a43, 0x10), 1 << 2, 0)?;
    phy_ocp_modify(bus, paged_reg(0x0a43, 0x11), 0, 1 << 4)
}

fn phy_soft_reset<B: RegisterBus>(bus: &mut B) -> Result<()> {
    phy_ocp_modify(
        bus,
        mii_reg(mii::BMCR),
        mii::BMCR_ISOLATE,
        mii::BMCR_RESET | mii::BMCR_AUTONEG_RESTART,
    )?;
    bus.delay_us(50_000);
    let mut failed = None;
    let done = poll(bus, Wait::PhyReset, 50_000, 12, |b| {
        match phy_ocp_read(b, mii_reg(mii::BMCR)) {
            Ok(bmcr) => bmcr & mii::BMCR_RESET == 0,
            Err(e) => {
                failed = Some(e);
                true
            }
        }
    });
    match failed {
        Some(e) => Err(e),
        None => done,
    }
}

/// Advertise every mode the PHY reports, with symmetric and asymmetric
/// pause and without EEE, and renegotiate when that changed anything.
fn phy_autoneg<B: RegisterBus>(bus: &mut B) -> Result<()> {
    let eee = phy_ocp_read(bus, PHY_EEE_ADVERTISEMENT)?;
    let eee_off = eee & !(mii::EEE_ADV_100 | mii::EEE_ADV_1000);
    let mut changed = eee_off != eee;
    if changed {
        phy_ocp_write(bus, PHY_EEE_ADVERTISEMENT, eee_off)?;
    }

    let bmsr = phy_ocp_read(bus, mii_reg(mii::BMSR))?;
    let abilities = [
        (mii::BMSR_10_HALF, mii::ANAR_10_HALF),
        (mii::BMSR_10_FULL, mii::ANAR_10_FULL),
        (mii::BMSR_100_HALF, mii::ANAR_100_HALF),
        (mii::BMSR_100_FULL, mii::ANAR_100_FULL),
    ];
    let advertise = abilities
        .iter()
        .filter(|(has, _)| bmsr & has != 0)
        .fold(mii::ANAR_PAUSE | mii::ANAR_ASYM_PAUSE, |a, (_, adv)| {
            a | adv
        });
    let all = mii::ANAR_10_HALF
        | mii::ANAR_10_FULL
        | mii::ANAR_100_HALF
        | mii::ANAR_100_FULL
        | mii::ANAR_PAUSE
        | mii::ANAR_ASYM_PAUSE;
    let anar = phy_ocp_read(bus, mii_reg(mii::ANAR))?;
    let new = (anar & !all) | advertise;
    if new != anar {
        phy_ocp_write(bus, mii_reg(mii::ANAR), new)?;
        changed = true;
    }

    if bmsr & mii::BMSR_EXTENDED_STATUS != 0 {
        let estatus = phy_ocp_read(bus, mii_reg(mii::ESTATUS))?;
        let mut gigabit = 0;
        if estatus & mii::ESTATUS_1000_FULL != 0 {
            gigabit |= mii::CTRL1000_FULL;
        }
        if estatus & mii::ESTATUS_1000_HALF != 0 {
            gigabit |= mii::CTRL1000_HALF;
        }
        let ctrl = phy_ocp_read(bus, mii_reg(mii::CTRL1000))?;
        let new = (ctrl & !(mii::CTRL1000_FULL | mii::CTRL1000_HALF)) | gigabit;
        if new != ctrl {
            phy_ocp_write(bus, mii_reg(mii::CTRL1000), new)?;
            changed = true;
        }
    }

    let bmcr = phy_ocp_read(bus, mii_reg(mii::BMCR))?;
    if changed || bmcr & mii::BMCR_AUTONEG_ENABLE == 0 || bmcr & mii::BMCR_ISOLATE != 0 {
        phy_ocp_write(
            bus,
            mii_reg(mii::BMCR),
            (bmcr & !mii::BMCR_ISOLATE) | mii::BMCR_AUTONEG_ENABLE | mii::BMCR_AUTONEG_RESTART,
        )?;
    }
    Ok(())
}

/// ASPM and CLKREQ off in the chip: the kernel does not manage the link's
/// power states.
fn aspm_clkreq_off<B: RegisterBus>(bus: &mut B) {
    mac_ocp_modify(bus, 0xe092, 0x00ff, 0);
    modify8(bus, CONFIG2, CONFIG2_CLKREQ_ENABLE, 0);
    modify8(bus, CONFIG5, CONFIG5_ASPM_ENABLE, 0);
}

const EPHY_SETTINGS: [(u8, u16, u16); 6] = [
    (0x1e, 0x0800, 0x0001),
    (0x1d, 0x0000, 0x0800),
    (0x05, 0xffff, 0x2089),
    (0x06, 0xffff, 0x5881),
    (0x04, 0xffff, 0x854a),
    (0x01, 0xffff, 0x068b),
];

/// L0s 7 us and L1 16 us entry latency, in PCI configuration byte 0x70f.
fn aspm_entry_latency<B: RegisterBus>(bus: &mut B, function: u8) -> Result<()> {
    let value = crate::bus::csi_read(bus, 0x70c, function)?;
    crate::bus::csi_write(bus, 0x70c, function, (value & 0x00ff_ffff) | 0x27 << 24)
}

/// This version's MAC, EPHY and FIFO settings.
fn configure_mac<B: RegisterBus>(bus: &mut B, function: u8) -> Result<()> {
    for (reg, clear, set) in EPHY_SETTINGS {
        let value = crate::bus::ephy_read(bus, reg)?;
        crate::bus::ephy_write(bus, reg, (value & !clear) | set)?;
    }
    eri_write(bus, 0xc8, EriMask::All, 0x08 << 16 | 0x02)?;
    eri_write(bus, 0xe8, EriMask::All, 0x10 << 16 | 0x06)?;
    eri_write(bus, 0xcc, EriMask::Byte0, 0x38)?;
    eri_write(bus, 0xd0, EriMask::Byte0, 0x48)?;
    aspm_entry_latency(bus, function)?;
    eri_modify(bus, 0xdc, 1 << 0, 0)?;
    eri_modify(bus, 0xdc, 0, 1 << 0)?;
    eri_modify(bus, 0xdc, 0, 0x001c)?;
    eri_write(bus, 0x5f0, EriMask::Bytes01, 0x4f87)?;
    modify32(bus, MISC, MISC_RXDV_GATED, 0);
    eri_write(bus, 0xc0, EriMask::Bytes01, 0)?;
    eri_write(bus, 0xb8, EriMask::Bytes01, 0)?;
    modify8(bus, DLLPR, DLLPR_PFM_ENABLE, 0);
    modify8(bus, MISC_1, MISC_1_PFM_D3COLD_ENABLE, 0);
    modify8(bus, DLLPR, DLLPR_TX_10M_PS_ENABLE, 0);
    eri_modify(bus, 0x1b0, 1 << 12, 0)?;
    modify8(bus, CONFIG3, CONFIG3_READY_TO_L23, 0);

    let saw_count = phy_ocp_read(bus, paged_reg(0x0c42, 0x13))? & 0x3fff;
    if saw_count > 0 {
        let per_ms = ((16_000_000 / u32::from(saw_count)) & 0x0fff) as u16;
        mac_ocp_modify(bus, 0xd412, 0x0fff, per_ms);
    }
    mac_ocp_modify(bus, 0xe056, 0x00f0, 0);
    mac_ocp_modify(bus, 0xe052, 0x6000, 0x8008);
    mac_ocp_modify(bus, 0xe0d6, 0x01ff, 0x017f);
    mac_ocp_modify(bus, 0xd420, 0x0fff, 0x047f);
    mac_ocp_write(bus, 0xe63e, 0x0001);
    mac_ocp_write(bus, 0xe63e, 0x0000);
    mac_ocp_write(bus, 0xc094, 0x0000);
    mac_ocp_write(bus, 0xc09e, 0x0000);
    Ok(())
}

fn start<B: RegisterBus>(
    bus: &mut B,
    probed: &Probed,
    rings: RingAddresses,
    function: u8,
) -> Result<()> {
    bus.write8(CFG9346, CFG9346_UNLOCK);
    aspm_clkreq_off(bus);
    bus.write16(CPLUS_CMD, probed.cplus);
    mac_ocp_write(bus, 0xe048, EEE_TX_IDLE);
    bus.write8(MAX_TX_PACKET_SIZE, MAX_TX_PACKET);
    configure_mac(bus, function)?;
    bus.write16(INTR_MITIGATE, 0);
    mac_ocp_modify(bus, 0xc0ac, 0, 0x1f80);
    aspm_clkreq_off(bus);
    bus.write16(RX_MAX_SIZE, rings.rx_buffer_len);
    bus.write32(TX_DESC_HIGH, (rings.tx >> 32) as u32);
    bus.write32(TX_DESC_LOW, rings.tx as u32);
    bus.write32(RX_DESC_HIGH, (rings.rx >> 32) as u32);
    bus.write32(RX_DESC_LOW, rings.rx as u32);
    bus.write8(CFG9346, CFG9346_LOCK);
    flush(bus);

    bus.write8(CHIP_CMD, CMD_TX_ENABLE | CMD_RX_ENABLE);
    bus.write32(RX_CONFIG, RX_CONFIG_BASE);
    bus.write32(TX_CONFIG, TX_CONFIG_VALUE);
    modify32(bus, RX_CONFIG, RX_ACCEPT_ERROR | RX_ACCEPT_RUNT, 0);
    bus.write32(MAR0 + 4, u32::MAX);
    bus.write32(MAR0, u32::MAX);
    modify32(bus, RX_CONFIG, 0x0f, RX_ACCEPT);
    bus.write16(INTR_MASK, INTERRUPTS);
    Ok(())
}

/// The MAC never asserts low-power idle on transmit.
fn tx_lpi_off<B: RegisterBus>(bus: &mut B) -> Result<()> {
    modify8(bus, EEE_LED, 0x07, 0);
    eri_modify(bus, 0x1b0, 0x0003, 0)
}

/// Bring the probed chip into service: power up and configure the PHY,
/// reset the MAC, call `rearm` to hand every RX descriptor back and empty
/// the TX ring while the chip is quiet, start the MAC on `rings` and start
/// autonegotiation. `function` is the NIC's PCI function number. A stall
/// `stop` carried on past is returned, not failed on.
pub fn up<B: RegisterBus>(
    bus: &mut B,
    probed: &Probed,
    rings: RingAddresses,
    function: u8,
    rearm: impl FnOnce(),
) -> Result<Stalls> {
    phy_power_up(bus)?;
    phy_configure(bus)?;
    phy_soft_reset(bus)?;
    let stalls = stop(bus)?;
    rearm();
    start(bus, probed, rings, function)?;
    phy_autoneg(bus)?;
    tx_lpi_off(bus)?;
    Ok(stalls)
}

/// Read the pending interrupt causes, then clear every cause, not only those
/// read: an event after the clear raises a fresh interrupt, and one before it
/// has its descriptor in place for a ring harvest that follows this call.
pub fn ack_interrupts<B: RegisterBus>(bus: &mut B) -> u16 {
    let status = bus.read16(INTR_STATUS);
    bus.write16(INTR_STATUS, 0xffff);
    status
}

/// The platform vendor validated PCIe ASPM with this chip (MAC OCP 0xc0b2);
/// otherwise the link's ASPM states are not safe to leave enabled.
pub fn aspm_validated<B: RegisterBus>(bus: &mut B) -> bool {
    mac_ocp_read(bus, 0xc0b2) & 0xf != 0
}

/// Have the chip fetch the normal-priority TX ring.
pub fn kick_tx<B: RegisterBus>(bus: &mut B) {
    bus.write8(TX_POLL, POLL_NORMAL_QUEUE);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speed {
    Mbps10,
    Mbps100,
    Mbps1000,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Link {
    pub up: bool,
    pub speed: Option<Speed>,
    pub full_duplex: bool,
}

impl Link {
    pub fn from_phy_status(status: u8) -> Self {
        let speed = if status & PHY_STATUS_1000 != 0 {
            Some(Speed::Mbps1000)
        } else if status & PHY_STATUS_100 != 0 {
            Some(Speed::Mbps100)
        } else if status & PHY_STATUS_10 != 0 {
            Some(Speed::Mbps10)
        } else {
            None
        };
        let up = status != 0xff && status & PHY_STATUS_LINK != 0;
        Self {
            up,
            speed: if up { speed } else { None },
            full_duplex: up && status & PHY_STATUS_FULL_DUPLEX != 0,
        }
    }
}

pub fn link<B: RegisterBus>(bus: &mut B) -> Link {
    Link::from_phy_status(bus.read8(PHY_STATUS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_status_decodes_speed_and_duplex_only_when_up() {
        let link =
            Link::from_phy_status(PHY_STATUS_LINK | PHY_STATUS_1000 | PHY_STATUS_FULL_DUPLEX);
        assert_eq!(
            link,
            Link {
                up: true,
                speed: Some(Speed::Mbps1000),
                full_duplex: true
            }
        );
        let half_100 = Link::from_phy_status(PHY_STATUS_LINK | PHY_STATUS_100);
        assert_eq!(
            (half_100.speed, half_100.full_duplex),
            (Some(Speed::Mbps100), false)
        );
        let down = Link::from_phy_status(PHY_STATUS_100 | PHY_STATUS_FULL_DUPLEX);
        assert_eq!(
            down,
            Link {
                up: false,
                speed: None,
                full_duplex: false
            }
        );
    }

    #[test]
    fn a_chip_that_reads_all_ones_has_no_link() {
        assert!(!Link::from_phy_status(0xff).up);
    }
}
