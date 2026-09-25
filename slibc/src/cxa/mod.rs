//! The Itanium C++ ABI's libc half, and the `atexit` list it shares.
//!
//! libc++abi names `__cxa_atexit` and `__cxa_finalize` as the C library's and
//! defines neither, so a C++ program's static destructors run only if they are
//! here. Clang emits `__cxa_atexit(dtor, object, &__dso_handle)` for every
//! one, which is why the list is keyed by a handle.
//!
//! Three things are load-bearing:
//!
//! - **One list.** `atexit(3)` registers here too, as a record whose argument
//!   is the handler itself, because C++ requires the two orders to interleave:
//!   a destructor registered after an `atexit` handler runs before it.
//! - **Order is a sequence number, not a slot position.** A slot a `dlclose`
//!   frees is refilled by the next registration, so a `dlopen`/`dlclose` loop
//!   cannot consume the table; taking the highest sequence rather than the
//!   highest index is what keeps that reuse from reversing LIFO.
//! - **The lock is not held across a call.** A destructor may register another
//!   one, call `exit`, or unload the object it belongs to.

use core::ffi::{c_int, c_void};
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

/// Registrations one process may hold. C requires 32; a miss is the `-1`
/// `atexit` is allowed to answer rather than a reallocation under the lock.
const CXA_MAX: usize = 256;

#[derive(Clone, Copy)]
struct Registration {
    dtor: unsafe extern "C" fn(*mut c_void),
    arg: *mut c_void,
    dso: *mut c_void,
    seq: u64,
}

static mut REGISTRATIONS: [Option<Registration>; CXA_MAX] = [None; CXA_MAX];
static mut NEXT_SEQ: u64 = 0;

/// A spin lock: the scans under it are bounded and short.
static LOCK: AtomicBool = AtomicBool::new(false);

pub(crate) struct Guard;

impl Guard {
    pub(crate) fn take() -> Self {
        while LOCK
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        Self
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        LOCK.store(false, Ordering::Release);
    }
}

fn slots() -> *mut Option<Registration> {
    (&raw mut REGISTRATIONS).cast()
}

fn push(dtor: unsafe extern "C" fn(*mut c_void), arg: *mut c_void, dso: *mut c_void) -> c_int {
    let _guard = Guard::take();
    let slots = slots();
    unsafe {
        for slot in 0..CXA_MAX {
            let entry = slots.add(slot);
            if (*entry).is_some() {
                continue;
            }
            *entry = Some(Registration {
                dtor,
                arg,
                dso,
                seq: NEXT_SEQ,
            });
            NEXT_SEQ += 1;
            return 0;
        }
    }
    -1
}

/// Takes the most recently registered entry `select` accepts, so the caller can
/// run it with the lock dropped. Returns `None` when the list holds no more.
fn take_last(select: impl Fn(&Registration) -> bool) -> Option<Registration> {
    let _guard = Guard::take();
    let slots = slots();
    let mut latest: Option<(u64, usize)> = None;
    unsafe {
        for slot in 0..CXA_MAX {
            let Some(held) = &*slots.add(slot) else {
                continue;
            };
            if select(held) && latest.is_none_or(|(seq, _)| held.seq > seq) {
                latest = Some((held.seq, slot));
            }
        }
        (*slots.add(latest?.1)).take()
    }
}

fn run_matching(select: impl Fn(&Registration) -> bool + Copy) {
    while let Some(registration) = take_last(select) {
        unsafe { (registration.dtor)(registration.arg) };
    }
}

/// # Safety
/// `dtor` is callable with `arg`, and `dso` is an object identity — compared,
/// never dereferenced.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __cxa_atexit(
    dtor: unsafe extern "C" fn(*mut c_void),
    arg: *mut c_void,
    dso: *mut c_void,
) -> c_int {
    push(dtor, arg, dso)
}

/// The `atexit(3)` handler takes no argument, so it is carried as its own
/// argument and this trampoline calls it. That is what puts both kinds of
/// registration in one list, in one order.
///
/// # Safety
/// `handler` is an `atexit` handler passed as its own argument.
unsafe extern "C" fn call_atexit_handler(handler: *mut c_void) {
    let handler: unsafe extern "C" fn() = core::mem::transmute(handler);
    handler();
}

