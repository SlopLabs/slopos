//! Device-visible memory: every ring, table and buffer the controller reads
//! or writes is a single zeroed 4 KiB page the kernel owns.

pub const PAGE_SIZE: usize = 4096;

/// One page the controller reaches by DMA, which it reads and writes
/// concurrently, so every access is volatile.
pub trait DmaPage {
    fn phys(&self) -> u64;
    fn read32(&self, offset: usize) -> u32;
    fn write32(&mut self, offset: usize, value: u32);
    fn write64(&mut self, offset: usize, value: u64);
    /// A load-acquire fence.
    fn acquire(&self);
    /// A store-release fence.
    fn release(&self);
}

/// Point DCBAA entry `slot` at `context`; entry 0 is the scratchpad array's.
pub fn set_device_context<P: DmaPage>(dcbaa: &mut P, slot: u8, context: u64) {
    dcbaa.write64(usize::from(slot) * 8, context);
}

/// List `buffers` in the scratchpad array and point DCBAA entry 0 at it;
/// how many it listed, at most a page's worth.
pub fn list_scratchpads<P: DmaPage>(
    dcbaa: &mut P,
    array: &mut P,
    buffers: impl IntoIterator<Item = u64>,
) -> usize {
    let mut listed = 0;
    for (i, buffer) in buffers.into_iter().take(PAGE_SIZE / 8).enumerate() {
        array.write64(i * 8, buffer);
        listed += 1;
    }
    set_device_context(dcbaa, 0, array.phys());
    listed
}

/// An Event Ring Segment Table of one entry: the segment at `segment`,
/// `trbs` TRBs long (§6.5).
pub fn write_segment_table<P: DmaPage>(table: &mut P, segment: u64, trbs: u16) {
    table.write64(0, segment);
    table.write32(8, u32::from(trbs));
    table.write32(12, 0);
}
