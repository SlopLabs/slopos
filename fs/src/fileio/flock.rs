//! Advisory file locks: `flock(2)`'s whole-file locks and `fcntl(2)`'s POSIX
//! record locks.
//!
//! A `flock` lock belongs to the **open file description**, so it survives
//! `dup` and `fork` and is released when the last descriptor naming that
//! description closes — which is why [`OpenFile`](super::OpenFile)'s `Drop`
//! calls into here. A record lock belongs to the **process**, and dies when
//! that process closes *any* descriptor naming the file or exits.
//!
//! One row array holds both kinds, but [`conflicting`] compares only rows of
//! the same kind, as `flock(2)` requires.
//!
//! Nothing here is taken under a descriptor-table lock: a blocking acquire
//! parks with only its own state released, so the table-then-state order in
//! `fs/src/fileio/mod.rs` is never inverted.

use core::sync::atomic::{AtomicUsize, Ordering};

use slopos_abi::Errno;
use slopos_abi::fs::UserFlock;
use slopos_abi::syscall::{
    F_RDLCK, F_SETLK, F_SETLKW, F_UNLCK, F_WRLCK, LOCK_EX, LOCK_NB, LOCK_SH, LOCK_UN,
};
use slopos_ostd::handle::Handle;
use slopos_ostd::lock_class;
use slopos_ostd::process::Process;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock, WaitAbort, WaitQueue};

use super::FdTable;

/// Lock rows the machine holds at once. Fixed rather than a `KVec`: the
/// release path runs from a descriptor's `Drop`, where an allocation has
/// nowhere to fail to.
pub(super) const MAX_LOCK_ROWS: usize = 128;

/// One principal's share of [`MAX_LOCK_ROWS`], so no single process can spend
/// the machine's table and leave every other one on `ENOLCK`.
pub(super) const MAX_PRINCIPAL_ROWS: usize = MAX_LOCK_ROWS / 8;

/// Slots held back for principals holding no row yet, so a principal can
/// always take a first lock however many rows the others hold.
pub(super) const FIRST_LOCK_RESERVE: usize = MAX_LOCK_ROWS / 4;

/// Blocking record-lock acquires visible to the deadlock check at once, and
/// the depth bound on the wait-for graph walk, which is why it is small.
const MAX_PARKED: usize = 16;

const _: () = assert!(
    MAX_PARKED <= u32::BITS as usize,
    "the wait-for graph walk carries its frontier in a u32 bitmask"
);

/// Which file a lock row is attached to.
///
/// `space` separates the two key spaces: a regular file keys on
/// `(filesystem, inode)`, everything else on `(kind, handle)`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct LockFile {
    space: u8,
    a: u64,
    b: u64,
}

impl LockFile {
    pub const fn inode(fs_addr: u64, inode: u64) -> Self {
        Self {
            space: 0,
            a: fs_addr,
            b: inode,
        }
    }

    pub const fn handle(kind: u8, handle: u64) -> Self {
        Self {
            space: 1,
            a: kind as u64,
            b: handle,
        }
    }
}

/// A record lock's owning process. Generation-checked because process ids
/// recycle: a bare `u32` would hand a dead process's locks to its successor.
#[derive(Clone, Copy, PartialEq, Eq)]
struct OwnerProcess {
    slot: u32,
    generation: u64,
}

impl OwnerProcess {
    fn of(handle: Handle<Process>) -> Self {
        Self {
            slot: handle.slot(),
            generation: handle.generation(),
        }
    }
}

/// The process a row's table slot is charged to — not the row's owner, since a
/// `flock` row can outlive the process that took it. `None` is the kernel's
/// own descriptor table.
type Principal = Option<OwnerProcess>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Owner {
    /// `flock(2)` — the open file description, by its never-reused id.
    Description(u64),
    /// `fcntl(2)` record lock — the process. `None` is the kernel's own
    /// descriptor table, which every kernel task shares.
    Process(Option<OwnerProcess>),
}

impl Owner {
    /// `flock` locks and record locks do not contend: `flock(2)` makes the two
    /// independent.
    fn same_kind(self, other: Owner) -> bool {
        matches!(
            (self, other),
            (Owner::Description(_), Owner::Description(_)) | (Owner::Process(_), Owner::Process(_))
        )
    }

    fn is_record(self) -> bool {
        matches!(self, Owner::Process(_))
    }
}

