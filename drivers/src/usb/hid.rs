//! `usb-hid`: keyboards and pointers. Every interface is read in the report
//! protocol through `hid-core`, a boot device in the boot protocol only when
//! its report descriptor is unusable. Reports are decoded wherever the drain
//! runs and fed to the machine's keyboard state and cursor, as the i8042's are
//! from its interrupt; the USB thread keeps each keyboard's repeat and LEDs.

use slopos_hid_core::keyboard::{self as keys, HeldByReport, Keys, Leds};
use slopos_hid_core::{Kind, boot, pointer};
use slopos_keymap_core::{KeyRepeat, LOCK_CAPS, LOCK_NUM, LOCK_SCROLL};
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{KArc, KBox, KVec, klog_info, lock_class};
use slopos_usb_core::device::descriptor::{Configuration, Endpoint, Item, TransferType};
use slopos_usb_core::device::request::Setup;
use slopos_usb_core::hid as class;
use slopos_usb_core::xhci::memory::PAGE_SIZE;
use slopos_usb_core::xhci::transfer::TransferError;

use super::bus::{BoundUsbDevice, UsbFunction, UsbMatch};
use super::xhci::device::{Control, Poll, Posted, ReportSink, UsbError};
use crate::driver_core::bus::{ProbeError, ProbeOutcome, Removal};
use crate::hid::ReportMap;
use crate::input_event::{self, PointerSource};
use crate::keyboard::{self, KeyboardSource};

/// HID interfaces bound at once.
pub const MAX_DEVICES: usize = 16;
const MAX_FIELDS: usize = 128;
const MAX_USAGES: usize = 256;
/// The LED output reports SlopOS sends fit in this.
const MAX_OUTPUT: usize = 64;

enum Protocol {
    BootKeyboard,
    BootMouse,
    Report(ReportMap),
}

impl Protocol {
    fn report_bytes(&self) -> usize {
        match self {
            Protocol::BootKeyboard => boot::KEYBOARD_LEN,
            Protocol::BootMouse => boot::MOUSE_LEN,
            Protocol::Report(map) => map.descriptor().max_report_bytes(Kind::Input),
        }
    }
}

struct State {
    retired: bool,
    keys: KBox<HeldByReport>,
    repeat: KeyRepeat,
    /// LEDs as `LOCK_*` bits: in flight, last tried and last acknowledged.
    sending: Option<u8>,
    tried: Option<u8>,
    acknowledged: Option<u8>,
}

/// A keyboard's lock LEDs: the output report for each of the eight lock
/// states, built at bind.
struct Lights {
    posted: KArc<Posted>,
    id: u8,
    len: usize,
    reports: KVec<u8>,
}

impl Lights {
    fn report(&self, locks: u8) -> &[u8] {
        let at = usize::from(locks & (LOCK_NUM | LOCK_CAPS | LOCK_SCROLL)) * self.len;
        &self.reports[at..at + self.len]
    }
}

/// One bound HID interface.
pub struct Hid {
    #[cfg(feature = "test-hooks")]
    controller: u8,
    #[cfg(feature = "test-hooks")]
    path: slopos_usb_core::bus::Path,
    interface: u8,
    protocol: Protocol,
    keyboard: Option<KeyboardSource>,
    pointer: Option<PointerSource>,
    lights: Option<Lights>,
    state: SpinLock<State>,
}

fn now_ms() -> u64 {
    slopos_kernel_services::clock::uptime_ms()
}

fn leds_of(locks: u8) -> Leds {
    Leds {
        num: locks & LOCK_NUM != 0,
        caps: locks & LOCK_CAPS != 0,
        scroll: locks & LOCK_SCROLL != 0,
    }
}

impl ReportSink for Hid {
    fn report(&self, report: &[u8]) {
        let now = now_ms();
        let mut state = self.state.lock();
        if state.retired {
            return;
        }
        match &self.protocol {
            Protocol::BootKeyboard => {
                if let Some(keys) = keys::boot(report) {
                    self.press(&mut state, 0, keys, now);
                }
            }
            Protocol::BootMouse => {
                if let (Some(source), Some(motion)) = (self.pointer, pointer::boot(report)) {
                    input_event::pointer_report(source, &motion, now);
                }
            }
            Protocol::Report(map) => {
                let desc = map.descriptor();
                let Some((id, payload)) = desc.split(report) else {
                    return;
                };
                if self.keyboard.is_some()
                    && let Some(keys) = keys::decode(&desc, id, payload)
                {
                    self.press(&mut state, id, keys, now);
                }
                if let Some(source) = self.pointer
                    && let Some(motion) = pointer::decode(&desc, id, payload)
                {
                    input_event::pointer_report(source, &motion, now);
                }
            }
        }
    }
}

