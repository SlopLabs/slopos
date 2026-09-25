//! The standing proof that slibc is the C library the `libc` crate declares.
//!
//! `x86_64-unknown-slopos` rides upstream `sys/pal/unix`, so every struct std
//! touches is declared once in `libc/src/unix/slopos/mod.rs` and defined once
//! in slibc. Nothing links the two: a struct that disagrees compiles clean on
//! both sides and then reads the wrong offset at run time. That is a
//! *miscompile*, not a compile error, which is why the check has to be a
//! running program rather than an assertion.
//!
//! The layout numbers themselves are pinned at compile time by the
//! `const _: () = assert!` blocks in `slibc/src/types.rs` and `abi/`, so this
//! file deliberately asserts none of them again. What it asserts is what only
//! a run can show: that the *translations* between the two shapes are
//! faithful, and that two layers reading the same fact agree on it.

use slopos_userland::syscall::UserCpuInfo;
use slopos_userland::syscall::core as sys_core;

use std::collections::BTreeSet;
use std::ffi::c_void;
use std::fs;
use std::mem;
use std::os::unix::process::ExitStatusExt;
use std::process::Command;
use std::ptr;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use slopos_abi::fs::UserIovec;
use slopos_abi::signal::{
    MINSIGSTKSZ, SA_ONSTACK, SA_RESTART, SA_SIGINFO, SEGV_ACCERR, UserSiginfo,
};
use slopos_abi::syscall::{
    CMSG_DATA_OFFSET, CmsgHdr, MAP_ANONYMOUS, MAP_PRIVATE, MsgHdr, PROT_READ, PROT_WRITE,
    SCM_RIGHTS, SOL_SOCKET, cmsg_len, cmsg_space,
};
use slopos_abi::unix::SockAddrUn;
use slopos_slibc::conf::{_SC_NPROCESSORS_CONF, _SC_NPROCESSORS_ONLN, _SC_PAGESIZE, sysconf};
use slopos_slibc::errno::{EBUSY, ETIMEDOUT};
use slopos_slibc::ffi::syscalls::{mmap, mprotect, munmap, realpath, slopos_getdents64};
use slopos_slibc::ffi::{O_DIRECTORY, O_RDONLY, close, open};
use slopos_slibc::io::dirent::DirentIter;
use slopos_slibc::net::{
    AF_UNIX, SOCK_STREAM, accept, bind, connect, listen, recvmsg, sendmsg, socket,
};
use slopos_slibc::signal::{self, SIG_DFL, SIGSEGV, SIGUSR1, SIGUSR2};
use slopos_slibc::test_harness::note;
use slopos_slibc::thread::{
    pthread_attr_t, pthread_cond_t, pthread_mutex_t, pthread_rwlock_t, pthread_self,
};
use slopos_slibc::time::{CLOCK_REALTIME, Timespec, clock_gettime};
use slopos_slibc::types::{dirent, sigaction as SigAction, sigset_t as SigSet, stack_t};

/// Writable on the disk root and on the initramfs alike, as the other userland
/// tests use `/var/<name>`.
const WORK: &str = "/var/libcabi";

