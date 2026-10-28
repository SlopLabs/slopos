//! The compositor's end of its protocol: clients in the session's runtime
//! directory, and the surfaces their requests make.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use slopos_abi::damage::{DamageRect, MAX_DAMAGE_REGIONS};
use slopos_chrome_core::placement;
use slopos_chrome_core::positioner::{Rect, Size};
use slopos_protocol::server::{MAX_CLIENTS, Server};
use slopos_protocol::types::{
    Event, OwnedFd, ProtocolError, Request, SurfaceId, ToplevelId, WINDOW_STRING_MAX,
};

use super::frame::TITLE_BAR_HEIGHT;
use super::window_info::{AppId, WindowInfo, WindowState};
use crate::syscall::CachedShmMapping;
use slopos_rt::slopfut::sync::Notify;

/// So a runaway selection cannot exhaust memory.
const MAX_CLIPBOARD_BYTES: u32 = 16 * 1024 * 1024;

const MAX_SURFACES: usize = 32;
const MAX_PENDING_DAMAGE: usize = 8;

const MAX_SURFACE_BUFFERS: usize = 2;
/// Past any screen, and small enough that no frame arithmetic overflows.
const SIZE_LIMIT_MAX: u32 = 1 << 14;
/// The least a window is resized or placed at, unless its own maximum is less.
const MIN_WINDOW_WIDTH: i32 = 200;
const MIN_WINDOW_HEIGHT: i32 = 150;
const NO_BUFFER: u8 = u8::MAX;

/// The bridge owns `fd`; the surface cache only borrows a mapping of it.
#[derive(Copy, Clone)]
struct SurfaceBuffer {
    fd: i32,
    width: u32,
    height: u32,
    registered: bool,
}

impl SurfaceBuffer {
    const EMPTY: Self = Self {
        fd: -1,
        width: 0,
        height: 0,
        registered: false,
    };
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum SurfaceRole {
    None,
    Toplevel,
}

struct ProtocolSurface {
    active: bool,
    client_idx: usize,
    surface_id: SurfaceId,
    toplevel_id: ToplevelId,
    /// The committed buffer's fd; 0 before one is.
    buffer_fd: u32,
    width: u32,
    height: u32,
    frame_width: u32,
    frame_height: u32,
    buffers: [SurfaceBuffer; MAX_SURFACE_BUFFERS],
    pending_buffer: u8,
    current_buffer: u8,
    pending_damage: [DamageRect; MAX_PENDING_DAMAGE],
    pending_damage_count: u8,
    committed_damage: [DamageRect; MAX_PENDING_DAMAGE],
    committed_damage_count: u8,
    dirty: bool,
    window_x: i32,
    window_y: i32,
    z_order: u32,
    visible: bool,
    window_state: WindowState,
    /// Tells a recycled slot from the surface that held it before.
    generation: u32,
    frame_callback_pending: bool,
    role: SurfaceRole,
    title: [u8; WINDOW_STRING_MAX],
    app_id: [u8; WINDOW_STRING_MAX],
    /// The client's `(min, max)` sizes; 0 is no limit.
    size_limits: ((u32, u32), (u32, u32)),
    /// When the window was placed, counted across windows; 0 until it is.
    placed_at: u64,
    cursor_shape: u8,
    last_enter_serial: u32,
    has_pointer: bool,
    /// With `has_keyboard_focus`, what a clipboard request must present, so
    /// a surface the person is not typing into cannot read or replace the
    /// selection.
    last_key_serial: u32,
    has_keyboard_focus: bool,
}

impl ProtocolSurface {
    const fn empty() -> Self {
        Self {
            active: false,
            client_idx: 0,
            surface_id: SurfaceId::NONE,
            toplevel_id: ToplevelId::NONE,
            buffer_fd: 0,
            width: 0,
            height: 0,
            frame_width: 0,
            frame_height: 0,
            buffers: [SurfaceBuffer::EMPTY; MAX_SURFACE_BUFFERS],
            pending_buffer: 0,
            current_buffer: NO_BUFFER,
            pending_damage: [DamageRect::invalid(); MAX_PENDING_DAMAGE],
            pending_damage_count: 0,
            committed_damage: [DamageRect::invalid(); MAX_PENDING_DAMAGE],
            committed_damage_count: 0,
            dirty: false,
            window_x: 0,
            window_y: 0,
            z_order: 0,
            visible: false,
            window_state: WindowState::Normal,
            generation: 0,
            frame_callback_pending: false,
            role: SurfaceRole::None,
            title: [0u8; WINDOW_STRING_MAX],
            app_id: [0u8; WINDOW_STRING_MAX],
            size_limits: ((0, 0), (0, 0)),
            placed_at: 0,
            cursor_shape: 0,
            last_enter_serial: 0,
            has_pointer: false,
            last_key_serial: 0,
            has_keyboard_focus: false,
        }
    }
}

/// A read-only mapping of the source memfd, which owns it, while there is
/// one.
struct Clipboard {
    source: Option<CachedShmMapping>,
    len: u32,
}

pub struct ProtocolBridge {
    server: Server,
    surfaces: [ProtocolSurface; MAX_SURFACES],
    next_z_order: u32,
    next_surface_gen: u32,
    clipboard: Clipboard,
    display_width: u32,
    display_height: u32,
    display_format: u32,
    display_pitch: u32,
    /// Where windows are placed: the screen between the menu bar and the dock.
    work_area: Rect,
    placements: u64,
    /// Slots and fds are recycled, so without this a stale per-client task
    /// could drive or tear down a successor's connection.
    client_gen: [u64; MAX_CLIENTS],
    next_gen: u64,
    client_pid: [u32; MAX_CLIENTS],
    /// Fired when a connection is torn down, so its own task drops the armed
    /// poll that otherwise holds the socket open past the close.
    hangups: [Notify; MAX_CLIENTS],
}

impl ProtocolBridge {
    pub fn new() -> Option<Self> {
        let path = slopos_protocol::socket_path();
        // A dead compositor's name: only the session writes its runtime
        // directory.
        let _ = std::fs::remove_file(OsStr::from_bytes(&path));
        let server = Server::bind(&path).ok()?;
        log!("protocol bridge listening on {}", printable(&path));

        Some(Self {
            server,
            surfaces: [const { ProtocolSurface::empty() }; MAX_SURFACES],
            next_z_order: 1,
            next_surface_gen: 1,
            clipboard: Clipboard {
                source: None,
                len: 0,
            },
            display_width: 0,
            display_height: 0,
            display_format: 0,
            display_pitch: 0,
            work_area: Rect::EMPTY,
            placements: 0,
            client_gen: [0u64; MAX_CLIENTS],
            next_gen: 1,
            client_pid: [0; MAX_CLIENTS],
            hangups: core::array::from_fn(|_| Notify::new()),
        })
    }

