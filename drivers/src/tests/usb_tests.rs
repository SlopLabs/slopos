//! The guest half of `just test-usb`: every device `scripts/test_usb.py`
//! attaches enumerated and bound, a stalled and an abandoned transfer
//! recovered, keys and motion the host injects reaching the keyboard state and
//! the cursor, a USB NIC leased and retired, and every device pulled and
//! plugged twice with no slot, page or claim left behind. The host acts on
//! the `USB-TEST:` lines.

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::input::{MODIFIER_CAPS_LOCK, MODIFIER_SHIFT};
use slopos_keymap_core::keycode::{KEY_LEFTSHIFT, KEY_X};
use slopos_keymap_core::{LOCK_CAPS, LOCK_NUM};
use slopos_net::iface::{self, Iface};
use slopos_net::neighbor::NEIGHBOR_CACHE;
use slopos_net::types::{DevIndex, Ipv4Addr, MacAddr};
use slopos_net::{DEVICE_REGISTRY, ROUTE_TABLE};
use slopos_ostd::{KArc, klog_info};
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};
use slopos_usb_core::bus::Path;
use slopos_usb_core::device::Speed;
use slopos_usb_core::device::descriptor::{TransferType, kind};
use slopos_usb_core::device::request::Setup;
use slopos_usb_core::xhci::transfer::TransferError;

use crate::driver_core::bus::{ProbeError, ProbeOutcome, Removal};
use crate::driver_core::shutdown::DeviceShutdown;
use crate::input_event;
use crate::keyboard::{self, KeyboardSource};
use crate::usb::bus::{BoundUsbDevice, UsbMatch};
use crate::usb::hid::{self, BoundKeyboard};
use crate::usb::xhci::device::{Device, Pipe, UsbError};
use crate::usb::xhci::{self, Controller};

const QEMU_XHCI: (u16, u16) = (0x1b36, 0x000d);
const NEC_XHCI: (u16, u16) = (0x1033, 0x0194);
const STICK: (u16, u16) = (0x46f4, 0x0001);
const HUB: (u16, u16) = (0x0409, 0x55aa);
/// Of two USB 3 and five USB 2 ports: a SuperSpeed stick, the hub, a
/// high-speed stick and a full-speed keyboard.
const ROOT_PORTS: [u8; 4] = [1, 4, 5, 6];
/// By root port and hub port, 0 for the root port's own device, on both
/// controllers.
const DEVICES: [(u8, u8, Speed); 7] = [
    (1, 0, Speed::Super),
    (4, 0, Speed::Full),
    (4, 1, Speed::Full),
    (4, 2, Speed::Full),
    (4, 3, Speed::Full),
    (5, 0, Speed::High),
    (6, 0, Speed::Full),
];
/// The sticks among them, which `usb-test` binds.
const STICKS: u32 = 3;
/// qemu-xhci's alone: an ext4 stick `usb-storage` binds as `sda`, the one
/// USB disk until `usb_disk_test` plugs a read-only drive on nec-usb-xhci's
/// port 7, and a usb-net behind the hub, its network 10.0.3.0/24.
const DISK: (u8, u8, Speed) = (7, 0, Speed::High);
const NET: (u8, u8, Speed) = (4, 4, Speed::Full);
const QEMU_ONLY: [(u8, u8, Speed); 2] = [DISK, NET];
/// `scripts/test_usb.py`'s, whose first byte older QEMU reports as 0x40 anyway.
const NET_MAC: MacAddr = MacAddr([0x40, 0x54, 0x00, 0x12, 0x34, 0x99]);
const NET_LEASE: Ipv4Addr = Ipv4Addr([10, 0, 3, 15]);
const NET_GATEWAY: Ipv4Addr = Ipv4Addr([10, 0, 3, 2]);
/// The echo peer on the NIC's network, which no other route reaches.
const NET_PEER: Ipv4Addr = Ipv4Addr([10, 0, 3, 100]);
/// The keyboard, the tablet and the mouse, which `usb-test-hid` tries and
/// `usb-hid` binds.
const HIDS: u32 = 3;
const SETTLE_MS: u32 = 30_000;
/// The host acts on a marker within this long, however slow the emulator.
const HOST_MS: u32 = 120_000;
/// Free dynamic MMIO ranges a boot with both controllers must keep.
const IO_MEM_HEADROOM: usize = 8;

