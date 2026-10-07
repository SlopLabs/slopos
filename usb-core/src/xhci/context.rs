//! Device and input contexts (§6.2) in either size HCCPARAMS1.CSZ selects: a
//! context is 32 bytes, or 64 of which the first 32 carry fields.

/// Every context carries eight dwords of fields.
pub type Dwords = [u32; 8];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextLayout {
    bytes: usize,
}

/// Device Context Index: endpoint 0 is 1, and endpoint `n` is `2n` out and
/// `2n + 1` in, up to 31.
pub fn dci(endpoint: u8, direction_in: bool) -> u8 {
    if endpoint == 0 {
        1
    } else {
        (endpoint & 0xf) * 2 + u8::from(direction_in)
    }
}

impl ContextLayout {
    pub fn new(context_64: bool) -> Self {
        Self {
            bytes: if context_64 { 64 } else { 32 },
        }
    }

    pub fn context_bytes(&self) -> usize {
        self.bytes
    }

    /// A device context: the slot context and 31 endpoint contexts.
    pub fn device_bytes(&self) -> usize {
        32 * self.bytes
    }

    /// An input context: the input control context ahead of a device
    /// context.
    pub fn input_bytes(&self) -> usize {
        33 * self.bytes
    }

    pub fn device_endpoint(&self, dci: u8) -> usize {
        usize::from(dci.clamp(1, 31)) * self.bytes
    }

    pub fn input_slot(&self) -> usize {
        self.bytes
    }

    pub fn input_endpoint(&self, dci: u8) -> usize {
        self.bytes + self.device_endpoint(dci)
    }
}

fn field(dword: u32, shift: u32, width: u32) -> u32 {
    (dword >> shift) & ((1 << width) - 1)
}

fn put(value: u32, shift: u32, width: u32) -> u32 {
    (value & ((1 << width) - 1)) << shift
}

/// The slot context (§6.2.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SlotContext {
    pub route_string: u32,
    pub speed: u8,
    pub multi_tt: bool,
    pub hub: bool,
    pub context_entries: u8,
    pub max_exit_latency: u16,
    pub root_hub_port: u8,
    pub ports: u8,
    pub tt_hub_slot: u8,
    pub tt_port: u8,
    pub tt_think_time: u8,
    pub interrupter: u16,
    pub address: u8,
    pub state: u8,
}

impl SlotContext {
    pub fn encode(&self) -> Dwords {
        let mut dw = [0; 8];
        dw[0] = put(self.route_string, 0, 20)
            | put(self.speed.into(), 20, 4)
            | put(self.multi_tt.into(), 25, 1)
            | put(self.hub.into(), 26, 1)
            | put(self.context_entries.into(), 27, 5);
        dw[1] = put(self.max_exit_latency.into(), 0, 16)
            | put(self.root_hub_port.into(), 16, 8)
            | put(self.ports.into(), 24, 8);
        dw[2] = put(self.tt_hub_slot.into(), 0, 8)
            | put(self.tt_port.into(), 8, 8)
            | put(self.tt_think_time.into(), 16, 2)
            | put(self.interrupter.into(), 22, 10);
        dw[3] = put(self.address.into(), 0, 8) | put(self.state.into(), 27, 5);
        dw
    }

    pub fn decode(dw: &Dwords) -> Self {
        Self {
            route_string: field(dw[0], 0, 20),
            speed: field(dw[0], 20, 4) as u8,
            multi_tt: field(dw[0], 25, 1) != 0,
            hub: field(dw[0], 26, 1) != 0,
            context_entries: field(dw[0], 27, 5) as u8,
            max_exit_latency: field(dw[1], 0, 16) as u16,
            root_hub_port: field(dw[1], 16, 8) as u8,
            ports: field(dw[1], 24, 8) as u8,
            tt_hub_slot: field(dw[2], 0, 8) as u8,
            tt_port: field(dw[2], 8, 8) as u8,
            tt_think_time: field(dw[2], 16, 2) as u8,
            interrupter: field(dw[2], 22, 10) as u16,
            address: field(dw[3], 0, 8) as u8,
            state: field(dw[3], 27, 5) as u8,
        }
    }
}

