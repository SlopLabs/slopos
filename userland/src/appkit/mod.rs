//! Application framework for SlopOS windowed applications.
//!
//! Provides `Surface`, `Window`, `Event`, and a generic `run()` loop that
//! eliminate the boilerplate of surface creation, pixel format negotiation,
//! event polling, and frame presentation.
//!
//! # Example
//!
//! ```rust,ignore
//! use crate::appkit::{self, ControlFlow, Event, Window, WindowedApp};
//! use crate::gfx::DrawBuffer;
//!
//! struct MyApp;
//!
//! impl WindowedApp for MyApp {
//!     fn init(&mut self, win: &mut Window) {
//!         win.set_title("My App");
//!         win.request_redraw();
//!     }
//!
//!     fn draw(&mut self, fb: &mut DrawBuffer<'_>) {
//!         // render here
//!     }
//! }
//!
//! pub fn main() -> ! {
//!     appkit::run(MyApp, 640, 480)
//! }
//! ```

pub mod event;
pub mod run;
pub mod surface;
pub mod window;

pub use event::Event;
pub use run::{ControlFlow, WindowedApp, run};
pub use surface::{Surface, SurfaceError};
pub use window::Window;

/// Generate a minimal `_start` entry point and panic handler for a
/// userland binary.
///
/// The provided function must have signature `fn(*mut T) -> !` or
/// `fn(*mut T)` (the latter will cleanly exit after returning).
///
/// If the application returns, the process exits with code 0 via
/// `sys_exit`.
///
/// # Usage
///
/// ```rust,ignore
/// #![no_std]
/// #![no_main]
/// slopos_userland::entry!(slopos_userland::apps::file_manager::file_manager_main);
/// ```
#[macro_export]
macro_rules! entry {
    ($main_fn:path) => {
        #[panic_handler]
        fn panic(_info: &core::panic::PanicInfo) -> ! {
            let _ = $crate::syscall::tty::write(b"panic!\n");
            $crate::syscall::core::exit_with_code(101);
        }

        #[unsafe(no_mangle)]
        #[allow(unreachable_code)]
        pub extern "C" fn _start() -> ! {
            $main_fn(core::ptr::null_mut());
            // If the app returns, exit cleanly instead of spinning.
            $crate::syscall::core::exit();
        }
    };
}
