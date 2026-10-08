//! The simulated controller's slot commands and doorbells, recording a
//! driver's breaches of the context rules as violations.

use super::storage::Bulk;
use super::*;

const SLOT_ADDRESSED: u8 = 2;
const SLOT_CONFIGURED: u8 = 3;

/// One TD, and where the ring's consumer goes next.
struct Fetched {
    trbs: Vec<(u64, Trb)>,
    next: u64,
    cycle: bool,
}

/// Port numbers along a route string, the first hub's first.
fn hops(route: u32) -> impl Iterator<Item = u8> {
    (0..5)
        .map(move |tier| ((route >> (4 * tier)) & 0xf) as u8)
        .take_while(|&port| port != 0)
}

impl SimController {
    fn layout(&self) -> ContextLayout {
        ContextLayout::new(self.config.context_64)
    }

    fn context_at(&self, address: u64, offset: usize) -> [u32; 8] {
        let mut dw = [0; 8];
        for (i, d) in dw.iter_mut().enumerate() {
            *d = self.mem.read32(address + (offset + 4 * i) as u64);
        }
        dw
    }

    fn output(&self, slot: u8) -> Option<u64> {
        let entry = self.mem.read64(self.dcbaap + 8 * u64::from(slot));
        (entry != 0).then_some(entry)
    }

    fn write_output(&mut self, slot: u8, offset: usize, dw: &[u32; 8]) {
        let Some(output) = self.output(slot) else {
            self.violations.push("a slot used with no device context");
            return;
        };
        for (i, d) in dw.iter().enumerate() {
            self.mem.write32(output + (offset + 4 * i) as u64, *d);
        }
    }

    fn set_endpoint_state(&mut self, slot: u8, dci: u8, state: u8) {
        self.slots[usize::from(slot)].endpoints[usize::from(dci)].state = state;
        let offset = self.layout().device_endpoint(dci);
        if let Some(output) = self.output(slot) {
            let at = output + offset as u64;
            let dword = self.mem.read32(at);
            self.mem.write32(at, (dword & !0b111) | u32::from(state));
        }
    }

    fn valid_slot(&self, slot: u8) -> bool {
        slot != 0
            && usize::from(slot) < self.slots.len()
            && slot <= self.config_reg as u8
            && self.slots[usize::from(slot)].enabled
    }

    pub(super) fn execute(&mut self, trb: Trb) -> (CompletionCode, u8) {
        let slot = (trb.control >> 24) as u8;
        let dci = ((trb.control >> 16) & 0x1f) as u8;
        let code = match trb.kind() {
            kind::NO_OP_COMMAND => CompletionCode::SUCCESS,
            kind::ENABLE_SLOT => return self.enable_slot(),
            _ if !self.valid_slot(slot) => CompletionCode::SLOT_NOT_ENABLED,
            kind::DISABLE_SLOT => {
                self.slots[usize::from(slot)] = SimSlot::default();
                self.disabled.push(slot);
                CompletionCode::SUCCESS
            }
            kind::ADDRESS_DEVICE => self.address_device(slot, trb.parameter & !0xf),
            kind::EVALUATE_CONTEXT => self.evaluate(slot, trb.parameter & !0xf),
            kind::CONFIGURE_ENDPOINT => {
                self.configure(slot, trb.parameter & !0xf, trb.control & 1 << 9 != 0)
            }
            kind::RESET_ENDPOINT => {
                let code = self.endpoint_command(slot, dci, |state| {
                    (state == endpoint_state::HALTED).then_some(endpoint_state::STOPPED)
                });
                if code == CompletionCode::SUCCESS && trb.control & 1 << 9 == 0 {
                    self.slots[usize::from(slot)].endpoints[usize::from(dci)].toggle = false;
                }
                code
            }
            kind::STOP_ENDPOINT => {
                let pending = self.slots[usize::from(slot)].endpoints[usize::from(dci)];
                let code = self.endpoint_command(slot, dci, |state| {
                    (state == endpoint_state::RUNNING).then_some(endpoint_state::STOPPED)
                });
                if code == CompletionCode::SUCCESS && pending.pending {
                    self.slots[usize::from(slot)].endpoints[usize::from(dci)].pending = false;
                    self.transfer_event(pending.dequeue, CompletionCode::STOPPED, 0, slot, dci);
                }
                code
            }
            kind::SET_TR_DEQUEUE => {
                let state = self.slots[usize::from(slot)].endpoints[usize::from(dci)].state;
                if state == endpoint_state::STOPPED || state == endpoint_state::ERROR {
                    let ep = &mut self.slots[usize::from(slot)].endpoints[usize::from(dci)];
                    ep.dequeue = trb.parameter & !0xf;
                    ep.cycle = trb.parameter & 1 != 0;
                    ep.pending = false;
                    CompletionCode::SUCCESS
                } else {
                    CompletionCode::CONTEXT_STATE
                }
            }
            _ => CompletionCode::TRB,
        };
        (code, slot)
    }

