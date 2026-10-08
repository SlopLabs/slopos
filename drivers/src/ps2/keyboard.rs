//! The i8042 keyboard: its set-1 scancodes decoded into usages for the
//! machine's keyboard state, and its lock LEDs.

use core::sync::atomic::{AtomicBool, Ordering};

use slopos_arch::cpu;
use slopos_keymap_core::{LOCK_CAPS, LOCK_NUM, LOCK_SCROLL, Set1Decoder};
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{klog_info, klog_warn, lock_class};

use crate::keyboard::{self, KeyboardSource};
use crate::ps2;

/// Keyboard device command: set the lock LEDs (followed by a 1-byte LED mask).
const DEV_CMD_SET_LEDS: u8 = 0xED;
const ACK_WAIT_ITERS: u32 = 50_000;
/// An LED exchange byte the keyboard has not answered by then is sent again.
const ANSWER_MS: u64 = 250;
/// Times a byte is sent again, on RESEND or unanswered, before the exchange
/// is given up.
const RETRIES: u8 = 2;

static DECODER: SpinLock<Set1Decoder> = SpinLock::new(
    Set1Decoder::new(),
    lock_class!("ps2kbd.DECODER", LOCK_LEVEL_RESOURCE),
);

#[derive(Clone, Copy)]
enum Stage {
    /// `0xED`; the LED byte follows its ACK.
    Command,
    Byte,
}

#[derive(Clone, Copy)]
struct Exchange {
    stage: Stage,
    locks: u8,
    since: u64,
    retries: u8,
}

impl Exchange {
    fn send(&self) {
        ps2::write_data(match self.stage {
            Stage::Command => DEV_CMD_SET_LEDS,
            Stage::Byte => led_byte(self.locks),
        });
    }
}

/// The LED exchange, which the keyboard's interrupt advances on each ACK, and
/// the locks it last tried and had acknowledged.
struct Leds {
    exchange: Option<Exchange>,
    tried: Option<u8>,
    acknowledged: Option<u8>,
}

static LEDS: SpinLock<Leds> = SpinLock::new(
    Leds {
        exchange: None,
        tried: None,
        acknowledged: None,
    },
    lock_class!("ps2kbd.LEDS", LOCK_LEVEL_RESOURCE),
);

/// Set once the keyboard acknowledged its reset; nothing is sent before.
static PRESENT: AtomicBool = AtomicBool::new(false);

pub fn init() {
    klog_info!("PS/2 keyboard: initialising device");

    ps2::write_data(ps2::DEV_CMD_RESET);
    let mut acknowledged = false;
    if ps2::wait_data() {
        let response = ps2::read_data_nowait();
        if response == ps2::DEV_ACK {
            acknowledged = true;
            if ps2::wait_data() {
                let test_result = ps2::read_data_nowait();
                if test_result != ps2::DEV_SELF_TEST_PASS {
                    klog_warn!("PS/2 keyboard: self-test returned 0x{:02x}", test_result);
                }
            }
        } else {
            klog_warn!("PS/2 keyboard: reset NAK 0x{:02x}", response);
        }
    } else {
        klog_warn!("PS/2 keyboard: reset timed out");
    }

    ps2::flush();

    *DECODER.lock() = Set1Decoder::new();
    if acknowledged {
        set_leds_polled(keyboard::locks());
    }
    PRESENT.store(acknowledged, Ordering::Release);

    klog_info!("PS/2 keyboard: initialised");
}

/// IRQ entry point: process one raw scancode byte from the controller.
pub fn handle_scancode(byte: u8) {
    let ts = slopos_kernel_services::clock::uptime_ms();
    if answered(byte, ts) {
        return;
    }
    let Some(step) = DECODER.lock().feed(byte) else {
        return;
    };
    keyboard::key(KeyboardSource::I8042, step.usage, step.pressed, ts);
}

fn led_byte(locks: u8) -> u8 {
    let mut led = 0u8;
    if locks & LOCK_SCROLL != 0 {
        led |= 0b001;
    }
    if locks & LOCK_NUM != 0 {
        led |= 0b010;
    }
    if locks & LOCK_CAPS != 0 {
        led |= 0b100;
    }
    led
}

