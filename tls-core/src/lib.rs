//! A sans-I/O TLS 1.3 (RFC 8446) client for SlopOS's userland and every
//! primitive under it: [`Client`] consumes bytes read from a transport and
//! produces bytes to write to it.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod aes;
pub mod bignum;
pub mod chacha;
pub mod client;
pub mod conn;
pub mod ct;
pub mod der;
pub mod ec;
pub mod gcm;
pub mod hash;
pub mod pem;
pub mod rsa;
#[cfg(any(test, feature = "test-server"))]
pub mod server;
pub mod suite;
#[cfg(any(test, feature = "test-server"))]
pub mod testpki;
pub mod x25519;
pub mod x509;

pub use client::{Client, ClientConfig};
pub use conn::Error;
pub use suite::CipherSuite;
pub use x509::{CertError, TrustStore};

#[cfg(test)]
mod tests;
