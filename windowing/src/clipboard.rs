//! Clipboard transfer for windowed applications.
//!
//! The wire is fd-based in both directions: a copy hands the compositor a memfd
//! holding the bytes, and a paste is a round trip — ask, be told the size, hand
//! back a destination memfd of exactly that size, then read it. The destination
//! is the receiver's because the server→client event path carries no fd.
//!
//! [`Clipboard`] owns the buffer between the two halves of that round trip,
//! which is the whole of the state a client needs to keep.

use crate::connection::ProtocolHandle;
use crate::memfd_buf::MemfdBuffer;

/// Ceiling on one transfer, matching the compositor's.
pub const MAX_CLIPBOARD_BYTES: usize = 16 * 1024 * 1024;

/// Where a paste round trip has got to.
///
/// Explicit, because the round trip is three messages long and a second
/// request part way through it used to drop the destination buffer the first
/// was still waiting on — losing that paste, or answering it with the *next*
/// transfer's buffer.
#[derive(Default)]
enum Paste {
    #[default]
    Idle,
    AwaitingOffer,
    AwaitingData(MemfdBuffer),
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
        let Ok(mut buffer) = MemfdBuffer::create(bytes.len()) else {
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

    /// Asks for the selection; the answer arrives as
    /// [`crate::Event::ClipboardOffer`].
    ///
    /// False while a round trip is already running, which leaves the caller to
    /// fall back rather than cancelling a paste that is about to land.
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

    /// Answers an offer with a destination buffer of exactly `len` bytes.
    ///
    /// Every failure here ends the round trip, or an empty selection would
    /// leave the clipboard refusing every later paste.
    pub fn accept_offer(&mut self, handle: &ProtocolHandle, len: u32) -> bool {
        if !matches!(self.paste, Paste::AwaitingOffer) {
            return false;
        }
        self.paste = Paste::Idle;
        let len = len as usize;
        if len == 0 || len > MAX_CLIPBOARD_BYTES {
            return false;
        }
        let Ok(buffer) = MemfdBuffer::create(len) else {
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

    /// Takes the pasted text once the compositor reports it written.
    ///
    /// Control bytes other than tab and newline are dropped: a clipboard is
    /// untrusted input, and an editor that accepted an escape sequence would be
    /// pasting something no one typed.
    pub fn take(&mut self, len: u32) -> Option<String> {
        let Paste::AwaitingData(mut buffer) = core::mem::take(&mut self.paste) else {
            return None;
        };
        let len = (len as usize).min(buffer.size());
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
