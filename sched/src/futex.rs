//! Futex (fast userspace mutex) wait queues over a fixed-size hash table of
//! buckets keyed by the futex word's identity, each holding an unbounded
//! intrusive list of waiting tasks.
//!
//! The list link lives in the task itself, so enqueue allocates nothing and a
//! bucket has no capacity to exhaust. A fixed-capacity bucket would have to
//! refuse the surplus waiter, and every userland futex wrapper discards that
//! error and retries — turning a blocked waiter into a full-core busy-spin.
//!
//! A private futex is keyed on the pair (address space, virtual address),
//! never on the address alone: the same number names a different word in
//! every process, so an address-only key would let one process's
//! `FUTEX_REQUEUE` restamp a foreign waiter onto a word its own `FUTEX_WAKE`
//! can never match again. A shared one (no `FUTEX_PRIVATE_FLAG`, on a word in
//! a `MAP_SHARED` mapping of a shared object) is keyed on (object, byte
//! offset), so processes mapping one object at different addresses meet; the
//! syscall layer derives which, as Linux's documented futex key does.

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
const _: () = assert!(FUTEX_HASH_BUCKETS <= u8::MAX as usize + 1);

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

/// Names a futex. A private key is the pair (address space, futex word) — see
/// the module doc. A shared key is (backing object, byte offset), so every
/// address space mapping that object at any address names the same word.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FutexKey {
    shared: bool,
    /// The address space's packed `process_vm` handle (0 for a kernel task),
    /// generation-checked so a reused `ProcessVm` slot never aliases the one
    /// it replaced; or the shared object's identity, which is generation-
    /// checked the same way.
    space: u64,
    /// The virtual address, or the byte offset into the shared object.
    addr: u64,
}

impl FutexKey {
    /// `uaddr` in the running task's own address space.
    pub fn private(uaddr: u64) -> Self {
        Self {
            shared: false,
            space: current_futex_space(),
            addr: uaddr,
        }
    }

    /// The word `offset` bytes into the shared backing object `object`.
    pub fn shared(object: u64, offset: u64) -> Self {
        Self {
            shared: true,
            space: object,
            addr: offset,
        }
    }

    #[inline]
    fn bucket(&self) -> usize {
        // Shifted by 2 because futex words are 4-byte aligned. The space term
        // is added, not mixed into the address multiply, so the per-address
        // stride is preserved and adjacent words stay in distinct buckets.
        // Salted per kind so an object identity and an address-space handle
        // that happen to share a value do not share a bucket.
        let salt = if self.shared {
            0xA24B_AED4_963E_E407
        } else {
            0
        };
        let h = (self.addr >> 2)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add((self.space ^ salt).wrapping_mul(0xD6E8_FEB8_6659_FD93));
        (h as usize) & (FUTEX_HASH_BUCKETS - 1)
    }
}

/// The address space of the running task. A CPU parked on its bootstrap stub
/// has no current task and gets 0, the same half every kernel task carries.
fn current_futex_space() -> u64 {
    crate::task_struct::Current::get().map_or(0, |g| g.task().process_vm_handle_raw())
}

/// The futex a parked waiter is waiting on. Read under the bucket lock, which
/// every writer of the key holds, so the three fields agree.
fn parked_futex_key(node: NonNull<crate::task_struct::Task>) -> FutexKey {
    placement::with_parked_node(node, |task| FutexKey {
        shared: task.futex_shared.load(Ordering::Relaxed),
        space: task.futex_space.load(Ordering::Relaxed),
        addr: task.futex_addr.load(Ordering::Relaxed),
    })
}

