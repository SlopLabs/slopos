//! The scrollbar every scrolling view draws and takes the pointer with: a
//! sunken track down its right edge, and a thumb as long as the share shown.

use crate::constraints::Rect;
use crate::paint::PaintContext;
use crate::traits::WidgetId;

// The hover parts a bar answers for its view, past any row the view numbers
// its own parts by. The track lights nothing, but takes the point from rows.
const THUMB: usize = usize::MAX;
const TRACK: usize = usize::MAX - 1;

/// A press anywhere on a bar puts the thumb's middle under the pointer and
/// drags it from there.
#[derive(Clone, Copy)]
pub(crate) struct Bar {
    track: Rect,
    thumb: Rect,
    /// How far the content scrolls.
    span: i64,
}

impl Bar {
    /// The bar down `area`'s right edge, `width` thick, for `shown` of `total`
    /// scrolled `offset` in, while some of it is hidden.
    pub(crate) fn needed(
        area: Rect,
        width: i32,
        min: i32,
        shown: i64,
        total: i64,
        offset: i64,
    ) -> Option<Self> {
        let track = Rect::new(area.x + area.width - width, area.y, width, area.height);
        let (at, len) = thumb(track.height, shown, total, offset, min)?;
        Some(Self {
            track,
            thumb: Rect::new(track.x, track.y + at, width, len),
            span: total - shown,
        })
    }

    /// Only the bar itself, since what its view shows lies beside it.
    pub(crate) fn grabs(&self, x: i32, y: i32) -> bool {
        self.track.contains(x, y)
    }

    /// The part the pointer at `(x, y)` lights, for its view's
    /// [`hover_part`](crate::traits::Widget::hover_part).
    pub(crate) fn hover(&self, x: i32, y: i32) -> Option<usize> {
        let on_thumb = y >= self.thumb.y && y < self.thumb.y + self.thumb.height;
        self.grabs(x, y)
            .then_some(if on_thumb { THUMB } else { TRACK })
    }

    /// The offset that puts the thumb's middle at `y`.
    pub(crate) fn offset_at(&self, y: i32) -> i64 {
        let len = self.thumb.height;
        let usable = self.track.height - len;
        if usable <= 0 {
            return 0;
        }
        let top = (y - self.track.y - len / 2).clamp(0, usable);
        top as i64 * self.span / usable as i64
    }

    /// Lit while `held` or while the pointer rests on the thumb of `owner`.
    pub(crate) fn paint(&self, ctx: &mut PaintContext, owner: WidgetId, held: bool) {
        let theme = ctx.theme();
        let lit = held || ctx.lit(owner) == Some(THUMB);
        let (t, b) = (self.track, self.thumb);
        ctx.fill_rect(t.x, t.y, t.width, t.height, theme.surface_sunken);
        let ink = if lit {
            theme.border_strong
        } else {
            theme.border
        };
        ctx.fill_rect(b.x, b.y, b.width, b.height, ink);
    }
}

/// Where the thumb starts along a track `track` long, and its length, for
/// `shown` of `total` scrolled `offset` in; `None` when all of it shows.
fn thumb(track: i32, shown: i64, total: i64, offset: i64, min: i32) -> Option<(i32, i32)> {
    if total <= shown || track <= 0 {
        return None;
    }
    let len = ((shown * track as i64) / total) as i32;
    let len = len.max(min).min(track);
    let span = total - shown;
    let at = (offset.clamp(0, span) * (track - len) as i64 / span) as i32;
    Some((at, len))
}