    fn enable_slot(&mut self) -> (CompletionCode, u8) {
        let limit = (self.config_reg as usize).min(self.slots.len() - 1);
        match (1..=limit).find(|&s| !self.slots[s].enabled) {
            Some(slot) => {
                self.slots[slot] = SimSlot {
                    enabled: true,
                    ..SimSlot::default()
                };
                (CompletionCode::SUCCESS, slot as u8)
            }
            None => (CompletionCode::NO_SLOTS, 0),
        }
    }

    fn endpoint_command(
        &mut self,
        slot: u8,
        dci: u8,
        next: impl FnOnce(u8) -> Option<u8>,
    ) -> CompletionCode {
        if dci == 0 {
            return CompletionCode::TRB;
        }
        let state = self.slots[usize::from(slot)].endpoints[usize::from(dci)].state;
        match next(state) {
            Some(state) => {
                self.set_endpoint_state(slot, dci, state);
                CompletionCode::SUCCESS
            }
            None => CompletionCode::CONTEXT_STATE,
        }
    }

    /// `None` unless every port on the way is enabled.
    pub fn device_mut(&mut self, root: u8, route: u32) -> Option<&mut SimDevice> {
        let index = usize::from(root).checked_sub(1)?;
        if self.ports.get(index)? & PORT_ENABLED == 0 {
            return None;
        }
        let mut device = self.roots.get_mut(index)?.as_mut()?;
        for port in hops(route) {
            if !device.port_enabled(port) {
                return None;
            }
            device = device.child_mut(port)?;
        }
        Some(device)
    }

    fn device_anywhere(&mut self, root: u8, route: u32) -> Option<&mut SimDevice> {
        let mut device = self
            .roots
            .get_mut(usize::from(root).checked_sub(1)?)?
            .as_mut()?;
        for port in hops(route) {
            device = device.child_mut(port)?;
        }
        Some(device)
    }

    fn device_ref(&self, root: u8, route: u32) -> Option<&SimDevice> {
        let mut device = self
            .roots
            .get(usize::from(root).checked_sub(1)?)?
            .as_ref()?;
        for port in hops(route) {
            device = device.child(port)?;
        }
        Some(device)
    }

    /// The nearest high-speed hub above a low- or full-speed device, and the
    /// port it is reached by.
    fn expected_tt(&self, root: u8, route: u32, speed: Speed) -> (u8, u8) {
        let mut tt = (0, 0);
        let Some(mut device) = self.roots[usize::from(root) - 1].as_ref() else {
            return tt;
        };
        for port in hops(route) {
            if device.speed == Speed::High {
                tt = (device.slot, port);
            }
            match device.child(port) {
                Some(child) => device = child,
                None => break,
            }
        }
        if speed < Speed::High { tt } else { (0, 0) }
    }

    /// Every hub above the device at `route` must already be a hub slot, and
    /// a SuperSpeed one must know its depth.
    fn check_parents(&mut self, root: u8, route: u32) {
        let mut problems = Vec::new();
        if let Some(mut device) = self.roots[usize::from(root) - 1].as_ref() {
            for (tier, port) in hops(route).enumerate() {
                let marked = self
                    .slots
                    .get(usize::from(device.slot))
                    .and_then(|s| s.hub_ports)
                    .is_some_and(|ports| ports >= port);
                if !marked {
                    problems.push("a child addressed before its hub's slot was marked a hub");
                }
                if let Some(hub) = &device.hub
                    && hub.super_speed
                    && hub.depth != Some(tier as u8)
                {
                    problems.push("a SuperSpeed hub's child addressed before SET_HUB_DEPTH");
                }
                match device.child(port) {
                    Some(child) => device = child,
                    None => break,
                }
            }
        }
        self.violations.extend(problems);
    }

