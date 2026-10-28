//! The event loop of an app that draws its own window.

use slopos_gfx::{DrawBuffer, RenderSurface};

use slopos_protocol::types::Event as ProtocolEvent;

use crate::connection;
use crate::event::Event;
use crate::window::{EVENT_BUF_LEN, Window};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlFlow {
    Continue,
    Exit,
}

pub trait WindowedApp {
    /// Once the window exists, before the first frame.
    fn init(&mut self, _win: &mut Window) {}

    fn on_event(&mut self, _win: &mut Window, event: Event) -> ControlFlow {
        match event {
            Event::CloseRequest => ControlFlow::Exit,
            _ => ControlFlow::Continue,
        }
    }

    /// After [`Window::request_redraw`].
    fn draw(&mut self, fb: &mut DrawBuffer<'_>);

    /// `Some(ms)` makes the loop request a redraw every `ms` even without input.
    fn refresh_interval_ms(&self) -> Option<u64> {
        None
    }
}

/// Runs `app` until it answers [`ControlFlow::Exit`], which exits the process.
pub fn run<A: WindowedApp>(mut app: A, width: u32, height: u32) -> ! {
    let handle = connection::connect().unwrap_or_else(|e| panic!("{e}"));
    let mut win = Window::new(handle.clone(), width, height).unwrap_or_else(|e| panic!("{e}"));

    app.init(&mut win);

    let refresh_interval = app.refresh_interval_ms();
    let mut last_refresh = slopos_cabi::monotonic_ms();

    loop {
        handle.flush_pending_destroys();
        handle.drain_ui_queue();

        let mut proto_buf: [ProtocolEvent; EVENT_BUF_LEN] =
            core::array::from_fn(|_| ProtocolEvent::FrameDone {
                surface: slopos_protocol::types::SurfaceId::NONE,
                timestamp_ms: 0,
            });
        let count = win.poll_protocol_events(&mut proto_buf);

        for pe in &proto_buf[..count] {
            if let Some(event) = Event::from_protocol(pe) {
                win.track_pointer(&event);

                if let Event::Configure { width, height } = event {
                    let _ = win.resize(width, height);
                }

                if app.on_event(&mut win, event) == ControlFlow::Exit {
                    std::process::exit(0);
                }
            }
        }

        if let Some(interval) = refresh_interval {
            let now = slopos_cabi::monotonic_ms();
            if now.saturating_sub(last_refresh) >= interval {
                last_refresh = now;
                win.request_redraw();
            }
        }

        if win.take_redraw() {
            if let Some(mut fb) = win.renderer_mut().frame() {
                app.draw(&mut fb);
                win.renderer_mut().present();
            } else {
                win.request_redraw();
            }
        }

        let timeout_ms: i64 = if win.needs_redraw() {
            0
        } else if let Some(interval) = refresh_interval {
            let now = slopos_cabi::monotonic_ms();
            let elapsed = now.saturating_sub(last_refresh);
            if elapsed >= interval {
                0
            } else {
                (interval - elapsed) as i64
            }
        } else {
            -1
        };
        if timeout_ms != 0 {
            handle.wait_events(timeout_ms);
        }
    }
}