impl Hid {
    /// A rollover report says nothing of what is held, so it changes nothing.
    fn press(&self, state: &mut State, id: u8, keys: Keys, now: u64) {
        let (Some(source), Keys::Held(held)) = (self.keyboard, keys) else {
            return;
        };
        let Some((before, after)) = state.keys.replace(id, held) else {
            return;
        };
        let next = state.repeat.next_ms();
        for (key, pressed) in keys::steps(before, after) {
            keyboard::key(source, key, pressed, now);
            if pressed {
                state.repeat.on_key_down(key, now);
            } else {
                state.repeat.on_key_up(key);
            }
        }
        if state.repeat.next_ms() != next {
            super::wake();
        }
    }

    /// The USB thread's turn: a due repeat, and the LEDs brought up to the
    /// locks. When it next needs one.
    fn serve(&self, now: u64, locks: u8) -> Option<u64> {
        let mut state = self.state.lock();
        if state.retired {
            return None;
        }
        if let Some(source) = self.keyboard
            && let Some(key) = state.repeat.tick(now)
        {
            keyboard::key(source, key, true, now);
        }
        self.send_leds(&mut state, locks);
        state.repeat.next_ms()
    }

    /// One output report at a time; a lock state the keyboard refused is not
    /// sent again until the locks change, but one cancelled by another
    /// request's halt is.
    fn send_leds(&self, state: &mut State, locks: u8) {
        let Some(lights) = &self.lights else {
            return;
        };
        match lights.posted.poll() {
            Poll::Out => return,
            Poll::Done(Ok(_)) => state.acknowledged = state.sending.take(),
            Poll::Done(Err(TransferError::Cancelled | TransferError::Lost)) => {
                state.sending = None;
                state.tried = None;
            }
            Poll::Done(Err(_)) => state.sending = None,
            Poll::Idle => {}
        }
        if state.tried == Some(locks) {
            return;
        }
        state.tried = Some(locks);
        let setup = Setup::set_output_report(self.interface, lights.id, lights.len as u16);
        match lights.posted.send(setup, lights.report(locks)) {
            Ok(()) => state.sending = Some(locks),
            Err(UsbError::Busy) => state.tried = None,
            Err(_) => {}
        }
    }

    /// Leaves the registry, and releases every key and button it held.
    fn retire(&self) {
        unregister(self);
        {
            let mut state = self.state.lock();
            if state.retired {
                return;
            }
            state.retired = true;
            state.repeat = KeyRepeat::new();
        }
        release(self.keyboard, self.pointer);
    }
}

static DEVICES: SpinLock<[Option<KArc<Hid>>; MAX_DEVICES]> = SpinLock::new(
    [const { None }; MAX_DEVICES],
    lock_class!("usb-hid.DEVICES", LOCK_LEVEL_RESOURCE),
);

fn register(hid: &KArc<Hid>) -> bool {
    let mut devices = DEVICES.lock();
    let Some(free) = devices.iter_mut().find(|d| d.is_none()) else {
        return false;
    };
    *free = Some(KArc::clone(hid));
    true
}

fn unregister(hid: &Hid) {
    let taken = {
        let mut devices = DEVICES.lock();
        devices
            .iter_mut()
            .find(|d| d.as_deref().is_some_and(|d| core::ptr::eq(d, hid)))
            .and_then(Option::take)
    };
    drop(taken);
}

/// The USB thread's pass over every keyboard, retrying the i8042's LEDs
/// too: when it next needs one.
pub(super) fn serve(now: u64) -> Option<u64> {
    crate::ps2::keyboard::sync_leds();
    let locks = keyboard::locks();
    DEVICES
        .lock()
        .iter()
        .flatten()
        .filter_map(|hid| hid.serve(now, locks))
        .min()
}

struct Unplugged(KArc<Hid>);

impl Removal for Unplugged {
    fn remove(&self) {
        self.0.retire();
    }
}

struct Found {
    subclass: u8,
    protocol: u8,
    report_length: u16,
    endpoint: Endpoint,
}

fn find(config: &Configuration<'_>, interface: u8) -> Option<Found> {
    let header = config
        .interfaces()
        .find(|i| i.number == interface && i.alternate == 0)?;
    let report_length = config.setting(interface, 0).find_map(|item| match item {
        Item::Other {
            kind: class::HID_DESCRIPTOR,
            bytes,
        } => class::report_descriptor_length(bytes),
        _ => None,
    });
    let endpoint = config
        .endpoints(interface, 0)
        .find(|e| e.transfer_type() == TransferType::Interrupt && e.is_in())?;
    Some(Found {
        subclass: header.subclass,
        protocol: header.protocol,
        report_length: report_length.unwrap_or(0),
        endpoint,
    })
}

