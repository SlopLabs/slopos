//! The eXtensible Host Controller Interface, 1.2.

pub mod bus;
pub mod context;
pub mod controller;
pub mod ext_cap;
pub mod memory;
pub mod regs;
pub mod ring;
pub mod trb;

#[cfg(test)]
mod sim;

pub use bus::{Error, RegisterBus, Wait};
pub use controller::{Drained, Handoff, Health, Setup};
pub use ext_cap::{Found, Protocol, Protocols, Speed};
pub use memory::DmaPage;
pub use regs::{Capabilities, Decline, Layout, Malformed, PortSc};
pub use ring::{CommandRing, EventRing, ProducerRing};
pub use trb::{CompletionCode, Event, Trb};
