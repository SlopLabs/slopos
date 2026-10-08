//! A disk as the rest of the kernel sees it: byte-addressed spans over the
//! logical blocks one namespace of an [`Engine`] serves. A span that starts or
//! ends inside a block is completed through a read-modify-write of that block.

use slopos_fs::blockdev::{BlockDevice, BlockDeviceError, WriteTicket, stats};
use slopos_ostd::KArc;

use super::engine::{BlkError, Engine, Namespace, SegCursor};

pub struct EngineDisk {
    engine: KArc<Engine>,
    ns: Namespace,
    capacity: u64,
    flushes: bool,
    write_protected: bool,
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
        }
    }

    /// A medium the device refuses to write.
    pub fn protected(mut self) -> Self {
        self.write_protected = true;
        self
    }

    pub fn engine(&self) -> &KArc<Engine> {
        &self.engine
    }

    pub fn namespace(&self) -> Namespace {
        self.ns
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
            let began = slopos_arch::tsc::rdtsc();
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
        self.engine
            .submit_write(self.ns, offset, segs, total)
            .map(Some)
    }
}

fn note_read_cycles(began: u64) {
    stats::note_request_cycles(false, slopos_arch::tsc::rdtsc().saturating_sub(began));
}

impl BlockDevice for EngineDisk {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        self.read_span(offset, buffer).map_err(Into::into)
    }

    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        self.write_span(offset, &[buffer]).map_err(Into::into)
    }

    /// Gathered into one request, so a run of contiguous kernel buffers costs
    /// one device request per largest transfer rather than one per buffer.
    fn write_vectored(&self, offset: u64, segs: &[&[u8]]) -> Result<(), BlockDeviceError> {
        self.write_span(offset, segs).map_err(Into::into)
    }

    fn submit_write(&self, offset: u64, segs: &[&[u8]]) -> Result<WriteTicket, BlockDeviceError> {
        let began = slopos_arch::tsc::rdtsc();
        match self.submit_write_span(offset, segs)? {
            Some(idx) => Ok(WriteTicket::new(idx as u64, began)),
            None => Ok(WriteTicket::new(WriteTicket::DONE, began)),
        }
    }

    fn complete_write(&self, ticket: WriteTicket) -> Result<(), BlockDeviceError> {
        if ticket.tag() == WriteTicket::DONE {
            return Ok(());
        }
        self.engine.complete(ticket.tag() as usize, &mut [])?;
        stats::note_request_cycles(
            true,
            slopos_arch::tsc::rdtsc().saturating_sub(ticket.began()),
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
        self.engine.flush(self.ns).map_err(Into::into)
    }
}
