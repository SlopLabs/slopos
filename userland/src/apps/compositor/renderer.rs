use slopos_abi::draw::{Canvas, Color32};
use slopos_abi::pixel::PixelFormat;
use slopos_gfx::image::{BitmapRef, ImageFit, ImageSampling};
use slopos_gfx::rounded::Rounded;
use slopos_image::{DecodeOptions, Image};
use slopos_protocol::{
    CURSOR_SHAPE_E_RESIZE, CURSOR_SHAPE_GRAB, CURSOR_SHAPE_GRABBING, CURSOR_SHAPE_N_RESIZE,
    CURSOR_SHAPE_NE_RESIZE, CURSOR_SHAPE_NW_RESIZE, CURSOR_SHAPE_S_RESIZE, CURSOR_SHAPE_SE_RESIZE,
    CURSOR_SHAPE_SW_RESIZE, CURSOR_SHAPE_TEXT, CURSOR_SHAPE_W_RESIZE,
};
use slopos_theme_core::with_alpha;

use crate::gfx::{self, DamageRect, DrawBuffer};

use super::confirm::Asking;
use super::consent::Consent;
use super::decorations;
use super::dock::Dock;
use super::frame::{self, draw_window_shadow, paint_in_outline, uncornered};
use super::gate::Gate;
use super::menu_bar::MenuBar;
use super::popover::Popover;
use super::region::{Region, intersection};
use super::surface_cache::ClientSurfaceCache;
use super::window_info::{WindowInfo, WindowState};
use slopos_chrome_core::status::BAR_HEIGHT;

const DEFAULT_WALLPAPER: &str = "/usr/share/slopos/wallpapers/default.png";
const WALLPAPER_SCREEN_DIVISOR: u32 = 3;

/// virtio-gpu's cursor size.
pub const HW_CURSOR_DIM: u32 = 64;
/// Where every shape's click-point is drawn in the overlay.
pub const HW_CURSOR_HOTSPOT: u32 = 20;

struct WallpaperCache {
    load_attempted: bool,
    source: Option<Image>,
    cache: Vec<u8>,
    cache_width: u32,
    cache_height: u32,
    cache_pitch: usize,
    cache_bpp: u8,
    cache_format: PixelFormat,
}

impl WallpaperCache {
    fn new() -> Self {
        Self {
            load_attempted: false,
            source: None,
            cache: Vec::new(),
            cache_width: 0,
            cache_height: 0,
            cache_pitch: 0,
            cache_bpp: 0,
            cache_format: PixelFormat::Argb8888,
        }
    }

    fn draw(&mut self, buf: &mut DrawBuffer, clip: &DamageRect) -> bool {
        if !clip.is_valid() || !self.ensure_cache(buf) {
            return false;
        }
        let clipped = clip.clip(buf.width() as i32, buf.height() as i32);
        if !clipped.is_valid() {
            return false;
        }

        let bpp = buf.bytes_pp() as usize;
        let dst_pitch = buf.pitch();
        let src_pitch = self.cache_pitch;
        let row_bytes = (clipped.x1 - clipped.x0 + 1) as usize * bpp;
        let x_byte = clipped.x0 as usize * bpp;
        let dst_data = buf.data_mut();
        for row in clipped.y0..=clipped.y1 {
            let row = row as usize;
            let src_off = row * src_pitch + x_byte;
            let dst_off = row * dst_pitch + x_byte;
            let src = &self.cache[src_off..src_off + row_bytes];
            dst_data[dst_off..dst_off + row_bytes].copy_from_slice(src);
        }
        buf.add_damage(clipped.x0, clipped.y0, clipped.x1, clipped.y1);
        true
    }

