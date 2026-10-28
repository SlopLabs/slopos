//! The dock: the pinned apps, then those running that no entry names, each
//! growing as the pointer nears and named in a tooltip while it is over it.

use slopos_appkit::app_icon;
use slopos_appkit::text::ui;
use slopos_gfx::image::{BitmapRef, ImageFit, ImageSampling};
use slopos_gfx::rounded::Rounded;
use slopos_image::Image;
use slopos_theme_core::{Icon, TextRole, radius, space, stroke};

use crate::app_entry::AppEntry;
use crate::gfx::{self, DamageRect, DrawBuffer};
use crate::syscall::UserWindowInfo;

const MAX_ENTRIES: usize = 16;

const ICON: i32 = 48;
const ICON_GAP: i32 = space::SM;
const PILL_PADDING_X: i32 = space::MD;
const PILL_PADDING_Y: i32 = 6;
const BOTTOM_MARGIN: i32 = space::XS;
const SEPARATOR_WIDTH: i32 = 2;
const SEPARATOR_HEIGHT: i32 = 32;
const SEPARATOR_MARGIN: i32 = space::SM;
const SEPARATOR_SPAN: i32 = SEPARATOR_MARGIN + SEPARATOR_WIDTH + SEPARATOR_MARGIN;
const DOT: i32 = 4;
const DOT_GAP: i32 = 3;
const LABEL: TextRole = TextRole::Label;

/// What the dock takes from the bottom of the screen, its dots included.
pub const DOCK_HEIGHT: i32 = ICON + 2 * PILL_PADDING_Y + BOTTOM_MARGIN + DOT + DOT_GAP;

const MAGNIFY_REACH_X: i32 = 120;
const MAGNIFY_REACH_Y: i32 = 80;
/// How much an icon under the pointer grows, out of 256.
const MAGNIFY_256: i32 = 84;

/// The smaller size an icon ships at, which the dock never draws past.
const PICTURE_PX: u32 = app_icon::SIZES[0];

pub struct DockEntry {
    pub name: String,
    pub exec: String,
    pub app_id: String,
    pub pinned: bool,
    pub running: bool,
    pub task_id: u32,
    /// The icon the entry names, or the template drawn for an app that
    /// ships none.
    picture: Image,
    has_icon: bool,
}

impl DockEntry {
    pub fn has_icon(&self) -> bool {
        self.has_icon
    }

    pub fn picture(&self) -> &Image {
        &self.picture
    }

    fn pinned(entry: AppEntry) -> Self {
        let icon = entry.icon.as_deref().and_then(crate::app_entry::read_icon);
        Self::new(entry.name, entry.exec, entry.id, true, icon)
    }

    fn new(name: String, exec: String, app_id: String, pinned: bool, icon: Option<Image>) -> Self {
        let has_icon = icon.is_some();
        let picture = icon.unwrap_or_else(|| {
            let tint = slopos_theme_core::series_of(match app_id.is_empty() {
                true => &name,
                false => &app_id,
            });
            Image {
                width: PICTURE_PX,
                height: PICTURE_PX,
                pixels: app_icon::render(PICTURE_PX, tint, Icon::AppWindow),
            }
        });
        Self {
            name,
            exec,
            app_id,
            pinned,
            running: false,
            task_id: 0,
            picture,
            has_icon,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct IconLayout {
    center_x: i32,
    /// Icons grow upward from a common bottom line.
    top_y: i32,
    size: i32,
}

/// Where everything is for one pointer position, which drawing and a press
/// both take from [`Dock::layout`], so a press is tested against where the
/// pointer is rather than where it was at the last frame.
struct Layout {
    icons: [IconLayout; MAX_ENTRIES],
    pill: Rounded,
    separator_x: Option<i32>,
    icons_bottom: i32,
    hovered: Option<usize>,
}

impl Layout {
    /// The name over the icon under the pointer, if it has one.
    fn label<'a>(&self, entries: &'a [DockEntry]) -> Option<(&'a str, Rounded)> {
        let at = self.hovered?;
        let name = entries[at].name.as_str();
        (!name.is_empty()).then(|| {
            (
                name,
                label_shape(name, self.icons[at].center_x, self.pill.y),
            )
        })
    }

    /// Every pixel the dock draws, its dots and its label included.
    fn bounds(&self, entries: &[DockEntry]) -> DamageRect {
        let pill = self.pill;
        let dock = DamageRect {
            x0: pill.x,
            y0: pill.y,
            x1: pill.x + pill.w - 1,
            y1: pill.y + pill.h + DOT_GAP + DOT,
        };
        match self.label(entries) {
            Some((_, shape)) => dock.union(&label_reach(&shape)),
            None => dock,
        }
    }
}

pub struct Dock {
    entries: Vec<DockEntry>,
    last_bounds: DamageRect,
    screen_w: u32,
    screen_h: u32,
    content_dirty: bool,
}

impl Dock {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            last_bounds: DamageRect::invalid(),
            screen_w: 0,
            screen_h: 0,
            content_dirty: false,
        }
    }

