//! USB: the xHCI host controllers and the thread that serves them, over
//! `slopos-usb-core`'s formats and sequences.

pub mod xhci;

use core::sync::atomic::{AtomicU8, Ordering};

use slopos_net::napi_waker::NapiWaker;
use slopos_ostd::sync::kernel_io_task::{KernelIoToken, KthreadWait};
use slopos_ostd::sync::{InitFlag, LOCK_LEVEL_RESOURCE};
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

/// How often the thread looks unasked for a controller that died without
/// an interrupt to say so, or for events a lost interrupt left in its ring.
const IDLE_MS: u32 = 1000;

static WAKER: NapiWaker =
    NapiWaker::new("usb", lock_class!("USB_STOP.waiters", LOCK_LEVEL_RESOURCE));
static THREAD: InitFlag = InitFlag::new();

pub(crate) fn wake() {
    WAKER.arm_and_wake();
}

/// The one thread serving every controller, started with the first.
fn start_thread() {
    if !THREAD.init_once() {
        return;
    }
    if let Err(err) = slopos_ostd::spawn_kernel_io!(WAKER.stop(), usb_thread) {
        klog_info!("USB: no thread to serve the controllers: {:?}", err);
    }
}

fn usb_thread(token: KernelIoToken<'static>) {
    loop {
        if WAKER.wait_timeout_ms(&token, IDLE_MS) == KthreadWait::Stop {
            break;
        }
        xhci::serve_all();
    }
    WAKER.stop().note_exited();
}