static BINDS: AtomicU32 = AtomicU32::new(0);
static UNBINDS: AtomicU32 = AtomicU32::new(0);
static TEST_UNIT_READY: AtomicU32 = AtomicU32::new(0);
static STALLS_RECOVERED: AtomicU32 = AtomicU32::new(0);
static HID_PROBES: AtomicU32 = AtomicU32::new(0);
static ABANDONED_RECOVERED: AtomicU32 = AtomicU32::new(0);

struct Unbind;

impl Removal for Unbind {
    fn remove(&self) {
        UNBINDS.fetch_add(1, Ordering::AcqRel);
    }
}

const CBW_SIGNATURE: u32 = 0x4342_5355;
const CSW_SIGNATURE: u32 = 0x5342_5355;
const CBW_TAG: u32 = 0x5553_4231;

/// The 1 MiB sticks `just test-usb` plugs for this driver; it declines any
/// other for `usb-storage`.
const TEST_STICK_BLOCKS: u32 = 2048;

/// A control request, then a Bulk-Only TEST UNIT READY through both bulk
/// pipes. A stray read of bulk-in stalls, as a stick waiting for a command
/// does, and a second TEST UNIT READY proves the endpoint recovered on both
/// sides.
fn probe_stick(bound: &mut BoundUsbDevice<'_>) -> Result<ProbeOutcome, ProbeError> {
    let info = *bound.info();
    let control = bound.control().map_err(|_| ProbeError::DeviceFault)?;
    let mut device = [0u8; 18];
    let read = control
        .read(Setup::get_descriptor(kind::DEVICE, 0, 0, 18), &mut device)
        .map_err(|_| ProbeError::DeviceFault)?;
    let vendor = u16::from_le_bytes([device[8], device[9]]);
    let product = u16::from_le_bytes([device[10], device[11]]);
    if read != 18 || (vendor, product) != (info.vendor, info.product) {
        return Err(ProbeError::Mismatch);
    }
    let bulk = |input: bool| {
        bound
            .descriptors(|config| {
                config
                    .endpoints(info.first_interface, 0)
                    .find(|e| e.transfer_type() == TransferType::Bulk && e.is_in() == input)
                    .map(|e| e.address)
            })
            .flatten()
    };
    let (Some(bulk_in), Some(bulk_out)) = (bulk(true), bulk(false)) else {
        return Err(ProbeError::Mismatch);
    };
    let pipe_out = bound.pipe(bulk_out).map_err(|_| ProbeError::DeviceFault)?;
    let pipe_in = bound.pipe(bulk_in).map_err(|_| ProbeError::DeviceFault)?;
    let ready = test_unit_ready(&pipe_out, &pipe_in);
    if blocks(&pipe_out, &pipe_in) != Some(TEST_STICK_BLOCKS) {
        return Ok(ProbeOutcome::Declined);
    }
    if ready {
        TEST_UNIT_READY.fetch_add(1, Ordering::AcqRel);
    }
    let mut stray = [0u8; 13];
    let stalled = pipe_in.read(&mut stray, 5000) == Err(UsbError::Transfer(TransferError::Stall));
    if stalled && test_unit_ready(&pipe_out, &pipe_in) {
        STALLS_RECOVERED.fetch_add(1, Ordering::AcqRel);
    }
    bound
        .on_remove(Unbind)
        .map_err(|_| ProbeError::OutOfMemory)?;
    BINDS.fetch_add(1, Ordering::AcqRel);
    Ok(ProbeOutcome::Bound)
}

fn test_unit_ready(pipe_out: &Pipe, pipe_in: &Pipe) -> bool {
    let mut cbw = [0u8; 31];
    cbw[0..4].copy_from_slice(&CBW_SIGNATURE.to_le_bytes());
    cbw[4..8].copy_from_slice(&CBW_TAG.to_le_bytes());
    cbw[14] = 6;
    if pipe_out.write(&cbw, 5000).is_err() {
        return false;
    }
    status(pipe_in)
}

/// READ CAPACITY (10): the stick's blocks.
fn blocks(pipe_out: &Pipe, pipe_in: &Pipe) -> Option<u32> {
    let mut cbw = [0u8; 31];
    cbw[0..4].copy_from_slice(&CBW_SIGNATURE.to_le_bytes());
    cbw[4..8].copy_from_slice(&CBW_TAG.to_le_bytes());
    cbw[8] = 8;
    cbw[12] = 0x80;
    cbw[14] = 10;
    cbw[15] = 0x25;
    pipe_out.write(&cbw, 5000).ok()?;
    let mut answer = [0u8; 8];
    let read = pipe_in.read(&mut answer, 5000).ok()?;
    if !status(pipe_in) || read != 8 {
        return None;
    }
    u32::from_be_bytes([answer[0], answer[1], answer[2], answer[3]]).checked_add(1)
}

