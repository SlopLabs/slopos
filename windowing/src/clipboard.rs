//! A copy hands the compositor a memfd holding the bytes; a paste asks, is told
//! the size and hands back a memfd of that size, since no event carries an fd.

use crate::connection::ProtocolHandle;
use slopos_cabi::SharedMemory;

/// Ceiling on one transfer, matching the compositor's.
pub const MAX_CLIPBOARD_BYTES: usize = 16 * 1024 * 1024;

/// Where a paste round trip has got to, so a second request during one does
/// not drop the buffer the first waits on.
#[derive(Default)]
enum Paste {
    #[default]
    Idle,
    AwaitingOffer,
    AwaitingData(SharedMemory),
}

#[derive(Default)]
pub struct Clipboard {
    paste: Paste,
}

impl Clipboard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publishes `text` as the selection. False when the transfer could not be
    /// set up — an empty string, an oversized one, or no compositor.
    pub fn copy(&self, handle: &ProtocolHandle, text: &str) -> bool {
        let bytes = text.as_bytes();
        if bytes.is_empty() || bytes.len() > MAX_CLIPBOARD_BYTES {
            return false;
        }
        let Ok(mut buffer) = SharedMemory::create(bytes.len()) else {
            return false;
        };
        buffer.as_mut_slice()[..bytes.len()].copy_from_slice(bytes);
        let Some(mut client) = handle.try_borrow_client() else {
            return false;
        };
        // The compositor dups the fd, so dropping `buffer` here leaves the
        // selection alive on its side.
        client
            .clipboard_copy(buffer.fd(), bytes.len() as u32)
            .is_ok()
    }

    /// Asks for the selection, which arrives as
    /// [`crate::Event::ClipboardOffer`]; false while a round trip runs.
    pub fn request(&mut self, handle: &ProtocolHandle) -> bool {
        if !matches!(self.paste, Paste::Idle) {
            return false;
        }
        let sent = match handle.try_borrow_client() {
            Some(mut client) => client.clipboard_paste().is_ok(),
            None => false,
        };
        if sent {
            self.paste = Paste::AwaitingOffer;
        }
        sent
    }

    /// Answers an offer with a buffer of exactly `len` bytes. Every failure
    /// ends the round trip, or an empty selection would refuse later pastes.
    pub fn accept_offer(&mut self, handle: &ProtocolHandle, len: u32) -> bool {
        if !matches!(self.paste, Paste::AwaitingOffer) {
            return false;
        }
        self.paste = Paste::Idle;
        let len = len as usize;
        if len == 0 || len > MAX_CLIPBOARD_BYTES {
            return false;
        }
        let Ok(buffer) = SharedMemory::create(len) else {
            return false;
        };
        let Some(mut client) = handle.try_borrow_client() else {
            return false;
        };
        if client.clipboard_read(buffer.fd(), len as u32).is_ok() {
            self.paste = Paste::AwaitingData(buffer);
            true
        } else {
            false
        }
    }

    /// The pasted text, less control bytes but tab and newline: a clipboard is
    /// untrusted, and an escape sequence pasted is something no one typed.
    pub fn take(&mut self, len: u32) -> Option<String> {
        let Paste::AwaitingData(mut buffer) = core::mem::take(&mut self.paste) else {
            return None;
        };
        let len = (len as usize).min(buffer.len());
        let bytes = &buffer.as_mut_slice()[..len];
        let mut out: Vec<u8> = Vec::with_capacity(len);
        for &byte in bytes {
            if byte == b'\r' {
                continue;
            }
            if byte < 0x20 && byte != b'\n' && byte != b'\t' {
                continue;
            }
            out.push(byte);
        }
        Some(String::from_utf8_lossy(&out).into_owned())
    }
}
