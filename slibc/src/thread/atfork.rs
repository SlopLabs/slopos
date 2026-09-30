//! `pthread_atfork`: handlers `fork` runs around itself, so a library holding
//! a lock across the call can take it in the parent before the copy and
//! release it on both sides after.

use core::ffi::c_int;

use crate::errno::ENOMEM;

use super::mutex::{
    PTHREAD_MUTEX_INITIALIZER, pthread_mutex_lock, pthread_mutex_t, pthread_mutex_unlock,
};

type Handler = Option<unsafe extern "C" fn()>;

#[derive(Clone, Copy)]
struct Entry {
    prepare: Handler,
    parent: Handler,
    child: Handler,
}

const CAPACITY: usize = 64;

static mut LOCK: pthread_mutex_t = PTHREAD_MUTEX_INITIALIZER;
static mut ENTRIES: [Entry; CAPACITY] = [Entry {
    prepare: None,
    parent: None,
    child: None,
}; CAPACITY];
static mut COUNT: usize = 0;

/// `pthread_atfork(3)`. A registration lasts as long as the object its
/// handlers are in, as glibc's does; 64 of them at most, `ENOMEM` past that.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_atfork(
    prepare: Option<unsafe extern "C" fn()>,
    parent: Option<unsafe extern "C" fn()>,
    child: Option<unsafe extern "C" fn()>,
) -> c_int {
    pthread_mutex_lock(&raw mut LOCK);
    let rc = if COUNT == CAPACITY {
        ENOMEM.raw()
    } else {
        ENTRIES[COUNT] = Entry {
            prepare,
            parent,
            child,
        };
        COUNT += 1;
        0
    };
    pthread_mutex_unlock(&raw mut LOCK);
    rc
}

/// Drop every registration with a handler in `[start, start + len)`, an
/// object `dlclose` is about to unmap.
pub(crate) unsafe fn forget_range(start: usize, len: usize) {
    let end = start.saturating_add(len);
    let inside = |handler: Handler| handler.is_some_and(|f| (start..end).contains(&(f as usize)));
    pthread_mutex_lock(&raw mut LOCK);
    let mut kept = 0;
    for at in 0..COUNT {
        let entry = ENTRIES[at];
        if !(inside(entry.prepare) || inside(entry.parent) || inside(entry.child)) {
            ENTRIES[kept] = entry;
            kept += 1;
        }
    }
    COUNT = kept;
    pthread_mutex_unlock(&raw mut LOCK);
}

/// Before the copy: the prepare handlers, last registered first. Holds the
/// registry until [`after_fork`], so no registration lands between them.
pub(crate) unsafe fn before_fork() {
    pthread_mutex_lock(&raw mut LOCK);
    for entry in ENTRIES[..COUNT].iter().rev() {
        if let Some(prepare) = entry.prepare {
            prepare();
        }
    }
}

/// After the copy, on each side: its handlers, in registration order.
pub(crate) unsafe fn after_fork(in_child: bool) {
    for entry in &ENTRIES[..COUNT] {
        if let Some(handler) = if in_child { entry.child } else { entry.parent } {
            handler();
        }
    }
    if in_child {
        // The child's only thread holds it.
        LOCK = PTHREAD_MUTEX_INITIALIZER;
    } else {
        pthread_mutex_unlock(&raw mut LOCK);
    }
}