    fn address_device(&mut self, slot: u8, input: u64) -> CompletionCode {
        let layout = self.layout();
        let control = InputControlContext::decode(&self.context_at(input, 0));
        if control.add & 0b11 != 0b11 {
            return CompletionCode::PARAMETER;
        }
        let context = SlotContext::decode(&self.context_at(input, layout.input_slot()));
        let ep0 = EndpointContext::decode(&self.context_at(input, layout.input_endpoint(1)));
        let (root, route) = (context.root_hub_port, context.route_string);
        if self.output(slot).is_none() {
            self.violations
                .push("Address Device with no device context");
        }
        let Some(speed) = self.device_ref(root, route).map(|d| d.speed) else {
            return CompletionCode::TRANSACTION;
        };
        let Some(device) = self.device_mut(root, route) else {
            return CompletionCode::TRANSACTION;
        };
        if device.faults.refuse_address > 0 {
            device.faults.refuse_address -= 1;
            return CompletionCode::TRANSACTION;
        }
        if context.speed != speed.default_psiv() {
            self.violations
                .push("slot context speed is not the device's");
        }
        if (context.tt_hub_slot, context.tt_port) != self.expected_tt(root, route, speed) {
            self.violations.push("slot context TT fields wrong");
        }
        if context.multi_tt {
            self.violations
                .push("MTT set with no multi-TT interface enabled");
        }
        let expected_ep0 = match speed {
            Speed::Low | Speed::Full => 8,
            Speed::High => 64,
            _ => 512,
        };
        if ep0.max_packet_size != expected_ep0 || ep0.kind != endpoint_type::CONTROL {
            self.violations
                .push("EP0 context wrong for the device's speed");
        }
        self.check_parents(root, route);
        let device = self.device_mut(root, route).expect("found above");
        device.address = slot;
        device.slot = slot;
        device.default_state = false;
        let entry = &mut self.slots[usize::from(slot)];
        entry.device = Some((root, route));
        entry.context = context;
        entry.endpoints[1] = SimEndpoint {
            state: endpoint_state::RUNNING,
            dequeue: ep0.dequeue,
            cycle: ep0.dequeue_cycle,
            kind: endpoint_type::CONTROL,
            max_packet: ep0.max_packet_size,
            pending: false,
            toggle: false,
        };
        let mut out = context;
        out.state = SLOT_ADDRESSED;
        out.address = slot;
        self.write_output(slot, 0, &out.encode());
        let mut ep0_out = ep0;
        ep0_out.state = endpoint_state::RUNNING;
        self.write_output(slot, layout.device_endpoint(1), &ep0_out.encode());
        CompletionCode::SUCCESS
    }

    fn evaluate(&mut self, slot: u8, input: u64) -> CompletionCode {
        let layout = self.layout();
        let control = InputControlContext::decode(&self.context_at(input, 0));
        if control.drop != 0 || control.add & !0b11 != 0 {
            return CompletionCode::TRB;
        }
        if control.add & 0b10 != 0 {
            let ep0 = EndpointContext::decode(&self.context_at(input, layout.input_endpoint(1)));
            self.slots[usize::from(slot)].endpoints[1].max_packet = ep0.max_packet_size;
        }
        CompletionCode::SUCCESS
    }

