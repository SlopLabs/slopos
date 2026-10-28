//! A window: its compositor objects, what it draws into, and the pointer.

use slopos_gfx::{RenderError, RenderSurface};
use slopos_protocol::types::Event as ProtocolEvent;

use crate::connection::ProtocolHandle;
use crate::event::Event;
use crate::soft_surface::SoftSurface;
use crate::surface::{Surface, SurfaceError};

pub struct Window {
    handle: ProtocolHandle,
    surface: Surface,
    renderer: SoftSurface,
    pointer_x: i32,
    pointer_y: i32,
}

impl Window {
    pub fn new(handle: ProtocolHandle, width: u32, height: u32) -> Result<Self, SurfaceError> {
        let surface = Surface::new(handle.clone(), width, height)?;
        let renderer = SoftSurface::new(
            handle.clone(),
            surface.surface_id(),
            surface.pixel_format(),
            width,
            height,
        )?;
        Ok(Self {
            handle,
            surface,
            renderer,
            pointer_x: 0,
            pointer_y: 0,
        })
    }

    pub fn set_title(&self, title: &str) {
        let toplevel_id = self.surface.toplevel_id();
        let mut client = self.handle.borrow_client();
        let _ = client.toplevel_set_title(toplevel_id, title.as_bytes());
    }

    /// The compositor matches windows to dock entries by app id, not by title.
    pub fn set_app_id(&self, app_id: &str) {
        let toplevel_id = self.surface.toplevel_id();
        let mut client = self.handle.borrow_client();
        let _ = client.toplevel_set_app_id(toplevel_id, app_id.as_bytes());
    }

    /// The sizes the window can be laid out at; a `max` equal to `min` keeps
    /// it from being resized at all.
    pub fn set_size_limits(&self, min: (u32, u32), max: (u32, u32)) {
        let toplevel_id = self.surface.toplevel_id();
        let mut client = self.handle.borrow_client();
        let _ = client.toplevel_set_size_limits(toplevel_id, min, max);
    }

    /// A `CURSOR_SHAPE_*`, taken while the pointer is over the window.
    pub fn set_cursor_shape(&self, shape: u8) {
        let surface_id = self.surface.surface_id();
        let mut client = self.handle.borrow_client();
        let _ = client.set_cursor_shape(surface_id, shape);
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), RenderError> {
        self.renderer.resize(width, height)?;
        self.surface.set_size(width, height);
        Ok(())
    }

    /// In window coordinates, as of the last event taken.
    #[inline]
    pub fn pointer(&self) -> (i32, i32) {
        (self.pointer_x, self.pointer_y)
    }

    #[inline]
    pub fn renderer(&self) -> &SoftSurface {
        &self.renderer
    }

    #[inline]
    pub fn renderer_mut(&mut self) -> &mut SoftSurface {
        &mut self.renderer
    }

    /// The next event waiting, or `None` once none is; a `BufferRelease`
    /// frees its buffer here instead, and `pointer()` follows the motion.
    pub fn next_event(&mut self) -> Option<Event> {
        loop {
            let event = self.handle.borrow_client().poll_event().ok().flatten()?;
            if let ProtocolEvent::BufferRelease { buffer_id, .. } = event {
                self.renderer.release_buffer(buffer_id);
                continue;
            }
            let Some(event) = Event::from_protocol(&event) else {
                continue;
            };
            if let Event::PointerEnter { x, y } | Event::PointerMotion { x, y } = event {
                self.pointer_x = x;
                self.pointer_y = y;
            }
            return Some(event);
        }
    }
}
