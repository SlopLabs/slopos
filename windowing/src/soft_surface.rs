//! A window drawn on the CPU into two shared-memory buffers; a consumer that
//! polls the socket itself hands `BufferRelease` to
//! [`SoftSurface::release_buffer`] and `FrameDone` to
//! [`SoftSurface::frame_done`].
//!
//! The compositor reads the buffer it was last committed, and the one before
//! it until it says otherwise, so a frame is drawn only into a buffer it has
//! released: [`SoftSurface::ready`] says whether one is free, and
//! [`RenderSurface::frame`] hands none out while none is.

use slopos_abi::damage::DamageRect;
use slopos_abi::pixel::PixelFormat;
use slopos_gfx::damage::{Geometry, Rects};
use slopos_gfx::swap::{BUFFERS, Swap};
use slopos_gfx::{DrawBuffer, RenderError, RenderSurface};
use slopos_protocol::types::SurfaceId;

use crate::connection::ProtocolHandle;
use crate::surface::SurfaceError;
use slopos_cabi::SharedMemory;

/// How long a frame waits for the compositor to say the last one reached
/// the screen: one that shows nothing of this surface says nothing, and the
/// surface must not stop drawing for good.
const FRAME_DONE_WAIT_MS: u64 = 100;

/// How long the compositor may keep the buffer it showed before a commit
/// without releasing it: past that the release is taken for lost, since a
/// surface waiting for it would never draw again. Long, because a
/// compositor that only stalled this long still reads that buffer.
const RELEASE_WAIT_MS: u64 = 5000;

pub struct SoftSurface {
    handle: ProtocolHandle,
    bufs: [SharedMemory; BUFFERS],
    swap: Swap,
    /// When the frame last committed stops holding the next one back.
    paced_until_ms: Option<u64>,
    committed_at_ms: u64,
    surface_id: SurfaceId,
    width: u32,
    height: u32,
    pitch: usize,
    bytes_pp: u8,
    pixel_format: PixelFormat,
}

impl SoftSurface {
    /// Nothing is attached until the first [`present`](RenderSurface::present).
    pub fn new(
        handle: ProtocolHandle,
        surface_id: SurfaceId,
        pixel_format: PixelFormat,
        width: u32,
        height: u32,
    ) -> Result<Self, SurfaceError> {
        let bytes_pp = pixel_format.bytes_per_pixel();
        let (bufs, pitch) = buffers(width, height, bytes_pp).map_err(|why| match why {
            RenderError::BadSize => SurfaceError::BadSize,
            RenderError::BufferUnavailable => SurfaceError::ShmFailed,
        })?;
        Ok(Self {
            handle,
            bufs,
            swap: Swap::new(),
            paced_until_ms: None,
            committed_at_ms: 0,
            surface_id,
            width,
            height,
            pitch,
            bytes_pp,
            pixel_format,
        })
    }

    /// Age of the buffer the next [`frame()`](RenderSurface::frame) will hand out,
    /// per the `EGL_EXT_buffer_age` convention: `n > 0` means it still holds the
    /// frame presented `n` frames ago; `0` means undefined contents, repaint in full.
    pub fn buffer_age(&self) -> u32 {
        self.swap.age()
    }

    pub fn release_buffer(&mut self, buffer_id: u32) {
        self.swap.release(buffer_id as usize);
    }

    /// The frame last committed reached the screen.
    pub fn frame_done(&mut self) {
        self.paced_until_ms = None;
    }

    /// Whether a frame can be drawn now: a buffer is free, and the frame
    /// last committed has reached the screen or has had its time to.
    pub fn ready(&self) -> bool {
        self.wait_ms() == 0
    }

    /// How long until a frame can be drawn, should no event come first: a
    /// `FrameDone` or a `BufferRelease` ends the wait early.
    pub fn wait_ms(&self) -> u64 {
        let now = crate::get_time_ms();
        let paced = self
            .paced_until_ms
            .map_or(0, |until| until.saturating_sub(now));
        match self.swap.free() {
            Some(_) => paced,
            None => (self.committed_at_ms + RELEASE_WAIT_MS)
                .saturating_sub(now)
                .max(paced),
        }
    }

    /// Takes a buffer whose release is overdue for free.
    fn reclaim_overdue(&mut self) {
        if self.swap.free().is_none() && self.wait_ms() == 0 {
            self.swap.reclaim();
        }
    }

    fn geometry(&self) -> Geometry {
        Geometry {
            width: self.width,
            height: self.height,
            pitch: self.pitch,
            bytes_pp: self.bytes_pp,
        }
    }