    fn configure(&mut self, slot: u8, input: u64, deconfigure: bool) -> CompletionCode {
        let layout = self.layout();
        if deconfigure {
            for dci in 2..32 {
                self.slots[usize::from(slot)].endpoints[dci] = SimEndpoint::default();
            }
            return CompletionCode::SUCCESS;
        }
        let control = InputControlContext::decode(&self.context_at(input, 0));
        if control.add & 1 == 0 || control.add & 0b10 != 0 || control.drop & 0b11 != 0 {
            return CompletionCode::TRB;
        }
        let context = SlotContext::decode(&self.context_at(input, layout.input_slot()));
        let added = control.add & !0b11;
        let last = 31 - added.leading_zeros().min(31);
        if added != 0 && u32::from(context.context_entries) < last {
            self.violations
                .push("Context Entries short of the last endpoint added");
        }
        let dropped = control.drop & !0b11;
        for dci in 2..32usize {
            if dropped & 1 << dci != 0 {
                self.slots[usize::from(slot)].endpoints[dci] = SimEndpoint::default();
            }
        }
        let in_use = self.slots[usize::from(slot)].endpoints[2..]
            .iter()
            .rposition(|e| e.state != endpoint_state::DISABLED)
            .map_or(1, |i| i + 2);
        if added == 0 && usize::from(context.context_entries) < in_use {
            self.violations
                .push("Context Entries short of an endpoint in use");
        }
        for dci in 2..32u8 {
            if added & 1 << dci == 0 {
                continue;
            }
            let ep = EndpointContext::decode(&self.context_at(input, layout.input_endpoint(dci)));
            if !self.mem.is_page(ep.dequeue & !(PAGE_SIZE as u64 - 1)) || ep.kind == 0 {
                return CompletionCode::PARAMETER;
            }
            self.slots[usize::from(slot)].endpoints[usize::from(dci)] = SimEndpoint {
                state: endpoint_state::RUNNING,
                dequeue: ep.dequeue,
                cycle: ep.dequeue_cycle,
                kind: ep.kind,
                max_packet: ep.max_packet_size,
                pending: false,
                toggle: false,
            };
            let mut out = ep;
            out.state = endpoint_state::RUNNING;
            self.write_output(slot, layout.device_endpoint(dci), &out.encode());
        }
        let entry = &mut self.slots[usize::from(slot)];
        entry.hub_ports = context.hub.then_some(context.ports);
        entry.context.hub = context.hub;
        entry.context.ports = context.ports;
        entry.context.tt_think_time = context.tt_think_time;
        if context.route_string != entry.context.route_string
            || context.tt_hub_slot != entry.context.tt_hub_slot
        {
            self.violations
                .push("Configure Endpoint changed the slot's route or TT");
        }
        if context.hub && context.ports == 0 {
            self.violations.push("a hub slot with no ports");
        }
        let mut out = context;
        out.state = SLOT_CONFIGURED;
        out.address = slot;
        self.write_output(slot, 0, &out.encode());
        CompletionCode::SUCCESS
    }

    /// Two USB 2 devices in the default state would both answer address 0.
    pub(super) fn enter_default_state(&mut self, root: u8, route: u32) {
        let mut others = 0;
        for device in self.roots.iter().flatten() {
            device.walk(&mut |d| {
                if d.default_state && !d.speed.is_super() {
                    others += 1;
                }
            });
        }
        let Some(device) = self.device_anywhere(root, route) else {
            return;
        };
        if device.speed.is_super() {
            return;
        }
        let already = device.default_state;
        device.default_state = true;
        device.address = 0;
        if others > usize::from(already) {
            self.violations
                .push("two devices in the default state at once");
        }
    }

    pub(super) fn ring_slot(&mut self, slot: u8, dci: u8) {
        if !self.valid_slot(slot) || !(1..32).contains(&dci) {
            self.violations.push("a doorbell rung for no endpoint");
            return;
        }
        self.run_endpoint(slot, dci);
    }

    fn transfer_event(&mut self, trb: u64, code: CompletionCode, residual: u32, slot: u8, dci: u8) {
        self.post_event(Trb {
            parameter: trb,
            status: u32::from(code.0) << 24 | residual & 0xff_ffff,
            control: u32::from(kind::TRANSFER_EVENT) << 10
                | u32::from(dci) << 16
                | u32::from(slot) << 24,
        });
    }

    fn read_trb(&self, address: u64) -> Trb {
        Trb {
            parameter: self.mem.read64(address),
            status: self.mem.read32(address + 8),
            control: self.mem.read32(address + 12),
        }
    }