/// Stamp `key` on a task about to be linked into bucket `bucket`, or moved
/// there. The caller holds that bucket's lock.
fn stamp_futex_key(task: &crate::task_struct::Task, key: FutexKey, bucket: usize) {
    task.futex_addr.store(key.addr, Ordering::Relaxed);
    task.futex_space.store(key.space, Ordering::Relaxed);
    task.futex_shared.store(key.shared, Ordering::Relaxed);
    task.futex_bucket.store(bucket as u8, Ordering::Relaxed);
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
/// calling task on `key`, which names the word `uaddr` maps.
///
/// `timeout_ms` of `None` waits indefinitely; it is relative and the deadline
/// is derived once, so a wait that loops over a spurious wake cannot re-arm
/// its own budget. A waiter only wakes on a wake whose mask intersects
/// `bitset`; `FUTEX_WAIT` passes [`FUTEX_BITSET_MATCH_ANY`].
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
pub fn futex_wait(
    key: FutexKey,
    uaddr: u64,
    expected: u32,
    timeout_ms: Option<u64>,
    bitset: u32,
) -> i64 {
    if bitset == 0 {
        return slopos_abi::syscall::ERRNO_EINVAL as i64;
    }
    let deadline_ms =
        timeout_ms.map(|ms| slopos_kernel_services::platform::get_time_ms().saturating_add(ms));

    let Some(current_guard) = crate::task_struct::Current::get() else {
        return slopos_abi::syscall::ERRNO_EAGAIN as i64;
    };
    let current = current_guard.as_ptr();
    let bucket_idx = key.bucket();

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
            stamp_futex_key(current_guard.task(), key, bucket_idx);
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
        if parked_on != key {
            return 0;
        }
    }
}

/// Unlink the current task from whichever bucket holds it, returning the key
/// it was parked on. `None` means a wake had already claimed it.
///
/// The bucket is re-read because `FUTEX_REQUEUE` can move a waiter to another
/// word — and therefore another bucket — while it sleeps; the re-check under
/// the lock closes the window between the read and the acquire, since only a
/// holder of the lock the waiter is under can move it.
fn futex_unlink_self() -> Option<FutexKey> {
    let current_guard = crate::task_struct::Current::get()?;
    let node = NonNull::new(current_guard.as_ptr())?;
    let task = current_guard.task();
    loop {
        let idx = task.futex_bucket.load(Ordering::Relaxed) as usize;
        let bucket = FUTEX_TABLE[idx & (FUTEX_HASH_BUCKETS - 1)].lock();
        if task.futex_bucket.load(Ordering::Relaxed) as usize != idx {
            continue;
        }
        let key = FutexKey {
            shared: task.futex_shared.load(Ordering::Relaxed),
            space: task.futex_space.load(Ordering::Relaxed),
            addr: task.futex_addr.load(Ordering::Relaxed),
        };
        return unlink_waiter(&bucket, node).then_some(key);
    }
}

