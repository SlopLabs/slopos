//! Launcher shelf (dock) — rendering, layout, magnification, and hit-testing
//! for the bottom launcher bar.

use slopos_abi::draw::Color32;
use slopos_appkit::text::ui;
use slopos_gfx::image::{BitmapRef, ImageFit, ImageSampling};
use slopos_image::{DecodeOptions, Image};

use crate::app_entry::AppEntry;
use crate::gfx::{self, DamageRect, DrawBuffer};
use crate::syscall::UserWindowInfo;
use crate::theme::*;

const MAX_ENTRIES: usize = 16;

/// Well past the size an icon is drawn at, so a huge one is refused before
/// it is decoded.
const ICON_PIXELS_MAX: usize = 256 * 256;
const ICON_BYTES_MAX: u64 = 1 << 20;

/// Font size for the hover label tooltip.
const LABEL_FONT_SIZE: u16 = 11;

const ICON_LABEL_FONT_SIZE: u16 = 20;

pub struct ShelfEntry {
    pub name: String,
    pub exec: String,
    pub app_id: String,
    pub pinned: bool,
    pub running: bool,
    pub task_id: u32,
    icon: Option<Image>,
}

impl ShelfEntry {
    pub fn has_icon(&self) -> bool {
        self.icon.is_some()
    }

    fn pinned(entry: AppEntry) -> Self {
        let icon = entry
            .icon
            .as_deref()
            .and_then(|path| crate::app_entry::read_bounded(path, ICON_BYTES_MAX))
            .and_then(|bytes| {
                let options = DecodeOptions {
                    max_pixels: ICON_PIXELS_MAX,
                    ..DecodeOptions::default()
                };
                slopos_image::decode(&bytes, options).ok()
            });
        Self {
            name: entry.name,
            exec: entry.exec,
            app_id: entry.id,
            pinned: true,
            running: false,
            task_id: 0,
            icon,
        }
    }
}

#[derive(Clone, Copy)]
struct IconLayout {
    center_x: i32,
    /// Top Y of this icon (icons grow upward from a common bottom line).
    top_y: i32,
    /// Rendered size after magnification scaling.
    size: i32,
}

/// Full shelf geometry for one cursor position.  The render pass and click
/// hit-testing compute it identically, so a click is tested against the
/// geometry at that cursor position rather than a stale hover cache.
struct ShelfGeometry {
    layouts: [IconLayout; MAX_ENTRIES],
    pill_x: i32,
    pill_y: i32,
    pill_w: i32,
    pill_h: i32,
    separator_x: i32,
    icons_bottom: i32,
    hovered: Option<usize>,
}

pub struct LauncherShelf {
    entries: Vec<ShelfEntry>,
    hovered_index: Option<usize>,
    last_bounds: DamageRect,
    /// Captured at draw time so `hit_test` can compute geometry for any cursor
    /// position without a renderer handle.
    screen_w: u32,
    screen_h: u32,
    content_dirty: bool,
}

