//! Safe CMOS/RTC index+data register window (ports 0x70 / 0x71).
//!
//! Bit 7 of the index port is the machine-global NMI mask, and the port is
//! write-only, so there is no prior value to read back and preserve. Every
//! access here writes it clear: the lockup watchdog, the fatal-panic
//! stop-the-world and the all-CPU probe are NMI deliveries that a masked NMI
//! silently disarms.

use crate::cpu::x86_64::interrupts::IrqDisabled;
use crate::io::port::IoPort;

/// Register selector occupies bits [6:0]; bit 7 is the NMI mask.
const INDEX_MASK: u8 = 0x7F;

/// Typed handle to the CMOS index (0x70) / data (0x71) register pair.
#[derive(Clone, Copy)]
pub struct CmosRegs {
    index: IoPort<u8>,
    data: IoPort<u8>,
}

impl CmosRegs {
    #[inline]
    pub const fn new(index: IoPort<u8>, data: IoPort<u8>) -> Self {
        Self { index, data }
    }

    /// The `IrqDisabled` witness *is* the serialisation: the index register is
    /// one machine-global latch, so anything running between the index write
    /// and the data read decides which byte comes back.
    #[inline]
    pub fn read(&self, _irq: &IrqDisabled<'_>, reg: u8) -> u8 {
        // SAFETY: the PC/AT protocol for CMOS is "write the register number to
        // the index port, then read the data port"; the two together are one
        // register read and have no other effect. Bit 7 is cleared, leaving
        // NMI unmasked.
        unsafe {
            self.index.write(reg & INDEX_MASK);
            self.data.read()
        }
    }
}
