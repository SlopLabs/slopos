//! I/O APIC hardware definitions.
//!
//! This module provides constants and types for I/O APIC hardware configuration:
//! register offsets, redirection entry flags, and capacity limits.
//!
//! ACPI MADT parsing (polarity/trigger decoding, entry type IDs) lives in
//! `slopos_acpi::madt` — not here.

// =============================================================================
// Size and Capacity Limits
// =============================================================================

/// Maximum number of IOAPIC controllers.
pub const IOAPIC_MAX_CONTROLLERS: usize = 8;

/// Maximum Interrupt Source Override entries.
pub const IOAPIC_MAX_ISO_ENTRIES: usize = 32;

// =============================================================================
// Register Offsets
// =============================================================================

/// Version register index.
pub const IOAPIC_REG_VER: u8 = 0x01;

/// Redirection table base register index.
pub const IOAPIC_REG_REDIR_BASE: u8 = 0x10;

// =============================================================================
// Register Masks
// =============================================================================

/// Writable bits in redirection entry low dword.
/// Includes: delivery mode (8-10), dest mode (11), polarity (13), trigger (15), mask (16).
pub const IOAPIC_REDIR_WRITABLE_MASK: u32 =
    (7 << 8) | (1 << 11) | (1 << 13) | (1 << 15) | (1 << 16);

// =============================================================================
// Redirection Entry Flags - Delivery Mode (bits [10:8])
// =============================================================================

/// Fixed delivery mode.
pub const IOAPIC_FLAG_DELIVERY_FIXED: u32 = 0u32 << 8;

// =============================================================================
// Redirection Entry Flags - Destination Mode (bit 11)
// =============================================================================

/// Physical destination mode.
pub const IOAPIC_FLAG_DEST_PHYSICAL: u32 = 0u32 << 11;

// =============================================================================
// Redirection Entry Flags - Polarity (bit 13)
// =============================================================================

/// Active high polarity.
pub const IOAPIC_FLAG_POLARITY_HIGH: u32 = 0u32 << 13;

/// Active low polarity.
pub const IOAPIC_FLAG_POLARITY_LOW: u32 = 1u32 << 13;

// =============================================================================
// Redirection Entry Flags - Trigger Mode (bit 15)
// =============================================================================

/// Edge-triggered.
pub const IOAPIC_FLAG_TRIGGER_EDGE: u32 = 0u32 << 15;

/// Level-triggered.
pub const IOAPIC_FLAG_TRIGGER_LEVEL: u32 = 1u32 << 15;

// =============================================================================
// Redirection Entry Flags - Mask (bit 16)
// =============================================================================

/// Interrupt masked.
pub const IOAPIC_FLAG_MASK: u32 = 1u32 << 16;