fn status(pipe_in: &Pipe) -> bool {
    let mut csw = [0u8; 13];
    let Ok(read) = pipe_in.read(&mut csw, 5000) else {
        return false;
    };
    let signature = u32::from_le_bytes([csw[0], csw[1], csw[2], csw[3]]);
    let tag = u32::from_le_bytes([csw[4], csw[5], csw[6], csw[7]]);
    read == 13 && signature == CSW_SIGNATURE && tag == CBW_TAG
}

/// An idle HID device NAKs its interrupt endpoint, so a read times out and is
/// abandoned, and the endpoint must come back idle once the USB thread has
/// moved its ring past it. Then it declines, for `usb-hid` to bind.
fn probe_hid(bound: &mut BoundUsbDevice<'_>) -> Result<ProbeOutcome, ProbeError> {
    let info = *bound.info();
    let reports = bound
        .descriptors(|config| {
            config
                .endpoints(info.first_interface, 0)
                .find(|e| e.transfer_type() == TransferType::Interrupt && e.is_in())
                .map(|e| e.address)
        })
        .flatten()
        .ok_or(ProbeError::Mismatch)?;
    let pipe = bound.pipe(reports).map_err(|_| ProbeError::DeviceFault)?;
    let mut report = [0u8; 8];
    for _ in 0..HID_READS {
        match pipe.read(&mut report, HID_READ_MS) {
            Ok(_) => continue,
            Err(UsbError::Timeout) => {
                if wait(SETTLE_MS, || pipe.is_idle()) {
                    ABANDONED_RECOVERED.fetch_add(1, Ordering::AcqRel);
                }
                break;
            }
            Err(_) => return Err(ProbeError::DeviceFault),
        }
    }
    HID_PROBES.fetch_add(1, Ordering::AcqRel);
    Ok(ProbeOutcome::Declined)
}

/// An idle device NAKs at the latest once its first reports are read.
const HID_READS: usize = 8;
const HID_READ_MS: u64 = 100;

crate::usb_driver! {
    static USB_TEST_DRIVER = {
        name: "usb-test",
        match_table: &[UsbMatch::Class {
            class: 8,
            subclass: Some(6),
            protocol: Some(0x50),
        }],
        priority: 64,
        probe: probe_stick,
    };
}

crate::usb_driver! {
    static USB_TEST_HID_DRIVER = {
        name: "usb-test-hid",
        match_table: &[UsbMatch::Class {
            class: 3,
            subclass: None,
            protocol: None,
        }],
        priority: 64,
        probe: probe_hid,
    };
}

fn controllers() -> Option<[KArc<Controller>; 2]> {
    let find = |ids| {
        (1..=xhci::MAX_CONTROLLERS as u8)
            .filter_map(xhci::controller)
            .find(|c| c.ids() == ids)
    };
    Some([find(QEMU_XHCI)?, find(NEC_XHCI)?])
}

fn wait(ms: u32, condition: impl Fn() -> bool) -> bool {
    crate::hpet::poll_wait(&condition, ms)
}

fn path(root: u8, hub_port: u8) -> Path {
    let root = Path::root(root);
    if hub_port == 0 {
        root
    } else {
        root.child(hub_port).unwrap_or(root)
    }
}

/// Every device, configured at its speed, and nothing else.
fn complete(c: &Controller) -> bool {
    let extras: &[(u8, u8, Speed)] = if c.ids() == QEMU_XHCI {
        &QEMU_ONLY
    } else {
        &[]
    };
    let devices = c.devices();
    devices.len() == DEVICES.len() + extras.len()
        && DEVICES
            .iter()
            .chain(extras)
            .all(|&(root, hub_port, speed)| {
                let want = path(root, hub_port);
                devices.iter().any(|d| {
                    d.node().is_some_and(|n| {
                        n.path == want && n.speed == speed && (n.configuration != 0 || n.hub)
                    })
                })
            })
}

fn every_controller(test: impl Fn(&Controller) -> bool) -> bool {
    controllers().is_some_and(|all| all.iter().all(|c| test(c)))
}

fn empty(c: &Controller) -> bool {
    c.slots_in_use() == 0 && c.devices().is_empty()
}

