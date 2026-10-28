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

/// Where the session's compositor listens: `compositor` in
/// `$XDG_RUNTIME_DIR`, which init sets, or in `/run/user/<euid>` when it is
/// unset or not absolute.
pub fn socket_path() -> alloc::vec::Vec<u8> {
    let mut path = slopos_cabi::getenv(c"XDG_RUNTIME_DIR", <[u8]>::to_vec)
        .filter(|dir| dir.first() == Some(&b'/'))
        .unwrap_or_else(|| alloc::format!("/run/user/{}", slopos_cabi::geteuid()).into_bytes());
    path.extend_from_slice(b"/compositor");
    path
}
