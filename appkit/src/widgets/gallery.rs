use std::any::Any;
use std::cell::Cell;

use slopos_gfx::image::{BitmapRef, ImageFit, ImageSampling};
use slopos_theme_core::{Icon, State, TextRole, radius, space, stroke};

use crate::components::GalleryTile;
use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{EventResponse, Key, MessageSink, NamedKey, PointerButton, WidgetEvent};
use crate::paint::PaintContext;
use crate::text::ui;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore};

type Pick = Box<dyn Fn(usize) -> Box<dyn Any>>;

pub const THUMBNAIL_WIDTH: i32 = 160;
pub const THUMBNAIL_HEIGHT: i32 = 90;

const LABEL: TextRole = TextRole::Caption;
const GAP: i32 = space::MD;
const BADGE: i32 = 20;
/// Room for the focus ring of a tile at the edge.
const INSET: i32 = crate::focus::RING_REACH;

struct Placed {
    thumbnail: Rect,
    label_cut: Cell<bool>,
}

/// Pictures in rows, one of which may be chosen; one tab stop, the arrows
/// moving the choice across and down.
pub struct GalleryWidget {
    core: WidgetCore,
    tiles: Vec<GalleryTile>,
    placed: Vec<Placed>,
    columns: usize,
    selected: Option<usize>,
    on_select: Option<Pick>,
    focused: bool,
}

impl GalleryWidget {
    pub fn new(tiles: Vec<GalleryTile>, selected: Option<usize>, on_select: Option<Pick>) -> Self {
        Self {
            core: WidgetCore::new(),
            tiles,
            placed: Vec::new(),
            columns: 1,
            selected,
            on_select,
            focused: false,
        }
    }

    const fn tile_height() -> i32 {
        THUMBNAIL_HEIGHT + space::XS + LABEL.style().line_height
    }

    fn columns_for(&self, width: i32) -> usize {
        let fit = (width - 2 * INSET + GAP) / (THUMBNAIL_WIDTH + GAP);
        (fit.max(1) as usize).min(self.tiles.len().max(1))
    }

    fn rows_for(&self, columns: usize) -> i32 {
        self.tiles.len().div_ceil(columns) as i32
    }

    fn height_for(&self, columns: usize) -> i32 {
        let rows = self.rows_for(columns);
        rows * Self::tile_height() + (rows - 1).max(0) * GAP + 2 * INSET
    }

    fn lay_out(&mut self, x: i32, y: i32, width: i32) {
        self.columns = self.columns_for(width);
        self.placed = (0..self.tiles.len())
            .map(|i| {
                let (row, column) = ((i / self.columns) as i32, (i % self.columns) as i32);
                let left = x + INSET + column * (THUMBNAIL_WIDTH + GAP);
                let top = y + INSET + row * (Self::tile_height() + GAP);
                Placed {
                    thumbnail: Rect::new(left, top, THUMBNAIL_WIDTH, THUMBNAIL_HEIGHT),
                    label_cut: Cell::new(false),
                }
            })
            .collect();
    }

    fn tile_rect(placed: &Placed) -> Rect {
        let t = placed.thumbnail;
        Rect::new(t.x, t.y, t.width, Self::tile_height())
    }

    fn tile_at(&self, x: i32, y: i32) -> Option<usize> {
        self.placed
            .iter()
            .position(|p| Self::tile_rect(p).contains(x, y))
    }

    fn cursor(&self) -> Option<usize> {
        self.selected
            .filter(|&i| i < self.tiles.len())
            .or_else(|| (!self.tiles.is_empty()).then_some(0))
    }

    fn pick(&self, at: usize, sink: &mut MessageSink) {
        if let Some(f) = &self.on_select {
            sink.emit_raw(f(at));
        }
    }

    fn moved(&self, from: usize, key: NamedKey) -> Option<usize> {
        let last = self.tiles.len().checked_sub(1)?;
        let columns = self.columns.max(1);
        match key {
            NamedKey::Left => from.checked_sub(1),
            NamedKey::Right => (from < last).then_some(from + 1),
            NamedKey::Up => from.checked_sub(columns),
            NamedKey::Down => (from + columns <= last).then_some(from + columns),
            NamedKey::Home => Some(0),
            NamedKey::End => Some(last),
            _ => None,
        }
    }

    fn paint_thumbnail(ctx: &mut PaintContext, tile: &GalleryTile, at: Rect) {
        let theme = ctx.theme();
        ctx.fill_rounded_rect(at, radius::CONTROL, theme.surface_sunken);
        match tile
            .image
            .as_ref()
            .and_then(|i| BitmapRef::new(i.width, i.height, i.pixels.as_ref()))
        {
            Some(bitmap) => ctx.draw_image(
                bitmap,
                at.x,
                at.y,
                at.width,
                at.height,
                ImageFit::Cover,
                ImageSampling::Bilinear,
            ),
            None => {
                let size = TextRole::Title.style().icon;
                let (x, y) = (at.x + (at.width - size) / 2, at.y + (at.height - size) / 2);
                ctx.draw_icon(Icon::Image, x, y, size, theme.text_secondary);
            }
        }
        ctx.draw_rounded_rect(at, radius::CONTROL, stroke::EDGE, theme.edge);
    }