fn work_dir() -> bool {
    let _ = fs::create_dir_all(WORK);
    fs::metadata(WORK).map(|m| m.is_dir()).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// The four pthread objects, zero-initialised.
//
// `PTHREAD_MUTEX_INITIALIZER`, `PTHREAD_COND_INITIALIZER` and
// `PTHREAD_RWLOCK_INITIALIZER` are all-zero in the libc module, and std
// allocates its objects with exactly those and only calls `_init` on some
// paths. So a zeroed object has to be a working one.
// ---------------------------------------------------------------------------

/// A C consumer's `pthread_mutex_t m = PTHREAD_MUTEX_INITIALIZER;` and
/// `pthread_rwlock_t rw = PTHREAD_RWLOCK_INITIALIZER;` — used with no `_init`
/// call at all. A zero state that meant "locked", or a reader count that
/// refused the second reader, fails here.
fn zeroed_pthread_locks_work_without_init() -> bool {
    let mut m: pthread_mutex_t = unsafe { mem::zeroed() };
    let mutex_ok = unsafe {
        slopos_slibc::thread::mutex::pthread_mutex_lock(&mut m) == 0
            && slopos_slibc::thread::mutex::pthread_mutex_trylock(&mut m) == EBUSY.raw()
            && slopos_slibc::thread::mutex::pthread_mutex_unlock(&mut m) == 0
            && slopos_slibc::thread::mutex::pthread_mutex_trylock(&mut m) == 0
            && slopos_slibc::thread::mutex::pthread_mutex_unlock(&mut m) == 0
    };
    if !mutex_ok {
        eprintln!("libc_abi_test: a zeroed pthread_mutex_t is not a usable unlocked mutex");
        return false;
    }

    let mut rw: pthread_rwlock_t = unsafe { mem::zeroed() };
    let rwlock_ok = unsafe {
        slopos_slibc::thread::rwlock::pthread_rwlock_rdlock(&mut rw) == 0
            && slopos_slibc::thread::rwlock::pthread_rwlock_tryrdlock(&mut rw) == 0
            && slopos_slibc::thread::rwlock::pthread_rwlock_trywrlock(&mut rw) == EBUSY.raw()
            && slopos_slibc::thread::rwlock::pthread_rwlock_unlock(&mut rw) == 0
            && slopos_slibc::thread::rwlock::pthread_rwlock_unlock(&mut rw) == 0
            && slopos_slibc::thread::rwlock::pthread_rwlock_trywrlock(&mut rw) == 0
            && slopos_slibc::thread::rwlock::pthread_rwlock_unlock(&mut rw) == 0
    };
    if !rwlock_ok {
        eprintln!(
            "libc_abi_test: a zeroed pthread_rwlock_t does not admit two readers then a writer"
        );
        return false;
    }
    true
}

/// A zeroed `pthread_cond_t` names clock 0, and clock 0 is `CLOCK_REALTIME`.
/// `pthread_cond_timedwait`'s `abstime` is measured against whichever clock
/// the object names, so a zero that meant `CLOCK_MONOTONIC` would read a
/// realtime deadline as a boot-relative one and block for decades instead of
/// timing out.
fn zeroed_pthread_cond_times_out_on_the_realtime_clock() -> bool {
    let mut cond: pthread_cond_t = unsafe { mem::zeroed() };
    let mut mutex: pthread_mutex_t = unsafe { mem::zeroed() };

    let mut now = Timespec::default();
    if unsafe { clock_gettime(CLOCK_REALTIME, &mut now) } != 0 {
        eprintln!("libc_abi_test: clock_gettime(CLOCK_REALTIME) failed");
        return false;
    }
    // Already elapsed: a correct implementation reports ETIMEDOUT at once.
    let deadline = Timespec {
        tv_sec: now.tv_sec - 1,
        tv_nsec: now.tv_nsec,
    };

    unsafe {
        if slopos_slibc::thread::mutex::pthread_mutex_lock(&mut mutex) != 0 {
            eprintln!("libc_abi_test: could not lock the cond's mutex");
            return false;
        }
        let rc =
            slopos_slibc::thread::condvar::pthread_cond_timedwait(&mut cond, &mut mutex, &deadline);
        if rc != ETIMEDOUT.raw() {
            eprintln!("libc_abi_test: pthread_cond_timedwait past its deadline returned {rc}");
            let _ = slopos_slibc::thread::mutex::pthread_mutex_unlock(&mut mutex);
            return false;
        }
        // POSIX: the mutex is re-acquired before the wait returns.
        if slopos_slibc::thread::mutex::pthread_mutex_trylock(&mut mutex) != EBUSY.raw() {
            eprintln!("libc_abi_test: pthread_cond_timedwait did not re-acquire the mutex");
            return false;
        }
        if slopos_slibc::thread::mutex::pthread_mutex_unlock(&mut mutex) != 0 {
            eprintln!("libc_abi_test: unlocking after pthread_cond_timedwait failed");
            return false;
        }
    }
    true
}

/// std's own `Mutex` and `Condvar` are `COpaque<libc::pthread_mutex_t>` and
/// `COpaque<libc::pthread_cond_t>` — allocated at libc's declared size and
/// driven by slibc's futex primitives. Four threads contending, then a
/// condvar handoff, is what a lost wakeup or a mutex that does not exclude
/// fails.
fn std_mutex_and_condvar_carry_real_threads() -> bool {
    const THREADS: usize = 4;
    const BUMPS: usize = 2000;

    let counter = Arc::new(Mutex::new(0usize));
    let mut handles = Vec::with_capacity(THREADS);
    for _ in 0..THREADS {
        let counter = Arc::clone(&counter);
        match thread::Builder::new().spawn(move || {
            for _ in 0..BUMPS {
                *counter.lock().unwrap() += 1;
            }
        }) {
            Ok(h) => handles.push(h),
            Err(e) => {
                eprintln!("libc_abi_test: spawning a counting thread failed: {e:?}");
                return false;
            }
        }
    }
    for h in handles {
        if h.join().is_err() {
            eprintln!("libc_abi_test: a counting thread panicked");
            return false;
        }
    }
    let total = *counter.lock().unwrap();
    if total != THREADS * BUMPS {
        eprintln!("libc_abi_test: {THREADS} threads x {BUMPS} bumps counted {total}");
        return false;
    }

    let gate = Arc::new((Mutex::new(0u32), Condvar::new()));
    let worker_gate = Arc::clone(&gate);
    let worker = match thread::Builder::new().spawn(move || {
        let (lock, cv) = &*worker_gate;
        let mut seen = lock.lock().unwrap();
        while *seen == 0 {
            seen = cv.wait(seen).unwrap();
        }
        // Answer back through the same pair, so the handoff is proved in both
        // directions rather than only on the notifying side.
        *seen = 2;
        cv.notify_all();
        *seen
    }) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("libc_abi_test: spawning the condvar worker failed: {e:?}");
            return false;
        }
    };

    {
        let (lock, cv) = &*gate;
        *lock.lock().unwrap() = 1;
        cv.notify_all();
        let mut seen = lock.lock().unwrap();
        while *seen != 2 {
            seen = cv.wait(seen).unwrap();
        }
    }

    match worker.join() {
        Ok(2) => true,
        Ok(other) => {
            eprintln!("libc_abi_test: the condvar worker saw {other}, expected 2");
            false
        }
        Err(_) => {
            eprintln!("libc_abi_test: the condvar worker panicked");
            false
        }
    }
}

/// `pthread_attr_t` is the one pthread object std fills in on its own stack
/// and hands to slibc: `Builder::stack_size` is `pthread_attr_init` plus
/// `pthread_attr_setstacksize` on a libc-sized allocation, and the thread's
/// real stack comes back out of it. A request that never reached the created
/// thread leaves it on slibc's 2 MiB default, which this sees.
fn a_requested_thread_stack_size_reaches_the_thread() -> bool {
    const WANT: usize = 512 * 1024;

    let reported = Arc::new(AtomicUsize::new(0));
    let out = Arc::clone(&reported);
    let handle = match thread::Builder::new().stack_size(WANT).spawn(move || {
        let mut attr: pthread_attr_t = unsafe { mem::zeroed() };
        let rc = unsafe { slopos_slibc::thread::pthread_getattr_np(pthread_self(), &mut attr) };
        if rc != 0 {
            return rc;
        }
        let mut size = 0usize;
        let rc = unsafe { slopos_slibc::thread::pthread_attr_getstacksize(&attr, &mut size) };
        out.store(size, Ordering::SeqCst);
        rc
    }) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("libc_abi_test: spawning a sized thread failed: {e:?}");
            return false;
        }
    };
    match handle.join() {
        Ok(0) => {}
        Ok(rc) => {
            eprintln!("libc_abi_test: reading the thread's own attr answered {rc}");
            return false;
        }
        Err(_) => {
            eprintln!("libc_abi_test: the sized thread panicked");
            return false;
        }
    }

    let got = reported.load(Ordering::SeqCst);
    // The guard page is subtracted from the mapping, so the usable stack is at
    // most what was asked for and never the 2 MiB default.
    if got == 0 || got > WANT {
        eprintln!("libc_abi_test: a {WANT}-byte stack request produced {got} usable bytes");
        return false;
    }
    true
}

/// POSIX's default `guardsize` is one page, and slibc's `pthread_create` reads
/// a zero on an attr that carries a stack size as a deliberate
/// `pthread_attr_setguardsize(attr, 0)` — so an `init` that lost the default
/// would give every std-created sized thread an unguarded stack, where an
/// overflow writes into whatever is mapped below instead of faulting.
fn attr_init_reports_a_guard_page() -> bool {
    let mut attr: pthread_attr_t = unsafe { mem::zeroed() };
    if unsafe { slopos_slibc::thread::pthread_attr_init(&mut attr) } != 0 {
        eprintln!("libc_abi_test: pthread_attr_init failed");
        return false;
    }
    let mut guard = 0usize;
    let rc = unsafe { slopos_slibc::thread::pthread_attr_getguardsize(&attr, &mut guard) };
    let _ = unsafe { slopos_slibc::thread::pthread_attr_destroy(&mut attr) };
    if rc != 0 || guard == 0 {
        eprintln!("libc_abi_test: pthread_attr_init reports guardsize {guard} (rc {rc})");
        return false;
    }
    true
}

