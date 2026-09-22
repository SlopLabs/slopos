//! Fixed-capacity append-only byte log.
//!
//! A `.bss`-resident buffer that many CPUs append to and one reads back,
//! backing the per-CPU klog capture rings. Buffer, length and overflow
//! counters move together under one lock.
//!
//! Reads take a closure: a returned `&'static [u8]` would outlive the lock
//! and re-open the race the lock exists to close.
//!
//! The lock masks interrupts while held and names its holder. A holder that
//! could be switched out would deadlock its own CPU's next writer, and that
//! writer, spinning under an interrupt-masking lock of its own, would stop
//! acking TLB shootdowns for the whole machine. An NMI can still land inside
//! a holder; its append finds this CPU named as the holder and drops its
//! bytes instead of waiting on the frame beneath it.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

use super::spin_relax;
use crate::cpu::x86_64 as cpu;
use crate::cpu::x86_64::pcr::current_cpu_id;

pub struct AppendLog<const N: usize> {
    /// Bytes plus the count of live ones. Both under `owner`.
    inner: UnsafeCell<Inner<N>>,
    /// The holding CPU's index plus one; zero while free.
    owner: AtomicUsize,
    /// Bytes that did not fit. Read without the lock; monotonic per window.
    dropped: AtomicUsize,
}

struct Inner<const N: usize> {
    buf: [u8; N],
    len: usize,
}

// SAFETY: every access to `inner` goes through `with_locked`, which holds
// `owner` for the whole borrow, so the `&mut` it hands out is exclusive
// across CPUs.
unsafe impl<const N: usize> Sync for AppendLog<N> {}

impl<const N: usize> Default for AppendLog<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> AppendLog<N> {
    pub const fn new() -> Self {
        Self {
            inner: UnsafeCell::new(Inner {
                buf: [0u8; N],
                len: 0,
            }),
            owner: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
        }
    }

    /// Run `f` holding the log, or `None` when this CPU already holds it.
    #[inline]
    fn with_locked<R>(&self, f: impl FnOnce(&mut Inner<N>) -> R) -> Option<R> {
        let me = current_cpu_id() + 1;
        let flags = cpu::save_flags_cli();
        loop {
            match self
                .owner
                .compare_exchange_weak(0, me, Ordering::Acquire, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(holder) if holder == me => {
                    cpu::restore_flags(flags);
                    return None;
                }
                Err(_) => {
                    spin_relax();
                    core::hint::spin_loop();
                }
            }
        }
        // SAFETY: the exchange above made this CPU the holder with interrupts
        // off, a nested entry from this CPU returned above, and every other
        // CPU spins until the store below, so this borrow is exclusive for
        // its whole extent.
        let result = f(unsafe { &mut *self.inner.get() });
        self.owner.store(0, Ordering::Release);
        cpu::restore_flags(flags);
        Some(result)
    }

    /// Discard the log's contents and its overflow count.
    pub fn reset(&self) {
        let _ = self.with_locked(|inner| inner.len = 0);
        self.dropped.store(0, Ordering::Relaxed);
    }

    /// Append what fits; count the rest as dropped.
    pub fn append(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let dropped = self
            .with_locked(|inner| {
                let take = bytes.len().min(N - inner.len);
                inner.buf[inner.len..inner.len + take].copy_from_slice(&bytes[..take]);
                inner.len += take;
                bytes.len() - take
            })
            .unwrap_or(bytes.len());
        if dropped > 0 {
            self.dropped.fetch_add(dropped, Ordering::Relaxed);
        }
    }

    /// Read the live bytes under the lock.
    ///
    /// `f` runs with appends from every CPU blocked; an append it makes to
    /// this same log is dropped. A read nested inside this CPU's own append
    /// sees an empty log.
    pub fn with_bytes<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        let mut f = Some(f);
        let read = self.with_locked(|inner| {
            let f = f.take().expect("the closure is taken once");
            f(&inner.buf[..inner.len])
        });
        match read {
            Some(result) => result,
            None => {
                let f = f.take().expect("the closure is taken once");
                f(&[])
            }
        }
    }

    pub fn len(&self) -> usize {
        self.with_locked(|inner| inner.len).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes lost to overflow since the last [`AppendLog::reset`].
    pub fn dropped_bytes(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_append_nested_in_a_read_is_dropped_rather_than_waited_for() {
        let log = AppendLog::<16>::new();
        log.append(b"abc");
        let seen = log.with_bytes(|bytes| {
            log.append(b"nested");
            bytes.len()
        });
        assert_eq!(seen, 3);
        assert_eq!(log.dropped_bytes(), 6, "the nested bytes count as dropped");
        assert_eq!(log.len(), 3, "and the log is free again");
    }

    #[test]
    fn overflow_is_counted_not_written() {
        let log = AppendLog::<4>::new();
        log.append(b"abcdef");
        assert_eq!(log.len(), 4);
        assert_eq!(log.dropped_bytes(), 2);
        log.reset();
        assert_eq!((log.len(), log.dropped_bytes()), (0, 0));
    }
}
