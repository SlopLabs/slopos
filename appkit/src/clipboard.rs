//! [`copy`] is done at once; [`request_paste`] only asks, and the text arrives
//! at [`crate::App::on_paste`]. A thread-local, so `update` reaches it.

use std::cell::RefCell;

use slopos_windowing::{Clipboard, ProtocolHandle};

thread_local! {
    static STATE: RefCell<Option<(ProtocolHandle, Clipboard)>> = const { RefCell::new(None) };
}

pub(crate) fn install(handle: ProtocolHandle) {
    STATE.with(|cell| {
        *cell.borrow_mut() = Some((handle, Clipboard::new()));
    });
}

/// False when there is no compositor, or the text is empty or too large.
pub fn copy(text: &str) -> bool {
    STATE.with(|cell| match cell.borrow_mut().as_mut() {
        Some((handle, clipboard)) => clipboard.copy(handle, text),
        None => false,
    })
}

/// Asks for the selection; it arrives at [`crate::App::on_paste`].
pub fn request_paste() -> bool {
    STATE.with(|cell| match cell.borrow_mut().as_mut() {
        Some((handle, clipboard)) => clipboard.request(handle),
        None => false,
    })
}

pub(crate) fn accept_offer(len: u32) -> bool {
    STATE.with(|cell| match cell.borrow_mut().as_mut() {
        Some((handle, clipboard)) => clipboard.accept_offer(handle, len),
        None => false,
    })
}

pub(crate) fn take(len: u32) -> Option<String> {
    STATE.with(|cell| cell.borrow_mut().as_mut().and_then(|(_, c)| c.take(len)))
}
