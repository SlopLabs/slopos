//! USB mass storage: Bulk-Only Transport's wrappers, the SCSI commands a disk
//! is driven with, and the transport that carries them over a device's bulk
//! pipes and recovers it.

pub mod bot;
pub mod scsi;
pub mod transport;

#[cfg(test)]
mod tests;

pub const CLASS: u8 = 0x08;
pub const SUBCLASS_SCSI: u8 = 0x06;
pub const PROTOCOL_BULK_ONLY: u8 = 0x50;
