//! The `sendmsg`/`recvmsg` wire format as it lives in user memory.
//!
//! `msghdr` and `cmsghdr` are Linux x86-64's layouts, and every kernel path
//! that touches them — the syscall handlers and SlopRing's `OP_RECVMSG` —
//! reads and writes them here, so a layout the two disagree about cannot
//! exist. Descriptor *ownership* stays with the caller that holds the fd
//! table: this module moves numbers and bytes, nothing more.

use slopos_abi::Errno;
use slopos_abi::fs::UIO_MAXIOV;
use slopos_abi::syscall::{
    CMSG_DATA_OFFSET, CmsgHdr, MSG_CTRUNC, MsgHdr, SCM_RIGHTS, SOL_SOCKET, cmsg_firsthdr, cmsg_len,
    cmsg_nxthdr, cmsg_space,
};
use slopos_ostd::util::byte_view::{pod_slice_as_bytes, pod_slice_as_bytes_mut};

use crate::user_copy::{copy_bytes_from_user, copy_bytes_to_user, copy_from_user, copy_to_user};
use crate::user_io_buf::{UserIovecBuf, stage_iovec};
use crate::user_ptr::{UserBytes, UserPtr};

/// Control-buffer bytes one `sendmsg` may declare, past which
/// [`scm_rights_fds`] answers `ENOBUFS`.
///
/// The sibling bound on the data segments is `UIO_MAXIOV`, which is ABI
/// because userland sizes its own `iovec` array against it. This one is
/// kernel policy — userland needs the `CMSG_*` helpers to *build* a control
/// buffer, not this number — so it lives with the walk it bounds.
///
/// `msg_controllen` is attacker-chosen and each item costs a
/// `copy_from_user` of its header, page-table walk under the per-process VM
/// lock included, while an item can be a bare 16-byte header naming no
/// descriptor at all: unbounded, an N-byte mapped control buffer buys N/16 of
/// those reads. Linux bounds the same walk by copying the control buffer into
/// a kernel allocation capped at `sysctl_optmem_max` and answering `ENOBUFS`
/// when it will not fit; this is that bound, sized to what this kernel can
/// act on rather than to a tunable. One item naming the whole
/// [`SCM_MAX_FDS`](slopos_abi::syscall::SCM_MAX_FDS) is `cmsg_space(4 * 4)` =
/// 32 bytes, so 256 bytes is room for eight maximal items and at most 16
/// header reads.
pub const SCM_CONTROLLEN_MAX: usize = 256;

/// A `msghdr`'s data segments as one flat buffer, every extent validated.
///
/// Linux caps `msg_iovlen` at `UIO_MAXIOV` and answers `EMSGSIZE` past it,
/// where `readv`/`writev` answer `EINVAL` for the same overflow; the staging
/// and per-segment validation below that are the same path.
pub fn msg_iovec_buf(msg: &MsgHdr) -> Result<UserIovecBuf, Errno> {
    let count = usize::try_from(msg.msg_iovlen).map_err(|_| Errno::EMSGSIZE)?;
    if count > UIO_MAXIOV {
        return Err(Errno::EMSGSIZE);
    }
    UserIovecBuf::new(&stage_iovec(msg.msg_iov, count)?)
}

/// Copy the caller's `msghdr` back with the fields `recvmsg(2)` writes:
/// `msg_namelen` — no receive path here reports a source address —
/// `msg_controllen`, and `msg_flags`. Linux writes all three on every
/// `recvmsg`, not only on one that carried ancillary data.
pub fn msghdr_write_out(
    msg_ptr: UserPtr<MsgHdr>,
    msg: &MsgHdr,
    controllen: u64,
    flags: i32,
) -> Result<(), Errno> {
    let updated = MsgHdr {
        msg_namelen: 0,
        msg_controllen: controllen,
        msg_flags: flags,
        ..*msg
    };
    copy_to_user(msg_ptr, &updated).map_err(|_| Errno::EFAULT)
}