    /// A control transfer up to its Status TRB, or Normal TRBs up to one
    /// that does not chain.
    fn fetch(&mut self, ep: SimEndpoint, control: bool) -> Option<Fetched> {
        let (mut at, mut cycle) = (ep.dequeue, ep.cycle);
        let mut trbs = Vec::new();
        for _ in 0..64 {
            let trb = self.read_trb(at);
            if trb.cycle() != cycle {
                return None;
            }
            if trb.kind() == kind::LINK {
                at = trb.parameter & !0xf;
                if trb.control & 1 << 1 != 0 {
                    cycle = !cycle;
                }
                continue;
            }
            trbs.push((at, trb));
            at += 16;
            let done = if control {
                trb.kind() == kind::STATUS
            } else {
                !trb.chains()
            };
            if done {
                return Some(Fetched {
                    trbs,
                    next: at,
                    cycle,
                });
            }
        }
        self.violations.push("a TD with no end");
        None
    }

    fn run_endpoint(&mut self, slot: u8, dci: u8) {
        loop {
            if self.halted {
                return;
            }
            let ep = self.slots[usize::from(slot)].endpoints[usize::from(dci)];
            match ep.state {
                endpoint_state::RUNNING => {}
                endpoint_state::STOPPED => {
                    self.set_endpoint_state(slot, dci, endpoint_state::RUNNING)
                }
                _ => return,
            }
            let control = dci == 1;
            let Some(td) = self.fetch(ep, control) else {
                return;
            };
            let advance = if control {
                self.run_control(slot, &td.trbs)
            } else if self.is_disk(slot) {
                self.run_bulk(slot, dci, &td.trbs)
            } else {
                self.run_normal(slot, dci, &td.trbs)
            };
            let ep = &mut self.slots[usize::from(slot)].endpoints[usize::from(dci)];
            match advance {
                Some(true) => {
                    ep.dequeue = td.next;
                    ep.cycle = td.cycle;
                    ep.pending = false;
                }
                Some(false) => {
                    ep.pending = true;
                    return;
                }
                None => {
                    self.set_endpoint_state(slot, dci, endpoint_state::HALTED);
                    return;
                }
            }
        }
    }

    fn located(&self, slot: u8) -> Option<(u8, u32)> {
        self.slots[usize::from(slot)].device
    }

    /// One control transfer; `None` if it halted the endpoint.
    fn run_control(&mut self, slot: u8, trbs: &[(u64, Trb)]) -> Option<bool> {
        let (setup_at, setup_trb) = trbs[0];
        if setup_trb.kind() != kind::SETUP {
            self.transfer_event(setup_at, CompletionCode::TRB, 0, slot, 1);
            return None;
        }
        let b = setup_trb.parameter.to_le_bytes();
        let setup = Setup {
            request_type: b[0],
            request: b[1],
            value: u16::from_le_bytes([b[2], b[3]]),
            index: u16::from_le_bytes([b[4], b[5]]),
            length: u16::from_le_bytes([b[6], b[7]]),
        };
        let data = trbs.iter().find(|(_, t)| t.kind() == kind::DATA).copied();
        let (status_at, _) = *trbs.last().expect("a status stage");
        let location = self.located(slot);
        if setup.request_type == 0 && setup.request == 9 && setup.value != 0 {
            let configured = self.slots[usize::from(slot)].endpoints[2..]
                .iter()
                .any(|e| e.state != endpoint_state::DISABLED);
            let needs = location
                .and_then(|(root, route)| self.device_ref(root, route))
                .is_some_and(|d| d.configurations.iter().any(|c| c.len() > 9 + 9));
            if needs && !configured {
                self.violations
                    .push("SET_CONFIGURATION before Configure Endpoint");
            }
        }
        let mut stranger = false;
        let reply = match location.and_then(|(root, route)| self.device_mut(root, route)) {
            None => Reply::Silent,
            Some(device) => {
                stranger = device.address != slot;
                device.control(setup)
            }
        };
        if stranger {
            self.violations
                .push("a request to a device not addressed in its slot");
        }
        let (root, route) = location.unwrap_or((0, 0));
        let pulled = self.device_ref(root, route).is_some_and(|d| d.pull_me);
        if pulled {
            self.pull(root, route);
        }
        let hub_reset = setup.request_type == 0x23 && setup.request == 3 && setup.value == 4;
        if hub_reset && matches!(reply, Reply::Data(_)) {
            let depth = hops(route).count();
            let child = route | u32::from(setup.index as u8 & 0xf) << (4 * depth);
            self.enter_default_state(root, child);
        }
        let fail_at = data.map_or(status_at, |(at, _)| at);
        match reply {
            Reply::Stall => {
                self.transfer_event(fail_at, CompletionCode::STALL, 0, slot, 1);
                None
            }
            Reply::Babble => {
                self.transfer_event(fail_at, CompletionCode::BABBLE, 0, slot, 1);
                None
            }
            Reply::Silent => {
                self.transfer_event(setup_at, CompletionCode::TRANSACTION, 0, slot, 1);
                None
            }
            Reply::Data(bytes) => {
                if let Some((data_at, data_trb)) = data {
                    let length = data_trb.status & 0x1_ffff;
                    let sent = (bytes.len() as u32).min(length);
                    if data_trb.control & 1 << 16 != 0 {
                        self.mem
                            .write_bytes(data_trb.parameter, &bytes[..sent as usize]);
                    }
                    if sent < length && data_trb.control & 1 << 2 != 0 {
                        self.transfer_event(
                            data_at,
                            CompletionCode::SHORT_PACKET,
                            length - sent,
                            slot,
                            1,
                        );
                    }
                }
                self.transfer_event(status_at, CompletionCode::SUCCESS, 0, slot, 1);
                self.kick();
                Some(true)
            }
        }
    }