    pub fn set_display_info(&mut self, width: u32, height: u32, format: u32, pitch: u32) {
        self.display_width = width;
        self.display_height = height;
        self.display_format = format;
        self.display_pitch = pitch;
    }

    pub fn set_work_area(&mut self, area: Rect) {
        self.work_area = area;
    }

    /// Title bar included.
    fn frame_of(&self, idx: usize) -> Rect {
        let s = &self.surfaces[idx];
        let width = if s.frame_width > 0 {
            s.frame_width
        } else {
            s.width
        };
        let height = if s.frame_height > 0 {
            s.frame_height
        } else {
            s.height
        };
        Rect::new(
            s.window_x,
            s.window_y - TITLE_BAR_HEIGHT,
            width as i32,
            height as i32 + TITLE_BAR_HEIGHT,
        )
    }

    /// Place the toplevel at `idx` as it first shows: cascaded from its app's
    /// latest window, by app id or else by client, or centred; shrunk to fit,
    /// which the client hears as a configure.
    fn place(&mut self, idx: usize) {
        self.placements += 1;
        if self.work_area.is_empty() {
            self.surfaces[idx].placed_at = self.placements;
            return;
        }
        let me = &self.surfaces[idx];
        let same_app = |other: &ProtocolSurface| match me.app_id[0] {
            0 => other.client_idx == me.client_idx,
            _ => other.app_id == me.app_id,
        };
        let latest = (0..MAX_SURFACES)
            .filter(|&i| i != idx)
            .filter(|&i| {
                let other = &self.surfaces[i];
                other.active
                    && other.visible
                    && other.role == SurfaceRole::Toplevel
                    && other.placed_at > 0
                    && same_app(other)
            })
            .max_by_key(|&i| self.surfaces[i].placed_at)
            .map(|i| self.frame_of(i));
        let wanted = self.frame_of(idx);
        let (min_w, min_h) = self.limited(idx, 0, 0);
        let min = Size {
            w: min_w,
            h: min_h + TITLE_BAR_HEIGHT,
        };
        let frame = placement::place(
            self.work_area,
            TITLE_BAR_HEIGHT,
            Size {
                w: wanted.w,
                h: wanted.h,
            },
            min,
            latest,
        );
        let surface = &mut self.surfaces[idx];
        surface.placed_at = self.placements;
        surface.window_x = frame.x;
        surface.window_y = frame.y + TITLE_BAR_HEIGHT;
        if (frame.w, frame.h) != (wanted.w, wanted.h) {
            let (w, h) = (frame.w as u32, (frame.h - TITLE_BAR_HEIGHT) as u32);
            surface.frame_width = w;
            surface.frame_height = h;
            self.send_configure(idx, w, h, slopos_protocol::toplevel_state::ACTIVATED);
        }
    }

    fn send_output_info(&mut self, idx: usize) {
        let _ = self.server.queue_event(
            idx,
            &Event::OutputInfo {
                width: self.display_width,
                height: self.display_height,
                format: self.display_format,
                pitch: self.display_pitch,
                scale: 1,
            },
        );
    }

    pub fn listen_fd(&self) -> i32 {
        self.server.listen_fd()
    }

    pub fn client_fd(&self, idx: usize) -> Option<i32> {
        self.server
            .clients
            .get(idx)
            .and_then(|slot| slot.as_ref().filter(|c| c.active).map(|c| c.conn.fd()))
    }

