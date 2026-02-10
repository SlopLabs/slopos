//! PCI bus hardware definitions, configuration space constants, and device structures.
//!
//! Single source of truth for PCI constants used across the driver subsystem.
//! Add new constants here only when a consumer exists.
//!
//! All PCI structures are `#[repr(C)]` for ABI stability between kernel subsystems.

// =============================================================================
// Configuration Space Register Offsets
// =============================================================================

/// Vendor ID register offset (16-bit).
pub const PCI_VENDOR_ID_OFFSET: u8 = 0x00;

/// Device ID register offset (16-bit).
pub const PCI_DEVICE_ID_OFFSET: u8 = 0x02;

/// Command register offset (16-bit).
pub const PCI_COMMAND_OFFSET: u8 = 0x04;

/// Status register offset (16-bit).
pub const PCI_STATUS_OFFSET: u8 = 0x06;

/// Revision ID register offset (8-bit).
pub const PCI_REVISION_ID_OFFSET: u8 = 0x08;

/// Programming Interface offset (8-bit).
pub const PCI_PROG_IF_OFFSET: u8 = 0x09;

/// Subclass register offset (8-bit).
pub const PCI_SUBCLASS_OFFSET: u8 = 0x0A;

/// Class Code register offset (8-bit).
pub const PCI_CLASS_CODE_OFFSET: u8 = 0x0B;

/// Header Type register offset (8-bit).
pub const PCI_HEADER_TYPE_OFFSET: u8 = 0x0E;

/// Base Address Register 0 offset.
pub const PCI_BAR0_OFFSET: u8 = 0x10;

/// Capabilities pointer offset (8-bit, header type 0).
pub const PCI_CAP_PTR_OFFSET: u8 = 0x34;

/// Interrupt Line register offset (8-bit).
pub const PCI_INTERRUPT_LINE_OFFSET: u8 = 0x3C;

/// Interrupt Pin register offset (8-bit).
pub const PCI_INTERRUPT_PIN_OFFSET: u8 = 0x3D;

// =============================================================================
// Status Register Bits
// =============================================================================

/// Status: Capabilities list present (bit 4).
pub const PCI_STATUS_CAP_LIST: u16 = 0x10;

// =============================================================================
// Command Register Bits
// =============================================================================

/// Enable memory space access (bit 1).
pub const PCI_COMMAND_MEMORY_SPACE: u16 = 0x0002;

/// Enable bus master capability (bit 2).
pub const PCI_COMMAND_BUS_MASTER: u16 = 0x0004;

// =============================================================================
// Device Classes
// =============================================================================

/// Display controller.
pub const PCI_CLASS_DISPLAY: u8 = 0x03;

// =============================================================================
// Capability IDs
// =============================================================================

/// PCI Capability ID: Vendor-specific.
pub const PCI_CAP_ID_VNDR: u8 = 0x09;

// =============================================================================
// Known Vendor IDs
// =============================================================================

/// VirtIO vendor ID (Red Hat).
pub const PCI_VENDOR_ID_VIRTIO: u16 = 0x1AF4;

/// Invalid vendor ID (no device present).
pub const PCI_VENDOR_ID_INVALID: u16 = 0xFFFF;

// =============================================================================
// Enumeration Limits
// =============================================================================

/// Maximum number of PCI buses.
pub const PCI_MAX_BUSES: usize = 256;

/// Maximum tracked PCI devices.
pub const PCI_MAX_DEVICES: usize = 256;

/// Maximum registered PCI drivers.
pub const PCI_DRIVER_MAX: usize = 16;

/// Maximum number of BARs per device.
pub const PCI_MAX_BARS: usize = 6;

// =============================================================================
// PCI Device Structures
// =============================================================================

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct PciBarInfo {
    pub base: u64,
    pub size: u64,
    pub is_io: u8,
    pub is_64bit: u8,
    pub prefetchable: u8,
}

impl PciBarInfo {
    pub const fn zeroed() -> Self {
        Self {
            base: 0,
            size: 0,
            is_io: 0,
            is_64bit: 0,
            prefetchable: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct PciDeviceInfo {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class_code: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub revision: u8,
    pub header_type: u8,
    pub irq_line: u8,
    pub irq_pin: u8,
    pub bar_count: u8,
    pub bars: [PciBarInfo; PCI_MAX_BARS],
}

impl PciDeviceInfo {
    pub const fn zeroed() -> Self {
        Self {
            bus: 0,
            device: 0,
            function: 0,
            vendor_id: 0,
            device_id: 0,
            class_code: 0,
            subclass: 0,
            prog_if: 0,
            revision: 0,
            header_type: 0,
            irq_line: 0,
            irq_pin: 0,
            bar_count: 0,
            bars: [PciBarInfo::zeroed(); PCI_MAX_BARS],
        }
    }
}