    fn ensure_cache(&mut self, buf: &mut DrawBuffer) -> bool {
        if !self.load_attempted {
            self.load_attempted = true;
            let path =
                std::env::var("SLOPOS_WALLPAPER").unwrap_or_else(|_| DEFAULT_WALLPAPER.into());
            self.source = slopos_image::load_path(path, DecodeOptions::default()).ok();
        }

        let Some(source) = self.source.as_ref() else {
            return false;
        };
        let width = buf.width();
        let height = buf.height();
        let pitch = buf.pitch();
        let bpp = buf.bytes_pp();
        let format = buf.pixel_format();
        if self.cache_width == width
            && self.cache_height == height
            && self.cache_pitch == pitch
            && self.cache_bpp == bpp
            && self.cache_format == format
            && !self.cache.is_empty()
        {
            return true;
        }

        let Some(size) = pitch.checked_mul(height as usize) else {
            self.cache.clear();
            return false;
        };
        self.cache.clear();
        self.cache.resize(size, 0);

        let Some(mut cache_buf) = DrawBuffer::new(&mut self.cache, width, height, pitch, bpp)
        else {
            self.cache.clear();
            return false;
        };
        cache_buf.set_pixel_format(format);
        cache_buf.clear_canvas(format.encode(desktop()));
        let Some(bitmap) = BitmapRef::new(source.width, source.height, &source.pixels) else {
            self.cache.clear();
            return false;
        };
        let (logo_w, logo_h) = slopos_gfx::image::integer_scale_to_fit(
            source.width,
            source.height,
            width / WALLPAPER_SCREEN_DIVISOR,
            height / WALLPAPER_SCREEN_DIVISOR,
        );
        slopos_gfx::image::draw_image(
            &mut cache_buf,
            bitmap,
            (width as i32 - logo_w as i32) / 2,
            (height as i32 - logo_h as i32) / 2,
            logo_w as i32,
            logo_h as i32,
            ImageFit::Stretch,
            ImageSampling::Nearest,
        );

        self.cache_width = width;
        self.cache_height = height;
        self.cache_pitch = pitch;
        self.cache_bpp = bpp;
        self.cache_format = format;
        true
    }
}

/// Behind the wallpaper and every window.
fn desktop() -> Color32 {
    slopos_theme_core::current().surface_sunken
}

pub struct Renderer {
    pub output_width: u32,
    pub output_height: u32,
    pub output_bytes_pp: u8,
    pub output_pitch: usize,
    wallpaper: WallpaperCache,
}

impl Renderer {
    pub fn new() -> Self {
        Self {
            output_width: 0,
            output_height: 0,
            output_bytes_pp: 4,
            output_pitch: 0,
            wallpaper: WallpaperCache::new(),
        }
    }

    pub fn set_output_info(&mut self, width: u32, height: u32, bytes_pp: u8, pitch: usize) {
        self.output_width = width;
        self.output_height = height;
        self.output_bytes_pp = bytes_pp;
        self.output_pitch = pitch;
    }