    /// A hub's status-change report, or a NAK; `None` if it halted the
    /// endpoint.
    fn run_normal(&mut self, slot: u8, dci: u8, trbs: &[(u64, Trb)]) -> Option<bool> {
        let (at, trb) = trbs[0];
        let Some((root, route)) = self.located(slot) else {
            self.transfer_event(at, CompletionCode::TRANSACTION, 0, slot, dci);
            return None;
        };
        let Some(device) = self.device_mut(root, route) else {
            self.transfer_event(at, CompletionCode::TRANSACTION, 0, slot, dci);
            return None;
        };
        if device.halted_endpoints & 1 << dci != 0 {
            self.transfer_event(at, CompletionCode::STALL, 0, slot, dci);
            return None;
        }
        if dci != 3 || device.hub.is_none() {
            return Some(false);
        }
        let Some(report) = device.status_report() else {
            return Some(false);
        };
        let length = trb.status & 0x1_ffff;
        let sent = (report.len() as u32).min(length);
        self.mem
            .write_bytes(trb.parameter, &report[..sent as usize]);
        let code = if sent < length {
            CompletionCode::SHORT_PACKET
        } else {
            CompletionCode::SUCCESS
        };
        self.transfer_event(at, code, length - sent, slot, dci);
        Some(true)
    }

    fn is_disk(&self, slot: u8) -> bool {
        self.located(slot)
            .and_then(|(root, route)| self.device_ref(root, route))
            .is_some_and(|d| d.storage.is_some())
    }

