use slopos_abi::draw::{Canvas, Color32};
use slopos_abi::pixel::PixelFormat;
use slopos_gfx::image::{BitmapRef, ImageFit, ImageSampling};
use slopos_gfx::rounded::Rounded;
use slopos_image::{DecodeOptions, Image};

use crate::gfx::{self, DamageRect, DrawBuffer};
use crate::syscall::UserWindowInfo;
use crate::theme::*;

use super::decorations;
use super::dock::LauncherShelf;
use super::frame::{self, draw_window_shadow, paint_in_outline, uncornered};
use super::menu_bar::SystemBar;
use super::popover::Popover;
use super::region::Region;
use super::surface_cache::ClientSurfaceCache;
use slopos_abi::surface::WINDOW_STATE_MINIMIZED;

const DEFAULT_WALLPAPER: &str = "/usr/share/slopos/wallpapers/default.png";
const WALLPAPER_SCREEN_DIVISOR: u32 = 3;

/// Hardware cursor overlay dimensions (virtio-gpu mandates 64×64).
pub const HW_CURSOR_DIM: u32 = 64;
/// Reference point every cursor shape draws its click-point at, so the
/// overlay's hotspot lands on the pointer position for every shape.
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
        cache_buf.clear_canvas(format.encode(DESKTOP_BG));
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

    /// Composite one frame into `buf`: every damaged pixel of `frame_damage`,
    /// which must be disjoint, painted once per layer that shows there, bottom
    /// to top.
    ///
    /// While the lock or login screen is up it is all that is drawn, over the
    /// dimmed wallpaper. Otherwise each window's frame less its corners hides
    /// what is beneath it, so a lower window paints only what no higher frame
    /// covers whole, and its shadow and corners blend over what is drawn
    /// before them.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        buf: &mut DrawBuffer,
        windows: &[UserWindowInfo],
        window_count: usize,
        focused_task: u32,
        signal_hovered_task: u32,
        mouse_x: i32,
        mouse_y: i32,
        cursor_shape: u8,
        surface_cache: &mut ClientSurfaceCache,
        system_bar: &SystemBar,
        shelf: &mut LauncherShelf,
        popover: &Popover,
        asking: &super::confirm::Asking,
        consent: &super::consent::Consent,
        gate: &super::gate::Gate,
        active_app_name: &str,
        frame_damage: &Region,
        hw_cursor: bool,
    ) {
        let output_rect = full_screen_clip(buf);
        let frame_damage = frame_damage.intersect_rect(&output_rect);
        if frame_damage.is_empty() {
            return;
        }

        if gate.shown() {
            for rect in frame_damage.rects() {
                if !self.draw_wallpaper(buf, rect) {
                    gfx::fill_rect(
                        buf,
                        rect.x0,
                        rect.y0,
                        rect.x1 - rect.x0 + 1,
                        rect.y1 - rect.y0 + 1,
                        DESKTOP_BG,
                    );
                }
                gfx::fill_rect_blended_clipped(
                    buf,
                    rect.x0,
                    rect.y0,
                    rect.x1 - rect.x0 + 1,
                    rect.y1 - rect.y0 + 1,
                    SCRIM,
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

        let mut order: Vec<usize> = Vec::with_capacity(window_count);
        for i in 0..window_count {
            if windows[i].state != WINDOW_STATE_MINIMIZED {
                order.push(i);
            }
        }

        let mut exposed = frame_damage.clone();
        let mut content_vis: Vec<Region> = Vec::with_capacity(order.len());
        let mut shadow_vis: Vec<Region> = Vec::with_capacity(order.len());
        content_vis.resize(order.len(), Region::new());
        shadow_vis.resize(order.len(), Region::new());

        for k in (0..order.len()).rev() {
            let w = &windows[order[k]];
            let opaque = frame::opaque(w);
            content_vis[k] = exposed.intersect_rect(&frame::frame_box(w));
            shadow_vis[k] = exposed.intersect_rect(&frame::bounds(w));
            for part in &opaque {
                shadow_vis[k].subtract(part);
                exposed.subtract(part);
            }
        }
        let background_visible = exposed;

        for rect in background_visible.rects() {
            if !self.draw_wallpaper(buf, rect) {
                gfx::fill_rect(
                    buf,
                    rect.x0,
                    rect.y0,
                    rect.x1 - rect.x0 + 1,
                    rect.y1 - rect.y0 + 1,
                    DESKTOP_BG,
                );
            }
        }

        // Bottom→top: shadow first, then content and decorations.
        for k in 0..order.len() {
            let w = windows[order[k]];
            let focused = w.task_id == focused_task;
            if let Some((caster, darkest)) = frame::shadow(&w, focused) {
                for rect in shadow_vis[k].rects() {
                    draw_window_shadow(buf, &caster, darkest, rect);
                }
            }
            if content_vis[k].is_empty() {
                continue;
            }
            let sig_hovered = w.task_id == signal_hovered_task;
            let title = w.title_str();
            let outline = frame::outline(&w);
            for rect in content_vis[k].rects() {
                buf.set_scissor(Some(*rect));
                self.draw_window_content(buf, &w, &outline, rect, surface_cache);
                decorations::draw_window_decorations(
                    buf,
                    &outline,
                    title,
                    focused,
                    sig_hovered,
                    rect,
                );
                buf.set_scissor(None);
            }
        }

        // Always-on-top chrome.  Both self-clip to the passed rect, so drawing
        // them once per damaged rect paints each portion exactly once.  The dock
        // is drawn unconditionally: its bounds are only known after it has drawn.
        let bar_rect = DamageRect {
            x0: 0,
            y0: 0,
            x1: buf.width() as i32 - 1,
            y1: SYSTEM_BAR_HEIGHT,
        };
        for rect in frame_damage.rects() {
            buf.set_scissor(Some(*rect));
            shelf.draw(
                buf,
                self.output_width,
                self.output_height,
                mouse_x,
                mouse_y,
                Some(*rect),
            );
            if intersect_rect(rect, &bar_rect).is_some() {
                system_bar.draw(
                    buf,
                    self.output_width,
                    active_app_name,
                    mouse_x,
                    mouse_y,
                    Some(*rect),
                );
            }
            // The popover holds a pointer grab while open, so anything drawn
            // over it would look clickable and not be.
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
        let cursor_rect = cursor_bounds(mouse_x, mouse_y, cursor_shape);
        for rect in frame_damage.rects() {
            if intersect_rect(rect, &cursor_rect).is_some() {
                self.draw_cursor(buf, mouse_x, mouse_y, cursor_shape, rect);
            }
        }
    }

    /// Rasterize `cursor_shape` into a 64×64 BGRA hardware-cursor image.
    /// `out` must be `HW_CURSOR_DIM * HW_CURSOR_DIM * 4` bytes; the shape is
    /// drawn at the hotspot reference so the overlay tracks the pointer.
    pub fn render_cursor_image(&self, cursor_shape: u8, out: &mut [u8]) {
        for b in out.iter_mut() {
            *b = 0; // transparent (alpha 0)
        }
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
        self.draw_cursor(
            &mut buf,
            HW_CURSOR_HOTSPOT as i32,
            HW_CURSOR_HOTSPOT as i32,
            cursor_shape,
            &clip,
        );
    }

    fn draw_cursor(
        &self,
        buf: &mut DrawBuffer,
        mx: i32,
        my: i32,
        cursor_shape: u8,
        clip: &DamageRect,
    ) {
        match cursor_shape {
            1 => self.draw_cursor_text(buf, mx, my, clip),
            3 | 4 => self.draw_cursor_ns(buf, mx, my, clip),
            5 | 6 => self.draw_cursor_ew(buf, mx, my, clip),
            7 | 10 => self.draw_cursor_nwse(buf, mx, my, clip),
            8 | 9 => self.draw_cursor_nesw(buf, mx, my, clip),
            11 => self.draw_cursor_grab(buf, mx, my, clip),
            12 => self.draw_cursor_grabbing(buf, mx, my, clip),
            _ => self.draw_cursor_default(buf, mx, my, clip),
        }
    }

    fn draw_wallpaper(&mut self, buf: &mut DrawBuffer, clip: &DamageRect) -> bool {
        self.wallpaper.draw(buf, clip)
    }

    fn draw_cursor_default(&self, buf: &mut DrawBuffer, mx: i32, my: i32, clip: &DamageRect) {
        // Classic arrow pointer, hotspot at the top-left corner.
        // 0 = transparent, 1 = border (black), 2 = fill (white)
        const W: usize = 12;
        const H: usize = 17;
        #[rustfmt::skip]
        const ARROW: [[u8; W]; H] = [
            [1,0,0,0,0,0,0,0,0,0,0,0],
            [1,1,0,0,0,0,0,0,0,0,0,0],
            [1,2,1,0,0,0,0,0,0,0,0,0],
            [1,2,2,1,0,0,0,0,0,0,0,0],
            [1,2,2,2,1,0,0,0,0,0,0,0],
            [1,2,2,2,2,1,0,0,0,0,0,0],
            [1,2,2,2,2,2,1,0,0,0,0,0],
            [1,2,2,2,2,2,2,1,0,0,0,0],
            [1,2,2,2,2,2,2,2,1,0,0,0],
            [1,2,2,2,2,2,2,2,2,1,0,0],
            [1,2,2,2,2,2,1,1,1,1,1,0],
            [1,2,2,1,2,2,1,0,0,0,0,0],
            [1,2,1,0,1,2,2,1,0,0,0,0],
            [1,1,0,0,1,2,2,1,0,0,0,0],
            [1,0,0,0,0,1,2,2,1,0,0,0],
            [0,0,0,0,0,1,2,2,1,0,0,0],
            [0,0,0,0,0,0,1,1,0,0,0,0],
        ];

        for row in 0..H {
            let py = my + row as i32;
            let mut col = 0;
            while col < W {
                let pixel = ARROW[row][col];
                if pixel == 0 {
                    col += 1;
                    continue;
                }
                let color = if pixel == 1 { CURSOR_EDGE } else { CURSOR_FILL };
                let start = col;
                while col < W && ARROW[row][col] == pixel {
                    col += 1;
                }
                gfx::fill_rect_clipped(
                    buf,
                    mx + start as i32,
                    py,
                    (col - start) as i32,
                    1,
                    color,
                    clip,
                );
            }
        }
    }

    fn draw_cursor_text(&self, buf: &mut DrawBuffer, mx: i32, my: i32, clip: &DamageRect) {
        const BEAM_HEIGHT: i32 = 16;
        const SERIF_WIDTH: i32 = 5;
        let top = my - BEAM_HEIGHT / 2;
        gfx::fill_rect_clipped(buf, mx, top, 1, BEAM_HEIGHT, CURSOR_FILL, clip);
        gfx::fill_rect_clipped(
            buf,
            mx - SERIF_WIDTH / 2,
            top,
            SERIF_WIDTH,
            1,
            CURSOR_FILL,
            clip,
        );
        gfx::fill_rect_clipped(
            buf,
            mx - SERIF_WIDTH / 2,
            top + BEAM_HEIGHT - 1,
            SERIF_WIDTH,
            1,
            CURSOR_FILL,
            clip,
        );
    }

    /// Vertical double-arrow cursor (N/S resize), hotspot centred.
    fn draw_cursor_ns(&self, buf: &mut DrawBuffer, mx: i32, my: i32, clip: &DamageRect) {
        const W: usize = 9;
        const H: usize = 17;
        #[rustfmt::skip]
        const BITMAP: [[u8; W]; H] = [
            [0,0,0,0,1,0,0,0,0],
            [0,0,0,1,2,1,0,0,0],
            [0,0,1,2,2,2,1,0,0],
            [0,1,2,2,2,2,2,1,0],
            [1,2,2,2,2,2,2,2,1],
            [1,1,1,1,2,1,1,1,1],
            [0,0,0,1,2,1,0,0,0],
            [0,0,0,1,2,1,0,0,0],
            [0,0,0,1,2,1,0,0,0],
            [0,0,0,1,2,1,0,0,0],
            [0,0,0,1,2,1,0,0,0],
            [1,1,1,1,2,1,1,1,1],
            [1,2,2,2,2,2,2,2,1],
            [0,1,2,2,2,2,2,1,0],
            [0,0,1,2,2,2,1,0,0],
            [0,0,0,1,2,1,0,0,0],
            [0,0,0,0,1,0,0,0,0],
        ];
        self.draw_cursor_bitmap::<W, H>(buf, mx - 4, my - 8, &BITMAP, clip);
    }

    /// Horizontal double-arrow cursor (E/W resize), hotspot centred.
    fn draw_cursor_ew(&self, buf: &mut DrawBuffer, mx: i32, my: i32, clip: &DamageRect) {
        const W: usize = 17;
        const H: usize = 9;
        #[rustfmt::skip]
        const BITMAP: [[u8; W]; H] = [
            [0,0,0,0,1,0,0,0,0,0,0,0,1,0,0,0,0],
            [0,0,0,1,1,0,0,0,0,0,0,0,1,1,0,0,0],
            [0,0,1,2,1,0,0,0,0,0,0,0,1,2,1,0,0],
            [0,1,2,2,1,1,1,1,1,1,1,1,1,2,2,1,0],
            [1,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,1],
            [0,1,2,2,1,1,1,1,1,1,1,1,1,2,2,1,0],
            [0,0,1,2,1,0,0,0,0,0,0,0,1,2,1,0,0],
            [0,0,0,1,1,0,0,0,0,0,0,0,1,1,0,0,0],
            [0,0,0,0,1,0,0,0,0,0,0,0,1,0,0,0,0],
        ];
        self.draw_cursor_bitmap::<W, H>(buf, mx - 8, my - 4, &BITMAP, clip);
    }

    /// Diagonal double-arrow cursor (NW/SE resize), hotspot centred.
    fn draw_cursor_nwse(&self, buf: &mut DrawBuffer, mx: i32, my: i32, clip: &DamageRect) {
        const W: usize = 15;
        const H: usize = 15;
        #[rustfmt::skip]
        const BITMAP: [[u8; W]; H] = [
            [1,1,1,1,1,1,0,0,0,0,0,0,0,0,0],
            [1,2,2,2,2,1,0,0,0,0,0,0,0,0,0],
            [1,2,2,2,1,0,0,0,0,0,0,0,0,0,0],
            [1,2,2,2,2,1,0,0,0,0,0,0,0,0,0],
            [1,2,1,2,2,2,1,0,0,0,0,0,0,0,0],
            [1,1,0,1,2,2,2,1,0,0,0,0,0,0,0],
            [0,0,0,0,1,2,2,2,1,0,0,0,0,0,0],
            [0,0,0,0,0,1,2,2,2,1,0,0,0,0,0],
            [0,0,0,0,0,0,1,2,2,2,1,0,0,0,0],
            [0,0,0,0,0,0,0,1,2,2,2,1,0,1,1],
            [0,0,0,0,0,0,0,0,1,2,2,2,1,2,1],
            [0,0,0,0,0,0,0,0,0,1,2,2,2,2,1],
            [0,0,0,0,0,0,0,0,0,0,1,2,2,2,1],
            [0,0,0,0,0,0,0,0,0,0,1,2,2,2,1],
            [0,0,0,0,0,0,0,0,0,1,1,1,1,1,1],
        ];
        self.draw_cursor_bitmap::<W, H>(buf, mx - 7, my - 7, &BITMAP, clip);
    }

    /// Diagonal double-arrow cursor (NE/SW resize), hotspot centred.
    fn draw_cursor_nesw(&self, buf: &mut DrawBuffer, mx: i32, my: i32, clip: &DamageRect) {
        const W: usize = 15;
        const H: usize = 15;
        #[rustfmt::skip]
        const BITMAP: [[u8; W]; H] = [
            [0,0,0,0,0,0,0,0,0,1,1,1,1,1,1],
            [0,0,0,0,0,0,0,0,0,1,2,2,2,2,1],
            [0,0,0,0,0,0,0,0,0,0,1,2,2,2,1],
            [0,0,0,0,0,0,0,0,0,1,2,2,2,2,1],
            [0,0,0,0,0,0,0,0,1,2,2,2,1,2,1],
            [0,0,0,0,0,0,0,1,2,2,2,1,0,1,1],
            [0,0,0,0,0,0,1,2,2,2,1,0,0,0,0],
            [0,0,0,0,0,1,2,2,2,1,0,0,0,0,0],
            [0,0,0,0,1,2,2,2,1,0,0,0,0,0,0],
            [1,1,0,1,2,2,2,1,0,0,0,0,0,0,0],
            [1,2,1,2,2,2,1,0,0,0,0,0,0,0,0],
            [1,2,2,2,2,1,0,0,0,0,0,0,0,0,0],
            [1,2,2,2,1,0,0,0,0,0,0,0,0,0,0],
            [1,2,2,2,2,1,0,0,0,0,0,0,0,0,0],
            [1,1,1,1,1,1,0,0,0,0,0,0,0,0,0],
        ];
        self.draw_cursor_bitmap::<W, H>(buf, mx - 7, my - 7, &BITMAP, clip);
    }

    /// Open hand cursor (grab), hotspot at (6,1).
    fn draw_cursor_grab(&self, buf: &mut DrawBuffer, mx: i32, my: i32, clip: &DamageRect) {
        const W: usize = 15;
        const H: usize = 16;
        #[rustfmt::skip]
        const BITMAP: [[u8; W]; H] = [
            [0,0,0,1,1,0,1,1,0,0,0,0,0,0,0],
            [0,0,1,2,2,1,2,2,1,0,0,0,0,0,0],
            [0,0,1,2,2,1,2,2,1,1,1,0,0,0,0],
            [0,0,1,2,2,1,2,2,1,2,2,1,0,0,0],
            [0,0,1,2,2,1,2,2,1,2,2,1,1,0,0],
            [1,1,1,2,2,1,2,2,1,2,2,1,2,1,0],
            [1,2,1,2,2,2,2,2,2,2,2,2,2,1,0],
            [1,2,1,2,2,2,2,2,2,2,2,2,2,1,0],
            [0,1,2,2,2,2,2,2,2,2,2,2,1,0,0],
            [0,1,2,2,2,2,2,2,2,2,2,2,1,0,0],
            [0,0,1,2,2,2,2,2,2,2,2,1,0,0,0],
            [0,0,1,2,2,2,2,2,2,2,2,1,0,0,0],
            [0,0,0,1,2,2,2,2,2,2,1,0,0,0,0],
            [0,0,0,1,2,2,2,2,2,2,1,0,0,0,0],
            [0,0,0,0,1,2,2,2,2,1,0,0,0,0,0],
            [0,0,0,0,0,1,1,1,1,0,0,0,0,0,0],
        ];
        self.draw_cursor_bitmap::<W, H>(buf, mx - 6, my - 1, &BITMAP, clip);
    }

    /// Closed fist cursor (grabbing), hotspot at (5,3).
    fn draw_cursor_grabbing(&self, buf: &mut DrawBuffer, mx: i32, my: i32, clip: &DamageRect) {
        const W: usize = 13;
        const H: usize = 13;
        #[rustfmt::skip]
        const BITMAP: [[u8; W]; H] = [
            [0,0,1,1,0,1,1,0,1,1,0,0,0],
            [0,1,2,2,1,2,2,1,2,2,1,0,0],
            [0,1,2,2,1,2,2,1,2,2,1,1,0],
            [1,1,2,2,2,2,2,2,2,2,1,2,1],
            [1,2,2,2,2,2,2,2,2,2,2,2,1],
            [1,2,2,2,2,2,2,2,2,2,2,2,1],
            [0,1,2,2,2,2,2,2,2,2,2,1,0],
            [0,1,2,2,2,2,2,2,2,2,2,1,0],
            [0,0,1,2,2,2,2,2,2,2,1,0,0],
            [0,0,1,2,2,2,2,2,2,2,1,0,0],
            [0,0,0,1,2,2,2,2,2,1,0,0,0],
            [0,0,0,1,2,2,2,2,2,1,0,0,0],
            [0,0,0,0,1,1,1,1,1,0,0,0,0],
        ];
        self.draw_cursor_bitmap::<W, H>(buf, mx - 5, my - 3, &BITMAP, clip);
    }

    /// Generic bitmap cursor renderer. 0=transparent, 1=border(black), 2=fill(white).
    fn draw_cursor_bitmap<const W: usize, const H: usize>(
        &self,
        buf: &mut DrawBuffer,
        ox: i32,
        oy: i32,
        bitmap: &[[u8; W]; H],
        clip: &DamageRect,
    ) {
        for row in 0..H {
            let py = oy + row as i32;
            let mut col = 0;
            while col < W {
                let pixel = bitmap[row][col];
                if pixel == 0 {
                    col += 1;
                    continue;
                }
                let color = if pixel == 1 { CURSOR_EDGE } else { CURSOR_FILL };
                let start = col;
                while col < W && bitmap[row][col] == pixel {
                    col += 1;
                }
                gfx::fill_rect_clipped(
                    buf,
                    ox + start as i32,
                    py,
                    (col - start) as i32,
                    1,
                    color,
                    clip,
                );
            }
        }
    }

    /// The window's content inside `outline`: its last committed buffer,
    /// cut to the frame, and where the frame has grown past the buffer, the
    /// window's surface colour; a window whose buffer cannot be mapped yet
    /// shows that colour alone.
    fn draw_window_content(
        &self,
        buf: &mut DrawBuffer,
        window: &UserWindowInfo,
        outline: &Rounded,
        clip: &DamageRect,
        surface_cache: &mut ClientSurfaceCache,
    ) {
        let bytes_pp = self.output_bytes_pp as usize;
        // Buffer, not frame, dimensions: during a resize the frame may be larger
        // than the SHM buffer, and using it as the pitch would corrupt the blit.
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
        let Some(content) = intersect_rect(clip, &content) else {
            return;
        };

        let src = surface_cache
            .get_or_create_index(
                window.task_id,
                window.buffer_generation,
                window.buffer_id,
                window.shm_token,
                src_pitch * buf_h.max(0) as usize,
            )
            .and_then(|index| surface_cache.get_slice(index));
        // Always show the last committed buffer, clipped to the frame: a shrink
        // crops, a grow shows the surface colour in the gap, and no
        // timing-dependent "resizing" flag can expose an inconsistent state.
        let shown = DamageRect {
            x0: window.x,
            y0: window.y,
            x1: window.x + buf_w.min(frame_w) - 1,
            y1: window.y + buf_h.min(frame_h) - 1,
        };
        let shown = src.and_then(|_| intersect_rect(&content, &shown));

        let mut gap = Region::from_rect(content);
        if let Some(shown) = shown {
            gap.subtract(&shown);
        }
        for part in gap.rects() {
            paint_in_outline(buf, outline, part, SURFACE);
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
            let ink = format.decode(raw);
            Color32::new(ink.red(), ink.green(), ink.blue(), 0xff)
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

fn full_screen_clip(buf: &DrawBuffer) -> DamageRect {
    DamageRect {
        x0: 0,
        y0: 0,
        x1: buf.width() as i32 - 1,
        y1: buf.height() as i32 - 1,
    }
}

fn intersect_rect(a: &DamageRect, b: &DamageRect) -> Option<DamageRect> {
    let x0 = a.x0.max(b.x0);
    let y0 = a.y0.max(b.y0);
    let x1 = a.x1.min(b.x1);
    let y1 = a.y1.min(b.y1);
    if x0 <= x1 && y0 <= y1 {
        Some(DamageRect { x0, y0, x1, y1 })
    } else {
        None
    }
}

fn cursor_bounds(mx: i32, my: i32, cursor_shape: u8) -> DamageRect {
    match cursor_shape {
        // Text beam
        1 => DamageRect {
            x0: mx - 2,
            y0: my - 8,
            x1: mx + 2,
            y1: my + 7,
        },
        // NS resize (9×17 centered)
        3 | 4 => DamageRect {
            x0: mx - 4,
            y0: my - 8,
            x1: mx + 4,
            y1: my + 8,
        },
        // EW resize (17×9 centered)
        5 | 6 => DamageRect {
            x0: mx - 8,
            y0: my - 4,
            x1: mx + 8,
            y1: my + 4,
        },
        // NWSE / NESW resize (15×15 centered)
        7 | 8 | 9 | 10 => DamageRect {
            x0: mx - 7,
            y0: my - 7,
            x1: mx + 7,
            y1: my + 7,
        },
        // Grab — open hand (15×16, hotspot at (6,1))
        11 => DamageRect {
            x0: mx - 6,
            y0: my - 1,
            x1: mx + 8,
            y1: my + 14,
        },
        // Grabbing — closed fist (13×13, hotspot at (5,3))
        12 => DamageRect {
            x0: mx - 5,
            y0: my - 3,
            x1: mx + 7,
            y1: my + 9,
        },
        // Default arrow (12×17, top-left hotspot)
        _ => DamageRect {
            x0: mx,
            y0: my,
            x1: mx + 11,
            y1: my + 16,
        },
    }
}
