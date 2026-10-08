//! Transfer Request Blocks (§6.4): the 16-byte entries of every ring, the
//! commands and transfers software writes and the events the controller
//! writes back.

pub const TRB_BYTES: usize = 16;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Trb {
    pub parameter: u64,
    pub status: u32,
    pub control: u32,
}

const CYCLE: u32 = 1 << 0;
const TOGGLE_CYCLE: u32 = 1 << 1;
const INTERRUPT_ON_SHORT: u32 = 1 << 2;
const CHAIN: u32 = 1 << 4;
const INTERRUPT_ON_COMPLETION: u32 = 1 << 5;
const IMMEDIATE_DATA: u32 = 1 << 6;
const BLOCK_SET_ADDRESS: u32 = 1 << 9;
const DECONFIGURE: u32 = 1 << 9;
const TRANSFER_STATE_PRESERVE: u32 = 1 << 9;
const DIRECTION_IN: u32 = 1 << 16;
const SUSPEND: u32 = 1 << 23;

/// TRB type numbers (Table 6-91).
pub mod kind {
    pub const NORMAL: u8 = 1;
    pub const SETUP: u8 = 2;
    pub const DATA: u8 = 3;
    pub const STATUS: u8 = 4;
    pub const LINK: u8 = 6;
    pub const ENABLE_SLOT: u8 = 9;
    pub const DISABLE_SLOT: u8 = 10;
    pub const ADDRESS_DEVICE: u8 = 11;
    pub const CONFIGURE_ENDPOINT: u8 = 12;
    pub const EVALUATE_CONTEXT: u8 = 13;
    pub const RESET_ENDPOINT: u8 = 14;
    pub const STOP_ENDPOINT: u8 = 15;
    pub const SET_TR_DEQUEUE: u8 = 16;
    pub const RESET_DEVICE: u8 = 17;
    pub const NO_OP_COMMAND: u8 = 23;
    pub const TRANSFER_EVENT: u8 = 32;
    pub const COMMAND_COMPLETION_EVENT: u8 = 33;
    pub const PORT_STATUS_CHANGE_EVENT: u8 = 34;
    pub const HOST_CONTROLLER_EVENT: u8 = 37;
}

impl Trb {
    fn new(kind: u8) -> Self {
        Self {
            control: u32::from(kind) << 10,
            ..Self::default()
        }
    }

    pub fn kind(&self) -> u8 {
        ((self.control >> 10) & 0x3f) as u8
    }

    pub fn cycle(&self) -> bool {
        self.control & CYCLE != 0
    }

    pub fn with_cycle(self, cycle: bool) -> Self {
        Self {
            control: (self.control & !CYCLE) | u32::from(cycle),
            ..self
        }
    }

    pub fn chains(&self) -> bool {
        self.control & CHAIN != 0
    }

    /// A Link TRB inside a TD carries the chain on (§4.11.5.1).
    pub fn with_chain(self, chain: bool) -> Self {
        Self {
            control: self.control & !CHAIN,
            ..self
        }
        .flag(CHAIN, chain)
    }

    fn slot(self, slot: u8) -> Self {
        Self {
            control: self.control | u32::from(slot) << 24,
            ..self
        }
    }

    fn endpoint(self, dci: u8) -> Self {
        Self {
            control: self.control | u32::from(dci & 0x1f) << 16,
            ..self
        }
    }

    fn flag(self, flag: u32, on: bool) -> Self {
        Self {
            control: if on {
                self.control | flag
            } else {
                self.control
            },
            ..self
        }
    }

    /// A Link TRB to `target` that toggles the consumer's cycle state.
    pub fn link(target: u64) -> Self {
        Self {
            parameter: target & !0xf,
            ..Self::new(kind::LINK)
        }
        .flag(TOGGLE_CYCLE, true)
    }

    pub fn no_op_command() -> Self {
        Self::new(kind::NO_OP_COMMAND)
    }

    pub fn enable_slot(slot_type: u8) -> Self {
        Self {
            control: Self::new(kind::ENABLE_SLOT).control | u32::from(slot_type & 0x1f) << 16,
            ..Self::default()
        }
    }

    pub fn disable_slot(slot: u8) -> Self {
        Self::new(kind::DISABLE_SLOT).slot(slot)
    }

    pub fn address_device(input_context: u64, slot: u8, block_set_address: bool) -> Self {
        Self {
            parameter: input_context & !0xf,
            ..Self::new(kind::ADDRESS_DEVICE)
        }
        .slot(slot)
        .flag(BLOCK_SET_ADDRESS, block_set_address)
    }

    pub fn configure_endpoint(input_context: u64, slot: u8, deconfigure: bool) -> Self {
        Self {
            parameter: input_context & !0xf,
            ..Self::new(kind::CONFIGURE_ENDPOINT)
        }
        .slot(slot)
        .flag(DECONFIGURE, deconfigure)
    }

    pub fn evaluate_context(input_context: u64, slot: u8) -> Self {
        Self {
            parameter: input_context & !0xf,
            ..Self::new(kind::EVALUATE_CONTEXT)
        }
        .slot(slot)
    }

