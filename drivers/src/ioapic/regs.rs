//! I/O APIC hardware definitions.
//!
//! Register offsets, redirection entry flags, and capacity limits.
//! Internal to the IOAPIC driver; ACPI MADT parsing lives in `slopos_acpi::madt`.

// =============================================================================
// Size and Capacity Limits
// =============================================================================

pub(crate) const IOAPIC_MAX_CONTROLLERS: usize = 8;
pub(crate) const IOAPIC_MAX_ISO_ENTRIES: usize = 32;

// =============================================================================
// Register Offsets
// =============================================================================

pub(crate) const IOAPIC_REG_VER: u8 = 0x01;
pub(crate) const IOAPIC_REG_REDIR_BASE: u8 = 0x10;

// =============================================================================
// Register Masks
// =============================================================================

/// Writable bits in redirection entry low dword.
/// Includes: delivery mode (8-10), dest mode (11), polarity (13), trigger (15), mask (16).
pub(crate) const IOAPIC_REDIR_WRITABLE_MASK: u32 =
    (7 << 8) | (1 << 11) | (1 << 13) | (1 << 15) | (1 << 16);

// =============================================================================
// Redirection Entry Flags
// =============================================================================

pub(crate) const IOAPIC_FLAG_DELIVERY_FIXED: u32 = 0u32 << 8;
pub(crate) const IOAPIC_FLAG_DEST_PHYSICAL: u32 = 0u32 << 11;
pub(crate) const IOAPIC_FLAG_POLARITY_HIGH: u32 = 0u32 << 13;
pub(crate) const IOAPIC_FLAG_POLARITY_LOW: u32 = 1u32 << 13;
pub(crate) const IOAPIC_FLAG_TRIGGER_EDGE: u32 = 0u32 << 15;
pub(crate) const IOAPIC_FLAG_TRIGGER_LEVEL: u32 = 1u32 << 15;
pub(crate) const IOAPIC_FLAG_MASK: u32 = 1u32 << 16;
