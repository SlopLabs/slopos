//! The controller as the bring-up sees it: BAR0, the function's bus-master
//! enable and a clock to wait by.

pub trait RegisterBus {
    fn read32(&mut self, offset: usize) -> u32;
    fn write8(&mut self, offset: usize, value: u8);
    fn write32(&mut self, offset: usize, value: u32);
    /// One qword store, as §5.1 asks of a 64-bit controller.
    fn write64(&mut self, offset: usize, value: u64);
    /// Bus Master Enable in the function's PCI command register.
    fn bus_master(&mut self, on: bool);
    fn delay_us(&mut self, us: u32);
}

/// A condition the controller did not reach within its bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    /// The firmware clearing BIOS Owned after the OS asked for the controller.
    BiosRelease,
    /// Controller Not Ready clearing.
    Ready,
    /// HCHalted setting after Run/Stop is cleared.
    Halt,
    /// HCRST clearing.
    Reset,
    /// HCHalted clearing after Run/Stop is set.
    Run,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Timeout(Wait),
    /// The registers read all ones: the function left the bus.
    Absent,
}

pub type Result<T, E = Error> = core::result::Result<T, E>;

/// Check `done` every `interval_us` for `bound_ms`; `None` from it means
/// the controller is gone.
pub(crate) fn poll<B: RegisterBus>(
    bus: &mut B,
    wait: Wait,
    interval_us: u32,
    bound_ms: u32,
    mut done: impl FnMut(&mut B) -> Option<bool>,
) -> Result<()> {
    let tries = (bound_ms * 1000).div_ceil(interval_us).max(1);
    for _ in 0..tries {
        match done(bus) {
            None => return Err(Error::Absent),
            Some(true) => return Ok(()),
            Some(false) => bus.delay_us(interval_us),
        }
    }
    match done(bus) {
        None => Err(Error::Absent),
        Some(true) => Ok(()),
        Some(false) => Err(Error::Timeout(wait)),
    }
}
