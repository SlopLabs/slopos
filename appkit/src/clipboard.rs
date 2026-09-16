//! Clipboard access for widget applications.
//!
//! A copy is a request to the compositor and a paste is a round trip, so the two
//! halves cannot be one call: [`copy`] is synchronous, [`request_paste`] only
//! asks, and the text arrives later through [`crate::App::on_paste`].
//!
//! The connection lives in a thread-local that [`crate::run_app`] installs,
//! rather than being threaded through `update()`: an application's message
//! handler has no window, and a clipboard that could only be reached from a
//! paint or an event handler would not be reachable from a menu action.

use std::cell::RefCell;

use slopos_windowing::{Clipboard, ProtocolHandle};

thread_local! {
    static STATE: RefCell<Option<(ProtocolHandle, Clipboard)>> = const { RefCell::new(None) };
}

/// Installs the connection the clipboard calls run against. Called once by
/// [`crate::run_app`].
pub(crate) fn install(handle: ProtocolHandle) {
    STATE.with(|cell| {
        *cell.borrow_mut() = Some((handle, Clipboard::new()));
    });
}

/// Publishes `text` as the system selection. False when there is no compositor
/// connection, or the text is empty or too large.
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