fn usb_nic() -> Option<Iface> {
    let mut found = None;
    iface::for_each(|i| {
        if i.mac == NET_MAC {
            found = Some(*i);
        }
    });
    found
}

/// The USB NIC, `eth1` beside virtio-net's `eth0`.
fn nic_published() -> Option<Iface> {
    let eth0 = iface::get_by_name(b"eth0")?;
    usb_nic().filter(|nic| nic.name.as_bytes() == b"eth1" && eth0.dev != nic.dev)
}

/// Nothing of a retired NIC is left: its interface, routes, neighbours,
/// DHCP client and device slot, while `eth0` keeps its name.
fn nic_retired(dev: DevIndex) -> bool {
    usb_nic().is_none()
        && ROUTE_TABLE.all_routes().iter().all(|r| r.dev != dev)
        && NEIGHBOR_CACHE.snapshot_owned(Some(dev)).1 == 0
        && !slopos_net::dhcp::is_running(dev)
        && DEVICE_REGISTRY.device_at(dev).is_none()
        && iface::get_by_name(b"eth0").is_some_and(|i| i.mac != NET_MAC)
}

/// Both controllers' keyboards, tablets and mice bound.
fn every_hid_bound() -> bool {
    hid::keyboards().len() == 2 && hid::pointers() == 4
}

pub fn test_usb_01_controllers_run() -> TestResult {
    let connected = wait(SETTLE_MS, || {
        every_controller(|c| c.is_running() && ROOT_PORTS.iter().all(|&p| c.port_connected(p)))
    });
    let Some([qemu, nec]) = controllers() else {
        return fail!("both QEMU xHCI models must have bound");
    };
    assert_test!(connected, "every port with a device must show it connected");
    assert_eq_test!(qemu.interrupt(), "MSI-X", "qemu-xhci takes MSI-X");
    assert_eq_test!(
        nec.interrupt(),
        "MSI",
        "nec-usb-xhci with msix=off takes MSI"
    );
    for c in [&qemu, &nec] {
        assert_eq_test!(c.max_ports(), 7, "two USB 3 ports and five USB 2");
        for port in [2, 3] {
            assert_test!(!c.port_connected(port), "no device on ports 2 and 3");
        }
    }
    assert_test!(qemu.port_connected(DISK.0), "qemu-xhci's disk is on port 7");
    assert_test!(
        !nec.port_connected(DISK.0),
        "nec-usb-xhci's port 7 is empty"
    );
    let free = slopos_ostd::mm::io_mem_ranges_free();
    klog_info!("USB-TEST: io-mem ranges free {}", free);
    assert_test!(
        free >= IO_MEM_HEADROOM,
        "the dynamic MMIO registry keeps its headroom"
    );
    pass!()
}

pub fn test_usb_02_every_device_enumerates() -> TestResult {
    let enumerated = wait(SETTLE_MS, || {
        crate::usb::settled() && every_controller(complete) && every_hid_bound()
    });
    assert_test!(
        enumerated,
        "every device, the hub's three included, must enumerate"
    );
    let Some(all) = controllers() else {
        return fail!("both controllers must be running");
    };
    for c in &all {
        for device in c.devices().iter() {
            let Some(node) = device.node() else {
                return fail!("a device with no record");
            };
            if node.path == path(1, 0) || node.path == path(5, 0) || node.path == path(4, 1) {
                assert_eq_test!((node.vendor, node.product), STICK, "a stick");
            }
            if node.path == path(4, 0) {
                assert_test!(node.hub, "the hub is driven as one");
                assert_eq_test!((node.vendor, node.product), HUB, "QEMU's hub");
            }
        }
    }
    assert_eq_test!(
        BINDS.load(Ordering::Acquire),
        2 * STICKS,
        "every stick bound"
    );
    assert_eq_test!(
        TEST_UNIT_READY.load(Ordering::Acquire),
        2 * STICKS,
        "every stick answered through its bulk pipes"
    );
    assert_eq_test!(
        STALLS_RECOVERED.load(Ordering::Acquire),
        2 * STICKS,
        "a stalled bulk-in recovers, the stick's halt cleared"
    );
    assert_eq_test!(
        HID_PROBES.load(Ordering::Acquire),
        2 * HIDS,
        "usb-test-hid tried every keyboard, tablet and mouse"
    );
    assert_eq_test!(
        ABANDONED_RECOVERED.load(Ordering::Acquire),
        2 * HIDS,
        "an abandoned read leaves its endpoint idle once moved past"
    );
    assert_eq_test!(
        xhci::clears_sent(),
        2 * STICKS,
        "a stalled endpoint's halt is cleared on the device, a stopped one's is not"
    );
    assert_eq_test!(
        crate::usb::bus::claims_held(),
        2 * (STICKS + HIDS) + 2,
        "one claim per bound function"
    );
    assert_test!(
        nic_published().is_some(),
        "usb-net published the adapter as eth1"
    );
    assert_test!(
        crate::usb::storage::has_disk(b"sda"),
        "usb-storage registered the ext4 stick as sda"
    );
    let listed = slopos_ostd::kconsole::runs(b'u');
    assert_test!(
        slopos_ostd::kconsole::request_informational(b'u').is_ok(),
        "the USB listing is an informational command"
    );
    assert_test!(
        wait(SETTLE_MS, || slopos_ostd::kconsole::runs(b'u') > listed),
        "the USB listing runs"
    );
    pass!()
}