    pub fn take_content_dirty(&mut self) -> bool {
        std::mem::take(&mut self.content_dirty)
    }

    /// Pin the apps `entries` name, in order, as many as the dock holds.
    pub fn pin(&mut self, entries: Vec<AppEntry>) {
        self.entries = entries
            .into_iter()
            .take(MAX_ENTRIES)
            .map(DockEntry::pinned)
            .collect();
        self.content_dirty = true;
    }

    pub fn entries(&self) -> &[DockEntry] {
        &self.entries
    }

    pub fn app_name(&self, app_id: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|e| e.pinned && !app_id.is_empty() && e.app_id == app_id)
            .map(|e| e.name.as_str())
    }

    /// Mark the pinned apps running, and add the running apps no entry
    /// names after them: by app id, or by window for one without.
    pub fn sync_running_apps(&mut self, windows: &[UserWindowInfo], count: u32) {
        let shown = |entries: &[DockEntry]| -> Vec<(bool, u32, String)> {
            entries
                .iter()
                .map(|e| (e.running, e.task_id, e.name.clone()))
                .collect()
        };
        let before = shown(&self.entries);
        let (pinned, mut loose): (Vec<DockEntry>, Vec<DockEntry>) =
            self.entries.drain(..).partition(|entry| entry.pinned);
        self.entries = pinned;
        for entry in &mut self.entries {
            entry.running = false;
            entry.task_id = 0;
        }

        for win in &windows[..(count as usize).min(windows.len())] {
            let app_id = win.app_id.as_str();
            let same_app = |entry: &DockEntry| match app_id.is_empty() {
                true => entry.app_id.is_empty() && entry.task_id == win.task_id,
                false => entry.app_id == app_id,
            };
            if let Some(entry) = self.entries.iter_mut().find(|e| same_app(e)) {
                entry.running = true;
                entry.task_id = win.task_id;
            } else if self.entries.len() < MAX_ENTRIES {
                let title = win.title_str();
                let mut entry = match loose.iter().position(|e| same_app(e)) {
                    Some(at) => loose.swap_remove(at),
                    None => DockEntry::new(title.into(), String::new(), app_id.into(), false, None),
                };
                entry.name = title.into();
                entry.running = true;
                entry.task_id = win.task_id;
                self.entries.push(entry);
            }
        }

        if before != shown(&self.entries) {
            self.content_dirty = true;
        }
    }

    fn layout(
        &self,
        screen_width: u32,
        screen_height: u32,
        cursor_x: i32,
        cursor_y: i32,
    ) -> Layout {
        let count = self.entries.len();
        let pinned = self.entries.iter().take_while(|e| e.pinned).count();
        let separated = pinned < count;
        let gap_before = |i: usize| match separated && i == pinned {
            true => SEPARATOR_SPAN,
            false => 0,
        };

        let mut sizes = [ICON; MAX_ENTRIES];
        let pill_bottom = screen_height as i32 - BOTTOM_MARGIN - DOT - DOT_GAP;
        let resting_top = pill_bottom - ICON - 2 * PILL_PADDING_Y;
        if cursor_y >= resting_top - MAGNIFY_REACH_Y && cursor_y <= screen_height as i32 {
            let resting_w = (0..count).map(|i| gap_before(i) + ICON).sum::<i32>()
                + ICON_GAP * (count as i32 - 1).max(0);
            let mut x = (screen_width as i32 - resting_w) / 2;
            for (i, size) in sizes[..count].iter_mut().enumerate() {
                x += gap_before(i);
                let near = MAGNIFY_REACH_X - (cursor_x - (x + ICON / 2)).abs();
                if near > 0 {
                    let reach = MAGNIFY_REACH_X;
                    *size = ICON * (256 + MAGNIFY_256 * near * near / (reach * reach)) / 256;
                }
                x += ICON + ICON_GAP;
            }
        }

        let content_w = (0..count).map(|i| gap_before(i) + sizes[i]).sum::<i32>()
            + ICON_GAP * (count as i32 - 1).max(0);
        let pill_w = content_w + 2 * PILL_PADDING_X;
        let pill_h = sizes[..count].iter().copied().max().unwrap_or(0) + 2 * PILL_PADDING_Y;
        let pill_x = (screen_width as i32 - pill_w) / 2;
        let pill_y = pill_bottom - pill_h;
        let icons_bottom = pill_y + pill_h - PILL_PADDING_Y;

        let mut icons = [IconLayout::default(); MAX_ENTRIES];
        let mut separator_x = None;
        let mut x = pill_x + PILL_PADDING_X;
        for (i, icon) in icons[..count].iter_mut().enumerate() {
            if gap_before(i) > 0 {
                separator_x = Some(x + SEPARATOR_MARGIN);
                x += SEPARATOR_SPAN;
            }
            let size = sizes[i];
            *icon = IconLayout {
                center_x: x + size / 2,
                top_y: icons_bottom - size,
                size,
            };
            x += size + ICON_GAP;
        }

        let hovered = icons[..count].iter().position(|l| {
            let left = l.center_x - l.size / 2;
            (left..left + l.size).contains(&cursor_x) && (l.top_y..icons_bottom).contains(&cursor_y)
        });

        Layout {
            icons,
            pill: Rounded::new(pill_x, pill_y, pill_w, pill_h, radius::CARD),
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
        clip: &DamageRect,
    ) {
        if self.entries.is_empty() {
            self.last_bounds = DamageRect::invalid();
            return;
        }
        self.screen_w = screen_width;
        self.screen_h = screen_height;
        let theme = slopos_theme_core::current();
        let layout = self.layout(screen_width, screen_height, cursor_x, cursor_y);
        let pill = layout.pill;
        pill.draw(buf, clip, theme.chrome);

        if let Some(x) = layout.separator_x {
            let y = layout.icons_bottom - (ICON + SEPARATOR_HEIGHT) / 2;
            gfx::fill_rect_clipped(
                buf,
                x,
                y,
                SEPARATOR_WIDTH,
                SEPARATOR_HEIGHT,
                theme.border,
                clip,
            );
        }

        for (entry, l) in self.entries.iter().zip(&layout.icons) {
            let icon = entry.picture();
            if let Some(bitmap) = BitmapRef::new(icon.width, icon.height, &icon.pixels) {
                slopos_gfx::image::draw_image_clipped(
                    buf,
                    bitmap,
                    l.center_x - l.size / 2,
                    l.top_y,
                    l.size,
                    l.size,
                    ImageFit::Stretch,
                    ImageSampling::for_resize(icon.width, icon.height, l.size, l.size),
                    clip,
                );
            }
            if entry.running {
                let y = pill.y + pill.h + DOT_GAP;
                Rounded::circle(l.center_x - DOT / 2, y, DOT).draw(buf, clip, theme.text);
            }
        }

        if let Some((name, shape)) = layout.label(&self.entries) {
            draw_label(buf, name, &shape, clip);
        }
        self.last_bounds = layout.bounds(&self.entries);
    }

    /// The entry at `(px, py)`, laid out for that position.
    pub fn hit_test(&self, px: i32, py: i32) -> Option<usize> {
        if self.entries.is_empty() || self.screen_w == 0 || self.screen_h == 0 {
            return None;
        }
        self.layout(self.screen_w, self.screen_h, px, py).hovered
    }

    /// Where it was last drawn.
    pub fn bounds(&self) -> DamageRect {
        self.last_bounds
    }

    /// Where it will be drawn with the pointer at `(x, y)`.
    pub fn reach(&self, x: i32, y: i32) -> DamageRect {
        if self.entries.is_empty() || self.screen_w == 0 || self.screen_h == 0 {
            return DamageRect::invalid();
        }
        self.layout(self.screen_w, self.screen_h, x, y)
            .bounds(&self.entries)
    }

    pub fn entry(&self, index: usize) -> Option<&DockEntry> {
        self.entries.get(index)
    }
}