#[derive(Clone, Copy)]
struct Row {
    file: LockFile,
    owner: Owner,
    charged: Principal,
    exclusive: bool,
    start: u64,
    /// Exclusive; `u64::MAX` is POSIX's "to the end of the file".
    end: u64,
    /// Reported by `F_GETLK`.
    pid: u32,
}

impl Row {
    fn overlaps(&self, start: u64, end: u64) -> bool {
        self.start < end && start < self.end
    }

    /// Overlapping *or* abutting: `[0, 10)` touches `[10, 20)`. What
    /// coalescing merges on.
    fn touches(&self, start: u64, end: u64) -> bool {
        self.start <= end && start <= self.end
    }
}

/// One parked blocking record-lock request — a node in the wait-for graph.
#[derive(Clone, Copy)]
struct Parked {
    owner: Owner,
    file: LockFile,
    exclusive: bool,
    start: u64,
    end: u64,
}

/// Rows and parked requests share one lock: the deadlock check reads both
/// together and must not see a request registered against a stale row set.
struct Table {
    rows: [Option<Row>; MAX_LOCK_ROWS],
    parked: [Option<Parked>; MAX_PARKED],
}

static TABLE: SpinLock<Table> = SpinLock::new(
    Table {
        rows: [None; MAX_LOCK_ROWS],
        parked: [None; MAX_PARKED],
    },
    lock_class!("FILE_LOCK_ROWS", LOCK_LEVEL_RESOURCE),
);

/// Read before the table lock, so closing a descriptor on a machine holding no
/// lock costs one relaxed load rather than a spinlock and a 128-row scan.
static ACTIVE_ROWS: AtomicUsize = AtomicUsize::new(0);

static LOCK_WAITERS: WaitQueue =
    WaitQueue::new(lock_class!("FILE_LOCK_WAITERS", LOCK_LEVEL_RESOURCE));

/// The single wake point: every table mutation that can make a parked
/// predicate true funnels through here.
fn wake_waiters() {
    #[cfg(feature = "tests")]
    WAKE_CALLS.fetch_add(1, Ordering::Relaxed);
    LOCK_WAITERS.wake_all();
}

fn conflicting(
    rows: &[Option<Row>; MAX_LOCK_ROWS],
    file: LockFile,
    owner: Owner,
    exclusive: bool,
    start: u64,
    end: u64,
) -> Option<Row> {
    rows.iter().flatten().copied().find(|row| {
        row.file == file
            && row.owner != owner
            && row.owner.same_kind(owner)
            && (exclusive || row.exclusive)
            && row.overlaps(start, end)
    })
}

fn free_slots(rows: &[Option<Row>; MAX_LOCK_ROWS]) -> usize {
    rows.iter().filter(|slot| slot.is_none()).count()
}

fn principal_rows(rows: &[Option<Row>; MAX_LOCK_ROWS], charged: Principal) -> usize {
    rows.iter()
        .flatten()
        .filter(|row| row.charged == charged)
        .count()
}

