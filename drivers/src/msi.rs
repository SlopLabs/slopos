//! MSI (Message Signaled Interrupts) support for PCI devices.
//!
//! An MSI device delivers an interrupt by writing a message straight to the
//! LAPIC, with no IOAPIC redirection. Register layout per PCI Local Bus Spec
//! §6.8. Drivers reach it through [`crate::driver_core::msi`], which allocates
//! the vector and owns the binding.

use crate::msi_common;
use crate::pci::{pci_config_read16, pci_config_write16, pci_config_write32};
use slopos_ostd::klog_info;

const MSI_CTRL_ENABLE: u16 = 1 << 0;
const MSI_CTRL_MMC_SHIFT: u16 = 1;
const MSI_CTRL_MME_MASK: u16 = 0x7 << 4;
const MSI_CTRL_64BIT: u16 = 1 << 7;
const MSI_CTRL_PVM: u16 = 1 << 8;

const MSI_REG_CONTROL: u16 = 0x02;
const MSI_REG_ADDR_LO: u16 = 0x04;
const MSI_REG_ADDR_HI: u16 = 0x08;
const MSI_REG_DATA_32: u16 = 0x08;
const MSI_REG_DATA_64: u16 = 0x0C;
const MSI_REG_MASK_32: u16 = 0x10;
const MSI_REG_MASK_64: u16 = 0x14;

/// Parsed MSI capability information for a PCI device.
#[derive(Debug, Clone, Copy)]
pub struct MsiCapability {
    /// Byte offset of the MSI capability in PCI config space.
    pub cap_offset: u16,
    /// Raw Message Control register value at parse time.
    pub control: u16,
    /// Whether the device supports 64-bit message addresses.
    pub is_64bit: bool,
    pub has_per_vector_masking: bool,
    /// log₂ of the maximum vectors the device can generate (0–5 → 1–32).
    pub multi_message_capable: u8,
}

impl MsiCapability {
    /// Maximum number of vectors the device can generate (1, 2, 4, 8, 16, or 32).
    #[inline]
    pub const fn max_vectors(&self) -> u8 {
        1u8 << self.multi_message_capable
    }

    /// Config-space offset of the Message Data register.
    #[inline]
    const fn data_offset(&self) -> u16 {
        if self.is_64bit {
            self.cap_offset + MSI_REG_DATA_64
        } else {
            self.cap_offset + MSI_REG_DATA_32
        }
    }

    /// Config-space offset of the Mask Bits register, if supported.
    #[inline]
    pub const fn mask_offset(&self) -> Option<u16> {
        if !self.has_per_vector_masking {
            return None;
        }
        Some(if self.is_64bit {
            self.cap_offset + MSI_REG_MASK_64
        } else {
            self.cap_offset + MSI_REG_MASK_32
        })
    }

    #[inline]
    pub const fn is_enabled(&self) -> bool {
        (self.control & MSI_CTRL_ENABLE) != 0
    }
}

/// Errors that can occur during MSI configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsiError {
    /// The supplied vector number is below 32 (reserved for CPU exceptions).
    InvalidVector,
    NoCapability,
}

/// Read and parse the MSI capability structure for a PCI device.
///
/// `cap_offset` is the config-space byte offset of the capability header, from
/// [`PciDeviceInfo::msi_cap_offset`] or [`pci_find_capability`].
pub fn msi_read_capability(bus: u8, dev: u8, func: u8, cap_offset: u16) -> MsiCapability {
    let control = pci_config_read16(bus, dev, func, cap_offset + MSI_REG_CONTROL);
    MsiCapability {
        cap_offset,
        control,
        is_64bit: (control & MSI_CTRL_64BIT) != 0,
        has_per_vector_masking: (control & MSI_CTRL_PVM) != 0,
        multi_message_capable: ((control >> MSI_CTRL_MMC_SHIFT) & 0x7) as u8,
    }
}

/// Configure MSI for a PCI device to deliver a single interrupt.
///
/// Programs the MSI capability registers so that the device writes an MSI
/// message targeting `target_apic_id` with interrupt `vector`.  Legacy INTx
/// is disabled and MSI is enabled last.
///
/// # Errors
///
/// Returns [`MsiError::InvalidVector`] if `vector < 32`.
pub fn msi_configure(
    bus: u8,
    dev: u8,
    func: u8,
    cap: &MsiCapability,
    vector: u8,
    target_apic_id: u8,
) -> Result<(), MsiError> {
    if !msi_common::is_valid_vector(vector) {
        return Err(MsiError::InvalidVector);
    }

    let cap_off = cap.cap_offset;

    let mut ctrl = pci_config_read16(bus, dev, func, cap_off + MSI_REG_CONTROL);
    ctrl &= !MSI_CTRL_ENABLE;
    pci_config_write16(bus, dev, func, cap_off + MSI_REG_CONTROL, ctrl);

    pci_config_write32(
        bus,
        dev,
        func,
        cap_off + MSI_REG_ADDR_LO,
        msi_common::lapic_msg_addr(target_apic_id),
    );

    // Message Upper Address is always 0 on x86.
    if cap.is_64bit {
        pci_config_write32(bus, dev, func, cap_off + MSI_REG_ADDR_HI, 0);
    }

    pci_config_write16(
        bus,
        dev,
        func,
        cap.data_offset(),
        msi_common::lapic_msg_data(vector) as u16,
    );

    // Multi-message enable = 0 requests exactly one vector.
    ctrl = pci_config_read16(bus, dev, func, cap_off + MSI_REG_CONTROL);
    ctrl &= !MSI_CTRL_ENABLE;
    ctrl &= !MSI_CTRL_MME_MASK;
    pci_config_write16(bus, dev, func, cap_off + MSI_REG_CONTROL, ctrl);

    msi_common::disable_intx(bus, dev, func);

    ctrl |= MSI_CTRL_ENABLE;
    pci_config_write16(bus, dev, func, cap_off + MSI_REG_CONTROL, ctrl);

    klog_info!(
        "MSI: Configured BDF {}:{}.{} -> vector 0x{:02x}, APIC ID {}{}{}",
        bus,
        dev,
        func,
        vector,
        target_apic_id,
        if cap.is_64bit { ", 64-bit" } else { "" },
        if cap.has_per_vector_masking {
            ", PVM"
        } else {
            ""
        },
    );

    Ok(())
}
