//! CPU feature detection via CPUID instruction.
//!
//! This module provides constants for CPUID feature flags used to detect
//! hardware capabilities like APIC, x2APIC, and other CPU features.
//!
//! Only flags actually referenced by kernel code are defined here.
//! Add new constants as needed when implementing feature detection.

// =============================================================================
// CPUID Leaf Numbers
// =============================================================================

/// Basic CPU information and feature flags.
pub const CPUID_LEAF_FEATURES: u32 = 0x01;

/// Structured extended feature flags (subleaf 0).
pub const CPUID_LEAF_STRUCTURED_EXT: u32 = 0x07;

/// Extended function information.
pub const CPUID_LEAF_EXT_INFO: u32 = 0x8000_0001;

// =============================================================================
// CPUID Leaf 1 - EDX Feature Flags
// =============================================================================

/// Physical Address Extension.
pub const CPUID_FEAT_EDX_PAE: u32 = 1 << 6;

/// APIC present (on-chip Advanced Programmable Interrupt Controller).
pub const CPUID_FEAT_EDX_APIC: u32 = 1 << 9;

/// Page Global Enable.
pub const CPUID_FEAT_EDX_PGE: u32 = 1 << 13;

/// Page Attribute Table.
pub const CPUID_FEAT_EDX_PAT: u32 = 1 << 16;

// =============================================================================
// CPUID Leaf 1 - ECX Feature Flags
// =============================================================================

/// Process Context Identifiers (PCID).
pub const CPUID_FEAT_ECX_PCID: u32 = 1 << 17;

/// x2APIC support.
pub const CPUID_FEAT_ECX_X2APIC: u32 = 1 << 21;

// =============================================================================
// CPUID Leaf 7 (Subleaf 0) - EBX Structured Extended Feature Flags
// =============================================================================

/// INVPCID instruction support.
pub const CPUID_SEXT_EBX_INVPCID: u32 = 1 << 10;

// =============================================================================
// CPUID Extended Leaf 0x80000001 - EDX Flags
// =============================================================================

/// Long mode (64-bit).
pub const CPUID_EXT_FEAT_EDX_LM: u32 = 1 << 29;