/// # Safety
/// `handler` is callable and outlives the process.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atexit(handler: unsafe extern "C" fn()) -> c_int {
    push(call_atexit_handler, handler as *mut c_void, ptr::null_mut())
}

/// # Safety
/// `dso` is an object identity, or null for "every registration".
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __cxa_finalize(dso: *mut c_void) {
    if dso.is_null() {
        run_matching(|_| true);
    } else {
        run_matching(|registration| registration.dso == dso);
    }
}

/// Runs the registrations of an object being unloaded, identified by the span
/// it was mapped at rather than by one handle: `__dso_handle` is hidden and
/// absent from the object's `.dynsym`, so the loader has nothing to look up.
///
/// All three fields are tested, not just the handle, because `atexit(3)`
/// registers a null one — glibc's `atexit` lives in `libc_nonshared.a` and is
/// linked into every object, slibc's is one shared copy with no per-caller
/// handle to record. Otherwise an `atexit` from inside a `dlopen`ed object
/// outlives its own code and is called at `exit` through an unmapped address.
pub unsafe fn finalize_range(start: usize, len: usize) {
    let end = start.saturating_add(len);
    let inside = move |addr: usize| addr >= start && addr < end;
    run_matching(move |registration| {
        inside(registration.dso as usize)
            || inside(registration.dtor as usize)
            || inside(registration.arg as usize)
    });
}

/// Thread-local destructors, run at thread exit in reverse registration order.
/// Per-thread, so no lock; a stack rather than the keyed list above, because
/// nothing selects a subset of it.
///
/// libc++abi has a `pthread_key_create` fallback for a libc without this, and
/// that fallback runs the initial thread's destructors from a static object's
/// destructor — after `exit` has begun. Implementing it is what puts them at
/// thread exit, where C++ says they belong.
///
/// The list is on the heap rather than in the thread's TLS block: an inline
/// array worth having is that many bytes in *every* thread's static block,
/// and a thread that registers none is the common case.
#[derive(Clone, Copy)]
struct ThreadDtor {
    dtor: unsafe extern "C" fn(*mut c_void),
    arg: *mut c_void,
}

#[thread_local]
static mut THREAD_DTORS: *mut ThreadDtor = ptr::null_mut();
#[thread_local]
static mut THREAD_DTOR_COUNT: usize = 0;
#[thread_local]
static mut THREAD_DTOR_CAPACITY: usize = 0;

const THREAD_DTORS_INITIAL: usize = 8;

/// # Safety
/// `dtor` is callable with `arg` on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __cxa_thread_atexit_impl(
    dtor: unsafe extern "C" fn(*mut c_void),
    arg: *mut c_void,
    _dso: *mut c_void,
) -> c_int {
    if THREAD_DTOR_COUNT == THREAD_DTOR_CAPACITY {
        let capacity = if THREAD_DTOR_CAPACITY == 0 {
            THREAD_DTORS_INITIAL
        } else {
            THREAD_DTOR_CAPACITY * 2
        };
        let bytes = match capacity.checked_mul(size_of::<ThreadDtor>()) {
            Some(bytes) => bytes,
            None => return -1,
        };
        let grown = crate::mem::malloc::realloc(THREAD_DTORS.cast(), bytes);
        if grown.is_null() {
            return -1;
        }
        THREAD_DTORS = grown.cast();
        THREAD_DTOR_CAPACITY = capacity;
    }
    THREAD_DTORS
        .add(THREAD_DTOR_COUNT)
        .write(ThreadDtor { dtor, arg });
    THREAD_DTOR_COUNT += 1;
    0
}

/// # Safety
/// Called on the exiting thread, before its TLS block is released.
pub unsafe fn run_thread_destructors() {
    // A destructor may register another one, so the count is re-read each
    // time round rather than captured — and the list may have moved.
    while THREAD_DTOR_COUNT > 0 {
        THREAD_DTOR_COUNT -= 1;
        let registration = THREAD_DTORS.add(THREAD_DTOR_COUNT).read();
        (registration.dtor)(registration.arg);
    }
    crate::mem::malloc::dealloc(THREAD_DTORS.cast());
    THREAD_DTORS = ptr::null_mut();
    THREAD_DTOR_CAPACITY = 0;
}