fn holding(usage: u16) -> Option<BoundKeyboard> {
    hid::keyboards()
        .iter()
        .copied()
        .find(|k| keyboard::holds(k.source, usage))
}

fn usb_repeats() -> u32 {
    hid::keyboards()
        .iter()
        .map(|k| keyboard::repeats(k.source))
        .sum()
}

fn i8042_repeats() -> u32 {
    keyboard::repeats(KeyboardSource::I8042)
}

fn each_device(mut visit: impl FnMut(&Device)) {
    for c in controllers().iter().flatten() {
        for device in c.devices().iter() {
            visit(device);
        }
    }
}

/// A held USB key repeats from the USB thread; the i8042's own repeated
/// press is marked a repeat as well. Every report is first abandoned as a
/// halt leaves it, so the keys arrive only if recovery posts them again.
pub fn test_usb_03_held_keys_repeat() -> TestResult {
    let mut abandoned = 0;
    each_device(|d| abandoned += d.abandon_reports());
    assert_eq_test!(
        abandoned,
        7,
        "both keyboards', tablets' and mice's reports, and the NIC's notifications"
    );
    assert_test!(
        wait(HOST_MS, || {
            let mut posted = true;
            each_device(|d| posted &= d.reports_posted());
            posted
        }),
        "every reporting endpoint recovers with a report posted"
    );
    let before = usb_repeats();
    klog_info!("USB-TEST: hold x");
    assert_test!(
        wait(HOST_MS, || holding(KEY_X).is_some()),
        "a USB keyboard holds the key the host pressed"
    );
    assert_test!(
        wait(SETTLE_MS, || usb_repeats() >= before + 3),
        "a held key repeats"
    );
    klog_info!("USB-TEST: release x");
    assert_test!(
        wait(HOST_MS, || holding(KEY_X).is_none()),
        "the release reaches the keyboard state"
    );
    let stopped = usb_repeats();
    crate::hpet::delay_ms(300);
    assert_eq_test!(usb_repeats(), stopped, "a released key stops repeating");

    let i8042 = i8042_repeats();
    klog_info!("USB-TEST: ps2 repeat x");
    assert_test!(
        wait(HOST_MS, || i8042_repeats() == i8042 + 1
            && !keyboard::holds(KeyboardSource::I8042, KEY_X)),
        "the i8042's second press of a held key is a repeat"
    );
    pass!()
}

fn locks_everywhere(locks: u8) -> bool {
    let keyboards = hid::keyboards();
    keyboard::locks() == locks
        && keyboards.len() == 2
        && keyboards.iter().all(|k| k.leds == Some(locks))
        && crate::ps2::keyboard::leds_acknowledged() == Some(locks)
}

/// Caps Lock on a USB keyboard lights every keyboard's LED, the i8042's
/// among them, and a second press puts them all out.
pub fn test_usb_04_caps_lock_lights_every_keyboard() -> TestResult {
    assert_test!(
        wait(SETTLE_MS, || locks_everywhere(LOCK_NUM)),
        "every keyboard starts with Num Lock alone lit"
    );
    klog_info!("USB-TEST: caps");
    assert_test!(
        wait(HOST_MS, || locks_everywhere(LOCK_NUM | LOCK_CAPS)),
        "Caps Lock lit on both USB keyboards and the i8042"
    );
    assert_test!(
        input_event::input_get_modifier_state() & MODIFIER_CAPS_LOCK != 0,
        "the modifier state carries Caps Lock"
    );
    klog_info!("USB-TEST: caps");
    assert_test!(
        wait(HOST_MS, || locks_everywhere(LOCK_NUM)),
        "a second press puts Caps Lock out everywhere"
    );
    pass!()
}

