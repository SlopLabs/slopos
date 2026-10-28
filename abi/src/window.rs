use crate::damage::DamageRect;

pub use crate::damage::MAX_DAMAGE_REGIONS as MAX_WINDOW_DAMAGE_REGIONS;

/// The seats `screen_acquire` and `input_sink_acquire` take, by rank: the
/// higher wins, so the kernel log and `/bin/roulette` can always take the
/// display back from the compositor.
pub const SEAT_COMPOSITOR_PRIMARY: u32 = 0;
pub const SEAT_VIRTCON: u32 = 1;

pub const CURSOR_SHAPE_DEFAULT: u8 = 0;
pub const CURSOR_SHAPE_TEXT: u8 = 1;
pub const CURSOR_SHAPE_POINTER: u8 = 2;
pub const CURSOR_SHAPE_N_RESIZE: u8 = 3;
pub const CURSOR_SHAPE_S_RESIZE: u8 = 4;
pub const CURSOR_SHAPE_W_RESIZE: u8 = 5;
pub const CURSOR_SHAPE_E_RESIZE: u8 = 6;
pub const CURSOR_SHAPE_NW_RESIZE: u8 = 7;
pub const CURSOR_SHAPE_NE_RESIZE: u8 = 8;
pub const CURSOR_SHAPE_SW_RESIZE: u8 = 9;
pub const CURSOR_SHAPE_SE_RESIZE: u8 = 10;
pub const CURSOR_SHAPE_GRAB: u8 = 11;
pub const CURSOR_SHAPE_GRABBING: u8 = 12;

/// The longest window title or app id, on the compositor's wire too.
pub const WINDOW_STRING_MAX: usize = 128;

/// Up to its first NUL, less a character a cut split.
fn text(bytes: &[u8]) -> &str {
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let bytes = &bytes[..len];
    core::str::from_utf8(bytes)
        .unwrap_or_else(|e| core::str::from_utf8(&bytes[..e.valid_up_to()]).unwrap_or(""))
}

/// Wayland's `set_app_id()`: what the compositor matches a window to its dock
/// entry by.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct AppId(pub [u8; WINDOW_STRING_MAX]);

impl AppId {
    pub const EMPTY: Self = Self([0u8; WINDOW_STRING_MAX]);

    /// `s`, cut short at a character boundary to fit.
    pub fn from_str(s: &str) -> Self {
        let mut len = s.len().min(WINDOW_STRING_MAX);
        while !s.is_char_boundary(len) {
            len -= 1;
        }
        let mut buf = [0u8; WINDOW_STRING_MAX];
        buf[..len].copy_from_slice(&s.as_bytes()[..len]);
        Self(buf)
    }

    pub fn as_str(&self) -> &str {
        text(&self.0)
    }
}

impl Default for AppId {
    fn default() -> Self {
        Self::EMPTY
    }
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct WindowInfo {
    pub task_id: u32,
    /// The client's process, as `SO_PEERCRED` named it when it connected.
    pub pid: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub state: u8,
    pub damage_count: u8,
    pub cursor_shape: u8,
    /// The committed double-buffer slot, which the surface cache keys on
    /// rather than on the fd in `shm_token`, which is recycled.
    pub buffer_id: u8,
    pub shm_token: u32,
    /// The surface's incarnation: `task_id` is a slot, which is recycled.
    pub generation: u32,
    pub damage_regions: [DamageRect; MAX_WINDOW_DAMAGE_REGIONS],
    pub title: [u8; WINDOW_STRING_MAX],
    pub app_id: AppId,
    /// The size the compositor last gave the frame, which the buffer may not
    /// match yet; 0 until it gives one.
    pub frame_width: u32,
    pub frame_height: u32,
    /// Asked not to be resized: no edge resizes it and it does not zoom.
    pub fixed_size: bool,
}

impl WindowInfo {
    #[inline]
    pub fn is_dirty(&self) -> bool {
        self.damage_count > 0
    }

    pub fn title_str(&self) -> &str {
        text(&self.title)
    }

    #[inline]
    pub fn effective_width(&self) -> u32 {
        if self.frame_width > 0 {
            self.frame_width
        } else {
            self.width
        }
    }

    #[inline]
    pub fn effective_height(&self) -> u32 {
        if self.frame_height > 0 {
            self.frame_height
        } else {
            self.height
        }
    }
}

impl Default for WindowInfo {
    fn default() -> Self {
        Self {
            task_id: 0,
            pid: 0,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            state: 0,
            damage_count: 0,
            cursor_shape: 0,
            buffer_id: 0,
            shm_token: 0,
            generation: 0,
            damage_regions: [DamageRect::default(); MAX_WINDOW_DAMAGE_REGIONS],
            title: [0; WINDOW_STRING_MAX],
            app_id: AppId::EMPTY,
            frame_width: 0,
            frame_height: 0,
            fixed_size: false,
        }
    }
}
