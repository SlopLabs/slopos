#![forbid(unsafe_code)]
//! A window on the compositor without the widget toolkit: a connection, a
//! surface to draw into and its input events.

pub mod clipboard;
pub mod connection;
pub mod event;
pub mod soft_surface;
pub mod surface;
pub mod window;

pub use clipboard::Clipboard;
pub use connection::{
    ConnectError, Protocol, ProtocolHandle, UiMessage, UiQueue, UiSender, connect,
};
pub use event::Event;
pub use slopos_gfx::{RenderError, RenderSurface};
pub use soft_surface::SoftSurface;
pub use surface::{Surface, SurfaceError};
pub use window::Window;

#[cfg(feature = "alloc")]
pub use slopos_gfx::HeadlessSurface;

#[inline]
pub fn get_time_ms() -> u64 {
    slopos_cabi::monotonic_ms()
}