/// Alt+PrintScreen and a command key on a USB keyboard run the command.
pub fn test_usb_05_sysrq_runs_a_command() -> TestResult {
    let runs = slopos_ostd::kconsole::runs(b'u');
    klog_info!("USB-TEST: sysrq u");
    assert_test!(
        wait(HOST_MS, || slopos_ostd::kconsole::runs(b'u') > runs),
        "the command key ran its command"
    );
    pass!()
}

fn cursor() -> (i32, i32) {
    input_event::input_get_pointer_position()
}

/// The tablet places the cursor, the mice move it, and a button stays down
/// while any device holds it.
pub fn test_usb_06_pointers_share_the_cursor() -> TestResult {
    let (width, height) = input_event::pointer_bounds();
    assert_test!(
        width > 1 && height > 1,
        "the video layer published a screen"
    );
    klog_info!("USB-TEST: tablet 16384 8192");
    let placed = (
        (16384i64 * i64::from(width - 1) / 0x7fff) as i32,
        (8192i64 * i64::from(height - 1) / 0x7fff) as i32,
    );
    assert_test!(
        wait(HOST_MS, || cursor() == placed),
        "the tablet places the cursor on the screen"
    );
    klog_info!("USB-TEST: mouse 12 -7");
    let moved = (placed.0 + 12, placed.1 - 7);
    assert_test!(
        wait(HOST_MS, || cursor() == moved),
        "a USB mouse moves the same cursor"
    );

    klog_info!("USB-TEST: tablet press");
    assert_test!(
        wait(HOST_MS, || input_event::button_holders(1) == 1),
        "the tablet holds the left button"
    );
    klog_info!("USB-TEST: mouse press");
    assert_test!(
        wait(HOST_MS, || input_event::button_holders(1) == 2),
        "the mouse holds it too"
    );
    klog_info!("USB-TEST: tablet release");
    assert_test!(
        wait(HOST_MS, || input_event::button_holders(1) == 1),
        "the tablet let go"
    );
    assert_test!(
        input_event::input_get_button_state() & 1 != 0,
        "the button stays down while the mouse holds it"
    );
    klog_info!("USB-TEST: mouse release");
    assert_test!(
        wait(HOST_MS, || input_event::input_get_button_state() & 1 == 0),
        "the last release lifts it"
    );

    klog_info!("USB-TEST: pull mice");
    assert_test!(wait(HOST_MS, || hid::pointers() == 2), "both mice leave");
    let before = cursor();
    klog_info!("USB-TEST: ps2 mouse 5 5");
    assert_test!(
        wait(HOST_MS, || cursor() == (before.0 + 5, before.1 + 5)),
        "the PS/2 mouse moves the same cursor"
    );
    klog_info!("USB-TEST: plug");
    assert_test!(
        wait(HOST_MS, || every_hid_bound() && crate::usb::settled()),
        "both mice return"
    );
    pass!()
}

fn shifted() -> bool {
    input_event::input_get_modifier_state() & MODIFIER_SHIFT != 0
}

fn pull(pulled: BoundKeyboard) -> bool {
    klog_info!(
        "USB-TEST: pull keyboard {}-{}",
        pulled.controller,
        pulled.path
    );
    wait(HOST_MS, || {
        hid::keyboards()
            .iter()
            .all(|k| (k.controller, k.path) != (pulled.controller, pulled.path))
            && !keyboard::holds(pulled.source, KEY_LEFTSHIFT)
    })
}

/// A keyboard pulled with Shift held releases it, unless another keyboard
/// still holds it.
pub fn test_usb_07_pulled_keyboard_releases_shift() -> TestResult {
    klog_info!("USB-TEST: hold shift");
    assert_test!(
        wait(HOST_MS, || holding(KEY_LEFTSHIFT).is_some()
            && keyboard::holds(KeyboardSource::I8042, KEY_LEFTSHIFT)),
        "a USB keyboard and the i8042 hold Shift"
    );
    let Some(first) = holding(KEY_LEFTSHIFT) else {
        return fail!("no USB keyboard holds Shift");
    };
    assert_test!(pull(first), "the keyboard holding Shift leaves");
    assert_test!(shifted(), "the i8042 still holds Shift");
    klog_info!("USB-TEST: ps2 release shift");
    assert_test!(
        wait(HOST_MS, || !shifted()),
        "nothing is shifted once the i8042 lets go"
    );

    klog_info!("USB-TEST: hold usb shift");
    assert_test!(
        wait(HOST_MS, || holding(KEY_LEFTSHIFT).is_some() && shifted()),
        "the other USB keyboard holds Shift"
    );
    let Some(second) = holding(KEY_LEFTSHIFT) else {
        return fail!("no USB keyboard holds Shift");
    };
    assert_test!(pull(second), "the second keyboard leaves");
    assert_test!(!shifted(), "pulling it left nothing shifted");

    klog_info!("USB-TEST: plug");
    assert_test!(
        wait(HOST_MS, || every_hid_bound()
            && every_controller(complete)
            && crate::usb::settled()),
        "both keyboards return"
    );
    assert_test!(
        wait(SETTLE_MS, || locks_everywhere(LOCK_NUM)),
        "a returning keyboard is told the locks"
    );
    pass!()
}

