//! The USB listing on the diagnostic console. Each device is read through its
//! own locks, one at a time, and none is held across the output.

use core::fmt;

use slopos_ostd::kconsole::{KCMD_INFORMATIONAL, KConsole};
use slopos_ostd::kline;

use super::xhci::device::{BindState, Device};
use super::xhci::{self, Controller};

slopos_ostd::kcommand! {
    name = usb,
    key = b'u',
    help = "USB controllers, ports, devices, drivers and endpoint queues",
    flags = KCMD_INFORMATIONAL,
    run = run_usb,
}

fn run_usb(kc: &mut KConsole<'_>) {
    let mut any = false;
    for controller in xhci::published() {
        any = true;
        print_controller(kc, controller);
    }
    if !any {
        kline!(kc, "usb: no controller");
    }
}

fn print_controller(kc: &mut KConsole<'_>, c: &Controller) {
    let (bus, device, function) = c.bdf();
    let (vendor, product) = c.ids();
    kline!(
        kc,
        "usb: xhci {} at {:02x}:{:02x}.{} ({:04x}:{:04x}) {} on {}, {} ports, {}",
        c.number(),
        bus,
        device,
        function,
        vendor,
        product,
        c.state_name(),
        c.interrupt(),
        c.max_ports(),
        if c.is_settled() {
            "settled"
        } else {
            "settling"
        }
    );
    for port in 1..=c.max_ports() {
        if kc.budget_left() == 0 {
            return;
        }
        if c.port_connected(port) {
            kline!(kc, "usb:   port {}-{} connected", c.number(), port);
        }
    }
    for device in c.devices().iter() {
        if kc.budget_left() == 0 {
            return;
        }
        print_device(kc, device);
    }
}

fn print_device(kc: &mut KConsole<'_>, d: &Device) {
    let Some(node) = d.node() else {
        kline!(kc, "usb:   slot {} enumerating", d.slot);
        return;
    };
    kline!(
        kc,
        "usb:   {}-{} slot {} {:04x}:{:04x} {}, {}",
        d.controller,
        node.path,
        d.slot,
        node.vendor,
        node.product,
        node.speed.name(),
        if node.hub {
            "hub"
        } else if node.configuration == 0 {
            "not configured"
        } else {
            "configured"
        }
    );
    let mut index = 0;
    while let Some(bind) = d.bind(index) {
        let f = bind.function;
        let (state, driver) = match bind.state {
            BindState::Pending => ("pending", ""),
            BindState::Bound { driver, .. } => ("bound ", driver),
            BindState::Unbound => ("no driver", ""),
            BindState::Skipped => ("skipped", ""),
            BindState::Released { driver, .. } => ("released ", driver),
        };
        kline!(
            kc,
            "usb:     function {} {} class {:02x}/{:02x}/{:02x}: {}{}",
            index,
            Interfaces(f.first_interface, f.interfaces),
            f.class,
            f.subclass,
            f.protocol,
            state,
            driver
        );
        index += 1;
    }
    let mut n = 0;
    while let Some((dci, address, queued)) = d.queue(n) {
        if dci == 1 {
            kline!(kc, "usb:     ep0 queued {}", queued);
        } else {
            kline!(
                kc,
                "usb:     ep {:#04x} dci {} queued {}",
                address,
                dci,
                queued
            );
        }
        n += 1;
    }
}

struct Interfaces(u8, u8);

impl fmt::Display for Interfaces {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self(first, count) = *self;
        match count {
            0 | 1 => write!(f, "interface {}", first),
            _ => write!(
                f,
                "interfaces {}-{}",
                first,
                first.saturating_add(count - 1)
            ),
        }
    }
}
