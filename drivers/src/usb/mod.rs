//! USB over `slopos-usb-core`. The USB thread steps every controller's tree
//! and never waits on a completion; the bind thread runs the drivers' probes
//! and removals, which may block.

pub mod bus;
mod kconsole;
pub mod xhci;

use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use slopos_net::napi_waker::NapiWaker;
use slopos_ostd::sync::kernel_io_task::{KernelIoToken, KthreadWait};
use slopos_ostd::sync::{InitFlag, LOCK_LEVEL_RESOURCE, WaitQueue};
use slopos_ostd::{klog_info, lock_class};
use slopos_usb_core::knob::{Knob, Mode};

static MODE: AtomicU8 = AtomicU8::new(Mode::On as u8);

/// Read the `usb=` knob; the PCI boot step calls this before any probe.
pub fn configure(cmdline: &str) {
    let knob = Knob::parse(cmdline);
    if let Some(value) = knob.ignored {
        klog_info!("USB: usb={} names no mode; ignored", value);
    }
    MODE.store(knob.mode as u8, Ordering::Release);
}

pub(crate) fn mode() -> Mode {
    Mode::from_u8(MODE.load(Ordering::Acquire))
}

static ENUMERATING: AtomicBool = AtomicBool::new(false);

/// Every PCI driver has probed, so a USB disk or NIC registers after the
/// machine's own.
pub fn start() {
    ENUMERATING.store(true, Ordering::Release);
    if xhci::published().next().is_some() {
        wake();
    }
}

pub(crate) fn enumerating() -> bool {
    ENUMERATING.load(Ordering::Acquire)
}

/// A controller may die, or an interrupt be lost, with nothing to say so.
const IDLE_MS: u64 = 1000;

static WAKER: NapiWaker =
    NapiWaker::new("usb", lock_class!("USB_STOP.waiters", LOCK_LEVEL_RESOURCE));
static BINDER: NapiWaker = NapiWaker::new(
    "usb-bind",
    lock_class!("USB_BIND_STOP.waiters", LOCK_LEVEL_RESOURCE),
);
static BINDING: AtomicBool = AtomicBool::new(false);
static THREADS: InitFlag = InitFlag::new();

/// Woken by transfer completions and ring recoveries.
pub(crate) static TRANSFERS: WaitQueue =
    WaitQueue::new(lock_class!("USB.transfers", LOCK_LEVEL_RESOURCE));

pub(crate) fn wake() {
    WAKER.arm_and_wake();
}

pub(crate) fn wake_binder() {
    BINDER.arm_and_wake();
}

pub(crate) fn binder_busy() -> bool {
    BINDING.load(Ordering::Acquire)
}

fn start_threads() {
    if !THREADS.init_once() {
        return;
    }
    if let Err(err) = slopos_ostd::spawn_kernel_io!(WAKER.stop(), usb_thread) {
        klog_info!("USB: no thread to serve the controllers: {:?}", err);
    }
    if let Err(err) = slopos_ostd::spawn_kernel_io!(BINDER.stop(), bind_thread) {
        klog_info!("USB: no thread to bind drivers: {:?}", err);
    }
}

fn usb_thread(token: KernelIoToken<'static>) {
    let mut timeout = IDLE_MS;
    loop {
        if WAKER.wait_timeout_ms(&token, timeout as u32) == KthreadWait::Stop {
            break;
        }
        let next = xhci::serve_all();
        let now = slopos_kernel_services::clock::uptime_ms();
        timeout = next.map_or(IDLE_MS, |at| at.saturating_sub(now).clamp(1, IDLE_MS));
    }
    WAKER.stop().note_exited();
}

fn bind_thread(token: KernelIoToken<'static>) {
    loop {
        if BINDER.wait_timeout_ms(&token, IDLE_MS as u32) == KthreadWait::Stop {
            break;
        }
        BINDING.store(true, Ordering::Release);
        bus::run_jobs();
        BINDING.store(false, Ordering::Release);
        wake();
    }
    BINDER.stop().note_exited();
}

/// Enumeration has begun, every tree has settled and the bind thread has
/// nothing left.
pub fn settled() -> bool {
    enumerating() && xhci::published().all(|c| c.is_settled()) && bus::idle()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settle {
    Settled,
    Unsettled,
    /// The USB threads cannot run until the BSP enters the scheduler.
    OneCpu,
}

/// Polls: boot steps run on the BSP with no current task, while the USB
/// threads run on the APs.
pub fn wait_settled(bound_ms: u32) -> Settle {
    if xhci::published().next().is_none() {
        return Settle::Settled;
    }
    if slopos_arch::pcr::get_online_cpu_count() < 2 {
        return Settle::OneCpu;
    }
    wake();
    if crate::hpet::poll_wait(&settled, bound_ms) {
        Settle::Settled
    } else {
        Settle::Unsettled
    }
}

pub(crate) fn start_serving() {
    start_threads();
    wake();
}