    pub fn reset_endpoint(slot: u8, dci: u8, preserve_transfer_state: bool) -> Self {
        Self::new(kind::RESET_ENDPOINT)
            .slot(slot)
            .endpoint(dci)
            .flag(TRANSFER_STATE_PRESERVE, preserve_transfer_state)
    }

    pub fn stop_endpoint(slot: u8, dci: u8, suspend: bool) -> Self {
        Self::new(kind::STOP_ENDPOINT)
            .slot(slot)
            .endpoint(dci)
            .flag(SUSPEND, suspend)
    }

    /// Point endpoint `dci`'s dequeue at `dequeue`, whose TRB carries
    /// `cycle` as its cycle state.
    pub fn set_tr_dequeue(dequeue: u64, cycle: bool, slot: u8, dci: u8) -> Self {
        Self {
            parameter: (dequeue & !0xf) | u64::from(cycle),
            ..Self::new(kind::SET_TR_DEQUEUE)
        }
        .slot(slot)
        .endpoint(dci)
    }

    pub fn reset_device(slot: u8) -> Self {
        Self::new(kind::RESET_DEVICE).slot(slot)
    }

    /// The Setup Stage of a control transfer, its eight bytes carried in the
    /// TRB itself (§6.4.1.2.1).
    pub fn setup(setup: [u8; 8], data_in: Option<bool>) -> Self {
        let transfer_type = match data_in {
            None => 0,
            Some(false) => 2,
            Some(true) => 3,
        };
        Self {
            parameter: u64::from_le_bytes(setup),
            status: 8,
            control: Self::new(kind::SETUP).control | IMMEDIATE_DATA | transfer_type << 16,
        }
    }

    pub fn data(buffer: u64, len: u32, data_in: bool) -> Self {
        Self {
            parameter: buffer,
            status: len & 0x1_ffff,
            ..Self::new(kind::DATA)
        }
        .flag(DIRECTION_IN, data_in)
        .flag(INTERRUPT_ON_SHORT, data_in)
    }

    /// The Status Stage runs opposite to the data stage, and in when there
    /// is none.
    pub fn status_stage(data_was_in: bool) -> Self {
        Self::new(kind::STATUS)
            .flag(DIRECTION_IN, !data_was_in)
            .flag(INTERRUPT_ON_COMPLETION, true)
    }

    pub fn normal(buffer: u64, len: u32, chain: bool) -> Self {
        Self::normal_sized(buffer, len, 0, chain)
    }

    /// `td_size` is how many packets of the TD follow this TRB's (§4.11.2.4).
    pub fn normal_sized(buffer: u64, len: u32, td_size: u32, chain: bool) -> Self {
        Self {
            parameter: buffer,
            status: len & 0x1_ffff | td_size.min(31) << 17,
            ..Self::new(kind::NORMAL)
        }
        .flag(CHAIN, chain)
        .flag(INTERRUPT_ON_SHORT, true)
        .flag(INTERRUPT_ON_COMPLETION, !chain)
    }
}

/// The completion code of an event (Table 6-90).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompletionCode(pub u8);

impl CompletionCode {
    pub const SUCCESS: Self = Self(1);
    pub const DATA_BUFFER: Self = Self(2);
    pub const BABBLE: Self = Self(3);
    pub const TRANSACTION: Self = Self(4);
    pub const TRB: Self = Self(5);
    pub const STALL: Self = Self(6);
    pub const RESOURCE: Self = Self(7);
    pub const NO_SLOTS: Self = Self(9);
    pub const SLOT_NOT_ENABLED: Self = Self(11);
    pub const SHORT_PACKET: Self = Self(13);
    pub const PARAMETER: Self = Self(17);
    pub const CONTEXT_STATE: Self = Self(19);
    pub const EVENT_RING_FULL: Self = Self(21);
    pub const COMMAND_RING_STOPPED: Self = Self(24);
    pub const COMMAND_ABORTED: Self = Self(25);
    pub const STOPPED: Self = Self(26);
    pub const STOPPED_LENGTH_INVALID: Self = Self(27);
    pub const STOPPED_SHORT_PACKET: Self = Self(28);

    pub fn is_success(self) -> bool {
        self == Self::SUCCESS
    }
}

/// What an event TRB reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Transfer {
        trb: u64,
        residual: u32,
        code: CompletionCode,
        event_data: bool,
        dci: u8,
        slot: u8,
    },
    CommandCompletion {
        trb: u64,
        code: CompletionCode,
        parameter: u32,
        slot: u8,
    },
    PortStatusChange {
        port: u8,
    },
    HostController {
        code: CompletionCode,
    },
    Other {
        kind: u8,
    },
}