/// Endpoint types (Table 6-9).
pub mod endpoint_type {
    pub const ISOCH_OUT: u8 = 1;
    pub const BULK_OUT: u8 = 2;
    pub const INTERRUPT_OUT: u8 = 3;
    pub const CONTROL: u8 = 4;
    pub const ISOCH_IN: u8 = 5;
    pub const BULK_IN: u8 = 6;
    pub const INTERRUPT_IN: u8 = 7;
}

/// The endpoint context (§6.2.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EndpointContext {
    pub state: u8,
    pub mult: u8,
    pub max_primary_streams: u8,
    pub linear_stream_array: bool,
    pub interval: u8,
    pub max_esit_payload: u32,
    pub error_count: u8,
    pub kind: u8,
    pub host_initiate_disable: bool,
    pub max_burst: u8,
    pub max_packet_size: u16,
    /// The TR Dequeue Pointer, 16-byte aligned.
    pub dequeue: u64,
    pub dequeue_cycle: bool,
    pub average_trb_length: u16,
}

impl EndpointContext {
    pub fn encode(&self) -> Dwords {
        let mut dw = [0; 8];
        dw[0] = put(self.state.into(), 0, 3)
            | put(self.mult.into(), 8, 2)
            | put(self.max_primary_streams.into(), 10, 5)
            | put(self.linear_stream_array.into(), 15, 1)
            | put(self.interval.into(), 16, 8)
            | put(self.max_esit_payload >> 16, 24, 8);
        dw[1] = put(self.error_count.into(), 1, 2)
            | put(self.kind.into(), 3, 3)
            | put(self.host_initiate_disable.into(), 7, 1)
            | put(self.max_burst.into(), 8, 8)
            | put(self.max_packet_size.into(), 16, 16);
        let dequeue = (self.dequeue & !0xf) | u64::from(self.dequeue_cycle);
        dw[2] = dequeue as u32;
        dw[3] = (dequeue >> 32) as u32;
        dw[4] = put(self.average_trb_length.into(), 0, 16) | put(self.max_esit_payload, 16, 16);
        dw
    }

    pub fn decode(dw: &Dwords) -> Self {
        let dequeue = u64::from(dw[2]) | u64::from(dw[3]) << 32;
        Self {
            state: field(dw[0], 0, 3) as u8,
            mult: field(dw[0], 8, 2) as u8,
            max_primary_streams: field(dw[0], 10, 5) as u8,
            linear_stream_array: field(dw[0], 15, 1) != 0,
            interval: field(dw[0], 16, 8) as u8,
            max_esit_payload: field(dw[0], 24, 8) << 16 | field(dw[4], 16, 16),
            error_count: field(dw[1], 1, 2) as u8,
            kind: field(dw[1], 3, 3) as u8,
            host_initiate_disable: field(dw[1], 7, 1) != 0,
            max_burst: field(dw[1], 8, 8) as u8,
            max_packet_size: field(dw[1], 16, 16) as u16,
            dequeue: dequeue & !0xf,
            dequeue_cycle: dequeue & 1 != 0,
            average_trb_length: field(dw[4], 0, 16) as u16,
        }
    }
}

/// The input control context (§6.2.5.1): which contexts a command drops and
/// adds, and the configuration it concerns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputControlContext {
    pub drop: u32,
    pub add: u32,
    pub configuration: u8,
    pub interface: u8,
    pub alternate: u8,
}

impl InputControlContext {
    pub fn encode(&self) -> Dwords {
        let mut dw = [0; 8];
        dw[0] = self.drop & !0b11;
        dw[1] = self.add;
        dw[7] = put(self.configuration.into(), 0, 8)
            | put(self.interface.into(), 8, 8)
            | put(self.alternate.into(), 16, 8);
        dw
    }