    fn draw_buffer(&mut self, slot: usize) -> Option<DrawBuffer<'_>> {
        let mut buf = DrawBuffer::new(
            self.bufs[slot].as_mut_slice(),
            self.width,
            self.height,
            self.pitch,
            self.bytes_pp,
        )?;
        buf.set_pixel_format(self.pixel_format);
        Some(buf)
    }

    /// The next frame's buffer holding the frame the compositor shows, so a
    /// caller repaints only what changed and presents those rects; `None`
    /// when no frame can be drawn or what the buffer holds is unknown, which
    /// a whole frame through [`frame`](RenderSurface::frame) mends.
    pub fn frame_over_shown(&mut self) -> Option<DrawBuffer<'_>> {
        self.reclaim_overdue();
        let over = self.swap.begin_over_shown()?;
        let [first, second] = &mut self.bufs;
        let (to, from) = match over.slot {
            0 => (first.as_mut_slice(), second.as_slice()),
            _ => (second.as_mut_slice(), first.as_slice()),
        };
        let bytes_pp = self.bytes_pp as usize;
        for rect in over.stale.as_slice() {
            let rect = rect.clip(self.width as i32, self.height as i32);
            if !rect.is_valid() {
                continue;
            }
            let (x0, len) = (
                rect.x0 as usize * bytes_pp,
                (rect.x1 - rect.x0 + 1) as usize * bytes_pp,
            );
            for y in rect.y0 as usize..=rect.y1 as usize {
                let at = y * self.pitch + x0;
                if let (Some(to), Some(from)) = (to.get_mut(at..at + len), from.get(at..at + len)) {
                    to.copy_from_slice(from);
                }
            }
        }
        self.draw_buffer(over.slot)
    }

    /// Commits the frame drawn where it differs from the one the compositor
    /// shows, and answers whether it differed at all: one that does not is
    /// dropped, and nothing is sent.
    pub fn present_changed(&mut self) -> bool {
        self.present_changed_within(self.geometry().whole())
    }

    /// [`present_changed`](Self::present_changed) of a frame that can differ
    /// from the one shown only within `bounds`.
    pub fn present_changed_within(&mut self, bounds: DamageRect) -> bool {
        let Some(slot) = self.swap.drawing() else {
            return false;
        };
        let Some(shown) = self.swap.shown() else {
            self.present();
            return true;
        };
        let changed = slopos_gfx::damage::changed_within(
            self.bufs[slot].as_slice(),
            self.bufs[shown].as_slice(),
            self.geometry(),
            bounds,
        );
        if changed.is_empty() {
            self.swap.drop_unchanged();
            return false;
        }
        self.commit(changed.as_slice());
        true
    }

    /// Commits the frame drawn, damaged in `rects`; one of none damages it
    /// whole.
    pub fn present_rects(&mut self, rects: &[DamageRect]) {
        match rects.iter().any(DamageRect::is_valid) {
            true => self.commit(Rects::of(rects).as_slice()),
            false => self.commit(&[self.geometry().whole()]),
        }
    }

    /// A buffer's fd is sent only on first use; later frames re-select the slot by
    /// id, so the compositor keeps a stable mapping per slot.
    fn commit(&mut self, damage: &[DamageRect]) {
        let Some(committed) = self.swap.commit(damage) else {
            return;
        };
        let slot = committed.slot as u32;
        let mut client = self.handle.borrow_client();
        let _ = match committed.first_use {
            true => client.surface_attach_buffer(
                self.surface_id,
                slot,
                self.bufs[committed.slot].fd(),
                self.width,
                self.height,
            ),
            false => client.surface_select_buffer(self.surface_id, slot, self.width, self.height),
        };
        for rect in damage.iter().filter(|rect| rect.is_valid()) {
            let (w, h) = (rect.x1 - rect.x0 + 1, rect.y1 - rect.y0 + 1);
            let _ = client.surface_damage(self.surface_id, rect.x0, rect.y0, w, h);
        }
        let _ = client.surface_frame(self.surface_id);
        let _ = client.surface_commit(self.surface_id);
        self.committed_at_ms = crate::get_time_ms();
        self.paced_until_ms = Some(self.committed_at_ms + FRAME_DONE_WAIT_MS);
    }
}

fn buffers(
    width: u32,
    height: u32,
    bytes_pp: u8,
) -> Result<([SharedMemory; BUFFERS], usize), RenderError> {
    if width == 0 || height == 0 {
        return Err(RenderError::BadSize);
    }
    let pitch = (width as usize)
        .checked_mul(bytes_pp as usize)
        .ok_or(RenderError::BadSize)?;
    let size = pitch
        .checked_mul(height as usize)
        .ok_or(RenderError::BadSize)?;
    let buffer = || SharedMemory::create(size).map_err(|_| RenderError::BufferUnavailable);
    Ok(([buffer()?, buffer()?], pitch))
}

impl RenderSurface for SoftSurface {
    /// `None` while no buffer is free; what one holds is as old as
    /// [`buffer_age`](SoftSurface::buffer_age) says.
    fn frame(&mut self) -> Option<DrawBuffer<'_>> {
        self.reclaim_overdue();
        let slot = self.swap.begin()?;
        self.draw_buffer(slot)
    }

    fn present(&mut self) {
        self.present_rects(&[]);
    }

    fn present_region(&mut self, x: i32, y: i32, w: i32, h: i32) {
        self.present_rects(&[DamageRect {
            x0: x,
            y0: y,
            x1: x + w - 1,
            y1: y + h - 1,
        }]);
    }

    fn resize(&mut self, new_width: u32, new_height: u32) -> Result<(), RenderError> {
        if (new_width, new_height) == (self.width, self.height) {
            return Ok(());
        }
        let (bufs, pitch) = buffers(new_width, new_height, self.bytes_pp)?;

        // New fds must be re-registered; the compositor evicts the old slot
        // mappings when the new fds become current.
        self.bufs = bufs;
        self.swap.replace_buffers();
        self.paced_until_ms = None;
        self.width = new_width;
        self.height = new_height;
        self.pitch = pitch;
        Ok(())
    }

    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn pixel_format(&self) -> PixelFormat {
        self.pixel_format
    }

    fn bytes_pp(&self) -> u8 {
        self.bytes_pp
    }

    fn pitch(&self) -> usize {
        self.pitch
    }
}
