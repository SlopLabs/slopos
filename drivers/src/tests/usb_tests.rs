//! The guest half of `just test-usb`: every device `scripts/test_usb.py`
//! attaches enumerated and bound, a stalled and an abandoned transfer
//! recovered, and every device pulled and plugged twice with no slot, page or
//! claim left behind. The host acts on the `USB-TEST:` lines.

use core::sync::atomic::{AtomicU32, Ordering};

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
use crate::usb::bus::{BoundUsbDevice, UsbMatch};
use crate::usb::xhci::device::{Pipe, UsbError};
use crate::usb::xhci::{self, Controller};

const QEMU_XHCI: (u16, u16) = (0x1b36, 0x000d);
const NEC_XHCI: (u16, u16) = (0x1033, 0x0194);
const STICK: (u16, u16) = (0x46f4, 0x0001);
const HUB: (u16, u16) = (0x0409, 0x55aa);
/// Of two USB 3 and four USB 2 ports: a SuperSpeed stick, the hub, a
/// high-speed stick and a full-speed keyboard.
const ROOT_PORTS: [u8; 4] = [1, 4, 5, 6];
/// By root port and hub port, 0 for the root port's own device.
const DEVICES: [(u8, u8, Speed); 6] = [
    (1, 0, Speed::Super),
    (4, 0, Speed::Full),
    (4, 1, Speed::Full),
    (4, 2, Speed::Full),
    (5, 0, Speed::High),
    (6, 0, Speed::Full),
];
/// The sticks among them, which `usb-test` binds.
const STICKS: u32 = 3;
/// The keyboard and the tablet, which `usb-test-hid` binds.
const HIDS: u32 = 2;
const SETTLE_MS: u32 = 30_000;
/// The host acts on a marker within this long, however slow the emulator.
const HOST_MS: u32 = 120_000;
/// Free dynamic MMIO ranges a boot with both controllers must keep.
const IO_MEM_HEADROOM: usize = 8;

static BINDS: AtomicU32 = AtomicU32::new(0);
static UNBINDS: AtomicU32 = AtomicU32::new(0);
static TEST_UNIT_READY: AtomicU32 = AtomicU32::new(0);
static STALLS_RECOVERED: AtomicU32 = AtomicU32::new(0);
static HID_BINDS: AtomicU32 = AtomicU32::new(0);
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
    if test_unit_ready(&pipe_out, &pipe_in) {
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
    let mut csw = [0u8; 13];
    let Ok(read) = pipe_in.read(&mut csw, 5000) else {
        return false;
    };
    let signature = u32::from_le_bytes([csw[0], csw[1], csw[2], csw[3]]);
    let tag = u32::from_le_bytes([csw[4], csw[5], csw[6], csw[7]]);
    read == 13 && signature == CSW_SIGNATURE && tag == CBW_TAG
}

/// An idle HID device NAKs its interrupt endpoint, so a read times out and
/// is abandoned, and the endpoint must come back idle once the USB thread
/// has moved its ring past it.
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
    HID_BINDS.fetch_add(1, Ordering::AcqRel);
    Ok(ProbeOutcome::Bound)
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
    let devices = c.devices();
    devices.len() == DEVICES.len()
        && DEVICES.iter().all(|&(root, hub_port, speed)| {
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

pub fn test_usb_1_controllers_run() -> TestResult {
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
        assert_eq_test!(c.max_ports(), 6, "two USB 3 ports and four USB 2");
        for port in [2, 3] {
            assert_test!(!c.port_connected(port), "no device on ports 2 and 3");
        }
    }
    let free = slopos_ostd::mm::io_mem_ranges_free();
    klog_info!("USB-TEST: io-mem ranges free {}", free);
    assert_test!(
        free >= IO_MEM_HEADROOM,
        "the dynamic MMIO registry keeps its headroom"
    );
    pass!()
}

pub fn test_usb_2_every_device_enumerates() -> TestResult {
    let enumerated = wait(SETTLE_MS, || {
        crate::usb::settled() && every_controller(complete)
    });
    assert_test!(
        enumerated,
        "every device, the hub's two included, must enumerate"
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
        HID_BINDS.load(Ordering::Acquire),
        2 * HIDS,
        "every keyboard and tablet bound"
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
        2 * (STICKS + HIDS),
        "one claim per bound function"
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
    let full = xhci::pages_held() as u32;
    if round == 1 {
        PAGES_FULL.store(full, Ordering::Release);
    }
    klog_info!("USB-TEST: pull");
    let gone = wait(HOST_MS, || {
        every_controller(empty)
            && crate::usb::settled()
            && UNBINDS.load(Ordering::Acquire) == unbinds + 2 * STICKS
    });
    assert_test!(
        gone,
        "every device must leave its slot, its drivers unbound"
    );
    assert_eq_test!(crate::usb::bus::claims_held(), 0, "no claim left");
    assert_test!(
        every_controller(|c| ROOT_PORTS.iter().all(|&p| !c.port_connected(p))),
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
    let hid_binds = HID_BINDS.load(Ordering::Acquire);
    let Some(irqs) = interrupts() else {
        return fail!("both controllers must be running");
    };
    klog_info!("USB-TEST: plug");
    let back = wait(HOST_MS, || {
        every_controller(complete)
            && crate::usb::settled()
            && BINDS.load(Ordering::Acquire) == binds + 2 * STICKS
            && HID_BINDS.load(Ordering::Acquire) == hid_binds + 2 * HIDS
    });
    assert_test!(back, "every device must enumerate again and bind");
    assert_eq_test!(
        crate::usb::bus::claims_held(),
        2 * (STICKS + HIDS),
        "one claim per bound function"
    );
    assert_test!(
        interrupted_since(irqs),
        "each controller interrupts for its plugs"
    );
    pass!()
}

pub fn test_usb_3_pulled_devices_leave() -> TestResult {
    pulled(1)
}

pub fn test_usb_4_plugged_devices_return() -> TestResult {
    plugged()
}

pub fn test_usb_5_pulled_again() -> TestResult {
    pulled(2)
}

pub fn test_usb_6_plugged_again() -> TestResult {
    plugged()
}

pub fn test_usb_7_shutdown_resets() -> TestResult {
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

slopos_testing::stest!(
    name = test_usb_1_controllers_run,
    flags = slopos_testing::FLAG_EXPLICIT | slopos_testing::FLAG_UNCAPTURED
);
slopos_testing::stest!(
    name = test_usb_2_every_device_enumerates,
    flags = slopos_testing::FLAG_EXPLICIT | slopos_testing::FLAG_UNCAPTURED
);
slopos_testing::stest!(
    name = test_usb_3_pulled_devices_leave,
    flags = slopos_testing::FLAG_EXPLICIT | slopos_testing::FLAG_UNCAPTURED
);
slopos_testing::stest!(
    name = test_usb_4_plugged_devices_return,
    flags = slopos_testing::FLAG_EXPLICIT | slopos_testing::FLAG_UNCAPTURED
);
slopos_testing::stest!(
    name = test_usb_5_pulled_again,
    flags = slopos_testing::FLAG_EXPLICIT | slopos_testing::FLAG_UNCAPTURED
);
slopos_testing::stest!(
    name = test_usb_6_plugged_again,
    flags = slopos_testing::FLAG_EXPLICIT | slopos_testing::FLAG_UNCAPTURED
);
slopos_testing::stest!(
    name = test_usb_7_shutdown_resets,
    flags = slopos_testing::FLAG_EXPLICIT | slopos_testing::FLAG_UNCAPTURED
);