/// Read the descriptor *numbers* a control buffer's `SCM_RIGHTS` items name
/// into `out`, returning how many were read.
///
/// Walks every item with [`cmsg_firsthdr`]/[`cmsg_nxthdr`] rather than reading
/// the first header and stopping: a caller that sends two items would
/// otherwise have the second silently dropped; [`SCM_CONTROLLEN_MAX`] is what
/// bounds that walk. `out.len()` bounds the descriptors — more than that is
/// `EINVAL`, as Linux answers past `SCM_MAX_FD`. A level or type this kernel
/// does not implement is `EINVAL` rather than ignored: a dropped ancillary
/// item is worse than a refusal.
pub fn scm_rights_fds(msg: &MsgHdr, out: &mut [i32]) -> Result<usize, Errno> {
    if msg.msg_control == 0 {
        return Ok(0);
    }
    let controllen = usize::try_from(msg.msg_controllen).map_err(|_| Errno::EINVAL)?;
    if controllen > SCM_CONTROLLEN_MAX {
        return Err(Errno::ENOBUFS);
    }
    let mut count = 0usize;
    let mut next = cmsg_firsthdr(controllen);

    while let Some(off) = next {
        let hdr_addr = msg
            .msg_control
            .checked_add(off as u64)
            .ok_or(Errno::EFAULT)?;
        let hdr_ptr = UserPtr::<CmsgHdr>::try_new(hdr_addr).map_err(|_| Errno::EFAULT)?;
        let cmsg: CmsgHdr = copy_from_user(hdr_ptr).map_err(|_| Errno::EFAULT)?;

        let len = usize::try_from(cmsg.cmsg_len).map_err(|_| Errno::EINVAL)?;
        if len > controllen - off {
            return Err(Errno::EINVAL);
        }
        let payload = len.checked_sub(CMSG_DATA_OFFSET).ok_or(Errno::EINVAL)?;
        if cmsg.cmsg_level != SOL_SOCKET || cmsg.cmsg_type != SCM_RIGHTS {
            return Err(Errno::EINVAL);
        }

        // A payload that is not a whole number of descriptors rounds down, as
        // Linux's `scm_fp_copy` does.
        let fds = payload / core::mem::size_of::<i32>();
        if fds > out.len() - count {
            return Err(Errno::EINVAL);
        }
        if fds > 0 {
            let data_addr = hdr_addr
                .checked_add(CMSG_DATA_OFFSET as u64)
                .ok_or(Errno::EFAULT)?;
            let bytes = fds * core::mem::size_of::<i32>();
            let user_fds = UserBytes::try_new(data_addr, bytes).map_err(|_| Errno::EFAULT)?;
            let dst = pod_slice_as_bytes_mut(&mut out[count..count + fds]);
            copy_bytes_from_user(user_fds, dst).map_err(|_| Errno::EFAULT)?;
        }
        count += fds;

        next = cmsg_nxthdr(controllen, off, len);
    }
    Ok(count)
}

/// Whether `msg`'s control buffer can hold one `SCM_RIGHTS` item naming
/// `n_fds` descriptors.
///
/// Asked *before* the descriptors are installed, so an item that cannot be
/// reported is dropped rather than installed into a table whose owner never
/// learns the numbers.
pub fn scm_rights_fits(msg: &MsgHdr, n_fds: usize) -> bool {
    if msg.msg_control == 0 {
        return false;
    }
    let capacity = usize::try_from(msg.msg_controllen).unwrap_or(0);
    capacity >= cmsg_len(n_fds * core::mem::size_of::<i32>())
}

/// Write one `SCM_RIGHTS` item naming `fds` into `msg`'s control buffer, then
/// `msg`'s out-fields. Only called once [`scm_rights_fits`] has agreed.
///
/// `msg_controllen` reports the bytes *consumed* — one item's `CMSG_SPACE`
/// clamped to what the buffer could hold — which is what Linux's `put_cmsg`
/// leaves behind.
pub fn put_scm_rights(msg_ptr: UserPtr<MsgHdr>, msg: &MsgHdr, fds: &[i32]) -> Result<(), Errno> {
    let payload = core::mem::size_of_val(fds);
    let cmsg = CmsgHdr {
        cmsg_len: cmsg_len(payload) as u64,
        cmsg_level: SOL_SOCKET,
        cmsg_type: SCM_RIGHTS,
    };
    let cmsg_ptr = UserPtr::<CmsgHdr>::try_new(msg.msg_control).map_err(|_| Errno::EFAULT)?;
    copy_to_user(cmsg_ptr, &cmsg).map_err(|_| Errno::EFAULT)?;

    let data_addr = msg
        .msg_control
        .checked_add(CMSG_DATA_OFFSET as u64)
        .ok_or(Errno::EFAULT)?;
    let fd_out = UserBytes::try_new(data_addr, payload).map_err(|_| Errno::EFAULT)?;
    copy_bytes_to_user(fd_out, pod_slice_as_bytes(fds)).map_err(|_| Errno::EFAULT)?;

    let capacity = usize::try_from(msg.msg_controllen).unwrap_or(0);
    msghdr_write_out(msg_ptr, msg, cmsg_space(payload).min(capacity) as u64, 0)
}

/// Report to the caller that ancillary data arrived and was discarded, which
/// is what `MSG_CTRUNC` is for: an empty control buffer alone reads as "no
/// descriptors were sent".
pub fn msghdr_report_ctrunc(msg_ptr: UserPtr<MsgHdr>, msg: &MsgHdr) -> Result<(), Errno> {
    msghdr_write_out(msg_ptr, msg, 0, MSG_CTRUNC)
}
