//! The register window as the bring-up sees it, and the indirect ports the
//! chip puts its other address spaces behind: ERI, the MAC's and the PHY's
//! OCP spaces, the PCIe PHY (EPHY) and PCI configuration space (CSI).

use crate::regs::{
    ACCESS_FLAG, CSI_ACCESS, CSI_DATA, EPHY_ACCESS, ERI_ACCESS, ERI_DATA, MAC_OCP, PHY_OCP,
};

/// The chip's register window. The kernel maps the memory BAR; host tests
/// put a simulated chip behind it.
pub trait RegisterBus {
    fn read8(&mut self, offset: usize) -> u8;
    fn read16(&mut self, offset: usize) -> u16;
    fn read32(&mut self, offset: usize) -> u32;
    fn write8(&mut self, offset: usize, value: u8);
    fn write16(&mut self, offset: usize, value: u16);
    fn write32(&mut self, offset: usize, value: u32);
    fn delay_us(&mut self, us: u32);
}

/// A condition the chip did not reach within its bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    TxFifoEmpty,
    RxTxFifoEmpty,
    LinkListReady,
    Reset,
    Eri,
    PhyOcp,
    Ephy,
    Csi,
    PhyReset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Timeout(Wait),
}

pub type Result<T, E = Error> = core::result::Result<T, E>;

/// Check `done` up to `tries` times, `interval_us` apart.
pub(crate) fn poll<B: RegisterBus>(
    bus: &mut B,
    wait: Wait,
    interval_us: u32,
    tries: u32,
    mut done: impl FnMut(&mut B) -> bool,
) -> Result<()> {
    for _ in 0..tries {
        if done(bus) {
            return Ok(());
        }
        bus.delay_us(interval_us);
    }
    Err(Error::Timeout(wait))
}

fn flag_set<B: RegisterBus>(offset: usize) -> impl FnMut(&mut B) -> bool {
    move |bus: &mut B| bus.read32(offset) & ACCESS_FLAG != 0
}

fn flag_clear<B: RegisterBus>(offset: usize) -> impl FnMut(&mut B) -> bool {
    move |bus: &mut B| bus.read32(offset) & ACCESS_FLAG == 0
}

/// ERI byte-enable masks, bits 12..16 of the access word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EriMask {
    Byte0 = 0x1,
    Bytes01 = 0x3,
    All = 0xf,
}

pub(crate) fn eri_read<B: RegisterBus>(bus: &mut B, addr: u16) -> Result<u32> {
    debug_assert_eq!(addr % 4, 0);
    bus.write32(ERI_ACCESS, (EriMask::All as u32) << 12 | u32::from(addr));
    poll(bus, Wait::Eri, 100, 100, flag_set(ERI_ACCESS))?;
    Ok(bus.read32(ERI_DATA))
}

pub(crate) fn eri_write<B: RegisterBus>(
    bus: &mut B,
    addr: u16,
    mask: EriMask,
    value: u32,
) -> Result<()> {
    debug_assert_eq!(addr % 4, 0);
    bus.write32(ERI_DATA, value);
    bus.write32(
        ERI_ACCESS,
        ACCESS_FLAG | (mask as u32) << 12 | u32::from(addr),
    );
    poll(bus, Wait::Eri, 100, 100, flag_clear(ERI_ACCESS))
}

pub(crate) fn eri_modify<B: RegisterBus>(
    bus: &mut B,
    addr: u16,
    clear: u32,
    set: u32,
) -> Result<()> {
    let value = eri_read(bus, addr)?;
    eri_write(bus, addr, EriMask::All, (value & !clear) | set)
}

/// An OCP address word: the even register number `reg` in bits 16..31.
fn ocp_address(reg: u16) -> u32 {
    debug_assert_eq!(reg % 2, 0);
    u32::from(reg) << 15
}

pub(crate) fn mac_ocp_read<B: RegisterBus>(bus: &mut B, reg: u16) -> u16 {
    bus.write32(MAC_OCP, ocp_address(reg));
    bus.read32(MAC_OCP) as u16
}