fn insert(rows: &mut [Option<Row>; MAX_LOCK_ROWS], row: Row) -> Result<(), Errno> {
    for slot in rows.iter_mut() {
        if slot.is_none() {
            *slot = Some(row);
            ACTIVE_ROWS.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
    }
    Err(Errno::ENOLCK)
}

/// The rows [`clear_range`] will remove, and whether it must split one. The
/// split's tail needs a slot reserved up front, since a failed re-insert would
/// destroy coverage the owner still believes it holds.
fn clear_shape(
    rows: &[Option<Row>; MAX_LOCK_ROWS],
    file: LockFile,
    owner: Owner,
    start: u64,
    end: u64,
) -> (usize, usize) {
    let mut freed = 0usize;
    let mut split = 0usize;
    for row in rows.iter().flatten() {
        if row.file != file || row.owner != owner || !row.overlaps(start, end) {
            continue;
        }
        if row.start >= start && row.end <= end {
            freed += 1;
        } else if row.start < start && row.end > end {
            split = 1;
        }
    }
    (freed, split)
}

/// Widen `[start, end)` over `owner`'s own same-type rows that it touches, so
/// abutting ranges become one row as they do on Linux.
///
/// One pass suffices: those rows are disjoint, so the widened range reaches
/// from the leftmost start to the rightmost end.
fn merge_bounds(
    rows: &[Option<Row>; MAX_LOCK_ROWS],
    file: LockFile,
    owner: Owner,
    exclusive: bool,
    start: u64,
    end: u64,
) -> (u64, u64) {
    let (mut lo, mut hi) = (start, end);
    for row in rows.iter().flatten() {
        if row.file != file
            || row.owner != owner
            || row.exclusive != exclusive
            || !row.touches(start, end)
        {
            continue;
        }
        lo = lo.min(row.start);
        hi = hi.max(row.end);
    }
    (lo, hi)
}

/// Remove `owner`'s coverage of `[start, end)` on `file`, splitting a row the
/// range falls inside. Answers whether anything was removed.
///
/// Callers must have reserved the split's slot via [`clear_shape`]: the
/// truncation lands before the tail is re-inserted.
fn clear_range(
    rows: &mut [Option<Row>; MAX_LOCK_ROWS],
    file: LockFile,
    owner: Owner,
    start: u64,
    end: u64,
) -> Result<bool, Errno> {
    let mut removed = false;
    let mut tail: Option<Row> = None;
    for slot in rows.iter_mut() {
        let Some(row) = *slot else { continue };
        if row.file != file || row.owner != owner || !row.overlaps(start, end) {
            continue;
        }
        removed = true;
        if row.start >= start && row.end <= end {
            *slot = None;
            ACTIVE_ROWS.fetch_sub(1, Ordering::Relaxed);
        } else if row.start < start && row.end > end {
            *slot = Some(Row { end: start, ..row });
            tail = Some(Row { start: end, ..row });
        } else if row.start < start {
            *slot = Some(Row { end: start, ..row });
        } else {
            *slot = Some(Row { start: end, ..row });
        }
    }
    if let Some(row) = tail {
        insert(rows, row)?;
    }
    Ok(removed)
}

fn admit(
    rows: &[Option<Row>; MAX_LOCK_ROWS],
    file: LockFile,
    owner: Owner,
    charged: Principal,
    start: u64,
    end: u64,
) -> Result<(), Errno> {
    let (freed, split) = clear_shape(rows, file, owner, start, end);
    let free = free_slots(rows);
    // The mutation's structural need: the split's tail plus the new row, less
    // the rows `clear_range` removes before either is inserted.
    if free + freed < split + 1 {
        return Err(Errno::ENOLCK);
    }
    let growth = (split + 1).saturating_sub(freed);
    if growth == 0 {
        return Ok(());
    }
    let held = principal_rows(rows, charged);
    if held + growth > MAX_PRINCIPAL_ROWS {
        return Err(Errno::ENOLCK);
    }
    if held > 0 && free < growth + FIRST_LOCK_RESERVE {
        return Err(Errno::ENOLCK);
    }
    Ok(())
}

/// One acquire attempt. `Ok(None)` took the lock; `Ok(Some(pid))` is the pid
/// of the holder that refused it.
fn try_acquire(
    file: LockFile,
    owner: Owner,
    charged: Principal,
    pid: u32,
    exclusive: bool,
    start: u64,
    end: u64,
) -> Result<Option<u32>, Errno> {
    let mut freed_a_row = false;
    let outcome = {
        let mut table = TABLE.lock();
        if let Some(blocker) = conflicting(&table.rows, file, owner, exclusive, start, end) {
            Some(blocker.pid)
        } else {
            let (start, end) = merge_bounds(&table.rows, file, owner, exclusive, start, end);
            admit(&table.rows, file, owner, charged, start, end)?;
            freed_a_row = clear_range(&mut table.rows, file, owner, start, end)?;
            insert(
                &mut table.rows,
                Row {
                    file,
                    owner,
                    charged,
                    exclusive,
                    start,
                    end,
                    pid,
                },
            )?;
            None
        }
    };
    // A downgrade frees the exclusive row here rather than in `release`, and
    // the wait queue is shared, so without this wake a waiter whose predicate
    // just came true sleeps until some unrelated file's lock is released.
    if freed_a_row {
        wake_waiters();
    }
    Ok(outcome)
}

fn release(file: LockFile, owner: Owner, start: u64, end: u64) -> Result<(), Errno> {
    let removed = {
        let mut table = TABLE.lock();
        let (freed, split) = clear_shape(&table.rows, file, owner, start, end);
        // An unlock strictly inside a held row splits it, and the tail needs a
        // slot. Refusing before anything is mutated is what keeps `ENOLCK`
        // meaning "nothing happened".
        if free_slots(&table.rows) + freed < split {
            return Err(Errno::ENOLCK);
        }
        clear_range(&mut table.rows, file, owner, start, end)?
    };
    if removed {
        wake_waiters();
    }
    Ok(())
}

/// A parked request's registration, unregistered on drop.
pub(super) struct ParkSlot(usize);

impl ParkSlot {
    fn register(
        owner: Owner,
        file: LockFile,
        exclusive: bool,
        start: u64,
        end: u64,
    ) -> Option<Self> {
        let mut table = TABLE.lock();
        let (index, slot) = table
            .parked
            .iter_mut()
            .enumerate()
            .find(|(_, slot)| slot.is_none())?;
        *slot = Some(Parked {
            owner,
            file,
            exclusive,
            start,
            end,
        });
        Some(Self(index))
    }
}

impl Drop for ParkSlot {
    fn drop(&mut self) {
        if let Some(slot) = TABLE.lock().parked.get_mut(self.0) {
            *slot = None;
        }
    }
}

fn refused_by(
    rows: &[Option<Row>; MAX_LOCK_ROWS],
    holder: Owner,
    file: LockFile,
    owner: Owner,
    exclusive: bool,
    start: u64,
    end: u64,
) -> bool {
    rows.iter().flatten().any(|row| {
        row.owner == holder
            && row.file == file
            && row.owner != owner
            && row.owner.same_kind(owner)
            && (exclusive || row.exclusive)
            && row.overlaps(start, end)
    })
}

/// The parked requests whose owners hold a row refusing this request — one
/// node's out-edges in the wait-for graph.
fn blocking_parked(
    table: &Table,
    file: LockFile,
    owner: Owner,
    exclusive: bool,
    start: u64,
    end: u64,
) -> u32 {
    let mut mask = 0u32;
    for (index, parked) in table.parked.iter().enumerate() {
        let Some(parked) = parked else { continue };
        if refused_by(
            &table.rows,
            parked.owner,
            file,
            owner,
            exclusive,
            start,
            end,
        ) {
            mask |= 1 << index;
        }
    }
    mask
}

/// Whether blocking this request would close a cycle in the wait-for graph:
/// nodes are the owners of parked *record-lock* requests, edges "is refused by
/// a row owned by", and every owner on a cycle is itself parked.
///
/// `flock` requests are not nodes; `flock(2)` performs no deadlock detection
/// on Linux either. Undetected: a cycle through a waiter that found no free
/// [`MAX_PARKED`] slot.
fn deadlocked(
    table: &Table,
    file: LockFile,
    owner: Owner,
    exclusive: bool,
    start: u64,
    end: u64,
) -> bool {
    let mut reachable = blocking_parked(table, file, owner, exclusive, start, end);
    let mut expanded = 0u32;
    while reachable != expanded {
        let index = (reachable & !expanded).trailing_zeros() as usize;
        expanded |= 1 << index;
        let Some(Some(parked)) = table.parked.get(index).copied() else {
            continue;
        };
        if refused_by(
            &table.rows,
            owner,
            parked.file,
            parked.owner,
            parked.exclusive,
            parked.start,
            parked.end,
        ) {
            return true;
        }
        reachable |= blocking_parked(
            table,
            parked.file,
            parked.owner,
            parked.exclusive,
            parked.start,
            parked.end,
        );
    }
    false
}

/// Park until the conflict clears, then let the caller retry.
fn wait_for_lock(
    file: LockFile,
    owner: Owner,
    exclusive: bool,
    start: u64,
    end: u64,
) -> Result<(), Errno> {
    let clear_now = || {
        let table = TABLE.lock();
        conflicting(&table.rows, file, owner, exclusive, start, end).is_none()
    };
    match LOCK_WAITERS.wait_event_interruptible(clear_now) {
        Ok(()) => Ok(()),
        Err(WaitAbort::Interrupted) => Err(Errno::EINTR),
        Err(WaitAbort::Killed) => Err(Errno::EINTR),
        // Nothing can park here, so the honest answer is the one a
        // non-blocking caller would get rather than a spin.
        Err(_) => Err(Errno::EAGAIN),
    }
}

#[expect(clippy::too_many_arguments, reason = "one lock row's full identity")]
fn acquire(
    file: LockFile,
    owner: Owner,
    charged: Principal,
    pid: u32,
    exclusive: bool,
    start: u64,
    end: u64,
    block: bool,
) -> Result<(), Errno> {
    let mut registration: Option<ParkSlot> = None;
    loop {
        match try_acquire(file, owner, charged, pid, exclusive, start, end)? {
            None => return Ok(()),
            Some(_) => {
                if !block {
                    return Err(Errno::EAGAIN);
                }
                if owner.is_record() {
                    if registration.is_none() {
                        registration = ParkSlot::register(owner, file, exclusive, start, end);
                    }
                    let cycle = {
                        let table = TABLE.lock();
                        deadlocked(&table, file, owner, exclusive, start, end)
                    };
                    if cycle {
                        return Err(Errno::EDEADLK);
                    }
                }
                wait_for_lock(file, owner, exclusive, start, end)?;
            }
        }
    }
}

/// `flock(2)`. The lock spans the whole file and belongs to `description`;
/// `table` only supplies the reporting pid and the principal charged.
pub fn file_lock_flock(
    file: LockFile,
    description: u64,
    table: FdTable,
    operation: u32,
) -> Result<(), Errno> {
    let block = (operation as u64 & LOCK_NB) == 0;
    let owner = Owner::Description(description);
    let charged = table.handle().map(OwnerProcess::of);
    let pid = table.id();
    match operation as u64 & !LOCK_NB {
        LOCK_UN => release(file, owner, 0, u64::MAX),
        LOCK_SH => acquire(file, owner, charged, pid, false, 0, u64::MAX, block),
        LOCK_EX => acquire(file, owner, charged, pid, true, 0, u64::MAX, block),
        _ => Err(Errno::EINVAL),
    }
}

/// `fcntl(2)` record locks. The range is already absolute: only the caller can
/// fold `l_whence` in, because only it sees the descriptor's position and the
/// file's size.
///
/// `F_GETLK` rewrites `lock` with the conflicting holder, or sets `l_type` to
/// `F_UNLCK` when the range is free.
pub fn file_lock_record(
    file: LockFile,
    table: FdTable,
    cmd: u64,
    start: u64,
    end: u64,
    lock: &mut UserFlock,
) -> Result<(), Errno> {
    let pid = table.id();
    let charged = table.handle().map(OwnerProcess::of);
    let owner = Owner::Process(charged);
    let exclusive = match lock.l_type {
        F_RDLCK => false,
        F_WRLCK => true,
        F_UNLCK => {
            return match cmd {
                F_SETLK | F_SETLKW => release(file, owner, start, end),
                _ => {
                    let found = {
                        let table = TABLE.lock();
                        conflicting(&table.rows, file, owner, true, start, end)
                    };
                    report_getlk(lock, found, start, end);
                    Ok(())
                }
            };
        }
        _ => return Err(Errno::EINVAL),
    };

    match cmd {
        F_SETLK => acquire(file, owner, charged, pid, exclusive, start, end, false),
        F_SETLKW => acquire(file, owner, charged, pid, exclusive, start, end, true),
        _ => {
            let found = {
                let table = TABLE.lock();
                conflicting(&table.rows, file, owner, exclusive, start, end)
            };
            report_getlk(lock, found, start, end);
            Ok(())
        }
    }
}

fn report_getlk(lock: &mut UserFlock, found: Option<Row>, start: u64, end: u64) {
    match found {
        Some(row) => {
            lock.l_type = if row.exclusive { F_WRLCK } else { F_RDLCK };
            lock.l_whence = slopos_abi::syscall::SEEK_SET as i16;
            lock.l_start = row.start as i64;
            lock.l_len = if row.end == u64::MAX {
                0
            } else {
                (row.end - row.start) as i64
            };
            lock.l_pid = row.pid as i32;
        }
        None => {
            lock.l_type = F_UNLCK;
            lock.l_whence = slopos_abi::syscall::SEEK_SET as i16;
            lock.l_start = start as i64;
            lock.l_len = if end == u64::MAX {
                0
            } else {
                (end - start) as i64
            };
            lock.l_pid = 0;
        }
    }
}

/// Drop every `flock` lock held by one open file description — the last close
/// of the last descriptor naming it.
pub(super) fn flock_release_description(description: u64) {
    if ACTIVE_ROWS.load(Ordering::Relaxed) == 0 {
        return;
    }
    drop_rows(|row| row.owner == Owner::Description(description));
}

/// Drop `process`'s record locks on one file — the `close(2)` path.
///
/// POSIX and Linux release *all* of a process's record locks on a file when it
/// closes *any* descriptor referring to it. It also stops a row outliving the
/// inode it keys on, whose number can be reissued.
pub(super) fn release_record_locks_on_close(process: Option<Handle<Process>>, file: LockFile) {
    if ACTIVE_ROWS.load(Ordering::Relaxed) == 0 {
        return;
    }
    let owner = Owner::Process(process.map(OwnerProcess::of));
    drop_rows(|row| row.owner == owner && row.file == file);
}

/// Drop every record lock held by a process; POSIX record locks die with the
/// process that took them. Keyed on the generation-checked handle, since a
/// rebound slot names a different process.
pub fn file_locks_release_process(process: Handle<Process>) {
    if ACTIVE_ROWS.load(Ordering::Relaxed) == 0 {
        return;
    }
    let owner = Owner::Process(Some(OwnerProcess::of(process)));
    drop_rows(|row| row.owner == owner);
}

fn drop_rows(predicate: impl Fn(&Row) -> bool) {
    let mut removed = false;
    {
        let mut table = TABLE.lock();
        for slot in table.rows.iter_mut() {
            let hit = matches!(slot, Some(row) if predicate(row));
            if hit && slot.take().is_some() {
                ACTIVE_ROWS.fetch_sub(1, Ordering::Relaxed);
                removed = true;
            }
        }
    }
    if removed {
        wake_waiters();
    }
}

/// Rows held right now. Fixture and test use.
pub fn file_locks_active() -> usize {
    ACTIVE_ROWS.load(Ordering::Relaxed)
}

/// Wakes this table has issued. Kernel-phase tests run before the scheduler
/// is online, so no task can be parked to observe a wake directly; this is
/// the notification itself.
#[cfg(feature = "tests")]
static WAKE_CALLS: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "tests")]
pub(super) fn lock_wake_count() -> usize {
    WAKE_CALLS.load(Ordering::Relaxed)
}