fn interrupts() -> Option<[u32; 2]> {
    controllers().map(|all| all.map(|c| c.interrupts_taken()))
}

/// Port events reach the thread by interrupt, not only by its once-a-second
/// drain, which would hide a drain that left the interrupter unarmed.
fn interrupted_since(before: [u32; 2]) -> bool {
    interrupts().is_some_and(|now| now.iter().zip(before).all(|(now, before)| *now > before))
}

static PAGES_EMPTY: AtomicU32 = AtomicU32::new(0);
static PAGES_FULL: AtomicU32 = AtomicU32::new(0);

/// No slot, no claim, and the pages held before the first plug.
fn pulled(round: u32) -> TestResult {
    let (Some(irqs), unbinds) = (interrupts(), UNBINDS.load(Ordering::Acquire)) else {
        return fail!("both controllers must be running");
    };
    let Some(nic) = usb_nic() else {
        return fail!("the USB NIC must be published before the pull");
    };
    let full = xhci::pages_held() as u32;
    if round == 1 {
        PAGES_FULL.store(full, Ordering::Release);
    }
    klog_info!("USB-TEST: pull");
    let gone = wait(HOST_MS, || {
        every_controller(empty)
            && crate::usb::settled()
            && UNBINDS.load(Ordering::Acquire) == unbinds + 2 * STICKS
            && !crate::usb::storage::has_disk(b"sda")
    });
    assert_test!(
        gone,
        "every device must leave its slot, its drivers unbound"
    );
    assert_test!(
        nic_retired(nic.dev),
        "the USB NIC's interface, routes, neighbours and DHCP client leave with it"
    );
    assert_eq_test!(crate::usb::bus::claims_held(), 0, "no claim left");
    assert_test!(
        hid::keyboards().is_empty() && hid::pointers() == 0,
        "no HID interface left bound"
    );
    assert_test!(
        every_controller(|c| ROOT_PORTS
            .iter()
            .chain(&[DISK.0])
            .all(|&p| !c.port_connected(p))),
        "every root port empty"
    );
    assert_test!(
        interrupted_since(irqs),
        "each controller interrupts for its pulls"
    );
    let empty = xhci::pages_held() as u32;
    klog_info!("USB-TEST: pages held {} full, {} empty", full, empty);
    if round == 1 {
        PAGES_EMPTY.store(empty, Ordering::Release);
    } else {
        assert_eq_test!(
            empty,
            PAGES_EMPTY.load(Ordering::Acquire),
            "a second pull gives back every page the second plug took"
        );
        assert_eq_test!(
            full,
            PAGES_FULL.load(Ordering::Acquire),
            "each plug takes as many"
        );
    }
    pass!()
}

fn plugged() -> TestResult {
    let binds = BINDS.load(Ordering::Acquire);
    let hid_probes = HID_PROBES.load(Ordering::Acquire);
    let Some(irqs) = interrupts() else {
        return fail!("both controllers must be running");
    };
    klog_info!("USB-TEST: plug");
    let back = wait(HOST_MS, || {
        every_controller(complete)
            && crate::usb::settled()
            && BINDS.load(Ordering::Acquire) == binds + 2 * STICKS
            && HID_PROBES.load(Ordering::Acquire) == hid_probes + 2 * HIDS
            && every_hid_bound()
            && crate::usb::storage::has_disk(b"sda")
            && nic_published().is_some()
    });
    assert_test!(back, "every device must enumerate again and bind");
    assert_eq_test!(
        crate::usb::bus::claims_held(),
        2 * (STICKS + HIDS) + 2,
        "one claim per bound function"
    );
    assert_test!(
        interrupted_since(irqs),
        "each controller interrupts for its plugs"
    );
    pass!()
}