    fn paint_chosen(ctx: &mut PaintContext, at: Rect) {
        let theme = ctx.theme();
        ctx.draw_rounded_rect(at, radius::CONTROL, stroke::FOCUS, theme.accent);
        let badge = Rect::new(
            at.x + at.width - BADGE - space::XS,
            at.y + space::XS,
            BADGE,
            BADGE,
        );
        ctx.fill_circle(badge.x, badge.y, BADGE, theme.accent_fill);
        let glyph = LABEL.style().icon;
        let inset = (BADGE - glyph) / 2;
        ctx.draw_icon(
            Icon::Check,
            badge.x + inset,
            badge.y + inset,
            glyph,
            theme.text_on_accent,
        );
    }
}

impl Widget for GalleryWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }

    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, _ctx: &mut MeasureCtx) -> Size {
        let width = if constraints.is_width_bounded() {
            constraints.max_width
        } else {
            let n = self.tiles.len().max(1) as i32;
            n * THUMBNAIL_WIDTH + (n - 1) * GAP + 2 * INSET
        };
        let height = self.height_for(self.columns_for(width));
        constraints.constrain(Size::new(width, height))
    }

    fn layout(&mut self, rect: Rect) {
        self.lay_out(rect.x, rect.y, rect.width);
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let ringed = self
            .cursor()
            .filter(|&at| self.placed.get(at).is_some_and(|p| ctx.shows(p.thumbnail)));
        for (at, (tile, placed)) in self.tiles.iter().zip(&self.placed).enumerate() {
            let thumbnail = placed.thumbnail;
            Self::paint_thumbnail(ctx, tile, thumbnail);
            if ctx.lit(self.id()) == Some(at) {
                let wash = ctx.theme().overlay(State::Hover);
                ctx.fill_rounded_rect(thumbnail, radius::CONTROL, wash);
            }
            let chosen = self.selected == Some(at);
            if chosen {
                Self::paint_chosen(ctx, thumbnail);
            }
            let ink = match chosen {
                true => ctx.theme().text,
                false => ctx.theme().text_secondary,
            };
            let label_top = ui::centred_top(
                thumbnail.y + thumbnail.height + space::XS,
                LABEL.style().line_height,
                LABEL,
            );
            let width = ui::width(&tile.label, LABEL).min(thumbnail.width);
            let left = thumbnail.x + (thumbnail.width - width) / 2;
            let cut = ctx.draw_text_fitted(left, label_top, &tile.label, LABEL, width, ink);
            placed.label_cut.set(cut);
            if self.focused && ringed == Some(at) {
                ctx.draw_focus_ring(thumbnail);
            }
        }
        if self.focused && ringed.is_none() {
            ctx.draw_inner_focus_ring(self.layout_rect());
        }
    }

    fn event(&mut self, event: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
        match event {
            WidgetEvent::FocusGained => {
                self.focused = true;
                EventResponse::Ignored
            }
            WidgetEvent::FocusLost => {
                self.focused = false;
                EventResponse::Ignored
            }
            WidgetEvent::PointerDown {
                x,
                y,
                button: PointerButton::Left,
                ..
            } => match self.tile_at(*x, *y) {
                Some(at) => {
                    self.pick(at, sink);
                    EventResponse::Consumed
                }
                None => EventResponse::Ignored,
            },
            WidgetEvent::KeyDown {
                key: Key::Named(key),
                modifiers,
                ..
            } if self.focused && !modifiers.alt => {
                let Some(cursor) = self.cursor() else {
                    return EventResponse::Ignored;
                };
                let target = match key {
                    NamedKey::Enter | NamedKey::Space if self.selected != Some(cursor) => {
                        Some(cursor)
                    }
                    NamedKey::Enter | NamedKey::Space => return EventResponse::Ignored,
                    other => self.moved(cursor, *other),
                };
                match target {
                    Some(t) => {
                        self.selected = Some(t);
                        self.pick(t, sink);
                        EventResponse::Consumed
                    }
                    None => EventResponse::Ignored,
                }
            }
            _ => EventResponse::Ignored,
        }
    }

    fn role(&self) -> Role {
        Role::List
    }

    fn accessible_name(&self) -> Option<&str> {
        self.tiles.first().map(|tile| tile.label.as_str())
    }

    fn focus_policy(&self) -> FocusPolicy {
        match self.on_select.is_some() && !self.tiles.is_empty() {
            true => FocusPolicy::StrongFocus,
            false => FocusPolicy::None,
        }
    }

    fn focus_rect(&self) -> Rect {
        match self.cursor().and_then(|at| self.placed.get(at)) {
            Some(placed) => placed.thumbnail,
            None => self.layout_rect(),
        }
    }

    fn hover_part(&self, x: i32, y: i32) -> Option<usize> {
        self.tile_at(x, y).filter(|_| self.on_select.is_some())
    }

    fn tooltip(&self, x: i32, y: i32) -> Option<&str> {
        let at = self.tile_at(x, y)?;
        let tile = &self.tiles[at];
        (self.placed[at].label_cut.get() || !tile.description.is_empty()).then(|| {
            match tile.description.is_empty() {
                true => tile.label.as_str(),
                false => tile.description.as_str(),
            }
        })
    }
}