pub(crate) fn mac_ocp_write<B: RegisterBus>(bus: &mut B, reg: u16, value: u16) {
    bus.write32(MAC_OCP, ACCESS_FLAG | ocp_address(reg) | u32::from(value));
}

pub(crate) fn mac_ocp_modify<B: RegisterBus>(bus: &mut B, reg: u16, clear: u16, set: u16) {
    let value = mac_ocp_read(bus, reg);
    mac_ocp_write(bus, reg, (value & !clear) | set);
}

pub(crate) fn phy_ocp_read<B: RegisterBus>(bus: &mut B, reg: u16) -> Result<u16> {
    bus.write32(PHY_OCP, ocp_address(reg));
    poll(bus, Wait::PhyOcp, 25, 10, flag_set(PHY_OCP))?;
    Ok(bus.read32(PHY_OCP) as u16)
}

pub(crate) fn phy_ocp_write<B: RegisterBus>(bus: &mut B, reg: u16, value: u16) -> Result<()> {
    bus.write32(PHY_OCP, ACCESS_FLAG | ocp_address(reg) | u32::from(value));
    poll(bus, Wait::PhyOcp, 25, 10, flag_clear(PHY_OCP))
}

/// Read-modify-write a PHY OCP register, skipping the write when nothing
/// changes.
pub(crate) fn phy_ocp_modify<B: RegisterBus>(
    bus: &mut B,
    reg: u16,
    clear: u16,
    set: u16,
) -> Result<()> {
    let value = phy_ocp_read(bus, reg)?;
    let new = (value & !clear) | set;
    if new != value {
        phy_ocp_write(bus, reg, new)?;
    }
    Ok(())
}

/// The OCP address of IEEE 802.3 clause 22 register `reg` of the internal
/// PHY.
pub fn mii_reg(reg: u8) -> u16 {
    0xa400 + u16::from(reg) * 2
}

/// The OCP address of register `reg` (0x10..0x18) on vendor page `page`.
pub fn paged_reg(page: u16, reg: u8) -> u16 {
    debug_assert!((0x10..0x18).contains(&reg));
    page * 16 + (u16::from(reg) - 0x10) * 2
}

pub(crate) fn ephy_read<B: RegisterBus>(bus: &mut B, reg: u8) -> Result<u16> {
    bus.write32(EPHY_ACCESS, u32::from(reg & 0x1f) << 16);
    poll(bus, Wait::Ephy, 10, 100, flag_set(EPHY_ACCESS))?;
    Ok(bus.read32(EPHY_ACCESS) as u16)
}

pub(crate) fn ephy_write<B: RegisterBus>(bus: &mut B, reg: u8, value: u16) -> Result<()> {
    bus.write32(
        EPHY_ACCESS,
        ACCESS_FLAG | u32::from(reg & 0x1f) << 16 | u32::from(value),
    );
    poll(bus, Wait::Ephy, 10, 100, flag_clear(EPHY_ACCESS))?;
    bus.delay_us(10);
    Ok(())
}

/// A CSI access word for configuration-space dword `addr` of PCI function
/// `function`, all four bytes enabled.
fn csi_address(addr: u16, function: u8) -> u32 {
    u32::from(function) << 16 | 0xf000 | u32::from(addr & 0xfff)
}

pub(crate) fn csi_read<B: RegisterBus>(bus: &mut B, addr: u16, function: u8) -> Result<u32> {
    bus.write32(CSI_ACCESS, csi_address(addr, function));
    poll(bus, Wait::Csi, 10, 100, flag_set(CSI_ACCESS))?;
    Ok(bus.read32(CSI_DATA))
}

pub(crate) fn csi_write<B: RegisterBus>(
    bus: &mut B,
    addr: u16,
    function: u8,
    value: u32,
) -> Result<()> {
    bus.write32(CSI_DATA, value);
    bus.write32(CSI_ACCESS, ACCESS_FLAG | csi_address(addr, function));
    poll(bus, Wait::Csi, 10, 100, flag_clear(CSI_ACCESS))
}