/// The USB NIC leases an address on its own network, routes the network
/// through itself, and resolves the gateway over it.
pub fn test_usb_08_nic_takes_a_lease() -> TestResult {
    let leased = wait(HOST_MS, || {
        nic_published().is_some_and(|nic| iface::our_ip(nic.dev) == Some(NET_LEASE))
    });
    assert_test!(leased, "eth1 leases 10.0.3.15 from its own network");
    let Some(nic) = usb_nic() else {
        return fail!("the USB NIC left");
    };
    assert_eq_test!(
        ROUTE_TABLE.lookup(NET_PEER).map(|(dev, _)| dev),
        Some(nic.dev),
        "10.0.3.0/24 is reached through eth1"
    );
    let sent = slopos_net::udp::udp_sendto(NET_LEASE.0, NET_GATEWAY.0, 40_000, 9, 0, b"usb-net");
    assert_eq_test!(sent, Ok(7), "a datagram leaves through eth1");
    assert_test!(
        wait(HOST_MS, || NEIGHBOR_CACHE
            .is_reachable(nic.dev, NET_GATEWAY)),
        "the gateway answers ARP over eth1"
    );
    pass!()
}

pub fn test_usb_09_pulled_devices_leave() -> TestResult {
    pulled(1)
}

pub fn test_usb_10_plugged_devices_return() -> TestResult {
    plugged()
}

pub fn test_usb_11_pulled_again() -> TestResult {
    pulled(2)
}

pub fn test_usb_12_plugged_again() -> TestResult {
    plugged()
}

/// After the userland tests have used the NIC and typed at the shell: each
/// controller's shutdown hook leaves it halted, reset and off the bus.
pub fn test_usb_13_shutdown_resets() -> TestResult {
    let Some(all) = controllers() else {
        return fail!("both controllers must be running");
    };
    for c in &all {
        c.shutdown();
        assert_test!(!c.is_running(), "a shut-down controller is not served");
        assert_test!(c.is_reset(), "halted, out of reset and off the bus");
        c.shutdown();
        assert_test!(c.is_reset(), "a second shutdown leaves the first's state");
    }
    pass!()
}

/// `just test`'s scratch stick: bound by `usb-storage` as `sda`, with the
/// dynamic MMIO ranges a controller takes still leaving headroom.
pub fn test_usb_scratch_stick_is_sda() -> TestResult {
    assert_test!(
        crate::usb::storage::has_disk(b"sda"),
        "usb-storage registered the stick as sda"
    );
    let free = slopos_ostd::mm::io_mem_ranges_free();
    assert_test!(
        free >= IO_MEM_HEADROOM,
        "{} dynamic MMIO ranges free, fewer than {}",
        free,
        IO_MEM_HEADROOM
    );
    pass!()
}

slopos_testing::stest!(name = test_usb_scratch_stick_is_sda);

const HOSTED: u32 = slopos_testing::FLAG_EXPLICIT | slopos_testing::FLAG_UNCAPTURED;

slopos_testing::stest!(name = test_usb_01_controllers_run, flags = HOSTED);
slopos_testing::stest!(name = test_usb_02_every_device_enumerates, flags = HOSTED);
slopos_testing::stest!(name = test_usb_03_held_keys_repeat, flags = HOSTED);
slopos_testing::stest!(
    name = test_usb_04_caps_lock_lights_every_keyboard,
    flags = HOSTED
);
slopos_testing::stest!(name = test_usb_05_sysrq_runs_a_command, flags = HOSTED);
slopos_testing::stest!(name = test_usb_06_pointers_share_the_cursor, flags = HOSTED);
slopos_testing::stest!(
    name = test_usb_07_pulled_keyboard_releases_shift,
    flags = HOSTED
);
slopos_testing::stest!(name = test_usb_08_nic_takes_a_lease, flags = HOSTED);
slopos_testing::stest!(name = test_usb_09_pulled_devices_leave, flags = HOSTED);
slopos_testing::stest!(name = test_usb_10_plugged_devices_return, flags = HOSTED);
slopos_testing::stest!(name = test_usb_11_pulled_again, flags = HOSTED);
slopos_testing::stest!(name = test_usb_12_plugged_again, flags = HOSTED);
slopos_testing::stest!(
    name = test_usb_13_shutdown_resets,
    flags = HOSTED,
    kind = Userland
);