/// An app's name centred on `center_x` above `above`, as appkit's tooltip
/// is drawn.
fn label_shape(name: &str, center_x: i32, above: i32) -> Rounded {
    let w = ui::width(name, LABEL) + 2 * space::SM;
    let h = LABEL.style().line_height + space::SM;
    Rounded::new(
        center_x - w / 2,
        above - space::SM - h,
        w,
        h,
        radius::CONTROL,
    )
}

fn label_caster(shape: &Rounded) -> Rounded {
    let mut caster = *shape;
    caster.y += slopos_theme_core::current().shadow_raised.offset_y;
    caster
}

fn label_reach(shape: &Rounded) -> DamageRect {
    let spread = slopos_theme_core::current().shadow_raised.spread;
    shape
        .bounds()
        .union(&label_caster(shape).grown(spread).bounds())
}

fn draw_label(buf: &mut DrawBuffer, name: &str, shape: &Rounded, clip: &DamageRect) {
    let theme = slopos_theme_core::current();
    let shadow = theme.shadow_raised;
    label_caster(shape).draw_shadow(buf, shadow.spread, clip, &shape.bands()[1], shadow.color);
    shape.draw(buf, clip, theme.surface_raised);
    shape.stroke(buf, stroke::EDGE, clip, theme.border);
    ui::draw(
        buf,
        shape.x + space::SM,
        ui::centred_top(shape.y, shape.h, LABEL),
        name,
        LABEL,
        theme.text,
        theme.surface_raised,
        clip,
    );
}
