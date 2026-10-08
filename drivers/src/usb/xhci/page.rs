//! The kernel's halves of the core's traits: BAR0 and the function's
//! command register as the register bus, and zeroed pages as DMA memory.

use core::sync::atomic::{AtomicUsize, Ordering, fence};

use slopos_mm::mmio::MmioRegion;
use slopos_mm::page_alloc::OwnedPageFrame;
use slopos_usb_core::xhci::memory::PAGE_SIZE;
use slopos_usb_core::xhci::{DmaPage, RegisterBus};

use crate::pci::{disable_bus_master, enable_bus_master};
use crate::pci_defs::PciDeviceInfo;

pub(super) struct Bus<'a> {
    pub regs: &'a MmioRegion,
    pub info: &'a PciDeviceInfo,
}

impl RegisterBus for Bus<'_> {
    fn read32(&mut self, offset: usize) -> u32 {
        self.regs.read::<u32>(offset)
    }

    fn write8(&mut self, offset: usize, value: u8) {
        self.regs.write::<u8>(offset, value);
    }

    fn write32(&mut self, offset: usize, value: u32) {
        self.regs.write::<u32>(offset, value);
    }

    fn write64(&mut self, offset: usize, value: u64) {
        self.regs.write::<u64>(offset, value);
    }

    fn bus_master(&mut self, on: bool) {
        if on {
            enable_bus_master(self.info);
        } else {
            disable_bus_master(self.info);
        }
    }

    fn delay_us(&mut self, us: u32) {
        crate::hpet::delay_ns(u64::from(us) * 1000);
    }
}

/// So a test can see a removal give back what enumeration took.
static PAGES: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "test-hooks")]
pub fn pages_held() -> usize {
    PAGES.load(Ordering::Acquire)
}

pub(crate) struct Page(OwnedPageFrame);

impl Page {
    pub fn alloc() -> Option<Self> {
        let frame = OwnedPageFrame::alloc_zeroed()?;
        PAGES.fetch_add(1, Ordering::AcqRel);
        Some(Self(frame))
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        PAGES.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A device's descriptors, which no controller is pointed at.
pub(crate) struct Store(Page);

impl Store {
    pub fn alloc() -> Option<Self> {
        Page::alloc().map(Self)
    }

    pub fn bytes(&self) -> &[u8] {
        self.0.0.slice_at(0, PAGE_SIZE).unwrap_or(&[])
    }

    pub fn bytes_mut(&mut self) -> &mut [u8] {
        self.0.0.slice_at_mut(0, PAGE_SIZE).unwrap_or(&mut [])
    }
}

impl DmaPage for Page {
    fn phys(&self) -> u64 {
        self.0.phys_u64()
    }

    fn read32(&self, offset: usize) -> u32 {
        self.0.read_volatile_at::<u32>(offset).unwrap_or(0)
    }

    fn write32(&mut self, offset: usize, value: u32) {
        self.0.write_volatile_at::<u32>(offset, value);
    }

    fn write64(&mut self, offset: usize, value: u64) {
        self.0.write_volatile_at::<u64>(offset, value);
    }

    fn read_bytes(&self, offset: usize, dst: &mut [u8]) {
        if !self.0.read_slice(offset, dst) {
            dst.fill(0);
        }
    }

    fn write_bytes(&mut self, offset: usize, src: &[u8]) {
        self.0.write_slice(offset, src);
    }

    fn acquire(&self) {
        fence(Ordering::Acquire);
    }

    fn release(&self) {
        fence(Ordering::Release);
    }
}