impl Event {
    pub fn decode(trb: Trb) -> Self {
        let code = CompletionCode((trb.status >> 24) as u8);
        let slot = (trb.control >> 24) as u8;
        match trb.kind() {
            kind::TRANSFER_EVENT => Self::Transfer {
                trb: trb.parameter,
                residual: trb.status & 0xff_ffff,
                code,
                event_data: trb.control & 1 << 2 != 0,
                dci: ((trb.control >> 16) & 0x1f) as u8,
                slot,
            },
            kind::COMMAND_COMPLETION_EVENT => Self::CommandCompletion {
                trb: trb.parameter & !0xf,
                code,
                parameter: trb.status & 0xff_ffff,
                slot,
            },
            kind::PORT_STATUS_CHANGE_EVENT => Self::PortStatusChange {
                port: (trb.parameter >> 24) as u8,
            },
            kind::HOST_CONTROLLER_EVENT => Self::HostController { code },
            kind => Self::Other { kind },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_place_their_fields() {
        let address = Trb::address_device(0x1_2345_6780, 7, true);
        assert_eq!(address.kind(), kind::ADDRESS_DEVICE);
        assert_eq!(address.parameter, 0x1_2345_6780);
        assert_eq!(address.control, 11 << 10 | 1 << 9 | 7 << 24);
        let enable = Trb::enable_slot(0);
        assert_eq!(enable.control, 9 << 10);
        let stop = Trb::stop_endpoint(3, 5, true);
        assert_eq!(stop.control, 15 << 10 | 5 << 16 | 1 << 23 | 3 << 24);
        let dequeue = Trb::set_tr_dequeue(0xabc0, true, 2, 4);
        assert_eq!(dequeue.parameter, 0xabc1);
        assert_eq!(dequeue.control, 16 << 10 | 4 << 16 | 2 << 24);
        assert_eq!(Trb::no_op_command().control, 23 << 10);
        let configure = Trb::configure_endpoint(0x2000, 1, true);
        assert_eq!(configure.control, 12 << 10 | 1 << 9 | 1 << 24);
        let reset = Trb::reset_endpoint(1, 3, true);
        assert_eq!(reset.control, 14 << 10 | 1 << 9 | 3 << 16 | 1 << 24);
        assert_eq!(Trb::disable_slot(6).control, 10 << 10 | 6 << 24);
        let evaluate = Trb::evaluate_context(0x3000, 2);
        assert_eq!(
            (evaluate.parameter, evaluate.control),
            (0x3000, 13 << 10 | 2 << 24)
        );
    }

    #[test]
    fn a_link_toggles_and_the_cycle_bit_is_bit_zero() {
        let link = Trb::link(0x5000);
        assert_eq!(link.control, 6 << 10 | 1 << 1);
        assert!(!link.cycle());
        let owned = link.with_cycle(true);
        assert!(owned.cycle());
        assert_eq!(owned.with_cycle(false), link);
    }

    #[test]
    fn control_transfer_stages() {
        let get_descriptor = [0x80, 6, 0, 1, 0, 0, 18, 0];
        let setup = Trb::setup(get_descriptor, Some(true));
        assert_eq!(setup.parameter, 0x0012_0000_0100_0680);
        assert_eq!(setup.status, 8);
        assert_eq!(setup.control, 2 << 10 | 1 << 6 | 3 << 16);
        let data = Trb::data(0x9000, 18, true);
        assert_eq!(data.control, 3 << 10 | 1 << 16 | 1 << 2);
        assert_eq!(data.status, 18);
        assert_eq!(Trb::status_stage(true).control, 4 << 10 | 1 << 5);
        assert_eq!(Trb::status_stage(false).control, 4 << 10 | 1 << 16 | 1 << 5);
        assert_eq!(Trb::setup([0; 8], None).control >> 16 & 3, 0);
        assert_eq!(Trb::normal(0, 512, true).control, 1 << 10 | 1 << 4 | 1 << 2);
        assert_eq!(
            Trb::normal(0, 512, false).control,
            1 << 10 | 1 << 5 | 1 << 2
        );
    }

    #[test]
    fn events_decode() {
        let completion = Trb {
            parameter: 0x1_0000_0230,
            status: 1 << 24 | 0x42,
            control: 33 << 10 | 1 | 5 << 24,
        };
        assert_eq!(
            Event::decode(completion),
            Event::CommandCompletion {
                trb: 0x1_0000_0230,
                code: CompletionCode::SUCCESS,
                parameter: 0x42,
                slot: 5,
            }
        );
        let port = Trb {
            parameter: 7 << 24,
            status: 1 << 24,
            control: 34 << 10 | 1,
        };
        assert_eq!(Event::decode(port), Event::PortStatusChange { port: 7 });
        let transfer = Trb {
            parameter: 0x8000,
            status: 13 << 24 | 100,
            control: 32 << 10 | 1 << 2 | 3 << 16 | 2 << 24,
        };
        assert_eq!(
            Event::decode(transfer),
            Event::Transfer {
                trb: 0x8000,
                residual: 100,
                code: CompletionCode::SHORT_PACKET,
                event_data: true,
                dci: 3,
                slot: 2,
            }
        );
        let lost = Trb {
            status: 21 << 24,
            control: 37 << 10,
            ..Trb::default()
        };
        assert_eq!(
            Event::decode(lost),
            Event::HostController {
                code: CompletionCode::EVENT_RING_FULL
            }
        );
        assert_eq!(
            Event::decode(Trb {
                control: 39 << 10,
                ..Trb::default()
            }),
            Event::Other { kind: 39 }
        );
    }
}