    pub fn decode(dw: &Dwords) -> Self {
        Self {
            drop: dw[0],
            add: dw[1],
            configuration: field(dw[7], 0, 8) as u8,
            interface: field(dw[7], 8, 8) as u8,
            alternate: field(dw[7], 16, 8) as u8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_sizes_place_contexts() {
        let small = ContextLayout::new(false);
        let large = ContextLayout::new(true);
        assert_eq!((small.device_bytes(), large.device_bytes()), (1024, 2048));
        assert_eq!((small.input_bytes(), large.input_bytes()), (1056, 2112));
        assert!(large.input_bytes() <= 4096);
        assert_eq!(small.device_endpoint(dci(0, false)), 32);
        assert_eq!(large.device_endpoint(dci(0, false)), 64);
        assert_eq!(small.input_slot(), 32);
        assert_eq!(large.input_slot(), 64);
        assert_eq!(small.input_endpoint(dci(1, true)), 32 + 3 * 32);
        assert_eq!(large.input_endpoint(dci(15, true)), 64 + 31 * 64);
        assert_eq!(large.input_endpoint(31) + 64, large.input_bytes());
    }

    #[test]
    fn device_context_index() {
        assert_eq!(dci(0, false), 1);
        assert_eq!(dci(0, true), 1);
        assert_eq!(dci(1, false), 2);
        assert_eq!(dci(1, true), 3);
        assert_eq!(dci(15, true), 31);
    }

    #[test]
    fn slot_context_round_trips_at_its_bit_positions() {
        let slot = SlotContext {
            route_string: 0x12345,
            speed: 3,
            multi_tt: true,
            hub: true,
            context_entries: 1,
            max_exit_latency: 0xbeef,
            root_hub_port: 6,
            ports: 4,
            tt_hub_slot: 9,
            tt_port: 2,
            tt_think_time: 3,
            interrupter: 0x2ff,
            address: 0x55,
            state: 2,
        };
        let dw = slot.encode();
        assert_eq!(dw[0], 0x12345 | 3 << 20 | 1 << 25 | 1 << 26 | 1 << 27);
        assert_eq!(dw[1], 0xbeef | 6 << 16 | 4 << 24);
        assert_eq!(dw[2], 9 | 2 << 8 | 3 << 16 | 0x2ff << 22);
        assert_eq!(dw[3], 0x55 | 2 << 27);
        assert_eq!(SlotContext::decode(&dw), slot);
    }

    #[test]
    fn endpoint_context_round_trips_at_its_bit_positions() {
        let ep = EndpointContext {
            state: 1,
            mult: 2,
            max_primary_streams: 5,
            linear_stream_array: true,
            interval: 7,
            max_esit_payload: 0x12_3456,
            error_count: 3,
            kind: endpoint_type::BULK_IN,
            host_initiate_disable: false,
            max_burst: 15,
            max_packet_size: 1024,
            dequeue: 0x1_2345_6780,
            dequeue_cycle: true,
            average_trb_length: 3072,
        };
        let dw = ep.encode();
        assert_eq!(dw[0], 1 | 2 << 8 | 5 << 10 | 1 << 15 | 7 << 16 | 0x12 << 24);
        assert_eq!(dw[1], 3 << 1 | 6 << 3 | 15 << 8 | 1024 << 16);
        assert_eq!((dw[2], dw[3]), (0x2345_6781, 1));
        assert_eq!(dw[4], 3072 | 0x3456 << 16);
        assert_eq!(EndpointContext::decode(&dw), ep);
    }

    #[test]
    fn input_control_never_drops_the_slot_or_ep0() {
        let ctl = InputControlContext {
            drop: 0xffff_ffff,
            add: 0b11,
            configuration: 1,
            interface: 2,
            alternate: 3,
        };
        let dw = ctl.encode();
        assert_eq!(dw[0], 0xffff_fffc);
        assert_eq!(dw[1], 3);
        assert_eq!(dw[7], 1 | 2 << 8 | 3 << 16);
    }
}