/// FUTEX_WAKE / FUTEX_WAKE_BITSET: wake up to `max_wake` tasks waiting on
/// `key` whose own mask intersects `bitset`.
///
/// Returns the number of tasks actually woken.
pub fn futex_wake(key: FutexKey, max_wake: u32, bitset: u32) -> i64 {
    if bitset == 0 {
        return slopos_abi::syscall::ERRNO_EINVAL as i64;
    }
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
/// `src`, then move up to `max_requeue` of the remainder onto `dst`. The two
/// keys need not be of one kind: a shared word's waiters can be moved onto a
/// private one and back, as on Linux.
///
/// `expected` is `Some` for `FUTEX_CMP_REQUEUE`, whose compare of `*uaddr`
/// (the word `src` names) happens under the bucket lock. Returns woken +
/// requeued, as Linux does.
///
/// The pair is acquired in bucket-index order, so two requeues in opposite
/// directions cannot deadlock; the nesting is declared with `lock_nested`
/// subclasses rather than waved through with `LO_DUPOK`, which would discard
/// the order check for every futex acquire in the kernel.
pub fn futex_requeue(
    src: FutexKey,
    uaddr: u64,
    dst: FutexKey,
    max_wake: u32,
    max_requeue: u32,
    expected: Option<u32>,
) -> i64 {
    let src_idx = src.bucket();
    let dst_idx = dst.bucket();
    let op = Requeue {
        src,
        dst,
        dst_idx,
        uaddr,
        max_wake,
        max_requeue,
        expected,
    };

    if src_idx == dst_idx {
        let bucket = FUTEX_TABLE[src_idx].lock();
        return op.run(&bucket, &bucket);
    }

    let src_first = src_idx < dst_idx;
    let (low, high) = if src_first {
        (src_idx, dst_idx)
    } else {
        (dst_idx, src_idx)
    };
    let outer = FUTEX_TABLE[low].lock_nested(0);
    let inner = FUTEX_TABLE[high].lock_nested(1);
    let (src_bucket, dst_bucket) = if src_first {
        (&*outer, &*inner)
    } else {
        (&*inner, &*outer)
    };
    op.run(src_bucket, dst_bucket)
}

struct Requeue {
    src: FutexKey,
    dst: FutexKey,
    dst_idx: usize,
    uaddr: u64,
    max_wake: u32,
    max_requeue: u32,
    expected: Option<u32>,
}

impl Requeue {
    fn run(&self, src: &FutexBucket, dst: &FutexBucket) -> i64 {
        if let Some(want) = self.expected {
            let Some(actual) = read_futex_word(self.uaddr) else {
                return slopos_abi::syscall::ERRNO_EFAULT as i64;
            };
            if actual != want {
                return slopos_abi::syscall::ERRNO_EAGAIN as i64;
            }
        }

        let woken = wake_matching(src, self.src, self.max_wake, FUTEX_BITSET_MATCH_ANY);

        let mut requeued = 0u32;
        let mut cursor = src.iter();
        while requeued < self.max_requeue {
            let Some(node) = cursor.next() else {
                break;
            };
            // When both keys share a bucket the moved waiters land at the
            // tail with `dst` stamped, so this same test skips them.
            if parked_futex_key(node) != self.src || src.remove(node).is_err() {
                continue;
            }
            placement::with_parked_node(node, |task| stamp_futex_key(task, self.dst, self.dst_idx));
            if dst.push_back(node).is_err() {
                // The membership reference is already off `src`; waking the
                // waiter releases it rather than stranding it on no queue.
                let owned = TaskRef::from_placement(node);
                let _ = unblock_task(&owned);
                task_put(owned);
                continue;
            }
            requeued += 1;
        }

        (woken + requeued) as i64
    }
}

/// Wake one waiter on the given futex address.
///
/// Used by the CLONE_CHILD_CLEARTID thread-exit path after the kernel writes 0
/// to the TID address, so `pthread_join` can complete.
pub fn futex_wake_one(uaddr: u64) -> i64 {
    futex_wake(FutexKey::private(uaddr), 1, FUTEX_BITSET_MATCH_ANY)
}

/// Waiters parked on `key`.
#[cfg(feature = "test-hooks")]
pub fn futex_waiters_for_test(key: FutexKey) -> usize {
    let bucket = FUTEX_TABLE[key.bucket()].lock();
    bucket
        .iter()
        .filter(|&node| parked_futex_key(node) == key)
        .count()
}

/// The wake mask a waiter on `key` parked.
#[cfg(feature = "test-hooks")]
pub fn futex_waiter_bitset_for_test(key: FutexKey) -> Option<u32> {
    let bucket = FUTEX_TABLE[key.bucket()].lock();
    bucket
        .iter()
        .find(|&node| parked_futex_key(node) == key)
        .map(parked_futex_bitset)
}

/// Queue the current task on `key` without blocking it, so a test can
/// observe the dequeue side without also having to be descheduled.
#[cfg(feature = "test-hooks")]
pub fn futex_park_for_test(key: FutexKey) -> bool {
    futex_park_bitset_for_test(key, FUTEX_BITSET_MATCH_ANY)
}

/// [`futex_park_for_test`] with an explicit wake mask.
#[cfg(feature = "test-hooks")]
pub fn futex_park_bitset_for_test(key: FutexKey, bitset: u32) -> bool {
    let Some(current_guard) = crate::task_struct::Current::get() else {
        return false;
    };
    let Some(node) = NonNull::new(current_guard.as_ptr()) else {
        return false;
    };
    let idx = key.bucket();
    let bucket = FUTEX_TABLE[idx].lock();
    stamp_futex_key(current_guard.task(), key, idx);
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

/// Remove the current task's entry for `key`. `false` means a wake had
/// already claimed it, or that the task is parked on some other word.
#[cfg(feature = "test-hooks")]
pub fn futex_remove_self_for_test(key: FutexKey) -> bool {
    let Some(current_guard) = crate::task_struct::Current::get() else {
        return false;
    };
    let task = current_guard.task();
    let parked = FutexKey {
        shared: task.futex_shared.load(Ordering::Relaxed),
        space: task.futex_space.load(Ordering::Relaxed),
        addr: task.futex_addr.load(Ordering::Relaxed),
    };
    if parked != key {
        return false;
    }
    futex_unlink_self().is_some()
}
