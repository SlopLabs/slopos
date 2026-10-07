//! The guest half of `just test-usb`: both of QEMU's xHCI models running,
//! each root port with a stick on it seen to detach and attach again as
//! `scripts/test_usb.py` pulls and plugs, and each controller reset by its
//! shutdown hook. The host acts on the `USB-TEST:` lines.

use slopos_ostd::{KArc, klog_info};
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::driver_core::shutdown::DeviceShutdown;
use crate::usb::xhci::{self, Controller};

const QEMU_XHCI: (u16, u16) = (0x1b36, 0x000d);
const NEC_XHCI: (u16, u16) = (0x1033, 0x0194);
/// Where `scripts/test_usb.py` plugs a stick on each controller, which it
/// runs with two USB 3 and four USB 2 ports: two SuperSpeed sticks on ports
/// 1 and 2, two high-speed ones on ports 5 and 6.
const PORTS: [u8; 4] = [1, 2, 5, 6];
const SETTLE_MS: u32 = 10_000;
/// The host acts on a marker within this long, however slow the emulator.
const HOST_MS: u32 = 60_000;
/// Free dynamic MMIO ranges a boot with both controllers must keep.
const IO_MEM_HEADROOM: usize = 8;

fn controllers() -> Option<[KArc<Controller>; 2]> {
    let find = |ids| {
        (1..=xhci::MAX_CONTROLLERS as u8)
            .filter_map(xhci::controller)
            .find(|c| c.ids() == ids)
    };
    Some([find(QEMU_XHCI)?, find(NEC_XHCI)?])
}

fn every_port(test: impl Fn(&Controller, u8) -> bool) -> bool {
    controllers().is_some_and(|all| all.iter().all(|c| PORTS.iter().all(|&port| test(c, port))))
}

fn wait(ms: u32, condition: impl Fn() -> bool) -> bool {
    crate::hpet::poll_wait(&condition, ms)
}

pub fn test_usb_1_controllers_run() -> TestResult {
    let settled = wait(SETTLE_MS, || {
        every_port(|c, port| c.is_running() && c.port_connected(port))
    });
    let Some([qemu, nec]) = controllers() else {
        return fail!("both QEMU xHCI models must have bound");
    };
    assert_test!(settled, "every port with a stick must show it connected");
    assert_eq_test!(qemu.interrupt(), "MSI-X", "qemu-xhci takes MSI-X");
    assert_eq_test!(
        nec.interrupt(),
        "MSI",
        "nec-usb-xhci with msix=off takes MSI"
    );
    for c in [&qemu, &nec] {
        assert_eq_test!(c.max_ports(), 6, "two USB 3 ports and four USB 2");
        for port in [3, 4] {
            assert_test!(
                !c.port_connected(port),
                "no stick on the USB 2 half of a USB 3 port"
            );
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

fn counts(read: impl Fn(&Controller, u8) -> u32) -> Option<[[u32; 4]; 2]> {
    let all = controllers()?;
    let mut out = [[0; 4]; 2];
    for (c, row) in all.iter().zip(out.iter_mut()) {
        for (count, &port) in row.iter_mut().zip(PORTS.iter()) {
            *count = read(c, port);
        }
    }
    Some(out)
}

fn moved_on(before: [[u32; 4]; 2], read: fn(&Controller, u8) -> u32) -> bool {
    counts(read).is_some_and(|now| {
        now.iter()
            .flatten()
            .zip(before.iter().flatten())
            .all(|(now, before)| now > before)
    })
}

fn interrupts() -> Option<[u32; 2]> {
    controllers().map(|all| all.map(|c| c.interrupts_taken()))
}

/// Port events reach the thread by interrupt, not only by its once-a-second
/// drain, which would hide a drain that left the interrupter unarmed.
fn interrupted_since(before: [u32; 2]) -> bool {
    interrupts().is_some_and(|now| now.iter().zip(before).all(|(now, before)| *now > before))
}

pub fn test_usb_2_pulled_ports_detach() -> TestResult {
    let (Some(before), Some(irqs)) = (counts(Controller::port_detaches), interrupts()) else {
        return fail!("both controllers must be running");
    };
    klog_info!("USB-TEST: pull");
    let detached = wait(HOST_MS, || {
        moved_on(before, Controller::port_detaches) && every_port(|c, p| !c.port_connected(p))
    });
    assert_test!(detached, "every pulled stick's port must log a detach");
    assert_test!(
        interrupted_since(irqs),
        "each controller interrupts for its pulls"
    );
    pass!()
}

pub fn test_usb_3_plugged_ports_attach() -> TestResult {
    let (Some(before), Some(irqs)) = (counts(Controller::port_attaches), interrupts()) else {
        return fail!("both controllers must be running");
    };
    klog_info!("USB-TEST: plug");
    let attached = wait(HOST_MS, || {
        moved_on(before, Controller::port_attaches) && every_port(Controller::port_connected)
    });
    assert_test!(attached, "every plugged stick's port must log an attach");
    assert_test!(
        interrupted_since(irqs),
        "each controller interrupts for its plugs"
    );
    pass!()
}

pub fn test_usb_4_shutdown_resets() -> TestResult {
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
    name = test_usb_2_pulled_ports_detach,
    flags = slopos_testing::FLAG_EXPLICIT | slopos_testing::FLAG_UNCAPTURED
);
slopos_testing::stest!(
    name = test_usb_3_plugged_ports_attach,
    flags = slopos_testing::FLAG_EXPLICIT | slopos_testing::FLAG_UNCAPTURED
);
slopos_testing::stest!(
    name = test_usb_4_shutdown_resets,
    flags = slopos_testing::FLAG_EXPLICIT | slopos_testing::FLAG_UNCAPTURED
);