/// Whether `byte` was the keyboard's answer to an LED exchange, which it
/// advances; the locks that changed meanwhile go out next. Any other byte
/// shows the keyboard scanning, so an exchange it left unanswered is retried.
fn answered(byte: u8, now: u64) -> bool {
    let mut leds = LEDS.lock();
    let Some(exchange) = leds.exchange else {
        return false;
    };
    let answer = match (exchange.stage, byte) {
        (Stage::Command, ps2::DEV_ACK) => {
            let next = Exchange {
                stage: Stage::Byte,
                since: now,
                retries: 0,
                ..exchange
            };
            next.send();
            leds.exchange = Some(next);
            true
        }
        (Stage::Byte, ps2::DEV_ACK) => {
            leds.acknowledged = Some(exchange.locks);
            leds.exchange = None;
            true
        }
        (_, ps2::DEV_RESEND) => {
            retry(&mut leds, now);
            true
        }
        _ => {
            expire(&mut leds, now);
            false
        }
    };
    if leds.exchange.is_none() {
        start(&mut leds, keyboard::locks(), now);
    }
    answer
}

/// Sends the exchange's byte again, or gives the exchange up.
fn retry(leds: &mut Leds, now: u64) {
    leds.exchange = leds
        .exchange
        .filter(|e| e.retries < RETRIES)
        .map(|e| Exchange {
            since: now,
            retries: e.retries + 1,
            ..e
        });
    if let Some(exchange) = &leds.exchange {
        exchange.send();
    }
}

fn expire(leds: &mut Leds, now: u64) {
    if leds
        .exchange
        .is_some_and(|e| now.saturating_sub(e.since) >= ANSWER_MS)
    {
        retry(leds, now);
    }
}

/// Sends `0xED` unless an exchange is under way or `locks` was already
/// tried.
fn start(leds: &mut Leds, locks: u8, now: u64) {
    expire(leds, now);
    if leds.exchange.is_some() || leds.tried == Some(locks) {
        return;
    }
    leds.tried = Some(locks);
    let exchange = Exchange {
        stage: Stage::Command,
        locks,
        since: now,
        retries: 0,
    };
    exchange.send();
    leds.exchange = Some(exchange);
}

/// Brings the LEDs towards the locks without waiting: the keyboard's
/// interrupt carries the exchange on from each ACK. From any context.
pub(crate) fn sync_leds() {
    if !PRESENT.load(Ordering::Acquire) {
        return;
    }
    let now = slopos_kernel_services::clock::uptime_ms();
    start(&mut LEDS.lock(), keyboard::locks(), now);
}

/// With the keyboard's interrupt still masked, so the ACKs are polled.
fn set_leds_polled(locks: u8) {
    let mut leds = LEDS.lock();
    leds.exchange = None;
    leds.tried = Some(locks);
    if exchange(led_byte(locks)) {
        leds.acknowledged = Some(locks);
    }
}

fn exchange(led: u8) -> bool {
    ps2::write_data(DEV_CMD_SET_LEDS);
    if !wait_ack() {
        return false;
    }
    ps2::write_data(led);
    wait_ack()
}

/// Bounded poll for a device ACK (0xFA). Stray bytes are discarded.
fn wait_ack() -> bool {
    for _ in 0..ACK_WAIT_ITERS {
        if ps2::has_data() {
            if ps2::read_data_nowait() == ps2::DEV_ACK {
                return true;
            }
        } else {
            cpu::pause();
        }
    }
    false
}

/// The locks the keyboard last acknowledged in its LEDs.
#[cfg(feature = "test-hooks")]
pub fn leds_acknowledged() -> Option<u8> {
    LEDS.lock().acknowledged
}

/// Resets the decoder and the keyboard state without device I/O, so a stuck
/// `E0` latch, a held modifier, a pending dead key or a loaded layout cannot
/// leak from one test into the next or into the live desktop.
#[cfg(feature = "test-hooks")]
pub fn reset_state_for_test() {
    *DECODER.lock() = Set1Decoder::new();
    keyboard::reset_for_test();
}

pub fn poll_wait_enter() {
    const ENTER_MAKE_CODE: u8 = 0x1C;

    loop {
        if ps2::has_data() {
            let scancode = ps2::read_data_nowait();
            if scancode == ENTER_MAKE_CODE {
                break;
            }
        }
        cpu::pause();
    }
}