    /// One TD on a disk's bulk pipe, its toggles held to the device's; `None`
    /// if it halted the endpoint.
    fn run_bulk(&mut self, slot: u8, dci: u8, trbs: &[(u64, Trb)]) -> Option<bool> {
        let (first, _) = trbs[0];
        let lengths: Vec<u32> = trbs.iter().map(|(_, t)| t.status & 0x1_ffff).collect();
        let total: u32 = lengths.iter().sum();
        let direction_in = dci % 2 == 1;
        let mut out = Vec::new();
        if !direction_in {
            for ((_, trb), &length) in trbs.iter().zip(&lengths) {
                let mut bytes = vec![0u8; length as usize];
                self.mem.read_bytes(trb.parameter, &mut bytes);
                out.extend_from_slice(&bytes);
            }
        }
        let endpoint = self.slots[usize::from(slot)].endpoints[usize::from(dci)];
        let now = self.now_us;
        let located = self.located(slot);
        let Some(device) = located.and_then(|(root, route)| self.device_mut(root, route)) else {
            self.transfer_event(first, CompletionCode::TRANSACTION, 0, slot, dci);
            return None;
        };
        if device.halted_endpoints & 1 << dci != 0 {
            self.transfer_event(first, CompletionCode::STALL, 0, slot, dci);
            return None;
        }
        if (device.toggles & 1 << dci != 0) != endpoint.toggle {
            self.violations.push("a data toggle out of step");
            self.transfer_event(first, CompletionCode::TRANSACTION, 0, slot, dci);
            return None;
        }
        let mut halted = device.halted_endpoints;
        let storage = device.storage.as_mut().expect("a disk");
        let reply = if direction_in {
            storage.bulk_in(total, now, &mut halted)
        } else {
            storage.bulk_out(&out, &mut halted)
        };
        device.halted_endpoints = halted;
        let moved = match reply {
            Bulk::Nak => return Some(false),
            Bulk::Stall => {
                self.transfer_event(first, CompletionCode::STALL, 0, slot, dci);
                return None;
            }
            Bulk::Moved(bytes) if direction_in => bytes,
            Bulk::Moved(_) => out,
        };
        let packet = usize::from(endpoint.max_packet.max(1));
        let short = direction_in && moved.len() < total as usize;
        let mut packets = moved.len().div_ceil(packet).max(1);
        if short && !moved.is_empty() && moved.len().is_multiple_of(packet) {
            packets += 1;
        }
        if packets % 2 == 1 {
            device.toggles ^= 1 << dci;
            self.slots[usize::from(slot)].endpoints[usize::from(dci)].toggle ^= true;
        }
        let mut at = 0usize;
        for (i, ((address, trb), &length)) in trbs.iter().zip(&lengths).enumerate() {
            let take = (moved.len() - at.min(moved.len())).min(length as usize);
            if direction_in {
                self.mem.write_bytes(trb.parameter, &moved[at..at + take]);
            }
            at += take;
            let last = i + 1 == trbs.len();
            if short && take < length as usize {
                let residual = length - take as u32;
                self.transfer_event(*address, CompletionCode::SHORT_PACKET, residual, slot, dci);
                if self.config.second_short_event && !last {
                    let (end, _) = trbs[trbs.len() - 1];
                    self.transfer_event(end, CompletionCode::SHORT_PACKET, residual, slot, dci);
                }
                break;
            }
            if last {
                self.transfer_event(*address, CompletionCode::SUCCESS, 0, slot, dci);
            }
        }
        self.kick();
        Some(true)
    }

    /// Runs every transfer a NAK left waiting.
    pub fn kick(&mut self) {
        for slot in 1..self.slots.len() {
            for dci in 2..32 {
                if self.slots[slot].endpoints[dci].pending {
                    self.slots[slot].endpoints[dci].pending = false;
                    self.run_endpoint(slot as u8, dci as u8);
                }
            }
        }
    }

    fn pull(&mut self, root: u8, route: u32) {
        let mut ports: Vec<u8> = hops(route).collect();
        let Some(last) = ports.pop() else {
            self.detach(root);
            return;
        };
        let parent_route = ports
            .iter()
            .enumerate()
            .fold(0, |r, (tier, &p)| r | u32::from(p) << (4 * tier));
        let mut parent = self.roots[usize::from(root) - 1].as_mut();
        for port in hops(parent_route) {
            parent = parent.and_then(|d| d.child_mut(port));
        }
        if let Some(parent) = parent {
            parent.unplug(last);
        }
    }

    pub fn plug_at(&mut self, root: u8, hubs: &[u8], port: u8, device: SimDevice) {
        let mut hub = self.roots[usize::from(root) - 1]
            .as_mut()
            .expect("a root device");
        for &p in hubs {
            hub = hub.child_mut(p).expect("a hub on the way");
        }
        hub.plug(port, device);
        self.kick();
    }

    pub fn unplug_at(&mut self, root: u8, hubs: &[u8], port: u8) -> Option<SimDevice> {
        let mut hub = self.roots[usize::from(root) - 1].as_mut()?;
        for &p in hubs {
            hub = hub.child_mut(p)?;
        }
        let device = hub.unplug(port);
        self.kick();
        device
    }

    pub fn device_at(&self, root: u8, hubs: &[u8]) -> Option<&SimDevice> {
        let route = hubs
            .iter()
            .enumerate()
            .fold(0, |r, (tier, &p)| r | u32::from(p) << (4 * tier));
        self.device_ref(root, route)
    }

    pub fn enabled_slots(&self) -> usize {
        self.slots.iter().filter(|s| s.enabled).count()
    }
}