    /// With [`client_fd`], names one connection.
    pub fn client_gen(&self, idx: usize) -> Option<u64> {
        if self.server.is_connected(idx) {
            self.client_gen.get(idx).copied()
        } else {
            None
        }
    }

    pub fn hangup(&self, idx: usize) -> Option<Notify> {
        self.hangups.get(idx).cloned()
    }

    /// Greet every waiting connection and name each in `out` as
    /// `(client_idx, client_fd, client_gen)`; how many.
    pub fn accept_and_collect(&mut self, out: &mut [(usize, i32, u64)]) -> usize {
        let mut n = 0;
        while n < out.len() {
            match self.server.accept() {
                Ok(Some(idx)) => {
                    let _ = self.server.greet(idx);
                    let generation = self.next_gen;
                    self.next_gen = self.next_gen.wrapping_add(1);
                    let fd = self.client_fd(idx).unwrap_or(-1);
                    if idx < MAX_CLIENTS {
                        self.client_gen[idx] = generation;
                        self.hangups[idx] = Notify::new();
                        self.client_pid[idx] = crate::syscall::net::peer_cred(fd)
                            .map_or(0, |cred| cred.pid.max(0) as u32);
                    }
                    out[n] = (idx, fd, generation);
                    n += 1;
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
        n
    }

    pub fn process_requests(&mut self) {
        for client_idx in 0..MAX_CLIENTS {
            self.process_client(client_idx);
        }
    }

    /// `false` once the client is gone, torn down already.
    pub fn process_client(&mut self, client_idx: usize) -> bool {
        if !self.server.is_connected(client_idx) {
            return false;
        }
        loop {
            match self.server.recv_request(client_idx) {
                Ok(Some(req)) => {
                    if let Err(err) = self.handle_request(client_idx, req) {
                        self.fail_client(client_idx, err);
                        return false;
                    }
                }
                Ok(None) => break,
                Err(ProtocolError::Disconnected) => {
                    self.cleanup_client(client_idx);
                    return false;
                }
                // Fatal, as in Wayland: what could not be decoded would
                // otherwise be read again forever.
                Err(err) => {
                    self.fail_client(client_idx, err);
                    return false;
                }
            }
        }
        true
    }

    fn fail_client(&mut self, client_idx: usize, err: ProtocolError) {
        let _ = self.server.queue_event(
            client_idx,
            &Event::Error {
                object_id: 0,
                code: err as u32,
            },
        );
        self.server.flush_clients();
        self.cleanup_client(client_idx);
    }

    fn handle_request(&mut self, client_idx: usize, req: Request) -> Result<(), ProtocolError> {
        match req {
            Request::Hello { .. } => self.send_output_info(client_idx),
            Request::CreateSurface { new_id } => self.handle_create_surface(client_idx, new_id)?,
            Request::SurfaceAttach {
                surface,
                buffer_id,
                width,
                height,
                has_fd: _,
                buffer_fd,
            } => {
                let fd = buffer_fd.map(|f| f.into_raw());
                self.handle_surface_attach(client_idx, surface, buffer_id, fd, width, height);
            }
            Request::SurfaceDamage {
                surface,
                x,
                y,
                w,
                h,
            } => {
                self.handle_surface_damage(client_idx, surface, x, y, w, h);
            }
            Request::SurfaceCommit { surface } => {
                self.handle_surface_commit(client_idx, surface);
            }
            Request::SurfaceFrame { surface } => {
                self.handle_surface_frame(client_idx, surface);
            }
            Request::SurfaceDestroy { surface } => {
                self.handle_surface_destroy(client_idx, surface);
            }
            Request::GetToplevel { surface, new_id } => {
                self.handle_get_toplevel(client_idx, surface, new_id);
            }
            Request::ToplevelSetTitle {
                toplevel,
                title,
                len,
            } => {
                self.handle_toplevel_set_title(client_idx, toplevel, &title, len as usize);
            }
            Request::ToplevelSetAppId {
                toplevel,
                app_id,
                len,
            } => {
                self.handle_toplevel_set_app_id(client_idx, toplevel, &app_id, len as usize);
            }
            Request::ToplevelDestroy { toplevel } => {
                self.handle_toplevel_destroy(client_idx, toplevel);
            }
            Request::SetCursorShape {
                surface,
                serial,
                shape,
            } => {
                self.handle_set_cursor_shape(client_idx, surface, serial, shape);
            }
            Request::ToplevelSetSizeLimits {
                toplevel,
                min_width,
                min_height,
                max_width,
                max_height,
            } => {
                if let Some(idx) = self.find_surface_by_toplevel(client_idx, toplevel) {
                    let cap = |v: u32| v.min(SIZE_LIMIT_MAX);
                    self.surfaces[idx].size_limits = (
                        (cap(min_width), cap(min_height)),
                        (cap(max_width), cap(max_height)),
                    );
                }
            }
            Request::ClipboardCopy {
                len,
                serial,
                buffer_fd,
            } => {
                self.handle_clipboard_copy(client_idx, serial, buffer_fd, len);
            }
            Request::ClipboardPaste { serial } => {
                self.handle_clipboard_paste(client_idx, serial);
            }
            Request::ClipboardRead {
                len,
                serial,
                buffer_fd,
            } => {
                self.handle_clipboard_read(client_idx, serial, buffer_fd, len);
            }
        }
        Ok(())
    }

    fn handle_create_surface(
        &mut self,
        client_idx: usize,
        new_id: SurfaceId,
    ) -> Result<(), ProtocolError> {
        if new_id == SurfaceId::NONE || self.find_surface(client_idx, new_id).is_some() {
            return Err(ProtocolError::MalformedMessage);
        }
        let slot = self
            .surfaces
            .iter()
            .position(|s| !s.active)
            .ok_or(ProtocolError::BufferFull)?;

        self.surfaces[slot] = ProtocolSurface::empty();
        self.surfaces[slot].active = true;
        self.surfaces[slot].client_idx = client_idx;
        self.surfaces[slot].surface_id = new_id;
        self.surfaces[slot].z_order = self.next_z_order;
        self.next_z_order = self.next_z_order.wrapping_add(1).max(1);
        self.surfaces[slot].generation = self.next_surface_gen;
        self.next_surface_gen = self.next_surface_gen.wrapping_add(1).max(1);
        Ok(())
    }

    /// `fd` comes with a slot's first attach alone; one that has no slot to go
    /// to is closed.
    fn handle_surface_attach(
        &mut self,
        client_idx: usize,
        surface_id: SurfaceId,
        buffer_id: u32,
        fd: Option<i32>,
        width: u32,
        height: u32,
    ) {
        let bid = buffer_id as usize;
        let idx = match self.find_surface(client_idx, surface_id) {
            Some(i) if bid < MAX_SURFACE_BUFFERS => i,
            _ => {
                if let Some(fd) = fd {
                    crate::syscall::memory::close(fd);
                }
                return;
            }
        };

        let surface = &mut self.surfaces[idx];
        surface.pending_buffer = buffer_id as u8;
        let slot = &mut surface.buffers[bid];
        if let Some(new_fd) = fd {
            if slot.registered && slot.fd >= 0 && slot.fd != new_fd {
                crate::syscall::memory::close(slot.fd);
            }
            slot.fd = new_fd;
            slot.registered = true;
        }
        slot.width = width;
        slot.height = height;
    }

    fn handle_surface_damage(
        &mut self,
        client_idx: usize,
        surface_id: SurfaceId,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) {
        if let Some(idx) = self.find_surface(client_idx, surface_id) {
            let surface = &mut self.surfaces[idx];
            let di = surface.pending_damage_count as usize;
            if di < MAX_PENDING_DAMAGE {
                surface.pending_damage[di] = DamageRect {
                    x0: x,
                    y0: y,
                    x1: x + w - 1,
                    y1: y + h - 1,
                };
                surface.pending_damage_count += 1;
            } else {
                surface.pending_damage[0] = DamageRect {
                    x0: 0,
                    y0: 0,
                    x1: surface.width.saturating_sub(1) as i32,
                    y1: surface.height.saturating_sub(1) as i32,
                };
                surface.pending_damage_count = 1;
            }
        }
    }

    fn handle_surface_commit(&mut self, client_idx: usize, surface_id: SurfaceId) {
        let Some(idx) = self.find_surface(client_idx, surface_id) else {
            return;
        };

        let release_prev = {
            let surface = &mut self.surfaces[idx];
            let prev = surface.current_buffer;
            let pend = surface.pending_buffer;
            surface.current_buffer = pend;

            let slot = surface.buffers[pend as usize];
            if slot.registered {
                surface.buffer_fd = slot.fd as u32;
                surface.width = slot.width;
                surface.height = slot.height;
                if surface.width > 0 && surface.height > 0 {
                    surface.visible = true;
                }
            }

            surface.committed_damage = surface.pending_damage;
            surface.committed_damage_count = surface.pending_damage_count;
            surface.pending_damage = [DamageRect::invalid(); MAX_PENDING_DAMAGE];
            surface.pending_damage_count = 0;
            surface.dirty = true;

            if prev != NO_BUFFER && prev != pend {
                Some(prev)
            } else {
                None
            }
        };

        let surface = &self.surfaces[idx];
        if surface.visible && surface.role == SurfaceRole::Toplevel && surface.placed_at == 0 {
            self.place(idx);
            log!("shown {}", printable(&self.surfaces[idx].app_id));
        }

        if let Some(prev) = release_prev {
            let _ = self.server.queue_event(
                client_idx,
                &Event::BufferRelease {
                    surface: surface_id,
                    buffer_id: prev as u32,
                },
            );
        }
    }

    fn handle_surface_frame(&mut self, client_idx: usize, surface_id: SurfaceId) {
        if let Some(idx) = self.find_surface(client_idx, surface_id) {
            self.surfaces[idx].frame_callback_pending = true;
        }
    }

    fn handle_surface_destroy(&mut self, client_idx: usize, surface_id: SurfaceId) {
        if let Some(idx) = self.find_surface(client_idx, surface_id) {
            self.destroy_surface(idx);
        }
    }

    /// Only for the surface the pointer is over, with its latest enter serial.
    fn handle_set_cursor_shape(
        &mut self,
        client_idx: usize,
        surface_id: SurfaceId,
        serial: u32,
        shape: u8,
    ) {
        if let Some(idx) = self.find_surface(client_idx, surface_id) {
            let s = &mut self.surfaces[idx];
            if s.has_pointer && serial != 0 && serial == s.last_enter_serial {
                s.cursor_shape = shape;
            }
        }
    }

    fn handle_get_toplevel(
        &mut self,
        client_idx: usize,
        surface_id: SurfaceId,
        new_id: ToplevelId,
    ) {
        let idx = match self.find_surface(client_idx, surface_id) {
            Some(i) => i,
            None => return,
        };

        if self.surfaces[idx].role != SurfaceRole::None {
            return;
        }

        self.surfaces[idx].role = SurfaceRole::Toplevel;
        self.surfaces[idx].toplevel_id = new_id;
    }

    fn handle_toplevel_set_title(
        &mut self,
        client_idx: usize,
        toplevel_id: ToplevelId,
        title: &[u8; WINDOW_STRING_MAX],
        title_len: usize,
    ) {
        if let Some(idx) = self.find_surface_by_toplevel(client_idx, toplevel_id) {
            let copy_len = title_len.min(WINDOW_STRING_MAX);
            self.surfaces[idx].title[..copy_len].copy_from_slice(&title[..copy_len]);
            if copy_len < WINDOW_STRING_MAX {
                self.surfaces[idx].title[copy_len..].fill(0);
            }
        }
    }

    fn handle_toplevel_set_app_id(
        &mut self,
        client_idx: usize,
        toplevel_id: ToplevelId,
        app_id: &[u8; WINDOW_STRING_MAX],
        app_id_len: usize,
    ) {
        if let Some(idx) = self.find_surface_by_toplevel(client_idx, toplevel_id) {
            let copy_len = app_id_len.min(WINDOW_STRING_MAX);
            self.surfaces[idx].app_id[..copy_len].copy_from_slice(&app_id[..copy_len]);
            if copy_len < WINDOW_STRING_MAX {
                self.surfaces[idx].app_id[copy_len..].fill(0);
            }
        }
    }

    fn handle_toplevel_destroy(&mut self, client_idx: usize, toplevel_id: ToplevelId) {
        if let Some(idx) = self.find_surface_by_toplevel(client_idx, toplevel_id) {
            self.surfaces[idx].role = SurfaceRole::None;
            self.surfaces[idx].toplevel_id = ToplevelId::NONE;
        }
    }

    /// The mapping keeps the memfd alive once the client closes its copy.
    fn handle_clipboard_copy(
        &mut self,
        client_idx: usize,
        serial: u32,
        buffer_fd: Option<OwnedFd>,
        len: u32,
    ) {
        let Some(fd) = buffer_fd else { return };
        if !self.client_holds_keyboard_serial(client_idx, serial) {
            return;
        }
        let len = len.min(MAX_CLIPBOARD_BYTES);
        if len == 0 {
            self.clipboard.source = None;
            self.clipboard.len = 0;
            return;
        }
        let raw = fd.into_raw();
        match CachedShmMapping::map_readonly_fd(raw, len as usize) {
            Some(mapping) => {
                self.clipboard.source = Some(mapping);
                self.clipboard.len = len;
            }
            None => {
                crate::syscall::memory::close(raw);
                self.clipboard.source = None;
                self.clipboard.len = 0;
            }
        }
    }

    /// The client sends a memfd to copy into next, since an event carries
    /// none. A refusal is answered too, as an empty clipboard, or the client
    /// would wait for ever.
    fn handle_clipboard_paste(&mut self, client_idx: usize, serial: u32) {
        let len = if self.client_holds_keyboard_serial(client_idx, serial) {
            self.clipboard.len
        } else {
            0
        };
        let _ = self
            .server
            .queue_event(client_idx, &Event::PasteReady { len });
    }

    /// `wl_data_device`'s rule: the clipboard is reached by an input event
    /// the person made, so a process that merely connected can neither read
    /// what was copied nor replace it.
    fn client_holds_keyboard_serial(&self, client_idx: usize, serial: u32) -> bool {
        serial != 0
            && self.surfaces.iter().any(|s| {
                s.active
                    && s.client_idx == client_idx
                    && s.has_keyboard_focus
                    && s.last_key_serial == serial
            })
    }

    fn handle_clipboard_read(
        &mut self,
        client_idx: usize,
        serial: u32,
        buffer_fd: Option<OwnedFd>,
        dst_len: u32,
    ) {
        let Some(fd) = buffer_fd else { return };
        if !self.client_holds_keyboard_serial(client_idx, serial) {
            return;
        }
        let dst_len = dst_len.min(MAX_CLIPBOARD_BYTES) as usize;
        let raw = fd.into_raw();
        let mut copied = 0u32;
        match CachedShmMapping::map_writable_fd(raw, dst_len) {
            Some(mut dst) => {
                if let Some(src) = self.clipboard.source.as_ref() {
                    let n = (self.clipboard.len as usize).min(dst_len);
                    dst.as_mut_slice()[..n].copy_from_slice(&src.as_slice()[..n]);
                    copied = n as u32;
                }
            }
            None => {
                crate::syscall::memory::close(raw);
            }
        }
        let _ = self
            .server
            .queue_event(client_idx, &Event::PasteResult { len: copied });
    }

    pub fn mark_frames_done(&mut self, timestamp_ms: u64) {
        for i in 0..MAX_SURFACES {
            let surface = &mut self.surfaces[i];
            if surface.active && surface.frame_callback_pending && surface.visible {
                surface.frame_callback_pending = false;

                let _ = self.server.queue_event(
                    surface.client_idx,
                    &Event::FrameDone {
                        surface: surface.surface_id,
                        timestamp_ms: timestamp_ms as u32,
                    },
                );
            }
        }
    }

    pub fn send_configure(&mut self, surface_idx: usize, width: u32, height: u32, states: u32) {
        let surface = match self.surfaces.get(surface_idx) {
            Some(s) if s.active && s.toplevel_id != ToplevelId::NONE => s,
            _ => return,
        };

        let client_idx = surface.client_idx;
        let toplevel = surface.toplevel_id;
        let _ = self.server.queue_event(
            client_idx,
            &Event::Configure {
                toplevel,
                width,
                height,
                states,
            },
        );
    }

    pub fn send_close(&mut self, surface_idx: usize) {
        let surface = match self.surfaces.get(surface_idx) {
            Some(s) if s.active && s.toplevel_id != ToplevelId::NONE => s,
            _ => return,
        };

        let client_idx = surface.client_idx;
        let toplevel = surface.toplevel_id;
        let _ = self
            .server
            .queue_event(client_idx, &Event::Close { toplevel });
    }

    /// What a later `SetCursorShape` must carry.
    pub fn send_pointer_enter(&mut self, surface_idx: usize, serial: u32, x: i32, y: i32) {
        let surface = match self.surfaces.get_mut(surface_idx) {
            Some(s) if s.active => s,
            _ => return,
        };

        surface.last_enter_serial = serial;
        surface.has_pointer = true;
        let client_idx = surface.client_idx;
        let surface_id = surface.surface_id;
        let _ = self.server.queue_event(
            client_idx,
            &Event::PointerEnter {
                surface: surface_id,
                serial,
                x,
                y,
            },
        );
    }

    pub fn send_pointer_leave(&mut self, surface_idx: usize) {
        let surface = match self.surfaces.get_mut(surface_idx) {
            Some(s) if s.active => s,
            _ => return,
        };

        surface.has_pointer = false;
        let client_idx = surface.client_idx;
        let surface_id = surface.surface_id;
        let _ = self.server.queue_event(
            client_idx,
            &Event::PointerLeave {
                surface: surface_id,
            },
        );
    }

    pub fn send_pointer_motion(&mut self, surface_idx: usize, time: u32, x: i32, y: i32) {
        let surface = match self.surfaces.get(surface_idx) {
            Some(s) if s.active => s,
            _ => return,
        };

        let client_idx = surface.client_idx;
        let _ = self
            .server
            .queue_event(client_idx, &Event::PointerMotion { time, x, y });
    }

    pub fn send_pointer_button(
        &mut self,
        surface_idx: usize,
        serial: u32,
        time: u32,
        button: u32,
        state: u32,
    ) {
        let surface = match self.surfaces.get(surface_idx) {
            Some(s) if s.active => s,
            _ => return,
        };

        let client_idx = surface.client_idx;
        let _ = self.server.queue_event(
            client_idx,
            &Event::PointerButton {
                serial,
                time,
                button,
                pressed: state != 0,
            },
        );
    }

    pub fn send_pointer_axis(&mut self, surface_idx: usize, time: u32, axis: u32, value: i32) {
        let surface = match self.surfaces.get(surface_idx) {
            Some(s) if s.active => s,
            _ => return,
        };

        let client_idx = surface.client_idx;
        let _ = self
            .server
            .queue_event(client_idx, &Event::PointerAxis { time, axis, value });
    }

    pub fn send_modifiers(&mut self, surface_idx: usize, mods: u32) {
        let surface = match self.surfaces.get(surface_idx) {
            Some(s) if s.active => s,
            _ => return,
        };

        let client_idx = surface.client_idx;
        let _ = self
            .server
            .queue_event(client_idx, &Event::Modifiers { mods });
    }

    pub fn send_modifiers_for_window(&mut self, window: u32, mods: u8) {
        if let Some(idx) = self.surface_of(window) {
            self.send_modifiers(idx, mods as u32);
        }
    }

    /// Modifiers go first: `wl_keyboard` has a client judge a key by the
    /// modifiers as they are, not as the last event left them.
    pub fn forward_key_event(
        &mut self,
        focused_window: u32,
        event: &slopos_abi::InputEvent,
        pressed: bool,
        modifier_state: u8,
        serial: &mut u32,
    ) {
        let Some(idx) = self.surface_of(focused_window) else {
            if pressed {
                log!(
                    "key {:#06x} dropped (no surface for focused window {focused_window})",
                    event.key_keycode()
                );
            }
            return;
        };
        *serial = serial.wrapping_add(1);
        for (i, s) in self.surfaces.iter_mut().enumerate() {
            s.has_keyboard_focus = i == idx && s.active;
        }
        self.surfaces[idx].last_key_serial = *serial;
        self.send_modifiers(idx, modifier_state as u32);
        let _ = self.server.queue_event(
            self.surfaces[idx].client_idx,
            &Event::Key {
                serial: *serial,
                time: event.timestamp_ms as u32,
                keycode: event.key_keycode() as u32,
                codepoint: slopos_chrome_core::key::text(event).map_or(0, u32::from),
                modifiers: modifier_state as u32,
                pressed,
            },
        );
    }

    pub fn send_pointer_motion_for_window(&mut self, window: u32, time: u32, x: i32, y: i32) {
        if let Some(idx) = self.surface_of(window) {
            let local_x = x - self.surfaces[idx].window_x;
            let local_y = y - self.surfaces[idx].window_y;
            self.send_pointer_motion(idx, time, local_x, local_y);
        }
    }

    pub fn send_pointer_button_for_window(
        &mut self,
        window: u32,
        serial: u32,
        time: u32,
        button: u32,
        state: u32,
    ) {
        if let Some(idx) = self.surface_of(window) {
            self.send_pointer_button(idx, serial, time, button, state);
        }
    }

    pub fn send_pointer_axis_for_window(&mut self, window: u32, time: u32, axis: u32, value: i32) {
        if let Some(idx) = self.surface_of(window) {
            self.send_pointer_axis(idx, time, axis, value);
        }
    }

    pub fn send_pointer_enter_for_window(&mut self, window: u32, serial: u32, x: i32, y: i32) {
        if let Some(idx) = self.surface_of(window) {
            let local_x = x - self.surfaces[idx].window_x;
            let local_y = y - self.surfaces[idx].window_y;
            self.send_pointer_enter(idx, serial, local_x, local_y);
        }
    }

    pub fn send_pointer_leave_for_window(&mut self, window: u32) {
        if let Some(idx) = self.surface_of(window) {
            self.send_pointer_leave(idx);
        }
    }

    /// The shown toplevels, bottom first.
    pub fn get_windows(&self, out: &mut [WindowInfo]) -> u32 {
        let mut indices = [0usize; MAX_SURFACES];
        let mut count = 0usize;

        for i in 0..MAX_SURFACES {
            let s = &self.surfaces[i];
            if s.active && s.visible && s.role == SurfaceRole::Toplevel {
                indices[count] = i;
                count += 1;
            }
        }

        for i in 1..count {
            let key = indices[i];
            let key_z = self.surfaces[key].z_order;
            let mut j = i;
            while j > 0 && self.surfaces[indices[j - 1]].z_order > key_z {
                indices[j] = indices[j - 1];
                j -= 1;
            }
            indices[j] = key;
        }

        let write_count = count.min(out.len());
        for i in 0..write_count {
            let s = &self.surfaces[indices[i]];
            let mut info = WindowInfo::default();
            info.id = indices[i] as u32 + 1;
            info.pid = self.client_pid[s.client_idx];
            info.x = s.window_x;
            info.y = s.window_y;
            info.width = s.width;
            info.height = s.height;
            info.state = s.window_state;
            info.buffer_fd = s.buffer_fd;
            info.buffer_id = if s.current_buffer == NO_BUFFER {
                0
            } else {
                s.current_buffer
            };
            info.generation = s.generation;
            info.cursor_shape = s.cursor_shape;
            info.frame_width = s.frame_width;
            info.frame_height = s.frame_height;
            let ((min_w, min_h), (max_w, max_h)) = s.size_limits;
            info.fixed_size = min_w > 0 && min_h > 0 && (min_w, min_h) == (max_w, max_h);
            info.title = s.title;
            info.app_id = AppId(s.app_id);

            if s.dirty {
                if s.committed_damage_count == 0 {
                    info.damage_count = u8::MAX;
                } else {
                    let dc = (s.committed_damage_count as usize).min(MAX_DAMAGE_REGIONS);
                    info.damage_count = dc as u8;
                    for j in 0..dc {
                        info.damage_regions[j] = s.committed_damage[j];
                    }
                }
            }

            out[i] = info;
        }

        write_count as u32
    }

    /// Only what reached the screen: a commit after this frame's snapshot
    /// stays dirty for the next.
    pub fn clear_presented(&mut self, presented: &[u32]) {
        for &window in presented {
            if let Some(idx) = self.surface_of(window) {
                self.surfaces[idx].dirty = false;
                self.surfaces[idx].committed_damage_count = 0;
            }
        }
    }

    pub fn set_window_position(&mut self, window: u32, x: i32, y: i32) {
        if let Some(idx) = self.surface_of(window) {
            self.surfaces[idx].window_x = x;
            self.surfaces[idx].window_y = y;
        }
    }

    pub fn set_window_size(&mut self, window: u32, w: u32, h: u32) {
        if let Some(idx) = self.surface_of(window) {
            self.surfaces[idx].frame_width = w;
            self.surfaces[idx].frame_height = h;
        }
    }

    /// `(w, h)` held to the compositor's least window and the window's own
    /// limits.
    pub fn clamp_window_size(&self, window: u32, w: i32, h: i32) -> (i32, i32) {
        match self.surface_of(window) {
            Some(idx) => self.limited(idx, w, h),
            None => (w.max(MIN_WINDOW_WIDTH), h.max(MIN_WINDOW_HEIGHT)),
        }
    }

    fn limited(&self, idx: usize, w: i32, h: i32) -> (i32, i32) {
        let ((min_w, min_h), (max_w, max_h)) = self.surfaces[idx].size_limits;
        let clamp = |v: i32, floor: i32, min: u32, max: u32| {
            let v = v.max(floor);
            let v = if max > 0 { v.min(max as i32) } else { v };
            v.max(min as i32)
        };
        (
            clamp(w, MIN_WINDOW_WIDTH, min_w, max_w),
            clamp(h, MIN_WINDOW_HEIGHT, min_h, max_h),
        )
    }

    pub fn set_window_state(&mut self, window: u32, state: WindowState) {
        if let Some(idx) = self.surface_of(window) {
            self.surfaces[idx].window_state = state;
        }
    }

    pub fn raise_window(&mut self, window: u32) {
        if let Some(idx) = self.surface_of(window) {
            self.surfaces[idx].z_order = self.next_z_order;
            self.next_z_order = self.next_z_order.wrapping_add(1).max(1);
        }
    }

    pub fn send_configure_for_window(&mut self, window: u32, width: u32, height: u32, states: u32) {
        if let Some(idx) = self.surface_of(window) {
            self.send_configure(idx, width, height, states);
        }
    }

    pub fn send_close_for_window(&mut self, window: u32) {
        if let Some(idx) = self.surface_of(window) {
            self.send_close(idx);
        }
    }

    /// Reap the clients that closed without a word, and those a write found
    /// gone.
    pub fn cleanup_disconnected(&mut self) {
        for idx in 0..MAX_CLIENTS {
            self.server.probe_disconnected(idx);
        }
        self.reap_disconnected_clients();
    }

    /// A client a write finds gone is reaped here, so its windows go within
    /// the frame.
    pub fn flush_all(&mut self) {
        self.server.flush_clients();
        self.reap_disconnected_clients();
    }

    pub fn disconnect_client(&mut self, client_idx: usize) {
        if self.server.is_connected(client_idx) {
            self.cleanup_client(client_idx);
        }
    }

    /// The one place a client's slot is freed, after its surfaces, so a live
    /// surface always has a live connection.
    fn cleanup_client(&mut self, client_idx: usize) {
        for i in 0..MAX_SURFACES {
            if self.surfaces[i].active && self.surfaces[i].client_idx == client_idx {
                self.destroy_surface(i);
            }
        }
        self.server.disconnect(client_idx);
        if let Some(hangup) = self.hangups.get(client_idx) {
            hangup.notify_one();
        }
    }

    fn reap_disconnected_clients(&mut self) {
        let mut dead = [0usize; MAX_CLIENTS];
        let n = self.server.take_disconnected(&mut dead);
        for &idx in &dead[..n] {
            self.cleanup_client(idx);
        }
    }

    fn find_surface(&self, client_idx: usize, surface_id: SurfaceId) -> Option<usize> {
        self.surfaces
            .iter()
            .position(|s| s.active && s.client_idx == client_idx && s.surface_id == surface_id)
    }

    fn find_surface_by_toplevel(
        &self,
        client_idx: usize,
        toplevel_id: ToplevelId,
    ) -> Option<usize> {
        self.surfaces
            .iter()
            .position(|s| s.active && s.client_idx == client_idx && s.toplevel_id == toplevel_id)
    }

    /// What tells the surface `window` named once from the one it names now.
    pub fn surface_generation_for_window(&self, window: u32) -> Option<u32> {
        self.surface_of(window)
            .map(|idx| self.surfaces[idx].generation)
    }

    fn surface_of(&self, window: u32) -> Option<usize> {
        let idx = (window as usize).checked_sub(1)?;
        if idx < MAX_SURFACES && self.surfaces[idx].active {
            Some(idx)
        } else {
            None
        }
    }

    fn destroy_surface(&mut self, idx: usize) {
        if idx >= MAX_SURFACES || !self.surfaces[idx].active {
            return;
        }
        // A mapping the surface cache still holds stays valid once the fd is
        // closed.
        for slot in &mut self.surfaces[idx].buffers {
            if slot.registered && slot.fd >= 0 {
                crate::syscall::memory::close(slot.fd);
            }
            *slot = SurfaceBuffer::EMPTY;
        }

        self.surfaces[idx] = ProtocolSurface::empty();
    }
}

/// Up to its NUL, every byte but visible ASCII shown as `?`, so a log line
/// carrying it stays one line of ours.
fn printable(name: &[u8]) -> String {
    name.iter()
        .take_while(|&&b| b != 0)
        .map(|&b| if b.is_ascii_graphic() { b as char } else { '?' })
        .collect()
}
