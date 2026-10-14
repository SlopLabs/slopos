//! SlopOS display protocol — typed enum wire format.
//!
//! Length-prefixed binary codec, all little-endian.

#![no_std]

extern crate alloc;

pub mod client;
pub mod codec;
pub mod connection;
pub mod server;
pub mod types;

pub use client::Client;
pub use codec::{Decode, Encode, FdFifo};
pub use connection::Connection;
pub use server::Server;
pub use types::*;

/// Where the compositor of `uid`'s session listens: in the session's runtime
/// directory, which init makes for it alone.
pub fn socket_path(uid: u32) -> alloc::vec::Vec<u8> {
    alloc::format!("/run/user/{uid}/compositor").into_bytes()
}