    /// Paint `frame_damage`, which is disjoint, bottom to top: a window only
    /// where no frame above covers it whole, its shadow and corners blended
    /// over what lies beneath; the lock or login screen alone while it is up.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        buf: &mut DrawBuffer,
        windows: &[WindowInfo],
        focused_window: u32,
        buttons_hovered_window: u32,
        mouse_x: i32,
        mouse_y: i32,
        cursor_shape: u8,
        surface_cache: &mut ClientSurfaceCache,
        menu_bar: &MenuBar,
        dock: &mut Dock,
        popover: &Popover,
        asking: &Asking,
        consent: &Consent,
        gate: &Gate,
        active_app_name: &str,
        frame_damage: &Region,
        hw_cursor: bool,
    ) {
        let theme = slopos_theme_core::current();
        let frame_damage = frame_damage.intersect_rect(&full_screen(buf));
        if frame_damage.is_empty() {
            return;
        }

        if gate.shown() {
            for rect in frame_damage.rects() {
                self.draw_desktop(buf, rect);
                gfx::fill_rect_blended_clipped(
                    buf,
                    rect.x0,
                    rect.y0,
                    rect.x1 - rect.x0 + 1,
                    rect.y1 - rect.y0 + 1,
                    theme.scrim,
                    rect,
                );
                gate.draw(buf, rect);
            }
            self.draw_cursor_over(
                buf,
                &frame_damage,
                mouse_x,
                mouse_y,
                cursor_shape,
                hw_cursor,
            );
            return;
        }

        let order: Vec<&WindowInfo> = windows
            .iter()
            .filter(|w| w.state != WindowState::Minimized)
            .collect();

        let mut exposed = frame_damage.clone();
        let mut content_vis = vec![Region::new(); order.len()];
        let mut shadow_vis = vec![Region::new(); order.len()];
        for (k, w) in order.iter().enumerate().rev() {
            content_vis[k] = exposed.intersect_rect(&frame::frame_box(w));
            shadow_vis[k] = exposed.intersect_rect(&frame::bounds(w));
            for part in &frame::opaque(w) {
                shadow_vis[k].subtract(part);
                exposed.subtract(part);
            }
        }
        for rect in exposed.rects() {
            self.draw_desktop(buf, rect);
        }

        for (k, w) in order.iter().enumerate() {
            let focused = w.id == focused_window;
            if let Some((caster, color)) = frame::shadow(w, focused) {
                for rect in shadow_vis[k].rects() {
                    draw_window_shadow(buf, &caster, color, rect);
                }
            }
            if content_vis[k].is_empty() {
                continue;
            }
            let hovered = w.id == buttons_hovered_window;
            let outline = frame::outline(w);
            for rect in content_vis[k].rects() {
                buf.set_scissor(Some(*rect));
                self.draw_window_content(buf, w, &outline, rect, surface_cache);
                decorations::draw_window_decorations(
                    buf,
                    &outline,
                    w.title_str(),
                    focused,
                    hovered,
                    !w.fixed_size,
                    rect,
                );
                buf.set_scissor(None);
            }
        }

        // The dock's bounds are known only once it has drawn, so it draws
        // into every rect.
        let bar_rect = DamageRect {
            x0: 0,
            y0: 0,
            x1: buf.width() as i32 - 1,
            y1: BAR_HEIGHT,
        };
        for rect in frame_damage.rects() {
            buf.set_scissor(Some(*rect));
            dock.draw(
                buf,
                self.output_width,
                self.output_height,
                mouse_x,
                mouse_y,
                rect,
            );
            if rect.intersects(&bar_rect) {
                menu_bar.draw(
                    buf,
                    self.output_width,
                    active_app_name,
                    mouse_x,
                    mouse_y,
                    rect,
                );
            }
            popover.draw(buf, rect);
            asking.draw(buf, rect);
            consent.draw(buf, rect);
            buf.set_scissor(None);
        }

        self.draw_cursor_over(
            buf,
            &frame_damage,
            mouse_x,
            mouse_y,
            cursor_shape,
            hw_cursor,
        );
    }

    fn draw_desktop(&mut self, buf: &mut DrawBuffer, rect: &DamageRect) {
        if !self.wallpaper.draw(buf, rect) {
            gfx::fill_rect(
                buf,
                rect.x0,
                rect.y0,
                rect.x1 - rect.x0 + 1,
                rect.y1 - rect.y0 + 1,
                desktop(),
            );
        }
    }

    fn draw_cursor_over(
        &self,
        buf: &mut DrawBuffer,
        frame_damage: &Region,
        mouse_x: i32,
        mouse_y: i32,
        cursor_shape: u8,
        hw_cursor: bool,
    ) {
        if hw_cursor {
            return;
        }
        let sprite = Sprite::of(cursor_shape);
        let cursor = sprite.bounds(mouse_x, mouse_y);
        for rect in frame_damage.rects() {
            if rect.intersects(&cursor) {
                sprite.draw(buf, mouse_x, mouse_y, rect);
            }
        }
    }

    /// `cursor_shape` as the hardware cursor's straight-alpha image, of
    /// `HW_CURSOR_DIM` squared pixels with its click-point at the hotspot.
    pub fn render_cursor_image(&self, cursor_shape: u8, out: &mut [u8]) {
        out.fill(0);
        let Some(mut buf) = DrawBuffer::new(
            out,
            HW_CURSOR_DIM,
            HW_CURSOR_DIM,
            (HW_CURSOR_DIM * 4) as usize,
            4,
        ) else {
            return;
        };
        buf.set_pixel_format(PixelFormat::Argb8888);
        let clip = DamageRect {
            x0: 0,
            y0: 0,
            x1: HW_CURSOR_DIM as i32 - 1,
            y1: HW_CURSOR_DIM as i32 - 1,
        };
        let hotspot = HW_CURSOR_HOTSPOT as i32;
        Sprite::of(cursor_shape).draw(&mut buf, hotspot, hotspot, &clip);
    }

    /// The window's last committed buffer, cut to its frame, and the
    /// window's surface colour where the frame has grown past it or before
    /// the buffer can be mapped.
    fn draw_window_content(
        &self,
        buf: &mut DrawBuffer,
        window: &WindowInfo,
        outline: &Rounded,
        clip: &DamageRect,
        surface_cache: &mut ClientSurfaceCache,
    ) {
        let bytes_pp = self.output_bytes_pp as usize;
        // The buffer's size, not the frame's, is its pitch while a resize
        // has the two differ.
        let (buf_w, buf_h) = (window.width as i32, window.height as i32);
        let src_pitch = (buf_w as usize) * bytes_pp;
        let (frame_w, frame_h) = (
            window.effective_width() as i32,
            window.effective_height() as i32,
        );
        let content = DamageRect {
            x0: window.x,
            y0: window.y,
            x1: window.x + frame_w - 1,
            y1: window.y + frame_h - 1,
        };
        let Some(content) = intersection(clip, &content) else {
            return;
        };

        let src = surface_cache
            .get_or_create_index(
                window.id,
                window.generation,
                window.buffer_id,
                window.buffer_fd,
                src_pitch * buf_h.max(0) as usize,
            )
            .and_then(|index| surface_cache.get_slice(index));
        let shown = DamageRect {
            x0: window.x,
            y0: window.y,
            x1: window.x + buf_w.min(frame_w) - 1,
            y1: window.y + buf_h.min(frame_h) - 1,
        };
        let shown = src.and_then(|_| intersection(&content, &shown));

        let mut gap = Region::from_rect(content);
        if let Some(shown) = shown {
            gap.subtract(&shown);
        }
        let surface = slopos_theme_core::current().surface;
        for part in gap.rects() {
            paint_in_outline(buf, outline, part, surface);
        }
        let (Some(src), Some(shown)) = (src, shown) else {
            return;
        };
        let format = buf.pixel_format();
        let pixel = |x: i32, y: i32| {
            let off = (y - window.y) as usize * src_pitch + (x - window.x) as usize * bytes_pp;
            let raw = src
                .get(off..off + 4)
                .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
            with_alpha(format.decode(raw), 0xff)
        };
        let dst_pitch = self.output_pitch;
        for whole in uncornered(outline, &shown).rects() {
            let dst = buf.data_mut();
            let width = (whole.x1 - whole.x0 + 1) as usize * bytes_pp;
            for y in whole.y0..=whole.y1 {
                let src_off =
                    (y - window.y) as usize * src_pitch + (whole.x0 - window.x) as usize * bytes_pp;
                let dst_off = y as usize * dst_pitch + whole.x0 as usize * bytes_pp;
                if let (Some(from), Some(to)) = (
                    src.get(src_off..src_off + width),
                    dst.get_mut(dst_off..dst_off + width),
                ) {
                    to.copy_from_slice(from);
                }
            }
        }
        for corner in outline.corners() {
            outline.fill(buf, &corner, &shown, pixel);
        }
    }
}