#[inline(never)]
fn read_map(control: &Control, info: &UsbFunction, length: u16) -> Option<ReportMap> {
    if length == 0 || usize::from(length) > PAGE_SIZE {
        return None;
    }
    let mut bytes = KVec::new();
    bytes.resize(usize::from(length), 0u8).ok()?;
    let setup = Setup::get_report_descriptor(info.first_interface, length);
    let read = control.read(setup, &mut bytes).ok()?;
    match ReportMap::parse(&bytes[..read], MAX_FIELDS, MAX_USAGES) {
        Ok(map) => Some(map),
        Err(why) => {
            klog_info!(
                "USB: {}-{} report descriptor refused: {:?}",
                info.controller,
                info.path,
                why
            );
            None
        }
    }
}

/// A device starts in the report protocol (HID 1.11 §7.2.6), so reading its
/// descriptor is right whether or not it honours `SET_PROTOCOL`; the boot
/// protocol is only right if it does, and NuPhy's 2.4 GHz receiver (19f5:2620)
/// acknowledges it and keeps sending its key bitmap.
fn choose(
    control: &Control,
    info: &UsbFunction,
    found: &Found,
    map: Option<ReportMap>,
) -> Option<Protocol> {
    let boot = found.subclass == class::SUBCLASS_BOOT;
    let interface = info.first_interface;
    if let Some(map) = map.filter(|m| {
        let desc = m.descriptor();
        keys::carries_keys(&desc) || pointer::carries_pointer(&desc)
    }) {
        if boot {
            let _ = control.write(Setup::set_protocol(interface, false), &[]);
        }
        return Some(Protocol::Report(map));
    }
    let fallback = match found.protocol {
        class::PROTOCOL_KEYBOARD if boot => Protocol::BootKeyboard,
        class::PROTOCOL_MOUSE if boot => Protocol::BootMouse,
        _ => return None,
    };
    if control
        .write(Setup::set_protocol(interface, true), &[])
        .is_err()
    {
        klog_info!(
            "USB: {}-{} refused the boot protocol",
            info.controller,
            info.path
        );
        return None;
    }
    Some(fallback)
}

fn probe(bound: &mut BoundUsbDevice<'_>) -> Result<ProbeOutcome, ProbeError> {
    let info = *bound.info();
    let Some(found) = bound
        .descriptors(|config| find(config, info.first_interface))
        .flatten()
    else {
        return Ok(ProbeOutcome::Declined);
    };
    let control = bound.control().map_err(|_| ProbeError::DeviceFault)?;
    let map = read_map(&control, &info, found.report_length);
    let Some(protocol) = choose(&control, &info, &found, map) else {
        return Ok(ProbeOutcome::Declined);
    };
    let _ = control.write(Setup::set_idle_forever(info.first_interface), &[]);
    let length = protocol
        .report_bytes()
        .max(found.endpoint.max_packet_size().into())
        .min(PAGE_SIZE) as u32;
    let Some(hid) = bind(bound, &info, protocol)? else {
        return Ok(ProbeOutcome::Declined);
    };
    if let Err(why) = bound.reports(found.endpoint.address, length, hid.clone()) {
        hid.retire();
        klog_info!(
            "USB: {}-{} reports not opened: {:?}",
            info.controller,
            info.path,
            why
        );
        return Err(ProbeError::DeviceFault);
    }
    if bound.on_remove(Unplugged(KArc::clone(&hid))).is_err() {
        hid.retire();
        return Err(ProbeError::OutOfMemory);
    }
    if !register(&hid) {
        hid.retire();
        full(&info, "HID interfaces");
        return Ok(ProbeOutcome::Declined);
    }
    super::wake();
    Ok(ProbeOutcome::Bound)
}

/// The output report for each lock state, when the keyboard has lock LEDs.
#[inline(never)]
fn lights(
    bound: &mut BoundUsbDevice<'_>,
    protocol: &Protocol,
) -> Result<Option<Lights>, ProbeError> {
    let mut reports = KVec::new();
    let (id, len) = match protocol {
        Protocol::BootMouse => return Ok(None),
        Protocol::BootKeyboard => {
            for locks in 0..8 {
                reports
                    .push(leds_of(locks).boot())
                    .map_err(|_| ProbeError::OutOfMemory)?;
            }
            (0, 1)
        }
        Protocol::Report(map) => {
            let desc = map.descriptor();
            let mut scratch = [0u8; MAX_OUTPUT];
            let Some((id, len)) = keys::led_report(&desc, Leds::default(), &mut scratch) else {
                return Ok(None);
            };
            reports
                .resize(8 * len, 0u8)
                .map_err(|_| ProbeError::OutOfMemory)?;
            for (locks, out) in reports.chunks_exact_mut(len).enumerate() {
                keys::led_report(&desc, leds_of(locks as u8), out);
            }
            (id, len)
        }
    };
    let posted = bound.posted().map_err(|_| ProbeError::OutOfMemory)?;
    Ok(Some(Lights {
        posted,
        id,
        len,
        reports,
    }))
}

