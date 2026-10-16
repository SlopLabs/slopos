//! Freeing a generation nothing holds any more.
//!
//! The booted generation stays for the boot: it costs no copy, and what init
//! made at boot, the consent pair among it, names its programs. Another that
//! is not current, an activation no longer shown among them, is held while an
//! open file names one of its inodes, while a mapping or a fault in flight
//! holds a page set of one, and while a live process runs one of its
//! programs. A socket's captured identity holds nothing: one whose generation
//! has gone reads as no program.
//!
//! Those three tables are where every reference to a base inode outlives a
//! system call, exec's included ([`super::Hold`]), so a pass reads them
//! rather than counting references beside them. A generation is freed once
//! two passes a second apart find nothing holding it, the first as soon as it
//! stops being current: a walk that resolved one of its inodes just before
//! has a second to open it, and one slower finds it gone. Passes that find
//! nothing to mark or free come further apart, up to a minute.

use core::sync::atomic::{AtomicBool, Ordering};

use slopos_ostd::process::{Program, with_process_registry};
use slopos_ostd::sync::kernel_io_task::{KernelIoStop, KernelIoToken, KthreadWait};
use slopos_ostd::sync::lock_tracking::LOCK_LEVEL_RESOURCE;
use slopos_ostd::sync::{InitFlag, Mutex};
use slopos_ostd::{klog_info, lock_class};

use super::{BASE_FS, BaseFs, Slot, extension, generation_of, is_base};

const PASS_MS: u64 = 1000;
/// A generation held pass after pass, as one the boot's programs run from
/// is for good, is looked at less often, up to this.
const PASS_MAX_MS: u64 = 60_000;

static REAP_STOP: KernelIoStop = KernelIoStop::new(
    "basefs-reap",
    lock_class!("BASEFS_REAP_STOP.waiters", LOCK_LEVEL_RESOURCE),
);
static REAPER_STARTED: InitFlag = InitFlag::new();
/// One pass at a time, the reaper's or a caller's of [`BaseFs::reap`]: a pass
/// judges a generation held and marks it later, so a second pass between the
/// two would find a mark the first was about to undo.
static PASSING: Mutex<()> = Mutex::new((), lock_class!("BASEFS_REAP_PASS", LOCK_LEVEL_RESOURCE));
/// A generation stopped being current since the last pass.
static SWITCHED: AtomicBool = AtomicBool::new(false);

impl BaseFs {
    fn held(&'static self, number: u32) -> bool {
        let mine = |inode| generation_of(inode) == number;
        crate::vfs::orphan::any_open(self, mine)
            || crate::filemap::any_mapped(self, mine)
            || runs_from(self, number)
    }

    /// One pass over the generations that are neither current nor kept: a
    /// held one is marked held, a quiet one marked quiet is freed and one
    /// found quiet for the first time is marked. Answers how many it freed.
    pub fn reap(&'static self) -> usize {
        self.pass().freed
    }

    fn pass(&'static self) -> Pass {
        let mut pass = Pass::default();
        let Ok(_one_at_a_time) = PASSING.lock() else {
            return pass;
        };
        let mut after = 0;
        while let Some(number) = self.next_retired(after) {
            after = number;
            let held = self.held(number);
            match self.mark(number, held) {
                Mark::Held => {}
                Mark::Quiet => pass.marked = true,
                Mark::Freed(slot) => {
                    crate::filemap::forget_where(self, |inode| generation_of(inode) == number);
                    drop(slot);
                    if is_base(self) {
                        klog_info!("BASE: generation {} freed", number);
                    } else {
                        klog_info!("EXT: activation {} freed", number);
                    }
                    pass.freed += 1;
                }
            }
        }
        pass
    }

    fn next_retired(&self, after: u32) -> Option<u32> {
        let table = self.table.lock();
        table
            .slots
            .iter()
            .flatten()
            .map(|slot| (slot.admitted, slot.generation.number))
            .filter(|&(admitted, number)| {
                !admitted && number != table.current && number != table.kept && number > after
            })
            .map(|(_, number)| number)
            .min()
    }

    fn mark(&self, number: u32, held: bool) -> Mark {
        let mut table = self.table.lock();
        let current = table.current;
        let Some(slot) = table.slots.iter_mut().find(|slot| {
            slot.as_ref()
                .is_some_and(|s| s.generation.number == number && number != current)
        }) else {
            return Mark::Held;
        };
        let Some(state) = slot.as_mut() else {
            return Mark::Held;
        };
        let mark = if held {
            state.quiet = false;
            Mark::Held
        } else if state.quiet {
            slot.take().map_or(Mark::Held, Mark::Freed)
        } else {
            state.quiet = true;
            Mark::Quiet
        };
        self.note_retired(&table);
        mark
    }
}

#[derive(Default)]
struct Pass {
    freed: usize,
    /// A generation was found quiet for the first time: the pass that may
    /// free it is due a second later.
    marked: bool,
}

impl Pass {
    fn and(self, other: Pass) -> Pass {
        Pass {
            freed: self.freed + other.freed,
            marked: self.marked || other.marked,
        }
    }
}

enum Mark {
    Held,
    Quiet,
    Freed(Slot),
}

/// Whether a live process runs a program of `fs`'s generation `number`.
pub(crate) fn runs_from(fs: &BaseFs, number: u32) -> bool {
    let base = is_base(fs);
    if !base && !extension::is_activation(fs) {
        return false;
    }
    with_process_registry(|table| {
        table.any(|process| {
            !process.has_exited()
                && match process.program() {
                    Program::Base(inode) => base && generation_of(inode.get()) == number,
                    Program::Extension(inode) => !base && generation_of(inode.get()) == number,
                    Program::Other => false,
                }
        })
    })
    .unwrap_or(false)
}

fn reaped() -> impl Iterator<Item = &'static BaseFs> {
    core::iter::once(&BASE_FS).chain(extension::POOL.iter())
}

fn any_retired() -> bool {
    reaped().any(|fs| fs.retired.load(Ordering::Acquire))
}

/// Have the base's and the activations' generations reaped from now on, and
/// wake the reaper.
pub(super) fn wake() {
    SWITCHED.store(true, Ordering::Release);
    if REAPER_STARTED.init_once() && slopos_ostd::spawn_kernel_io!(&REAP_STOP, reaper).is_err() {
        REAPER_STARTED.reset();
    }
    REAP_STOP.wake_for_work();
}

fn reaper(token: KernelIoToken<'static>) {
    let mut interval = PASS_MS;
    loop {
        let waited = if any_retired() {
            token.park_timeout(&REAP_STOP, || SWITCHED.load(Ordering::Acquire), interval)
        } else {
            token.park(&REAP_STOP, any_retired)
        };
        if waited == KthreadWait::Stop {
            REAP_STOP.note_exited();
            return;
        }
        let switched = SWITCHED.swap(false, Ordering::AcqRel);
        let pass = reaped().fold(Pass::default(), |pass, fs| pass.and(fs.pass()));
        interval = if pass.freed > 0 || pass.marked || switched {
            PASS_MS
        } else {
            (interval * 2).min(PASS_MAX_MS)
        };
    }
}