// ---------------------------------------------------------------------------
// `readdir` against the kernel's own records.
// ---------------------------------------------------------------------------

/// `std::fs::read_dir` is `opendir`/`readdir`/`closedir`, and slibc re-packs
/// every kernel record on the way: the `getdents64` header pads to 24 bytes
/// and `struct dirent`'s name starts at 19. Enumerating the same directory
/// both ways is what catches a re-pack that truncates, shifts or drops a name.
fn read_dir_names_match_getdents64() -> bool {
    if !work_dir() {
        eprintln!("libc_abi_test: {WORK} is not usable");
        return false;
    }
    let dir = format!("{WORK}/dents");
    let _ = fs::remove_dir_all(&dir);
    if fs::create_dir_all(&dir).is_err() {
        eprintln!("libc_abi_test: could not create {dir}");
        return false;
    }

    // Lengths chosen to straddle the five-byte difference between the two
    // header sizes and the record's 8-byte alignment padding.
    let mut want = BTreeSet::new();
    for len in [1usize, 2, 3, 4, 5, 6, 7, 8, 9, 16, 23, 24, 25, 254, 255] {
        let name: String = core::iter::repeat_n('n', len - 1)
            .chain(core::iter::once(char::from(b'0' + (len % 10) as u8)))
            .collect();
        if fs::write(format!("{dir}/{name}"), b"x").is_err() {
            eprintln!("libc_abi_test: could not create a {len}-byte name");
            return false;
        }
        want.insert(name);
    }

    let mut via_std = BTreeSet::new();
    match fs::read_dir(&dir) {
        Ok(rd) => {
            for entry in rd {
                match entry {
                    Ok(e) => {
                        via_std.insert(e.file_name().to_string_lossy().into_owned());
                    }
                    Err(e) => {
                        eprintln!("libc_abi_test: read_dir entry failed: {e:?}");
                        return false;
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("libc_abi_test: read_dir({dir}) failed: {e:?}");
            return false;
        }
    }

    let mut via_kernel = BTreeSet::new();
    let path = format!("{dir}\0");
    let fd = unsafe { open(path.as_ptr() as *const i8, O_RDONLY | O_DIRECTORY) };
    if fd < 0 {
        eprintln!("libc_abi_test: open({dir}, O_DIRECTORY) failed");
        return false;
    }
    let mut buf = [0u8; 4096];
    loop {
        let n = unsafe { slopos_getdents64(fd, buf.as_mut_ptr(), buf.len()) };
        if n < 0 {
            eprintln!("libc_abi_test: slopos_getdents64 failed");
            close(fd);
            return false;
        }
        if n == 0 {
            break;
        }
        for rec in DirentIter::new(&buf[..n as usize]) {
            if rec.name == b"." || rec.name == b".." {
                continue;
            }
            via_kernel.insert(String::from_utf8_lossy(rec.name).into_owned());
        }
    }
    close(fd);

    if via_std != want {
        eprintln!(
            "libc_abi_test: read_dir saw {} of {} names",
            via_std.len(),
            want.len()
        );
        for name in want.symmetric_difference(&via_std) {
            eprintln!(
                "libc_abi_test:   std differs on {:?} ({} bytes)",
                name,
                name.len()
            );
        }
        return false;
    }
    if via_kernel != want {
        eprintln!(
            "libc_abi_test: getdents64 saw {} of {} names",
            via_kernel.len(),
            want.len()
        );
        for name in want.symmetric_difference(&via_kernel) {
            eprintln!(
                "libc_abi_test:   kernel differs on {:?} ({} bytes)",
                name,
                name.len()
            );
        }
        return false;
    }

    // `readdir`'s own record, read as C reads it: the name field at 19.
    let cpath = format!("{dir}\0");
    let dirp = unsafe { slopos_slibc::io::dir::opendir(cpath.as_ptr() as *const i8) };
    if dirp.is_null() {
        eprintln!("libc_abi_test: opendir({dir}) failed");
        return false;
    }
    let mut via_readdir = BTreeSet::new();
    loop {
        let ent: *mut dirent = unsafe { slopos_slibc::io::dir::readdir(dirp) };
        if ent.is_null() {
            break;
        }
        let name_ptr = unsafe { (*ent).d_name.as_ptr() } as *const u8;
        let len = slopos_slibc::u_strnlen(name_ptr, 256);
        let bytes = unsafe { core::slice::from_raw_parts(name_ptr, len) };
        if bytes == b"." || bytes == b".." {
            continue;
        }
        via_readdir.insert(String::from_utf8_lossy(bytes).into_owned());
    }
    unsafe { slopos_slibc::io::dir::closedir(dirp) };

    if via_readdir != want {
        eprintln!(
            "libc_abi_test: readdir(3) saw {} of {} names",
            via_readdir.len(),
            want.len()
        );
        for name in want.symmetric_difference(&via_readdir) {
            eprintln!(
                "libc_abi_test:   readdir differs on {:?} ({} bytes)",
                name,
                name.len()
            );
        }
        return false;
    }

    let _ = fs::remove_dir_all(&dir);
    true
}

// ---------------------------------------------------------------------------
// `sigaction` through the 152-byte userspace struct.
// ---------------------------------------------------------------------------

static USR1_SIGNO: AtomicU32 = AtomicU32::new(0);
static USR1_COUNT: AtomicU32 = AtomicU32::new(0);

extern "C" fn on_usr1_siginfo(sig: i32, info: *mut UserSiginfo, _uc: *mut c_void) {
    USR1_COUNT.fetch_add(1, Ordering::SeqCst);
    let signo = if info.is_null() {
        -1
    } else {
        unsafe { (*info).si_signo }
    };
    USR1_SIGNO.store(signo as u32, Ordering::SeqCst);
    let _ = sig;
}

/// The userspace `struct sigaction` is 152 bytes — handler at 0, a 128-byte
/// mask at 8, `sa_flags` at 136 — and the kernel's is 32 with a single-`u64`
/// mask. slibc narrows one to the other on the way in and widens it back on
/// the way out, so a query has to return what was installed: the same
/// handler, the same flags, and a mask read from offset 8 rather than from
/// wherever the kernel struct happens to keep its own.
fn sigaction_roundtrips_the_152_byte_struct() -> bool {
    USR1_COUNT.store(0, Ordering::SeqCst);
    USR1_SIGNO.store(0, Ordering::SeqCst);

    let mut mask = SigSet::empty();
    unsafe { signal::sigaddset(&mut mask, SIGUSR2) };

    let want_flags = (SA_SIGINFO | SA_RESTART) as i32;
    let act = SigAction {
        sa_sigaction: on_usr1_siginfo as *const () as usize,
        sa_mask: mask,
        sa_flags: want_flags,
        sa_restorer: None,
    };
    if unsafe { signal::sigaction(SIGUSR1, &act, ptr::null_mut()) } != 0 {
        eprintln!("libc_abi_test: installing the SIGUSR1 handler failed");
        return false;
    }

    let mut back = SigAction::zeroed();
    if unsafe { signal::sigaction(SIGUSR1, ptr::null(), &mut back) } != 0 {
        eprintln!("libc_abi_test: querying the SIGUSR1 action failed");
        return false;
    }

    let mut ok = true;
    if back.sa_sigaction != act.sa_sigaction {
        eprintln!(
            "libc_abi_test: handler came back as {:#x}, installed {:#x}",
            back.sa_sigaction, act.sa_sigaction
        );
        ok = false;
    }
    if back.sa_flags & want_flags != want_flags {
        eprintln!(
            "libc_abi_test: flags came back as {:#x}, want {want_flags:#x} set",
            back.sa_flags
        );
        ok = false;
    }
    if unsafe { signal::sigismember(&back.sa_mask, SIGUSR2) } != 1 {
        eprintln!("libc_abi_test: SIGUSR2 is missing from the queried sa_mask");
        ok = false;
    }
    if unsafe { signal::sigismember(&back.sa_mask, SIGUSR1) } != 0 {
        eprintln!("libc_abi_test: the queried sa_mask names SIGUSR1, which was never set");
        ok = false;
    }
    if back.sa_restorer.is_none() {
        eprintln!("libc_abi_test: the queried action carries no restorer");
        ok = false;
    }

    if ok {
        if unsafe { signal::raise(SIGUSR1) } != 0 {
            eprintln!("libc_abi_test: raise(SIGUSR1) failed");
            ok = false;
        } else {
            let count = USR1_COUNT.load(Ordering::SeqCst);
            let signo = USR1_SIGNO.load(Ordering::SeqCst);
            if count != 1 {
                eprintln!("libc_abi_test: the SA_SIGINFO handler ran {count} times");
                ok = false;
            }
            if signo != SIGUSR1 as u32 {
                eprintln!("libc_abi_test: the handler's siginfo named signal {signo}");
                ok = false;
            }
        }
    }

    let _ = unsafe { signal::signal(SIGUSR1, SIG_DFL) };
    ok
}

// ---------------------------------------------------------------------------
// `sigset_t`: signal N is bit N-1, narrowed to the kernel's single word.
// ---------------------------------------------------------------------------

/// Signal N is bit N-1 of `sigset_t.__val[0]`, and every narrowing to the
/// kernel's single-`u64` mask rests on that off-by-one: a set that placed
/// SIGUSR1 one bit over would block the wrong signal, and a member past `NSIG`
/// would name a bit the kernel's mask cannot carry at all.
fn sigset_narrows_signal_n_to_bit_n_minus_one() -> bool {
    let mut set = SigSet::empty();
    if unsafe { signal::sigemptyset(&mut set) } != 0 {
        eprintln!("libc_abi_test: sigemptyset failed");
        return false;
    }
    if unsafe { signal::sigismember(&set, SIGUSR1) } != 0 {
        eprintln!("libc_abi_test: a fresh sigset_t already names SIGUSR1");
        return false;
    }
    if unsafe { signal::sigaddset(&mut set, SIGUSR1) } != 0 {
        eprintln!("libc_abi_test: sigaddset rejected SIGUSR1");
        return false;
    }
    if unsafe { signal::sigismember(&set, SIGUSR1) } != 1
        || set.kernel_mask() != 1u64 << (SIGUSR1 - 1)
    {
        eprintln!(
            "libc_abi_test: SIGUSR1 ({SIGUSR1}) narrows to mask {:#x}, want only bit {}",
            set.kernel_mask(),
            SIGUSR1 - 1
        );
        return false;
    }
    if unsafe { signal::sigdelset(&mut set, SIGUSR1) } != 0
        || unsafe { signal::sigismember(&set, SIGUSR1) } != 0
        || set.kernel_mask() != 0
    {
        eprintln!("libc_abi_test: sigdelset left SIGUSR1 in the kernel mask");
        return false;
    }

    // Signal 0 is `kill`'s existence probe and never a set member; anything
    // past `NSIG` names a realtime signal this kernel has not got.
    let nsig = slopos_slibc::types::NSIG;
    if unsafe { signal::sigaddset(&mut set, 0) } != -1
        || unsafe { signal::sigaddset(&mut set, nsig + 1) } != -1
        || unsafe { signal::sigismember(&set, nsig + 1) } != -1
    {
        eprintln!("libc_abi_test: a signal outside 1..=NSIG was accepted into a sigset");
        return false;
    }
    if unsafe { signal::sigaddset(&mut set, nsig) } != 0 {
        eprintln!("libc_abi_test: sigaddset rejected NSIG ({nsig}), the last signal there is");
        return false;
    }

    // `sigfillset` fills only what can be raised: a bit above `NSIG` is a
    // promise to block something that cannot arrive.
    if unsafe { signal::sigfillset(&mut set) } != 0
        || unsafe { signal::sigismember(&set, SIGUSR2) } != 1
        || set.has_unsupported_bits()
    {
        eprintln!(
            "libc_abi_test: sigfillset reached past the kernel's mask (word 0 = {:#x})",
            set.__val[0]
        );
        return false;
    }
    true
}

// ---------------------------------------------------------------------------
// `siginfo_t.si_addr` at offset 16.
// ---------------------------------------------------------------------------

static SEGV_ADDR: AtomicU64 = AtomicU64::new(0);
static SEGV_CODE: AtomicU32 = AtomicU32::new(0);
static SEGV_COUNT: AtomicU32 = AtomicU32::new(0);
static SEGV_FIX_BASE: AtomicU64 = AtomicU64::new(0);
static SEGV_FIX_LEN: AtomicUsize = AtomicUsize::new(0);

/// Records the faulting address, then makes the page writable and returns.
/// A fault signal re-executes the faulting instruction on `iretq`, so
/// repairing the mapping is the recovery: the store completes and the test
/// carries on. A handler that could not repair anything would loop.
extern "C" fn on_segv(_sig: i32, info: *mut UserSiginfo, _uc: *mut c_void) {
    SEGV_COUNT.fetch_add(1, Ordering::SeqCst);
    if !info.is_null() {
        SEGV_ADDR.store(unsafe { (*info).si_addr() }, Ordering::SeqCst);
        SEGV_CODE.store(unsafe { (*info).si_code } as u32, Ordering::SeqCst);
    }
    let base = SEGV_FIX_BASE.load(Ordering::SeqCst);
    let len = SEGV_FIX_LEN.load(Ordering::SeqCst);
    if base != 0 && len != 0 {
        unsafe { mprotect(base as *mut c_void, len, (PROT_READ | PROT_WRITE) as i32) };
    }
}

/// `si_addr` moved from offset 24 to Linux's 16 because std's unix PAL reads
/// it; a regression makes std's own stack-overflow report read zero and say
/// nothing. The kernel side is checked against the raw frame in
/// `core/src/syscall/tests_build_floor_signal.rs`; this is the other end — a real
/// userland `SA_SIGINFO` handler reading the field through the `siginfo_t`
/// slibc and `libc` both declare.
fn sigsegv_handler_reads_the_faulting_address() -> bool {
    let page = unsafe { sysconf(_SC_PAGESIZE) } as usize;
    let region = unsafe {
        mmap(
            ptr::null_mut(),
            page,
            PROT_READ as i32,
            (MAP_PRIVATE | MAP_ANONYMOUS) as i32,
            -1,
            0,
        )
    };
    if region as isize <= 0 {
        eprintln!("libc_abi_test: mmap of a read-only page failed");
        return false;
    }

    let mut alt = vec![0u8; MINSIGSTKSZ];
    let ss = stack_t {
        ss_sp: alt.as_mut_ptr() as u64,
        ss_flags: 0,
        _pad: 0,
        ss_size: alt.len() as u64,
    };
    if unsafe { signal::sigaltstack(&ss, ptr::null_mut()) } != 0 {
        eprintln!("libc_abi_test: sigaltstack failed");
        unsafe { munmap(region, page) };
        return false;
    }

    SEGV_COUNT.store(0, Ordering::SeqCst);
    SEGV_ADDR.store(0, Ordering::SeqCst);
    SEGV_CODE.store(0, Ordering::SeqCst);
    SEGV_FIX_BASE.store(region as u64, Ordering::SeqCst);
    SEGV_FIX_LEN.store(page, Ordering::SeqCst);

    let act = SigAction {
        sa_sigaction: on_segv as *const () as usize,
        sa_mask: SigSet::empty(),
        sa_flags: (SA_SIGINFO | SA_ONSTACK) as i32,
        sa_restorer: None,
    };
    if unsafe { signal::sigaction(SIGSEGV, &act, ptr::null_mut()) } != 0 {
        eprintln!("libc_abi_test: installing the SIGSEGV handler failed");
        unsafe { munmap(region, page) };
        return false;
    }

    // Deliberately not offset 0, so a handler that reported the mapping's base
    // rather than the faulting address would be caught too.
    const OFFSET: usize = 0x2a8;
    let target = unsafe { (region as *mut u8).add(OFFSET) };
    // Read first, so the leaf is present and the write that follows is a
    // protection violation (`SEGV_ACCERR`) rather than a demand fault on a page
    // that was only promised.
    core::hint::black_box(unsafe { ptr::read_volatile(target) });
    unsafe { ptr::write_volatile(target, 0x5au8) };
    let landed = unsafe { ptr::read_volatile(target) };

    let count = SEGV_COUNT.load(Ordering::SeqCst);
    let addr = SEGV_ADDR.load(Ordering::SeqCst);
    let code = SEGV_CODE.load(Ordering::SeqCst) as i32;

    let _ = unsafe { signal::signal(SIGSEGV, SIG_DFL) };
    let disable = stack_t {
        ss_sp: 0,
        ss_flags: slopos_abi::signal::SS_DISABLE,
        _pad: 0,
        ss_size: 0,
    };
    let _ = unsafe { signal::sigaltstack(&disable, ptr::null_mut()) };
    unsafe { munmap(region, page) };

    if count != 1 {
        eprintln!("libc_abi_test: the SIGSEGV handler ran {count} times, expected 1");
        return false;
    }
    if addr != target as u64 {
        eprintln!(
            "libc_abi_test: si_addr read {addr:#x}, faulted at {:#x}",
            target as u64
        );
        return false;
    }
    if code != SEGV_ACCERR {
        eprintln!("libc_abi_test: si_code was {code}, expected SEGV_ACCERR ({SEGV_ACCERR})");
        return false;
    }
    if landed != 0x5a {
        eprintln!("libc_abi_test: the repaired store left {landed:#x}");
        return false;
    }
    true
}

// ---------------------------------------------------------------------------
// `msghdr`/`cmsghdr`: a descriptor over an AF_UNIX socket.
// ---------------------------------------------------------------------------

/// Two already-connected AF_UNIX stream endpoints.
///
/// slibc refuses `socketpair(2)` because SlopOS has no syscall that makes two
/// endpoints at once, and it declines to fake it. A test may: `unix_connect`
/// allocates the pair the moment the listener has backlog room, so
/// connect/accept complete inline in one process with nothing to race.
fn connected_unix_pair(path: &str) -> Option<(i32, i32, i32)> {
    let mut addr = SockAddrUn::default();
    addr.family = AF_UNIX as u16;
    let bytes = path.as_bytes();
    addr.path[..bytes.len()].copy_from_slice(bytes);
    let addrlen = (2 + bytes.len()) as u32;
    let sa = &addr as *const SockAddrUn as *const slopos_slibc::net::SockAddr;

    let listener = unsafe { socket(AF_UNIX, SOCK_STREAM, 0) };
    if listener < 0 {
        eprintln!("libc_abi_test: socket(AF_UNIX) failed");
        return None;
    }
    if unsafe { bind(listener, sa, addrlen) } != 0 {
        eprintln!("libc_abi_test: bind({path}) failed");
        close(listener);
        return None;
    }
    if unsafe { listen(listener, 1) } != 0 {
        eprintln!("libc_abi_test: listen({path}) failed");
        close(listener);
        return None;
    }
    let client = unsafe { socket(AF_UNIX, SOCK_STREAM, 0) };
    if client < 0 {
        eprintln!("libc_abi_test: socket(AF_UNIX) for the client failed");
        close(listener);
        return None;
    }
    if unsafe { connect(client, sa, addrlen) } != 0 {
        eprintln!("libc_abi_test: connect({path}) failed");
        close(client);
        close(listener);
        return None;
    }
    let server = unsafe { accept(listener, ptr::null_mut(), ptr::null_mut()) };
    if server < 0 {
        eprintln!("libc_abi_test: accept({path}) failed");
        close(client);
        close(listener);
        return None;
    }
    Some((listener, client, server))
}

/// `msghdr` is Linux's 56-byte form with a real `*mut iovec` the kernel walks,
/// and `cmsghdr` is 16 bytes with the payload at +16. Two *non-adjacent*
/// iovec segments prove the array is walked rather than the first descriptor
/// being read inline; the `SCM_RIGHTS` item resolving to the same file proves
/// `CMSG_DATA`'s offset. The kernel half of this is covered by
/// `core/src/syscall/tests.rs`; this is slibc's half.
fn sendmsg_passes_a_descriptor_and_scattered_data() -> bool {
    if !work_dir() {
        eprintln!("libc_abi_test: {WORK} is not usable");
        return false;
    }
    let payload = b"libc_abi_test scm_rights payload";
    let file = format!("{WORK}/scm_source");
    if fs::write(&file, payload).is_err() {
        eprintln!("libc_abi_test: could not write {file}");
        return false;
    }
    let sock_path = format!("{WORK}/scm.sock");
    let _ = fs::remove_file(&sock_path);

    let Some((listener, client, server)) = connected_unix_pair(&sock_path) else {
        return false;
    };

    let cfile = format!("{file}\0");
    let passed = unsafe { open(cfile.as_ptr() as *const i8, O_RDONLY) };
    if passed < 0 {
        eprintln!("libc_abi_test: could not open {file} for passing");
        close(server);
        close(client);
        close(listener);
        return false;
    }

    let seg_a = *b"ABCD";
    let seg_b = *b"wxyz";
    let iov = [
        UserIovec {
            iov_base: seg_a.as_ptr() as u64,
            iov_len: seg_a.len() as u64,
        },
        UserIovec {
            iov_base: seg_b.as_ptr() as u64,
            iov_len: seg_b.len() as u64,
        },
    ];

    let mut ctl = [0u8; 32];
    assert!(ctl.len() >= cmsg_space(mem::size_of::<i32>()));
    let hdr = CmsgHdr {
        cmsg_len: cmsg_len(mem::size_of::<i32>()) as u64,
        cmsg_level: SOL_SOCKET,
        cmsg_type: SCM_RIGHTS,
    };
    unsafe {
        ptr::write_unaligned(ctl.as_mut_ptr() as *mut CmsgHdr, hdr);
        ptr::write_unaligned(ctl.as_mut_ptr().add(CMSG_DATA_OFFSET) as *mut i32, passed);
    }

    let msg = MsgHdr {
        msg_name: 0,
        msg_namelen: 0,
        _pad0: 0,
        msg_iov: iov.as_ptr() as u64,
        msg_iovlen: iov.len() as u64,
        msg_control: ctl.as_ptr() as u64,
        msg_controllen: cmsg_space(mem::size_of::<i32>()) as u64,
        msg_flags: 0,
        _pad1: 0,
    };

    let sent = unsafe { sendmsg(client, &msg, 0) };
    close(passed);
    if sent != (seg_a.len() + seg_b.len()) as isize {
        eprintln!("libc_abi_test: sendmsg sent {sent} bytes of 8");
        close(server);
        close(client);
        close(listener);
        return false;
    }

    let mut got_a = [0u8; 4];
    let mut got_b = [0u8; 4];
    let riov = [
        UserIovec {
            iov_base: got_a.as_mut_ptr() as u64,
            iov_len: got_a.len() as u64,
        },
        UserIovec {
            iov_base: got_b.as_mut_ptr() as u64,
            iov_len: got_b.len() as u64,
        },
    ];
    let mut rctl = [0u8; 32];
    let mut rmsg = MsgHdr {
        msg_name: 0,
        msg_namelen: 0,
        _pad0: 0,
        msg_iov: riov.as_ptr() as u64,
        msg_iovlen: riov.len() as u64,
        msg_control: rctl.as_mut_ptr() as u64,
        msg_controllen: rctl.len() as u64,
        msg_flags: 0,
        _pad1: 0,
    };
    let received = unsafe { recvmsg(server, &mut rmsg, 0) };
    close(server);
    close(client);
    close(listener);
    let _ = fs::remove_file(&sock_path);

    if received != 8 {
        eprintln!("libc_abi_test: recvmsg read {received} bytes of 8");
        return false;
    }
    if got_a != seg_a || got_b != seg_b {
        eprintln!(
            "libc_abi_test: the scattered segments came back as {:?}/{:?}",
            got_a, got_b
        );
        return false;
    }
    // Both lengths exactly: a surplus descriptor is a *larger* item, so a `<`
    // bound passes while the extra fd lands in this process's table and the
    // case still reports green.
    if rmsg.msg_controllen != cmsg_space(mem::size_of::<i32>()) as u64 {
        eprintln!(
            "libc_abi_test: recvmsg reported {} control bytes, not CMSG_SPACE(sizeof(int)) = {}",
            rmsg.msg_controllen,
            cmsg_space(mem::size_of::<i32>())
        );
        return false;
    }
    let rhdr: CmsgHdr = unsafe { ptr::read_unaligned(rctl.as_ptr() as *const CmsgHdr) };
    if rhdr.cmsg_len != cmsg_len(mem::size_of::<i32>()) as u64 {
        eprintln!(
            "libc_abi_test: cmsg_len is {}, not CMSG_LEN(sizeof(int)) = {}",
            rhdr.cmsg_len,
            cmsg_len(mem::size_of::<i32>())
        );
        return false;
    }
    if rhdr.cmsg_level != SOL_SOCKET || rhdr.cmsg_type != SCM_RIGHTS {
        eprintln!(
            "libc_abi_test: the ancillary item is level {} type {}",
            rhdr.cmsg_level, rhdr.cmsg_type
        );
        return false;
    }
    let delivered: i32 =
        unsafe { ptr::read_unaligned(rctl.as_ptr().add(CMSG_DATA_OFFSET) as *const i32) };
    if delivered < 0 {
        eprintln!("libc_abi_test: CMSG_DATA at +{CMSG_DATA_OFFSET} named fd {delivered}");
        return false;
    }

    let mut back = [0u8; 64];
    let n =
        unsafe { slopos_slibc::ffi::read(delivered, back.as_mut_ptr() as *mut c_void, back.len()) };
    close(delivered);
    if n != payload.len() as isize || &back[..payload.len()] != payload {
        eprintln!("libc_abi_test: the received descriptor read {n} bytes, not the source file");
        return false;
    }
    true
}

// ---------------------------------------------------------------------------
// `sysconf`.
// ---------------------------------------------------------------------------

/// std rounds stack sizes and guard pages with `sysconf(_SC_PAGESIZE)`, so the
/// number has to be the granularity the kernel actually enforces. `mprotect`
/// is the probe: it accepts a page-aligned address and refuses anything else,
/// so a reported size that is twice or half the real one flips exactly one of
/// the two answers.
fn sysconf_pagesize_is_the_kernels_mapping_unit() -> bool {
    let page = unsafe { sysconf(_SC_PAGESIZE) };
    if page <= 0 || !(page as usize).is_power_of_two() {
        eprintln!("libc_abi_test: sysconf(_SC_PAGESIZE) answered {page}");
        return false;
    }
    let page = page as usize;

    let region = unsafe {
        mmap(
            ptr::null_mut(),
            2 * page,
            (PROT_READ | PROT_WRITE) as i32,
            (MAP_PRIVATE | MAP_ANONYMOUS) as i32,
            -1,
            0,
        )
    };
    if region as isize <= 0 {
        eprintln!("libc_abi_test: mmap of two pages failed");
        return false;
    }
    let base = region as usize;

    let aligned = unsafe { mprotect((base + page) as *mut c_void, page, PROT_READ as i32) };
    let misaligned =
        unsafe { mprotect((base + page / 2) as *mut c_void, page / 2, PROT_READ as i32) };
    unsafe { munmap(region, 2 * page) };

    if aligned != 0 {
        eprintln!("libc_abi_test: mprotect one _SC_PAGESIZE into a two-page mapping failed");
        return false;
    }
    if misaligned == 0 {
        eprintln!("libc_abi_test: mprotect half an _SC_PAGESIZE in was accepted");
        return false;
    }
    true
}

/// `std::thread::available_parallelism` *is* `sysconf(_SC_NPROCESSORS_ONLN)`
/// on this target, so comparing the two would prove nothing. The independent
/// witness is the kernel's own `cpu_info`: `sysconf` counts the bits
/// `sched_getaffinity` wrote, `cpu_info` reports what the kernel brought up,
/// and a disagreement means one of the two paths is lying about the machine.
fn sysconf_nprocs_matches_the_kernels_cpu_count() -> bool {
    let mut info = UserCpuInfo::default();
    if sys_core::cpu_info(&mut info) < 0 {
        eprintln!("libc_abi_test: cpu_info failed");
        return false;
    }
    let onln = unsafe { sysconf(_SC_NPROCESSORS_ONLN) };
    let conf = unsafe { sysconf(_SC_NPROCESSORS_CONF) };
    if onln != info.cpu_count as i64 || conf != info.cpu_count as i64 {
        eprintln!(
            "libc_abi_test: sysconf says {onln} online / {conf} configured, the kernel says {}",
            info.cpu_count
        );
        return false;
    }
    match thread::available_parallelism() {
        Ok(n) if n.get() as i64 == onln => true,
        Ok(n) => {
            eprintln!("libc_abi_test: available_parallelism answered {n}, sysconf {onln}");
            false
        }
        Err(e) => {
            eprintln!("libc_abi_test: available_parallelism failed: {e:?}");
            false
        }
    }
}

// ---------------------------------------------------------------------------
// `environ`.
// ---------------------------------------------------------------------------

/// Walks the exported `environ` array. A null array is the empty environment,
/// which is what a test binary the kernel spawns with no `envp` starts with.
fn environ_entries() -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        let mut p = slopos_slibc::env::environ;
        if p.is_null() {
            return out;
        }
        while !(*p).is_null() {
            let entry = *p;
            let len = slopos_slibc::u_strlen(entry);
            out.push(String::from_utf8_lossy(core::slice::from_raw_parts(entry, len)).into_owned());
            p = p.add(1);
        }
    }
    out
}

/// std's unix PAL walks the exported `environ` array directly for `vars()` and
/// calls `getenv` for `var()`, so the two have to answer the same thing. A
/// `setenv` that grows the array and leaves `environ` pointing at the old one
/// is invisible until something reads the array — which is exactly what
/// `std::env::vars()` does, and what this walks.
///
/// A utest binary starts with no environment at all, so every `setenv` here
/// takes the growing path rather than overwriting a slot: the case the stale
/// pointer would break.
fn environ_and_getenv_are_one_environment() -> bool {
    const NAMES: [(&str, &str); 3] = [
        ("LIBC_ABI_TEST_A", "alpha"),
        ("LIBC_ABI_TEST_B", ""),
        ("LIBC_ABI_TEST_C", "gamma=with=equals"),
    ];

    let before = environ_entries().len();
    for (name, value) in NAMES {
        unsafe { std::env::set_var(name, value) };
    }

    let entries = environ_entries();
    if entries.len() != before + NAMES.len() {
        eprintln!(
            "libc_abi_test: environ holds {} entries after {} setenv calls on {before}",
            entries.len(),
            NAMES.len()
        );
        return false;
    }

    // Every name the array lists is a name `getenv` answers with the same
    // value: the two are one environment or they are two.
    for entry in &entries {
        let Some((name, value)) = entry.split_once('=') else {
            eprintln!("libc_abi_test: environ entry {entry:?} has no '='");
            return false;
        };
        match std::env::var(name) {
            Ok(v) if v == value => {}
            Ok(v) => {
                eprintln!("libc_abi_test: environ says {name}={value:?}, getenv says {v:?}");
                return false;
            }
            Err(e) => {
                eprintln!("libc_abi_test: getenv({name}) failed although environ lists it: {e:?}");
                return false;
            }
        }
    }

    for (name, value) in NAMES {
        if !entries.iter().any(|e| e == &format!("{name}={value}")) {
            eprintln!("libc_abi_test: a setenv of {name} is not in the environ array");
            return false;
        }
    }

    // `vars()` reads the array, `var()` calls `getenv`; a count that disagrees
    // means one of them is reading a copy nothing else updates.
    let std_count = std::env::vars().count();
    if std_count != entries.len() {
        eprintln!(
            "libc_abi_test: std::env::vars() saw {std_count} entries, environ has {}",
            entries.len()
        );
        return false;
    }

    for (name, _) in NAMES {
        unsafe { std::env::remove_var(name) };
    }
    let after = environ_entries();
    if after.len() != before {
        eprintln!(
            "libc_abi_test: environ holds {} entries after the unsets, started at {before}",
            after.len()
        );
        return false;
    }
    for (name, _) in NAMES {
        if std::env::var(name).is_ok() {
            eprintln!("libc_abi_test: getenv still answers {name} after unsetenv");
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// `realpath`.
// ---------------------------------------------------------------------------

fn realpath_of(path: &str) -> Option<String> {
    let c = format!("{path}\0");
    let mut buf = [0u8; 4096];
    let got = unsafe { realpath(c.as_ptr() as *const i8, buf.as_mut_ptr() as *mut i8) };
    if got.is_null() {
        return None;
    }
    let len = slopos_slibc::u_strnlen(buf.as_ptr(), buf.len());
    Some(String::from_utf8_lossy(&buf[..len]).into_owned())
}

/// `realpath` resolved a relative symlink target against the wrong directory:
/// it stripped a component it had not appended, so `/bin/ls -> coreutils`
/// canonicalised to `/coreutils` and then answered `ENOENT`.
///
/// The working directory is deliberately `/` and the argument absolute, which
/// is what separates this from `cd_test`'s case: there the cwd *was* the
/// link's directory, so resolving the target against either one gives the same
/// answer, and a resolver that used the cwd would pass. Here it cannot.
fn realpath_resolves_a_relative_link_against_the_links_directory() -> bool {
    if !work_dir() {
        eprintln!("libc_abi_test: {WORK} is not usable");
        return false;
    }
    let dir = format!("{WORK}/rp");
    let _ = fs::remove_dir_all(&dir);
    if fs::create_dir_all(&dir).is_err() {
        eprintln!("libc_abi_test: could not create {dir}");
        return false;
    }
    if fs::write(format!("{dir}/target"), b"t").is_err() {
        eprintln!("libc_abi_test: could not create the link target");
        return false;
    }
    let link = format!("{dir}/link\0");
    let rc = unsafe {
        slopos_slibc::ffi::syscalls::symlink(c"target".as_ptr(), link.as_ptr() as *const i8)
    };
    if rc != 0 {
        eprintln!("libc_abi_test: symlink(target, {dir}/link) failed");
        return false;
    }

    if std::env::set_current_dir("/").is_err() {
        eprintln!("libc_abi_test: cd / failed");
        return false;
    }

    let mut ok = true;
    let want = format!("{dir}/target");
    match realpath_of(&format!("{dir}/link")) {
        Some(got) if got == want => {}
        Some(got) => {
            eprintln!("libc_abi_test: realpath({dir}/link) answered {got:?}, want {want:?}");
            ok = false;
        }
        None => {
            eprintln!("libc_abi_test: realpath({dir}/link) failed");
            ok = false;
        }
    }

    // The shipped instance of the same shape: `/bin/ls` is a relative symlink
    // to the multicall binary, and it is what answered `ENOENT` before.
    match realpath_of("/bin/ls") {
        Some(got) if got == "/bin/coreutils" => {}
        Some(got) => {
            eprintln!("libc_abi_test: realpath(/bin/ls) answered {got:?}");
            ok = false;
        }
        None => {
            eprintln!("libc_abi_test: realpath(/bin/ls) failed");
            ok = false;
        }
    }

    let _ = fs::remove_dir_all(&dir);
    ok
}

/// The half of the C library only C can reach: `setjmp`, the `long double`
/// family, and the generated headers compiled as C. The ordered checks live
/// in `/bin/libc_probe`; its exit status is the number of the one that failed.
fn a_c_program_uses_the_whole_libc_surface() -> bool {
    // Captured rather than inherited: a utest's stdio is init's console, not
    // the serial line this run is read from.
    match Command::new("/bin/libc_probe").output() {
        Ok(out) => {
            if out.status.code() == Some(0) {
                return true;
            }
            let said = String::from_utf8_lossy(&out.stderr);
            match out.status.code() {
                Some(code) => note(&format!("check {code}: {}", said.trim())),
                None => note(&format!(
                    "died by {:?}: {}",
                    out.status.signal(),
                    said.trim()
                )),
            }
            false
        }
        Err(e) => {
            note(&format!("spawning /bin/libc_probe failed: {e}"));
            false
        }
    }
}

const CASES: &[(&str, fn() -> bool)] = &[
    (
        "zeroed_pthread_locks_work_without_init",
        zeroed_pthread_locks_work_without_init,
    ),
    (
        "zeroed_pthread_cond_times_out_on_the_realtime_clock",
        zeroed_pthread_cond_times_out_on_the_realtime_clock,
    ),
    (
        "std_mutex_and_condvar_carry_real_threads",
        std_mutex_and_condvar_carry_real_threads,
    ),
    (
        "a_requested_thread_stack_size_reaches_the_thread",
        a_requested_thread_stack_size_reaches_the_thread,
    ),
    (
        "attr_init_reports_a_guard_page",
        attr_init_reports_a_guard_page,
    ),
    (
        "read_dir_names_match_getdents64",
        read_dir_names_match_getdents64,
    ),
    (
        "sigaction_roundtrips_the_152_byte_struct",
        sigaction_roundtrips_the_152_byte_struct,
    ),
    (
        "sigset_narrows_signal_n_to_bit_n_minus_one",
        sigset_narrows_signal_n_to_bit_n_minus_one,
    ),
    (
        "sigsegv_handler_reads_the_faulting_address",
        sigsegv_handler_reads_the_faulting_address,
    ),
    (
        "sendmsg_passes_a_descriptor_and_scattered_data",
        sendmsg_passes_a_descriptor_and_scattered_data,
    ),
    (
        "sysconf_pagesize_is_the_kernels_mapping_unit",
        sysconf_pagesize_is_the_kernels_mapping_unit,
    ),
    (
        "sysconf_nprocs_matches_the_kernels_cpu_count",
        sysconf_nprocs_matches_the_kernels_cpu_count,
    ),
    (
        "environ_and_getenv_are_one_environment",
        environ_and_getenv_are_one_environment,
    ),
    (
        "realpath_resolves_a_relative_link_against_the_links_directory",
        realpath_resolves_a_relative_link_against_the_links_directory,
    ),
    (
        "a_c_program_uses_the_whole_libc_surface",
        a_c_program_uses_the_whole_libc_surface,
    ),
];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