fn full_screen(buf: &DrawBuffer) -> DamageRect {
    DamageRect {
        x0: 0,
        y0: 0,
        x1: buf.width() as i32 - 1,
        y1: buf.height() as i32 - 1,
    }
}

/// The pointer, white on any theme. A row's `#` is a black edge, `o` fill and
/// anything else clear.
struct Sprite {
    hot_x: i32,
    hot_y: i32,
    rows: &'static [&'static str],
}

impl Sprite {
    const ALL: [&'static Sprite; 8] = [&ARROW, &BEAM, &NS, &EW, &NWSE, &NESW, &GRAB, &GRABBING];

    fn of(shape: u8) -> &'static Sprite {
        match shape {
            CURSOR_SHAPE_TEXT => &BEAM,
            CURSOR_SHAPE_N_RESIZE | CURSOR_SHAPE_S_RESIZE => &NS,
            CURSOR_SHAPE_W_RESIZE | CURSOR_SHAPE_E_RESIZE => &EW,
            CURSOR_SHAPE_NW_RESIZE | CURSOR_SHAPE_SE_RESIZE => &NWSE,
            CURSOR_SHAPE_NE_RESIZE | CURSOR_SHAPE_SW_RESIZE => &NESW,
            CURSOR_SHAPE_GRAB => &GRAB,
            CURSOR_SHAPE_GRABBING => &GRABBING,
            _ => &ARROW,
        }
    }

    fn bounds(&self, x: i32, y: i32) -> DamageRect {
        let width = self.rows.iter().map(|row| row.len()).max().unwrap_or(0) as i32;
        DamageRect {
            x0: x - self.hot_x,
            y0: y - self.hot_y,
            x1: x - self.hot_x + width - 1,
            y1: y - self.hot_y + self.rows.len() as i32 - 1,
        }
    }

    fn draw(&self, buf: &mut DrawBuffer, x: i32, y: i32, clip: &DamageRect) {
        let (left, top) = (x - self.hot_x, y - self.hot_y);
        for (dy, row) in self.rows.iter().enumerate() {
            let row = row.as_bytes();
            let mut col = 0;
            while col < row.len() {
                let ink = match row[col] {
                    b'#' => Color32::BLACK,
                    b'o' => Color32::WHITE,
                    _ => {
                        col += 1;
                        continue;
                    }
                };
                let start = col;
                while col < row.len() && row[col] == row[start] {
                    col += 1;
                }
                gfx::fill_rect_clipped(
                    buf,
                    left + start as i32,
                    top + dy as i32,
                    (col - start) as i32,
                    1,
                    ink,
                    clip,
                );
            }
        }
    }
}