impl LauncherShelf {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            hovered_index: None,
            last_bounds: DamageRect::invalid(),
            screen_w: 0,
            screen_h: 0,
            content_dirty: false,
        }
    }

    /// Returns `true` and clears the flag if shelf content changed since last call.
    pub fn take_content_dirty(&mut self) -> bool {
        let dirty = self.content_dirty;
        self.content_dirty = false;
        dirty
    }

    /// Pin the apps `entries` name, in order, as many as the shelf holds.
    pub fn pin(&mut self, entries: Vec<AppEntry>) {
        self.entries = entries
            .into_iter()
            .take(MAX_ENTRIES)
            .map(ShelfEntry::pinned)
            .collect();
        self.content_dirty = true;
    }

    pub fn entries(&self) -> &[ShelfEntry] {
        &self.entries
    }

    /// Synchronize running state from the current window list; running apps
    /// that match no pinned entry are appended after the pinned group.
    pub fn sync_running_apps(&mut self, windows: &[UserWindowInfo], count: u32) {
        let before: Vec<(bool, u32)> = self
            .entries
            .iter()
            .map(|e| (e.running, e.task_id))
            .collect();
        self.entries.retain(|entry| entry.pinned);
        for entry in &mut self.entries {
            entry.running = false;
            entry.task_id = 0;
        }

        for win in &windows[..(count as usize).min(windows.len())] {
            let title = title_str(&win.title);
            let app_id = win.app_id.as_str();
            let pinned = if app_id.is_empty() {
                self.entries
                    .iter_mut()
                    .find(|entry| names_match(&entry.name, title))
            } else {
                self.entries.iter_mut().find(|entry| entry.app_id == app_id)
            };
            if let Some(entry) = pinned {
                entry.running = true;
                entry.task_id = win.task_id;
            } else if self.entries.len() < MAX_ENTRIES {
                self.entries.push(ShelfEntry {
                    name: title.to_string(),
                    exec: String::new(),
                    app_id: app_id.to_string(),
                    pinned: false,
                    running: true,
                    task_id: win.task_id,
                    icon: None,
                });
            }
        }

        let after = self.entries.iter().map(|e| (e.running, e.task_id));
        if !before.iter().copied().eq(after) {
            self.content_dirty = true;
        }
    }

    /// Compute the full shelf geometry for one cursor position.  Used by both
    /// `draw` and `hit_test` so the two can never disagree.
    fn compute_geometry(
        &self,
        screen_width: u32,
        screen_height: u32,
        cursor_x: i32,
        cursor_y: i32,
    ) -> ShelfGeometry {
        let pinned_count = self.pinned_count();
        let running_non_pinned = self.entries.len() - pinned_count;
        let has_separator = running_non_pinned > 0;

        // Magnification scale is 8.8 fixed-point; proximity is measured against
        // the base (un-magnified) layout.
        let mut icon_sizes = [SHELF_ICON_SIZE; MAX_ENTRIES];

        let base_total_w = self.base_content_width(pinned_count, has_separator);
        let base_pill_w = base_total_w + 2 * SHELF_PILL_PADDING_X;
        let base_pill_x = (screen_width as i32 - base_pill_w) / 2;
        let base_pill_y =
            screen_height as i32 - SHELF_BOTTOM_MARGIN - SHELF_ICON_SIZE - 2 * SHELF_PILL_PADDING_Y;
        let shelf_top = base_pill_y;

        let in_y_proximity =
            cursor_y >= shelf_top - MAGNIFICATION_PROXIMITY_Y && cursor_y <= screen_height as i32;

        if in_y_proximity {
            let mut cx = base_pill_x + SHELF_PILL_PADDING_X;
            for i in 0..self.entries.len() {
                if i == pinned_count && has_separator {
                    cx +=
                        SHELF_SEPARATOR_MARGIN_X + SHELF_SEPARATOR_WIDTH + SHELF_SEPARATOR_MARGIN_X;
                }
                let icon_cx = cx + SHELF_ICON_SIZE / 2;
                let dist = abs_i32(cursor_x - icon_cx);

                if dist < MAGNIFICATION_PROXIMITY_X {
                    let prox = MAGNIFICATION_PROXIMITY_X;
                    let d = prox - dist;
                    let scale_256 = 256 + MAGNIFICATION_AMOUNT_256 * d * d / (prox * prox);
                    icon_sizes[i] = SHELF_ICON_SIZE * scale_256 / 256;
                }
                cx += SHELF_ICON_SIZE + SHELF_ICON_SPACING;
            }
        }

        let total_w = self.scaled_content_width(&icon_sizes, pinned_count, has_separator);
        let pill_w = total_w + 2 * SHELF_PILL_PADDING_X;
        let max_icon_size = max_in_slice(&icon_sizes, self.entries.len());
        let pill_h = max_icon_size + 2 * SHELF_PILL_PADDING_Y;
        let pill_x = (screen_width as i32 - pill_w) / 2;
        let pill_y = screen_height as i32 - SHELF_BOTTOM_MARGIN - pill_h;

        let icons_bottom = pill_y + pill_h - SHELF_PILL_PADDING_Y;

        let mut layouts = [IconLayout {
            center_x: 0,
            top_y: 0,
            size: 0,
        }; MAX_ENTRIES];
        let mut cx = pill_x + SHELF_PILL_PADDING_X;
        let mut separator_x = 0i32;

        for i in 0..self.entries.len() {
            if i == pinned_count && has_separator {
                separator_x = cx + SHELF_SEPARATOR_MARGIN_X;
                cx += SHELF_SEPARATOR_MARGIN_X + SHELF_SEPARATOR_WIDTH + SHELF_SEPARATOR_MARGIN_X;
            }
            let sz = icon_sizes[i];
            layouts[i] = IconLayout {
                center_x: cx + sz / 2,
                top_y: icons_bottom - sz,
                size: sz,
            };
            cx += sz + SHELF_ICON_SPACING;
        }

        let mut hovered = None;
        for i in 0..self.entries.len() {
            let l = &layouts[i];
            let left = l.center_x - l.size / 2;
            if cursor_x >= left
                && cursor_x < left + l.size
                && cursor_y >= l.top_y
                && cursor_y < icons_bottom
            {
                hovered = Some(i);
                break;
            }
        }

        ShelfGeometry {
            layouts,
            pill_x,
            pill_y,
            pill_w,
            pill_h,
            separator_x,
            icons_bottom,
            hovered,
        }
    }

    pub fn draw(
        &mut self,
        buf: &mut DrawBuffer,
        screen_width: u32,
        screen_height: u32,
        cursor_x: i32,
        cursor_y: i32,
        clip: Option<DamageRect>,
    ) {
        if self.entries.len() == 0 {
            self.last_bounds = DamageRect::invalid();
            return;
        }

        self.screen_w = screen_width;
        self.screen_h = screen_height;

        let clip_rect = clip.unwrap_or_else(|| full_screen_clip(screen_width, screen_height));

        let pinned_count = self.pinned_count();
        let running_non_pinned = self.entries.len() - pinned_count;
        let has_separator = running_non_pinned > 0;

        let geo = self.compute_geometry(screen_width, screen_height, cursor_x, cursor_y);
        let ShelfGeometry {
            layouts,
            pill_x,
            pill_y,
            pill_w,
            pill_h,
            separator_x,
            icons_bottom,
            hovered,
        } = geo;
        self.hovered_index = hovered;

        let shelf_bg = Color32::new(
            SHELF_BG.red(),
            SHELF_BG.green(),
            SHELF_BG.blue(),
            SHELF_BG_ALPHA,
        );
        slopos_gfx::canvas_ops::rounded_rect_filled(
            buf,
            pill_x,
            pill_y,
            pill_w,
            pill_h,
            SHELF_PILL_RADIUS,
            shelf_bg,
        );

        if has_separator {
            let sep_centered_y = icons_bottom - (SHELF_ICON_SIZE + SHELF_SEPARATOR_HEIGHT) / 2;
            let sep_color = Color32::new(
                SHELF_SEPARATOR.red(),
                SHELF_SEPARATOR.green(),
                SHELF_SEPARATOR.blue(),
                SHELF_SEPARATOR_ALPHA,
            );
            gfx::fill_rect_clipped(
                buf,
                separator_x,
                sep_centered_y,
                SHELF_SEPARATOR_WIDTH,
                SHELF_SEPARATOR_HEIGHT,
                sep_color,
                &clip_rect,
            );
        }

        for i in 0..self.entries.len() {
            let l = &layouts[i];
            let entry = &self.entries[i];
            let icon_x = l.center_x - l.size / 2;
            let icon_y = l.top_y;

            if let Some(icon) = &entry.icon
                && let Some(bitmap) = BitmapRef::new(icon.width, icon.height, &icon.pixels)
            {
                slopos_gfx::image::draw_image_clipped(
                    buf,
                    bitmap,
                    icon_x,
                    icon_y,
                    l.size,
                    l.size,
                    ImageFit::Stretch,
                    ImageSampling::for_resize(icon.width, icon.height, l.size, l.size),
                    &clip_rect,
                );
            } else {
                draw_letter_icon(buf, entry, l, icon_x, icon_y, &clip_rect);
            }

            if entry.running {
                let dot_cx = l.center_x;
                let dot_cy = pill_y + pill_h + SHELF_DOT_MARGIN_Y + SHELF_DOT_DIAMETER / 2;
                let dot_color = Color32::new(
                    SHELF_DOT_ACTIVE.red(),
                    SHELF_DOT_ACTIVE.green(),
                    SHELF_DOT_ACTIVE.blue(),
                    SHELF_DOT_ACTIVE_ALPHA,
                );
                gfx::draw_circle_filled(buf, dot_cx, dot_cy, SHELF_DOT_DIAMETER / 2, dot_color);
            }
        }

        let mut label = None;
        if let Some(hi) = self.hovered_index {
            let entry = &self.entries[hi];
            let l = &layouts[hi];
            let name = entry.name.as_str();
            if !name.is_empty() {
                let label_bg = Color32::new(
                    SHELF_LABEL_BG.red(),
                    SHELF_LABEL_BG.green(),
                    SHELF_LABEL_BG.blue(),
                    SHELF_LABEL_BG_ALPHA,
                );
                let text_color = Color32::new(
                    TEXT_PRIMARY.red(),
                    TEXT_PRIMARY.green(),
                    TEXT_PRIMARY.blue(),
                    TEXT_PRIMARY_ALPHA,
                );

                let lbl_w = ui::width(name, LABEL_FONT_SIZE) + 2 * SHELF_LABEL_PADDING_X;
                let lbl_h = ui::line_height(LABEL_FONT_SIZE) + 2 * SHELF_LABEL_PADDING_Y;
                let lbl_x = l.center_x - lbl_w / 2;
                let lbl_y = pill_y - SHELF_LABEL_GAP_Y - lbl_h;
                label = Some(bounds(lbl_x, lbl_y, lbl_w, lbl_h));

                slopos_gfx::canvas_ops::rounded_rect_filled(
                    buf,
                    lbl_x,
                    lbl_y,
                    lbl_w,
                    lbl_h,
                    SHELF_LABEL_RADIUS,
                    label_bg,
                );
                ui::draw_clipped(
                    buf,
                    lbl_x + SHELF_LABEL_PADDING_X,
                    lbl_y + SHELF_LABEL_PADDING_Y,
                    name,
                    LABEL_FONT_SIZE,
                    ui::Weight::Regular,
                    text_color,
                    label_bg,
                    &clip_rect,
                );
            }
        }

        let dots = SHELF_DOT_MARGIN_Y + SHELF_DOT_DIAMETER;
        let shelf = bounds(pill_x, pill_y, pill_w, pill_h + dots + 1);
        self.last_bounds = label.map_or(shelf, |label| shelf.union(&label));
    }

    /// Hit-test a screen coordinate against shelf icons.
    ///
    /// Geometry is computed at `(px, py)` rather than reused from the last draw
    /// pass: a click processed in the same frame batch as the motion that
    /// brought the cursor here would otherwise be tested against where the
    /// cursor was at the previous render.
    pub fn hit_test(&self, px: i32, py: i32) -> Option<usize> {
        if self.entries.len() == 0 || self.screen_w == 0 || self.screen_h == 0 {
            return None;
        }
        self.compute_geometry(self.screen_w, self.screen_h, px, py)
            .hovered
    }

    /// Return the bounding rectangle of the most recently drawn shelf.
    pub fn bounds(&self) -> DamageRect {
        self.last_bounds
    }

    pub fn entry(&self, index: usize) -> Option<&ShelfEntry> {
        if index < self.entries.len() {
            Some(&self.entries[index])
        } else {
            None
        }
    }

    /// Count how many entries are pinned (they are always at the front).
    fn pinned_count(&self) -> usize {
        let mut n = 0;
        for i in 0..self.entries.len() {
            if self.entries[i].pinned {
                n += 1;
            } else {
                break;
            }
        }
        n
    }

    /// Total content width at the base (un-magnified) icon size.
    fn base_content_width(&self, _pinned_count: usize, has_separator: bool) -> i32 {
        if self.entries.len() == 0 {
            return 0;
        }
        let icon_total = SHELF_ICON_SIZE * self.entries.len() as i32
            + SHELF_ICON_SPACING * (self.entries.len() as i32 - 1);
        let sep_total = if has_separator {
            SHELF_SEPARATOR_MARGIN_X + SHELF_SEPARATOR_WIDTH + SHELF_SEPARATOR_MARGIN_X
                - SHELF_ICON_SPACING // separator replaces one inter-icon gap
        } else {
            0
        };
        icon_total + sep_total
    }

    fn scaled_content_width(
        &self,
        sizes: &[i32; MAX_ENTRIES],
        pinned_count: usize,
        has_separator: bool,
    ) -> i32 {
        if self.entries.len() == 0 {
            return 0;
        }
        let mut w = 0i32;
        for i in 0..self.entries.len() {
            if i == pinned_count && has_separator {
                w += SHELF_SEPARATOR_MARGIN_X + SHELF_SEPARATOR_WIDTH + SHELF_SEPARATOR_MARGIN_X;
            }
            w += sizes[i];
            if i + 1 < self.entries.len() {
                w += SHELF_ICON_SPACING;
            }
        }
        w
    }
}

