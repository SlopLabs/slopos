#![forbid(unsafe_code)]
//! A window on the compositor without the widget toolkit: a connection, a
//! surface to draw into, its input events and an event loop ([`run`]).

pub mod app;
pub mod clipboard;
pub mod connection;
pub mod event;
pub mod soft_surface;
pub mod surface;
pub mod window;

pub use app::{ControlFlow, WindowedApp, run};
pub use clipboard::Clipboard;
pub use connection::{
    ConnectError, Protocol, ProtocolHandle, UiMessage, UiQueue, UiSender, connect,
};
pub use event::Event;
pub use slopos_abi::handle::{
    DisplayHandle, HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
    WindowHandle,
};
pub use slopos_gfx::{RenderError, RenderSurface};
pub use soft_surface::SoftSurface;
pub use surface::{Surface, SurfaceError};
pub use window::{EVENT_BUF_LEN, Window};

#[cfg(feature = "alloc")]
pub use slopos_gfx::HeadlessSurface;

#[inline]
pub fn get_time_ms() -> u64 {
    slopos_cabi::monotonic_ms()
}
