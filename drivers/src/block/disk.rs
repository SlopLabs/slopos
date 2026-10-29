//! A disk as the rest of the kernel sees it: byte-addressed spans over the
//! logical blocks one namespace of an [`Engine`] serves. A span that starts or
//! ends inside a block is completed through a read-modify-write of that block.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use slopos_boot_core::disk_id::DiskId;
use slopos_fs::blockdev::{BlockDevice, BlockDeviceError, WriteTicket, stats};
use slopos_kernel_services::clock::monotonic_ns;
use slopos_ostd::KArc;

use super::engine::{BlkError, Engine, Namespace, SegCursor};

pub struct EngineDisk {
    engine: KArc<Engine>,
    ns: Namespace,
    capacity: u64,
    flushes: bool,
    write_protected: bool,
    id: Option<KArc<DiskId>>,
    counters: Counters,
}

/// Counted here, apart from the machine-wide [`stats`], which tests reset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiskActivity {
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub read_requests: u64,
    pub write_requests: u64,
    pub flushes: u64,
    pub busy_ns: u64,
}

/// `busy_ns` is Linux's `io_ticks`: each start or end folds the span since the
/// last into it when a request was in flight.
struct Counters {
    read_bytes: AtomicU64,
    write_bytes: AtomicU64,
    read_requests: AtomicU64,
    write_requests: AtomicU64,
    flushes: AtomicU64,
    in_flight: AtomicU32,
    stamp: AtomicU64,
    busy_ns: AtomicU64,
}

impl Counters {
    const fn new() -> Self {
        Self {
            read_bytes: AtomicU64::new(0),
            write_bytes: AtomicU64::new(0),
            read_requests: AtomicU64::new(0),
            write_requests: AtomicU64::new(0),
            flushes: AtomicU64::new(0),
            in_flight: AtomicU32::new(0),
            stamp: AtomicU64::new(0),
            busy_ns: AtomicU64::new(0),
        }
    }

