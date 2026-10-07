//! The kernel's halves of the core's traits: BAR0 and the function's
//! command register as the register bus, and zeroed pages as DMA memory.

use core::sync::atomic::{Ordering, fence};

use slopos_mm::mmio::MmioRegion;
use slopos_mm::page_alloc::OwnedPageFrame;
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

pub(super) struct Page(OwnedPageFrame);

impl Page {
    pub fn alloc() -> Option<Self> {
        OwnedPageFrame::alloc_zeroed().map(Self)
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

    fn acquire(&self) {
        fence(Ordering::Acquire);
    }

    fn release(&self) {
        fence(Ordering::Release);
    }
}
