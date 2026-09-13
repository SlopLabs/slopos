use slopos_abi::Errno;
use slopos_abi::fs::UserIovec;
use slopos_abi::io::{IoBufRead, IoBufWrite};
use slopos_ostd::KVec;

use crate::user_copy::{copy_bytes_from_user, copy_bytes_to_user};
use crate::user_ptr::{UserBytes, UserVirtAddr};

/// Allocate a kernel buffer and copy user data into it in one step.
///
/// `EINVAL` above `max_size`, `ENOMEM` if the allocation fails (never
/// panics), `EFAULT` if the copy fails.
pub fn memdup_user(addr: u64, len: usize, max_size: usize) -> Result<KVec<u8>, Errno> {
    if len > max_size {
        return Err(Errno::EINVAL);
    }
    let user_bytes = UserBytes::try_new(addr, len).map_err(|_| Errno::EFAULT)?;
    let mut buf = KVec::<u8>::zeroed(len).map_err(|_| Errno::ENOMEM)?;
    copy_bytes_from_user(user_bytes, &mut buf).map_err(|_| Errno::EFAULT)?;
    Ok(buf)
}

/// The upfront `access_ok()` equivalent: rejects null, non-canonical,
/// kernel-space and overflowing ranges before any I/O buffer is constructed.
/// Individual copies still re-validate via `UserBytes::try_new`.
fn validate_user_range(addr: u64, len: usize) -> Result<(), Errno> {
    if len == 0 {
        return Ok(());
    }
    UserVirtAddr::try_new(addr, len).map_err(|_| Errno::EFAULT)?;
    Ok(())
}

pub struct UserReadBuf {
    addr: u64,
    len: usize,
}

impl UserReadBuf {
    pub fn new(addr: u64, len: usize) -> Option<Self> {
        validate_user_range(addr, len).ok()?;
        Some(Self { addr, len })
    }
}

impl IoBufRead for UserReadBuf {
    fn copy_out(&self, offset: usize, dst: &mut [u8]) -> Result<usize, Errno> {
        if offset >= self.len {
            return Ok(0);
        }
        let remaining = self.len - offset;
        let n = dst.len().min(remaining);
        if n == 0 {
            return Ok(0);
        }
        let source_addr = self.addr.checked_add(offset as u64).ok_or(Errno::EFAULT)?;
        let user_bytes = UserBytes::try_new(source_addr, n).map_err(|_| Errno::EFAULT)?;
        copy_bytes_from_user(user_bytes, &mut dst[..n]).map_err(|_| Errno::EFAULT)
    }

    fn len(&self) -> usize {
        self.len
    }
}

pub struct UserWriteBuf {
    addr: u64,
    len: usize,
}

impl UserWriteBuf {
    pub fn new(addr: u64, len: usize) -> Option<Self> {
        validate_user_range(addr, len).ok()?;
        Some(Self { addr, len })
    }
}

impl IoBufWrite for UserWriteBuf {
    fn copy_in(&mut self, offset: usize, src: &[u8]) -> Result<usize, Errno> {
        if offset >= self.len {
            return Ok(0);
        }
        let remaining = self.len - offset;
        let n = src.len().min(remaining);
        if n == 0 {
            return Ok(0);
        }
        let target_addr = self.addr.checked_add(offset as u64).ok_or(Errno::EFAULT)?;
        let user_bytes = UserBytes::try_new(target_addr, n).map_err(|_| Errno::EFAULT)?;
        copy_bytes_to_user(user_bytes, &src[..n]).map_err(|_| Errno::EFAULT)
    }

    fn len(&self) -> usize {
        self.len
    }
}

/// One segment of a vectored transfer, plus where it starts in the flattened
/// byte stream so a chunk lands with a binary search rather than a walk.
struct IovecSeg {
    addr: u64,
    len: usize,
    start: usize,
}

/// A `readv`/`writev` segment list presented as one flat buffer.
///
/// Each chunk re-validates the exact range it touches: the upfront pass
/// rejects an obviously bad descriptor array, it does not license a later copy
/// against a mapping that has since gone.
pub struct UserIovecBuf {
    segs: KVec<IovecSeg>,
    total: usize,
}

impl UserIovecBuf {
    /// `EFAULT` for a segment that is not a valid user range, `EINVAL` when the
    /// lengths sum past `isize::MAX`, `ENOMEM` when the list does not fit.
    /// Zero-length segments are dropped; an all-zero total is a legal empty
    /// transfer, not an error.
    pub fn new(iov: &[UserIovec]) -> Result<Self, Errno> {
        let mut segs = KVec::<IovecSeg>::with_capacity(iov.len()).map_err(|_| Errno::ENOMEM)?;
        let mut total = 0usize;
        for entry in iov {
            let len = usize::try_from(entry.iov_len).map_err(|_| Errno::EINVAL)?;
            if len == 0 {
                continue;
            }
            validate_user_range(entry.iov_base, len)?;
            let next = total.checked_add(len).ok_or(Errno::EINVAL)?;
            if next > isize::MAX as usize {
                return Err(Errno::EINVAL);
            }
            segs.push(IovecSeg {
                addr: entry.iov_base,
                len,
                start: total,
            })
            .map_err(|_| Errno::ENOMEM)?;
            total = next;
        }
        Ok(Self { segs, total })
    }

    /// Index of the segment covering `offset`, or `None` past the end.
    fn seg_at(&self, offset: usize) -> Option<usize> {
        if offset >= self.total {
            return None;
        }
        let idx = self
            .segs
            .partition_point(|seg| seg.start + seg.len <= offset);
        (idx < self.segs.len()).then_some(idx)
    }
}

impl IoBufRead for UserIovecBuf {
    fn copy_out(&self, offset: usize, dst: &mut [u8]) -> Result<usize, Errno> {
        let Some(mut idx) = self.seg_at(offset) else {
            return Ok(0);
        };
        let mut within = offset - self.segs[idx].start;
        let mut done = 0usize;
        while done < dst.len() && idx < self.segs.len() {
            let seg = &self.segs[idx];
            let n = (seg.len - within).min(dst.len() - done);
            let addr = seg.addr.checked_add(within as u64).ok_or(Errno::EFAULT)?;
            let user_bytes = UserBytes::try_new(addr, n).map_err(|_| Errno::EFAULT)?;
            copy_bytes_from_user(user_bytes, &mut dst[done..done + n])
                .map_err(|_| Errno::EFAULT)?;
            done += n;
            idx += 1;
            within = 0;
        }
        Ok(done)
    }

    fn len(&self) -> usize {
        self.total
    }
}

impl IoBufWrite for UserIovecBuf {
    fn copy_in(&mut self, offset: usize, src: &[u8]) -> Result<usize, Errno> {
        let Some(mut idx) = self.seg_at(offset) else {
            return Ok(0);
        };
        let mut within = offset - self.segs[idx].start;
        let mut done = 0usize;
        while done < src.len() && idx < self.segs.len() {
            let seg = &self.segs[idx];
            let n = (seg.len - within).min(src.len() - done);
            let addr = seg.addr.checked_add(within as u64).ok_or(Errno::EFAULT)?;
            let user_bytes = UserBytes::try_new(addr, n).map_err(|_| Errno::EFAULT)?;
            copy_bytes_to_user(user_bytes, &src[done..done + n]).map_err(|_| Errno::EFAULT)?;
            done += n;
            idx += 1;
            within = 0;
        }
        Ok(done)
    }

    fn len(&self) -> usize {
        self.total
    }
}