    /// Of two racing events, the compare-exchange lets one fold a span.
    fn advance(&self, now: u64, busy: bool) {
        let stamp = self.stamp.load(Ordering::Relaxed);
        if now > stamp
            && self
                .stamp
                .compare_exchange(stamp, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            && busy
        {
            self.busy_ns.fetch_add(now - stamp, Ordering::Relaxed);
        }
    }

    fn start(&self) {
        let busy = self.in_flight.load(Ordering::Relaxed) > 0;
        self.advance(monotonic_ns(), busy);
        self.in_flight.fetch_add(1, Ordering::Relaxed);
    }

    fn end(&self) {
        self.advance(monotonic_ns(), true);
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
    }

    fn read(&self, bytes: usize) {
        self.read_requests.fetch_add(1, Ordering::Relaxed);
        self.read_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    fn write(&self, bytes: usize) {
        self.write_requests.fetch_add(1, Ordering::Relaxed);
        self.write_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    fn flush(&self) {
        self.flushes.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> DiskActivity {
        let mut busy_ns = self.busy_ns.load(Ordering::Relaxed);
        if self.in_flight.load(Ordering::Relaxed) > 0 {
            busy_ns += monotonic_ns().saturating_sub(self.stamp.load(Ordering::Relaxed));
        }
        DiskActivity {
            read_bytes: self.read_bytes.load(Ordering::Relaxed),
            write_bytes: self.write_bytes.load(Ordering::Relaxed),
            read_requests: self.read_requests.load(Ordering::Relaxed),
            write_requests: self.write_requests.load(Ordering::Relaxed),
            flushes: self.flushes.load(Ordering::Relaxed),
            busy_ns,
        }
    }
}

fn total_seg_len(segs: &[&[u8]]) -> Result<usize, BlkError> {
    segs.iter()
        .try_fold(0usize, |total, seg| total.checked_add(seg.len()))
        .ok_or(BlkError::OutOfBounds)
}

impl EngineDisk {
    /// Namespace `nsid` of `engine`: `capacity` bytes in `block_size`-byte
    /// logical blocks, a power of two. `flushes` when the device has a
    /// volatile write cache a flush must empty.
    pub fn new(
        engine: KArc<Engine>,
        nsid: u32,
        block_size: u32,
        capacity: u64,
        flushes: bool,
    ) -> Self {
        Self {
            engine,
            ns: Namespace {
                nsid,
                block_shift: block_size.trailing_zeros() as u8,
            },
            capacity,
            flushes,
            write_protected: false,
            id: None,
            counters: Counters::new(),
        }
    }

    /// A medium the device refuses to write.
    pub fn protected(mut self) -> Self {
        self.write_protected = true;
        self
    }

    pub fn identified(mut self, id: Option<DiskId>) -> Self {
        self.id = id.and_then(|id| KArc::try_new(id).ok());
        self
    }

    pub fn id(&self) -> Option<KArc<DiskId>> {
        self.id.clone()
    }

    pub fn engine(&self) -> &KArc<Engine> {
        &self.engine
    }

    pub fn namespace(&self) -> Namespace {
        self.ns
    }

    pub fn activity(&self) -> DiskActivity {
        self.counters.snapshot()
    }

    fn block(&self) -> u64 {
        1 << self.ns.block_shift
    }

    fn check_span(&self, offset: u64, len: usize) -> Result<(), BlkError> {
        if !self.engine.is_ready() {
            return Err(BlkError::NotReady);
        }
        match offset.checked_add(len as u64) {
            Some(end) if end <= self.capacity => Ok(()),
            _ => Err(BlkError::OutOfBounds),
        }
    }

    /// Partial head and tail blocks come out of a slot's pages; the aligned
    /// middle transfers in requests of up to the engine's largest transfer,
    /// two in flight at a time.
    fn read_span(&self, offset: u64, buf: &mut [u8]) -> Result<(), BlkError> {
        if buf.is_empty() {
            return Ok(());
        }
        self.check_span(offset, buf.len())?;
        let block = self.block();
        let bsize = block as usize;
        let max = self.engine.max_transfer();
        let mut pos = 0usize;
        let mut at = offset;

        let head_within = (at % block) as usize;
        if head_within != 0 {
            let n = (bsize - head_within).min(buf.len());
            self.engine.read_partial(
                self.ns,
                at - head_within as u64,
                bsize,
                head_within,
                &mut buf[..n],
            )?;
            pos += n;
            at += n as u64;
        }

        while buf.len() - pos >= bsize {
            let whole = (buf.len() - pos) / bsize * bsize;
            let n = whole.min(max);
            let second = (whole - n).min(max);
            let began = slopos_ostd::arch::x86_64::tsc::rdtsc();
            if second >= bsize
                && let Ok(first) = self.engine.submit_read(self.ns, at, n)
            {
                let next = at + n as u64;
                let second_idx = self.engine.submit_read(self.ns, next, second).ok();
                let (head, tail) = buf[pos..].split_at_mut(n);
                let tail = &mut tail[..second];
                // Both are collected before either is read again: a caller
                // holding a slot must not wait for one.
                let first_done = self.engine.complete(first, head);
                let second_done = second_idx.map(|idx| self.engine.complete(idx, tail));
                self.finish_pipelined(first_done.map_err(Some), at, head, began)?;
                let second_done = second_done.map_or(Err(None), |done| done.map_err(Some));
                self.finish_pipelined(second_done, next, tail, began)?;
                pos += n + second;
                at += (n + second) as u64;
                continue;
            }
            self.engine.read(self.ns, at, &mut buf[pos..pos + n])?;
            pos += n;
            at += n as u64;
        }

        if pos < buf.len() {
            self.engine
                .read_partial(self.ns, at, bsize, 0, &mut buf[pos..])?;
        }
        self.counters.read(buf.len());
        Ok(())
    }

    /// What a pipelined request came to: done, or read again synchronously
    /// when it was never sent (`None`) or failed in a way a retry may not.
    fn finish_pipelined(
        &self,
        done: Result<(), Option<BlkError>>,
        offset: u64,
        dst: &mut [u8],
        began: u64,
    ) -> Result<(), BlkError> {
        match done {
            Ok(()) => {
                note_read_cycles(began);
                Ok(())
            }
            Err(Some(e)) if !e.retryable() => Err(e),
            Err(_) => self.engine.read(self.ns, offset, dst),
        }
    }

    /// A partial head or tail block is read-modify-written, so bytes outside
    /// the span are never clobbered; the aligned middle is gathered across
    /// segment boundaries into requests of up to the largest transfer.
    fn write_span(&self, offset: u64, segs: &[&[u8]]) -> Result<(), BlkError> {
        let total = total_seg_len(segs)?;
        if total == 0 {
            return Ok(());
        }
        self.check_span(offset, total)?;
        let block = self.block();
        let bsize = block as usize;
        let max = self.engine.max_transfer();
        let mut cur = SegCursor::new(segs);
        let mut at = offset;
        let mut left = total;

        let head_within = (at % block) as usize;
        if head_within != 0 {
            let n = (bsize - head_within).min(left);
            self.engine.read_modify_write(
                self.ns,
                at - head_within as u64,
                bsize,
                head_within,
                n,
                &mut cur,
            )?;
            at += n as u64;
            left -= n;
        }

        while left >= bsize {
            let n = (left - left % bsize).min(max);
            self.engine.write(self.ns, at, &mut cur, n)?;
            at += n as u64;
            left -= n;
        }

        if left > 0 {
            self.engine
                .read_modify_write(self.ns, at, bsize, 0, left, &mut cur)?;
        }
        self.counters.write(total);
        Ok(())
    }

    /// One whole-block write of at most the largest transfer, submitted
    /// without waiting: `Some(slot)`, or `None` when the span took the
    /// synchronous road instead.
    fn submit_write_span(&self, offset: u64, segs: &[&[u8]]) -> Result<Option<usize>, BlkError> {
        let total = total_seg_len(segs)?;
        let block = self.block();
        if total == 0
            || total > self.engine.max_transfer()
            || !offset.is_multiple_of(block)
            || !(total as u64).is_multiple_of(block)
        {
            return self.write_span(offset, segs).map(|()| None);
        }
        self.check_span(offset, total)?;
        let slot = self.engine.submit_write(self.ns, offset, segs, total)?;
        self.counters.write(total);
        Ok(Some(slot))
    }
}

fn note_read_cycles(began: u64) {
    stats::note_request_cycles(
        false,
        slopos_ostd::arch::x86_64::tsc::rdtsc().saturating_sub(began),
    );
}

impl BlockDevice for EngineDisk {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        self.counters.start();
        let read = self.read_span(offset, buffer);
        self.counters.end();
        read.map_err(Into::into)
    }

    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        self.write_vectored(offset, &[buffer])
    }

    /// Gathered into one request, so a run of contiguous kernel buffers costs
    /// one device request per largest transfer rather than one per buffer.
    fn write_vectored(&self, offset: u64, segs: &[&[u8]]) -> Result<(), BlockDeviceError> {
        self.counters.start();
        let written = self.write_span(offset, segs);
        self.counters.end();
        written.map_err(Into::into)
    }

    /// In flight until `complete_write`.
    fn submit_write(&self, offset: u64, segs: &[&[u8]]) -> Result<WriteTicket, BlockDeviceError> {
        let began = slopos_ostd::arch::x86_64::tsc::rdtsc();
        self.counters.start();
        let submitted = self.submit_write_span(offset, segs);
        if !matches!(submitted, Ok(Some(_))) {
            self.counters.end();
        }
        let tag = submitted?.map_or(WriteTicket::DONE, |slot| slot as u64);
        Ok(WriteTicket::new(tag, began))
    }

    fn complete_write(&self, ticket: WriteTicket) -> Result<(), BlockDeviceError> {
        if ticket.tag() == WriteTicket::DONE {
            return Ok(());
        }
        let completed = self.engine.complete(ticket.tag() as usize, &mut []);
        self.counters.end();
        completed?;
        stats::note_request_cycles(
            true,
            slopos_ostd::arch::x86_64::tsc::rdtsc().saturating_sub(ticket.began()),
        );
        Ok(())
    }

    fn write_depth(&self) -> usize {
        self.engine.write_depth()
    }

    fn capacity(&self) -> u64 {
        self.capacity
    }

    fn logical_block_size(&self) -> u32 {
        1 << self.ns.block_shift
    }

    fn write_protected(&self) -> bool {
        self.write_protected
    }

    /// Without a volatile write cache every acknowledged write is already
    /// durable, and there is nothing to send.
    fn flush(&self) -> Result<(), BlockDeviceError> {
        stats::note_flush();
        if !self.flushes {
            return Ok(());
        }
        self.counters.start();
        let flushed = self.engine.flush(self.ns);
        self.counters.end();
        if flushed.is_ok() {
            self.counters.flush();
        }
        flushed.map_err(Into::into)
    }
}