/// A principal no live process can be, for rows a fixture parks in the table.
#[cfg(feature = "tests")]
const TEST_PRINCIPAL: Principal = Some(OwnerProcess {
    slot: u32::MAX,
    generation: u64::MAX,
});

/// Occupy `count` rows on files nothing else names, bypassing admission, and
/// answer how many landed. Reaching these states for real needs dozens of
/// live processes, because admission bounds what one principal can take.
#[cfg(feature = "tests")]
pub(super) fn occupy_rows_for_test(count: usize) -> usize {
    let mut table = TABLE.lock();
    let mut placed = 0usize;
    for index in 0..count {
        let row = Row {
            file: LockFile::handle(u8::MAX, index as u64),
            owner: Owner::Description(u64::MAX - index as u64),
            charged: TEST_PRINCIPAL,
            exclusive: true,
            start: 0,
            end: u64::MAX,
            pid: 0,
        };
        if insert(&mut table.rows, row).is_err() {
            break;
        }
        placed += 1;
    }
    placed
}

/// Drop everything [`occupy_rows_for_test`] placed.
#[cfg(feature = "tests")]
pub(super) fn release_test_rows() {
    drop_rows(|row| row.charged == TEST_PRINCIPAL);
}

/// Register the parked record-lock request `owner_table`'s process would have
/// if blocked on `[start, end)`, so a test can build one arm of a wait-for
/// cycle without a second task.
#[cfg(feature = "tests")]
pub(super) fn park_record_for_test(
    owner_table: FdTable,
    file: LockFile,
    exclusive: bool,
    start: u64,
    end: u64,
) -> Option<ParkSlot> {
    let owner = Owner::Process(owner_table.handle().map(OwnerProcess::of));
    ParkSlot::register(owner, file, exclusive, start, end)
}

/// The decision `F_SETLKW` turns into `EDEADLK`, exposed on its own so a test
/// can assert it without a second task to park.
#[cfg(feature = "tests")]
pub(super) fn would_deadlock_for_test(
    owner_table: FdTable,
    file: LockFile,
    exclusive: bool,
    start: u64,
    end: u64,
) -> bool {
    let owner = Owner::Process(owner_table.handle().map(OwnerProcess::of));
    let table = TABLE.lock();
    deadlocked(&table, file, owner, exclusive, start, end)
}