/// An entry without an icon, and a running app no entry names: its name's
/// first letter on a square.
fn draw_letter_icon(
    buf: &mut DrawBuffer,
    entry: &ShelfEntry,
    l: &IconLayout,
    icon_x: i32,
    icon_y: i32,
    clip: &DamageRect,
) {
    let c = argb(ICON_DEFAULT, ICON_DEFAULT_ALPHA);
    slopos_gfx::canvas_ops::rounded_rect_filled(
        buf,
        icon_x,
        icon_y,
        l.size,
        l.size,
        SHELF_ICON_CORNER_RADIUS,
        Color32(c),
    );
    let letter = entry
        .name
        .chars()
        .next()
        .map_or('?', |c| c.to_ascii_uppercase());
    let mut utf8 = [0u8; 4];
    let s = letter.encode_utf8(&mut utf8);
    let text_color = Color32::new(
        TEXT_PRIMARY.red(),
        TEXT_PRIMARY.green(),
        TEXT_PRIMARY.blue(),
        TEXT_PRIMARY_ALPHA,
    );
    let tx = icon_x + (l.size - ui::width(s, ICON_LABEL_FONT_SIZE)) / 2;
    let ty = ui::centred_top(icon_y, l.size, ICON_LABEL_FONT_SIZE);
    ui::draw_clipped(
        buf,
        tx,
        ty,
        s,
        ICON_LABEL_FONT_SIZE,
        ui::Weight::Regular,
        text_color,
        Color32(c),
        clip,
    );
}

