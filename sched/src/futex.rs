//! Futex (fast userspace mutex) wait queues over a fixed-size hash table of
//! buckets keyed by the futex word's identity, each holding an unbounded
//! intrusive list of waiting tasks.
//!
//! The list link lives in the task itself, so enqueue allocates nothing and a
//! bucket has no capacity to exhaust. A fixed-capacity bucket would have to
//! refuse the surplus waiter, and every userland futex wrapper discards that
//! error and retries — turning a blocked waiter into a full-core busy-spin.
//!
//! A futex is keyed on the pair (address space, virtual address), never on the
//! address alone: the same number names a different word in every process, so
//! an address-only key would let one process's `FUTEX_REQUEUE` restamp a
//! foreign waiter onto a word its own `FUTEX_WAKE` can never match again. The
//! space half is the task's packed `process_vm` handle, so threads sharing a
//! VM share keys and `FUTEX_PRIVATE_FLAG` is a no-op the syscall layer accepts.

use core::ptr::NonNull;
use core::sync::atomic::Ordering;
use slopos_ostd::lock_class;

use slopos_abi::syscall::FUTEX_BITSET_MATCH_ANY;
use slopos_abi::task::BlockReason;
use slopos_mm::user_ptr::UserPtr;
use slopos_ostd::sync::{IntrusiveDList, KernelSync, LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::task::{FutexRole, placement};

use super::scheduler::{
    mark_current_blocked, set_current_runnable, unblock_task, yield_blocked_task,
    yield_blocked_task_with_timeout,
};
use super::task::{TaskRef, task_put};

/// Must be a power of two.
const FUTEX_HASH_BUCKETS: usize = 64;

/// Membership parks one strong reference per waiter, so a waiter cannot be
/// freed out from under the bucket ("linked implies owned").
///
/// The `KernelSync` asserts that cross-CPU access to the raw pointers inside
/// `Task` is serialised by the bucket lock.
type FutexBucket = KernelSync<IntrusiveDList<crate::task_struct::Task, FutexRole>>;

static FUTEX_TABLE: [SpinLock<FutexBucket>; FUTEX_HASH_BUCKETS] = {
    const BUCKET: SpinLock<FutexBucket> = SpinLock::new(
        KernelSync::new(IntrusiveDList::new()),
        lock_class!("FUTEX_TABLE", LOCK_LEVEL_RESOURCE),
    );
    [BUCKET; FUTEX_HASH_BUCKETS]
};

/// Unlink `task` from `bucket`, releasing the reference membership parked.
/// The caller holds the bucket's lock. `false` when it was not a member.
fn unlink_waiter(bucket: &FutexBucket, task: NonNull<crate::task_struct::Task>) -> bool {
    if bucket.remove(task).is_err() {
        return false;
    }
    // Membership parked exactly one reference in `futex_wait`; the unlink
    // above is what consumes it.
    task_put(TaskRef::from_placement(task));
    true
}

/// Names a futex by the pair (address space, futex word) — see the module doc.
#[derive(Clone, Copy, PartialEq, Eq)]
struct FutexKey {
    /// The owning task's packed `process_vm` handle; 0 for a kernel task.
    /// Generation-checked, so a reused `ProcessVm` slot never aliases the
    /// address space it replaced.
    space: u64,
    addr: u64,
}

impl FutexKey {
    #[inline]
    fn bucket(&self) -> usize {
        // Shifted by 2 because futex words are 4-byte aligned. The space term
        // is added, not mixed into the address multiply, so the per-address
        // stride is preserved and adjacent words stay in distinct buckets.
        let h = (self.addr >> 2)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(self.space.wrapping_mul(0xD6E8_FEB8_6659_FD93));
        (h as usize) & (FUTEX_HASH_BUCKETS - 1)
    }
}

/// The address space of the running task. A CPU parked on its bootstrap stub
/// has no current task and gets 0, the same half every kernel task carries.
fn current_futex_space() -> u64 {
    crate::task_struct::Current::get().map_or(0, |g| g.task().process_vm_handle_raw())
}

fn current_futex_key(addr: u64) -> FutexKey {
    FutexKey {
        space: current_futex_space(),
        addr,
    }
}

/// The futex a parked waiter is waiting on. The space is read from the waiter
/// rather than stored beside the link: a task's `process_vm` handle is stamped
/// once at creation, and membership holds a strong reference, so the pair
/// cannot shift underneath the comparison.
fn parked_futex_key(node: NonNull<crate::task_struct::Task>) -> FutexKey {
    placement::with_parked_node(node, |task| FutexKey {
        space: task.process_vm_handle_raw(),
        addr: task.futex_addr.load(Ordering::Relaxed),
    })
}

/// The wake mask a parked waiter will accept. `FUTEX_WAIT` parks
/// [`FUTEX_BITSET_MATCH_ANY`], so one intersection test serves both forms.
fn parked_futex_bitset(node: NonNull<crate::task_struct::Task>) -> u32 {
    placement::with_parked_node(node, |task| task.futex_bitset.load(Ordering::Relaxed))
}

/// Read the futex word without ever blocking: `copy_from_user`, not a raw
/// load, because it opens the kernel's sole AC window, without which the
/// access faults under CR4.SMAP. A demand fault must fail rather than sleep.
fn read_futex_word(uaddr: u64) -> Option<u32> {
    UserPtr::<u32>::try_new(uaddr)
        .ok()
        .and_then(|p| slopos_mm::user_copy::copy_from_user::<u32>(p).ok())
}

/// FUTEX_WAIT: atomically check that `*uaddr == expected` and block the
/// calling task on the futex queue keyed by `uaddr`.
///
/// `timeout_ms` of `None` waits indefinitely; it is relative and the deadline
/// is derived once, so a wait that loops over a spurious wake cannot re-arm
/// its own budget.
///
/// Returns:
///  *  0 on success (woken by FUTEX_WAKE, or requeued and then released)
///  * -EAGAIN if `*uaddr != expected` at the time of the check
///  * -EFAULT if `*uaddr` is unreadable
///  * -EINTR if the caller was marked for death or has a signal to act on
///  * -ETIMEDOUT if the deadline elapsed
///
/// `uaddr` must be a user-space virtual address of a u32 aligned to 4 bytes.
/// The caller (syscall handler) is responsible for validating the pointer.
pub fn futex_wait(uaddr: u64, expected: u32, timeout_ms: Option<u64>) -> i64 {
    futex_wait_bitset(uaddr, expected, timeout_ms, FUTEX_BITSET_MATCH_ANY)
}

/// FUTEX_WAIT_BITSET: [`futex_wait`] that only a wake whose mask intersects
/// `bitset` may claim.
pub fn futex_wait_bitset(uaddr: u64, expected: u32, timeout_ms: Option<u64>, bitset: u32) -> i64 {
    if bitset == 0 {
        return slopos_abi::syscall::ERRNO_EINVAL as i64;
    }
    let deadline_ms =
        timeout_ms.map(|ms| slopos_kernel_services::platform::get_time_ms().saturating_add(ms));

    let Some(current_guard) = crate::task_struct::Current::get() else {
        return slopos_abi::syscall::ERRNO_EAGAIN as i64;
    };
    let current = current_guard.as_ptr();
    let bucket_idx = FutexKey {
        space: current_guard.task().process_vm_handle_raw(),
        addr: uaddr,
    }
    .bucket();

    loop {
        // The bucket lock covers the read+compare of `*uaddr`, the enqueue and
        // the Running→Blocked CAS; FUTEX_WAKE takes the same lock to dequeue.
        // Doing the CAS under the lock is what closes the lost-wakeup window: a
        // waker that observes our waiter necessarily observes Blocked too.
        let blocked = {
            let bucket = FUTEX_TABLE[bucket_idx].lock();

            let Some(current_val) = read_futex_word(uaddr) else {
                return slopos_abi::syscall::ERRNO_EFAULT as i64;
            };
            if current_val != expected {
                return slopos_abi::syscall::ERRNO_EAGAIN as i64;
            }

            // `current` is the running task, kept alive by its dispatch
            // reference, so parking a reference here is sound.
            let Some(node) = NonNull::new(current) else {
                return slopos_abi::syscall::ERRNO_EAGAIN as i64;
            };
            current_guard
                .task()
                .futex_addr
                .store(uaddr, Ordering::Relaxed);
            current_guard
                .task()
                .futex_bitset
                .store(bitset, Ordering::Relaxed);
            // Retain before link: membership must never name a task the
            // bucket does not hold a reference to.
            placement::task_placement_retain(node);
            if bucket.push_back(node).is_err() {
                task_put(TaskRef::from_placement(node));
                return slopos_abi::syscall::ERRNO_EAGAIN as i64;
            }

            // Stamped before the status flip, so a reader never observes
            // Blocked without the reason that goes with it.
            current_guard
                .task()
                .store_block_reason(BlockReason::FutexWait);

            mark_current_blocked()
        };

        // A failed Running→Blocked CAS means a wake got there first, and that
        // wakeup is already preserved in the current Running/Ready status.
        if blocked {
            match deadline_ms {
                None => yield_blocked_task(),
                Some(deadline) => {
                    let now = slopos_kernel_services::platform::get_time_ms();
                    if now >= deadline {
                        set_current_runnable();
                    } else {
                        let remaining = deadline.saturating_sub(now).min(u32::MAX as u64) as u32;
                        yield_blocked_task_with_timeout(remaining);
                    }
                }
            }
        }

        // A wake unlinks us when it is the one that woke us, so still being
        // linked means a signal, a kill or the deadline did, and the bucket's
        // strong reference would be stranded.
        let Some(parked_on) = futex_unlink_self() else {
            return 0;
        };

        if current_guard.task().is_killed()
            || crate::task::task_has_deliverable_signal(current_guard.task())
        {
            return slopos_abi::syscall::ERRNO_EINTR as i64;
        }
        if deadline_ms.is_some_and(|d| slopos_kernel_services::platform::get_time_ms() >= d) {
            return slopos_abi::syscall::ERRNO_ETIMEDOUT as i64;
        }
        // A requeue moved this waiter to another word; comparing `expected`
        // against the original would read the wrong futex, so report the
        // spurious wake userland already has to tolerate.
        if parked_on != uaddr {
            return 0;
        }
    }
}

/// Unlink the current task from whichever bucket holds it, returning the
/// address it was parked on. `None` means a wake had already claimed it.
///
/// The address is re-read because `FUTEX_REQUEUE` can move a waiter to another
/// word — and therefore another bucket — while it sleeps; the re-check under
/// the lock closes the window between the read and the acquire.
fn futex_unlink_self() -> Option<u64> {
    let current_guard = crate::task_struct::Current::get()?;
    let node = NonNull::new(current_guard.as_ptr())?;
    let space = current_guard.task().process_vm_handle_raw();
    loop {
        let addr = current_guard.task().futex_addr.load(Ordering::Relaxed);
        let bucket = FUTEX_TABLE[FutexKey { space, addr }.bucket()].lock();
        if current_guard.task().futex_addr.load(Ordering::Relaxed) != addr {
            continue;
        }
        return unlink_waiter(&bucket, node).then_some(addr);
    }
}

/// FUTEX_WAKE: wake up to `max_wake` tasks waiting on the futex at `uaddr`.
///
/// Returns the number of tasks actually woken.
pub fn futex_wake(uaddr: u64, max_wake: u32) -> i64 {
    futex_wake_bitset(uaddr, max_wake, FUTEX_BITSET_MATCH_ANY)
}

/// FUTEX_WAKE_BITSET: [`futex_wake`] restricted to waiters whose own mask
/// intersects `bitset`.
pub fn futex_wake_bitset(uaddr: u64, max_wake: u32, bitset: u32) -> i64 {
    if bitset == 0 {
        return slopos_abi::syscall::ERRNO_EINVAL as i64;
    }
    let key = current_futex_key(uaddr);
    let bucket = FUTEX_TABLE[key.bucket()].lock();
    let woken = wake_matching(&bucket, key, max_wake, bitset);
    drop(bucket);
    woken as i64
}

/// One pass over `bucket`, waking every matching waiter up to `max_wake`.
///
/// `DIter` advances its cursor before yielding a node, so unlinking the
/// yielded node mid-walk is sound and the whole bucket costs one traversal,
/// where a wake-all restarting from the head per wake would be quadratic.
fn wake_matching(bucket: &FutexBucket, key: FutexKey, max_wake: u32, bitset: u32) -> u32 {
    let mut woken = 0u32;
    let mut cursor = bucket.iter();
    while woken < max_wake {
        let Some(node) = cursor.next() else {
            break;
        };
        if parked_futex_key(node) != key || parked_futex_bitset(node) & bitset == 0 {
            continue;
        }
        // The bucket lock is held across the unblock. Lock order is bucket
        // (RESOURCE) → run queue, no cycle. The reference released below is
        // never the last: the task holds its own until reap.
        if bucket.remove(node).is_err() {
            continue;
        }
        let owned = TaskRef::from_placement(node);
        let _ = unblock_task(&owned);
        task_put(owned);
        woken += 1;
    }
    woken
}

/// FUTEX_REQUEUE / FUTEX_CMP_REQUEUE: wake up to `max_wake` waiters on
/// `uaddr`, then move up to `max_requeue` of the remainder onto `uaddr2`.
///
/// `expected` is `Some` for `FUTEX_CMP_REQUEUE`, whose compare of `*uaddr`
/// happens under the bucket lock. Returns woken + requeued, as Linux does.
///
/// The pair is acquired in bucket-index order, so two requeues in opposite
/// directions cannot deadlock; the nesting is declared with `lock_nested`
/// subclasses rather than waved through with `LO_DUPOK`, which would discard
/// the order check for every futex acquire in the kernel.
pub fn futex_requeue(
    uaddr: u64,
    uaddr2: u64,
    max_wake: u32,
    max_requeue: u32,
    expected: Option<u32>,
) -> i64 {
    let space = current_futex_space();
    let src_key = FutexKey { space, addr: uaddr };
    let dst_key = FutexKey {
        space,
        addr: uaddr2,
    };
    let src_idx = src_key.bucket();
    let dst_idx = dst_key.bucket();

    if src_idx == dst_idx {
        let bucket = FUTEX_TABLE[src_idx].lock();
        return requeue_locked(
            &bucket,
            &bucket,
            src_key,
            dst_key,
            max_wake,
            max_requeue,
            expected,
        );
    }

    let src_first = src_idx < dst_idx;
    let (low, high) = if src_first {
        (src_idx, dst_idx)
    } else {
        (dst_idx, src_idx)
    };
    let outer = FUTEX_TABLE[low].lock_nested(0);
    let inner = FUTEX_TABLE[high].lock_nested(1);
    let (src, dst) = if src_first {
        (&*outer, &*inner)
    } else {
        (&*inner, &*outer)
    };
    requeue_locked(src, dst, src_key, dst_key, max_wake, max_requeue, expected)
}

fn requeue_locked(
    src: &FutexBucket,
    dst: &FutexBucket,
    src_key: FutexKey,
    dst_key: FutexKey,
    max_wake: u32,
    max_requeue: u32,
    expected: Option<u32>,
) -> i64 {
    if let Some(want) = expected {
        let Some(actual) = read_futex_word(src_key.addr) else {
            return slopos_abi::syscall::ERRNO_EFAULT as i64;
        };
        if actual != want {
            return slopos_abi::syscall::ERRNO_EAGAIN as i64;
        }
    }

    let woken = wake_matching(src, src_key, max_wake, FUTEX_BITSET_MATCH_ANY);

    let mut requeued = 0u32;
    let mut cursor = src.iter();
    while requeued < max_requeue {
        let Some(node) = cursor.next() else {
            break;
        };
        // When both addresses share a bucket the moved waiters land at the
        // tail with `uaddr2` stamped, so this same test skips them.
        if parked_futex_key(node) != src_key || src.remove(node).is_err() {
            continue;
        }
        // Only the address moves: both keys carry the caller's address space.
        placement::with_parked_node(node, |task| {
            task.futex_addr.store(dst_key.addr, Ordering::Relaxed)
        });
        if dst.push_back(node).is_err() {
            // The membership reference is already off `src`; waking the waiter
            // releases it rather than stranding it on no queue at all.
            let owned = TaskRef::from_placement(node);
            let _ = unblock_task(&owned);
            task_put(owned);
            continue;
        }
        requeued += 1;
    }

    (woken + requeued) as i64
}

/// Wake one waiter on the given futex address.
///
/// Used by the CLONE_CHILD_CLEARTID thread-exit path after the kernel writes 0
/// to the TID address, so `pthread_join` can complete.
pub fn futex_wake_one(uaddr: u64) -> i64 {
    futex_wake(uaddr, 1)
}

/// Waiters the *running task's* address space has parked on `uaddr`. Keys off
/// `Current` exactly as the syscall entry points do.
#[cfg(feature = "test-hooks")]
pub fn futex_waiters_for_test(uaddr: u64) -> usize {
    let key = current_futex_key(uaddr);
    let bucket = FUTEX_TABLE[key.bucket()].lock();
    bucket
        .iter()
        .filter(|&node| parked_futex_key(node) == key)
        .count()
}

/// The wake mask a waiter on `uaddr` parked.
#[cfg(feature = "test-hooks")]
pub fn futex_waiter_bitset_for_test(uaddr: u64) -> Option<u32> {
    let key = current_futex_key(uaddr);
    let bucket = FUTEX_TABLE[key.bucket()].lock();
    bucket
        .iter()
        .find(|&node| parked_futex_key(node) == key)
        .map(parked_futex_bitset)
}

/// Queue the current task for `uaddr` without blocking it, so a test can
/// observe the dequeue side without also having to be descheduled.
#[cfg(feature = "test-hooks")]
pub fn futex_park_for_test(uaddr: u64) -> bool {
    futex_park_bitset_for_test(uaddr, FUTEX_BITSET_MATCH_ANY)
}

/// [`futex_park_for_test`] with an explicit wake mask.
#[cfg(feature = "test-hooks")]
pub fn futex_park_bitset_for_test(uaddr: u64, bitset: u32) -> bool {
    let Some(current_guard) = crate::task_struct::Current::get() else {
        return false;
    };
    let Some(node) = NonNull::new(current_guard.as_ptr()) else {
        return false;
    };
    let bucket = FUTEX_TABLE[FutexKey {
        space: current_guard.task().process_vm_handle_raw(),
        addr: uaddr,
    }
    .bucket()]
    .lock();
    current_guard
        .task()
        .futex_addr
        .store(uaddr, Ordering::Relaxed);
    current_guard
        .task()
        .futex_bitset
        .store(bitset, Ordering::Relaxed);
    placement::task_placement_retain(node);
    if bucket.push_back(node).is_err() {
        task_put(TaskRef::from_placement(node));
        return false;
    }
    true
}

/// Remove the current task's entry for `uaddr`. `false` means a wake had
/// already claimed it, or that the task is parked on some other word.
#[cfg(feature = "test-hooks")]
pub fn futex_remove_self_for_test(uaddr: u64, _task_id: u32) -> bool {
    let Some(current_guard) = crate::task_struct::Current::get() else {
        return false;
    };
    if current_guard.task().futex_addr.load(Ordering::Relaxed) != uaddr {
        return false;
    }
    futex_unlink_self().is_some()
}