/// Claims the keyboard and pointer slots the interface needs; `None` when a
/// table is full, which is logged.
#[inline(never)]
fn bind(
    bound: &mut BoundUsbDevice<'_>,
    info: &UsbFunction,
    protocol: Protocol,
) -> Result<Option<KArc<Hid>>, ProbeError> {
    let (is_keyboard, is_pointer) = match &protocol {
        Protocol::BootKeyboard => (true, false),
        Protocol::BootMouse => (false, true),
        Protocol::Report(map) => {
            let desc = map.descriptor();
            (keys::carries_keys(&desc), pointer::carries_pointer(&desc))
        }
    };
    let lights = lights(bound, &protocol)?;
    let keys = KBox::try_new(HeldByReport::EMPTY).map_err(|_| ProbeError::OutOfMemory)?;
    let keyboard = match is_keyboard.then(keyboard::claim) {
        Some(None) => {
            full(info, "keyboards");
            return Ok(None);
        }
        claimed => claimed.flatten(),
    };
    let pointer = match is_pointer.then(input_event::claim_pointer) {
        Some(None) => {
            release(keyboard, None);
            full(info, "pointers");
            return Ok(None);
        }
        claimed => claimed.flatten(),
    };
    let Ok(hid) = KArc::try_new(Hid {
        #[cfg(feature = "test-hooks")]
        controller: info.controller,
        #[cfg(feature = "test-hooks")]
        path: info.path,
        interface: info.first_interface,
        protocol,
        keyboard,
        pointer,
        lights,
        state: SpinLock::new(
            State {
                retired: false,
                keys,
                repeat: KeyRepeat::new(),
                sending: None,
                tried: None,
                acknowledged: None,
            },
            lock_class!("usb-hid.state", LOCK_LEVEL_RESOURCE),
        ),
    }) else {
        release(keyboard, pointer);
        return Err(ProbeError::OutOfMemory);
    };
    Ok(Some(hid))
}

fn release(keyboard: Option<KeyboardSource>, pointer: Option<PointerSource>) {
    let now = now_ms();
    if let Some(source) = keyboard {
        keyboard::release(source, now);
    }
    if let Some(source) = pointer {
        input_event::release_pointer(source, now);
    }
}

fn full(info: &UsbFunction, what: &str) {
    klog_info!(
        "USB: {}-{} declined: no room for more {}",
        info.controller,
        info.path,
        what
    );
}

crate::usb_driver! {
    pub static USB_HID = {
        name: "usb-hid",
        match_table: &[UsbMatch::Class {
            class: class::CLASS,
            subclass: None,
            protocol: None,
        }],
        probe: probe,
    };
}

/// A bound keyboard: where it is, its slot in the keyboard state, and the
/// locks its LEDs last acknowledged.
#[cfg(feature = "test-hooks")]
#[derive(Clone, Copy, Debug)]
pub struct BoundKeyboard {
    pub controller: u8,
    pub path: slopos_usb_core::bus::Path,
    pub source: KeyboardSource,
    pub leds: Option<u8>,
}

#[cfg(feature = "test-hooks")]
pub struct Keyboards {
    found: [Option<BoundKeyboard>; MAX_DEVICES],
}

#[cfg(feature = "test-hooks")]
impl Keyboards {
    pub fn iter(&self) -> impl Iterator<Item = &BoundKeyboard> {
        self.found.iter().flatten()
    }

    pub fn len(&self) -> usize {
        self.iter().count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(feature = "test-hooks")]
pub fn keyboards() -> Keyboards {
    let mut keyboards = Keyboards {
        found: [None; MAX_DEVICES],
    };
    for (hid, out) in DEVICES.lock().iter().zip(keyboards.found.iter_mut()) {
        if let Some(hid) = hid
            && let Some(source) = hid.keyboard
        {
            *out = Some(BoundKeyboard {
                controller: hid.controller,
                path: hid.path,
                source,
                leds: hid.state.lock().acknowledged,
            });
        }
    }
    keyboards
}

#[cfg(feature = "test-hooks")]
pub fn pointers() -> usize {
    DEVICES
        .lock()
        .iter()
        .flatten()
        .filter(|hid| hid.pointer.is_some())
        .count()
}