#[inline]
fn abs_i32(x: i32) -> i32 {
    if x < 0 { -x } else { x }
}

fn max_in_slice(arr: &[i32; MAX_ENTRIES], n: usize) -> i32 {
    let mut m = 0;
    for i in 0..n {
        if arr[i] > m {
            m = arr[i];
        }
    }
    m
}

fn bounds(x: i32, y: i32, w: i32, h: i32) -> DamageRect {
    DamageRect {
        x0: x,
        y0: y,
        x1: x + w - 1,
        y1: y + h - 1,
    }
}

fn full_screen_clip(w: u32, h: u32) -> DamageRect {
    DamageRect {
        x0: 0,
        y0: 0,
        x1: w as i32 - 1,
        y1: h as i32 - 1,
    }
}

/// The title up to the first NUL, less a character the cut split.
fn title_str(title: &[u8; 32]) -> &str {
    let len = title.iter().position(|&b| b == 0).unwrap_or(title.len());
    match core::str::from_utf8(&title[..len]) {
        Ok(title) => title,
        Err(e) => core::str::from_utf8(&title[..e.valid_up_to()]).unwrap_or(""),
    }
}

/// Case-insensitive prefix match of a shelf entry name against a window title.
fn names_match(name: &str, title: &str) -> bool {
    !name.is_empty()
        && title
            .get(..name.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(name))
}