/// Every pixel the pointer at `(x, y)` may cover, whatever its shape.
pub fn cursor_reach(x: i32, y: i32) -> DamageRect {
    Sprite::ALL
        .iter()
        .map(|sprite| sprite.bounds(x, y))
        .reduce(|a, b| a.union(&b))
        .unwrap_or_else(DamageRect::invalid)
}

const ARROW: Sprite = Sprite {
    hot_x: 0,
    hot_y: 0,
    rows: &[
        "#",
        "##",
        "#o#",
        "#oo#",
        "#ooo#",
        "#oooo#",
        "#ooooo#",
        "#oooooo#",
        "#ooooooo#",
        "#oooooooo#",
        "#ooooo#####",
        "#oo#oo#",
        "#o# #oo#",
        "##  #oo#",
        "#    #oo#",
        "     #oo#",
        "      ##",
    ],
};

const BEAM: Sprite = Sprite {
    hot_x: 3,
    hot_y: 9,
    rows: &[
        "#######", "#ooooo#", "###o###", "  #o#", "  #o#", "  #o#", "  #o#", "  #o#", "  #o#",
        "  #o#", "  #o#", "  #o#", "  #o#", "  #o#", "  #o#", "###o###", "#ooooo#", "#######",
    ],
};

const NS: Sprite = Sprite {
    hot_x: 4,
    hot_y: 8,
    rows: &[
        "    #",
        "   #o#",
        "  #ooo#",
        " #ooooo#",
        "#ooooooo#",
        "####o####",
        "   #o#",
        "   #o#",
        "   #o#",
        "   #o#",
        "   #o#",
        "####o####",
        "#ooooooo#",
        " #ooooo#",
        "  #ooo#",
        "   #o#",
        "    #",
    ],
};

const EW: Sprite = Sprite {
    hot_x: 8,
    hot_y: 4,
    rows: &[
        "    #       #",
        "   ##       ##",
        "  #o#       #o#",
        " #oo#########oo#",
        "#ooooooooooooooo#",
        " #oo#########oo#",
        "  #o#       #o#",
        "   ##       ##",
        "    #       #",
    ],
};

const NWSE: Sprite = Sprite {
    hot_x: 7,
    hot_y: 7,
    rows: &[
        "######",
        "#oooo#",
        "#ooo#",
        "#oooo#",
        "#o#ooo#",
        "## #ooo#",
        "    #ooo#",
        "     #ooo#",
        "      #ooo#",
        "       #ooo# ##",
        "        #ooo#o#",
        "         #oooo#",
        "          #ooo#",
        "          #ooo#",
        "         ######",
    ],
};

const NESW: Sprite = Sprite {
    hot_x: 7,
    hot_y: 7,
    rows: &[
        "         ######",
        "         #oooo#",
        "          #ooo#",
        "         #oooo#",
        "        #ooo#o#",
        "       #ooo# ##",
        "      #ooo#",
        "     #ooo#",
        "    #ooo#",
        "## #ooo#",
        "#o#ooo#",
        "#oooo#",
        "#ooo#",
        "#oooo#",
        "######",
    ],
};

const GRAB: Sprite = Sprite {
    hot_x: 6,
    hot_y: 1,
    rows: &[
        "   ## ##",
        "  #oo#oo#",
        "  #oo#oo###",
        "  #oo#oo#oo#",
        "  #oo#oo#oo##",
        "###oo#oo#oo#o#",
        "#o#oooooooooo#",
        "#o#oooooooooo#",
        " #oooooooooo#",
        " #oooooooooo#",
        "  #oooooooo#",
        "  #oooooooo#",
        "   #oooooo#",
        "   #oooooo#",
        "    #oooo#",
        "     ####",
    ],
};

const GRABBING: Sprite = Sprite {
    hot_x: 5,
    hot_y: 3,
    rows: &[
        "  ## ## ##",
        " #oo#oo#oo#",
        " #oo#oo#oo##",
        "##oooooooo#o#",
        "#ooooooooooo#",
        "#ooooooooooo#",
        " #ooooooooo#",
        " #ooooooooo#",
        "  #ooooooo#",
        "  #ooooooo#",
        "   #ooooo#",
        "   #ooooo#",
        "    #####",
    ],
};
