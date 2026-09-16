//! Vector icons, drawn rather than rasterized.
//!
//! An icon font would be a third font file and a codepoint agreement between
//! the toolkit and whatever shipped it; these are a dozen shapes drawn from
//! rectangles and triangles, which is also what keeps them crisp at the one size
//! a 10-pixel cell can afford. Every shape is derived from the box it is given,
//! so the same code draws a 12 px chevron and a 20 px one.

use slopos_abi::draw::Color32;

use crate::paint::PaintContext;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum IconKind {
    /// Collapsed disclosure triangle.
    ChevronRight,
    /// Expanded disclosure triangle.
    ChevronDown,
    Folder,
    FolderOpen,
    File,
    Close,
    /// Unsaved-changes dot.
    Dot,
    Search,
}

/// Draws `kind` inside the `size × size` box at `(x, y)`.
pub fn draw_icon(
    ctx: &mut PaintContext,
    kind: IconKind,
    x: i32,
    y: i32,
    size: i32,
    color: Color32,
) {
    let s = size.max(6);
    match kind {
        IconKind::ChevronRight => chevron(ctx, x, y, s, color, false),
        IconKind::ChevronDown => chevron(ctx, x, y, s, color, true),
        IconKind::Folder => folder(ctx, x, y, s, color, false),
        IconKind::FolderOpen => folder(ctx, x, y, s, color, true),
        IconKind::File => file(ctx, x, y, s, color),
        IconKind::Close => close(ctx, x, y, s, color),
        IconKind::Dot => {
            let r = (s / 3).max(2);
            ctx.fill_rounded_rect(x + s / 2 - r, y + s / 2 - r, r * 2, r * 2, r, color);
        }
        IconKind::Search => search(ctx, x, y, s, color),
    }
}

/// A solid triangle, stepped one row at a time — the shapes here are small
/// enough that a scanline loop is the whole rasterizer they need.
fn chevron(ctx: &mut PaintContext, x: i32, y: i32, s: i32, color: Color32, down: bool) {
    let w = (s / 2).max(4);
    let h = w;
    let ox = x + (s - w) / 2;
    let oy = y + (s - h) / 2;
    if down {
        for row in 0..h {
            let inset = (row * w) / (2 * h);
            let width = w - inset * 2;
            if width > 0 {
                ctx.fill_rect(ox + inset, oy + row, width, 1, color);
            }
        }
    } else {
        for col in 0..w {
            let inset = (col * h) / (2 * w);
            let height = h - inset * 2;
            if height > 0 {
                ctx.fill_rect(ox + col, oy + inset, 1, height, color);
            }
        }
    }
}

fn folder(ctx: &mut PaintContext, x: i32, y: i32, s: i32, color: Color32, open: bool) {
    let w = s;
    let h = (s * 3) / 4;
    let oy = y + (s - h) / 2;
    let tab_w = w / 2;
    let tab_h = (h / 4).max(2);
    ctx.fill_rect(x, oy, tab_w, tab_h, color);
    if open {
        ctx.fill_rect(x, oy + tab_h, w, h - tab_h, color);
        ctx.fill_rect(
            x + 1,
            oy + tab_h + 1,
            w - 2,
            h - tab_h - 2,
            Color32::new(color.red(), color.green(), color.blue(), 0x55),
        );
    } else {
        ctx.fill_rect(x, oy + tab_h, w, h - tab_h, color);
    }
}

fn file(ctx: &mut PaintContext, x: i32, y: i32, s: i32, color: Color32) {
    let w = (s * 3) / 4;
    let h = s;
    let ox = x + (s - w) / 2;
    let fold = (w / 3).max(2);
    ctx.fill_rect(ox, y, w - fold, 1, color);
    ctx.fill_rect(ox, y, 1, h, color);
    ctx.fill_rect(ox, y + h - 1, w, 1, color);
    ctx.fill_rect(ox + w - 1, y + fold, 1, h - fold, color);
    // The dog-ear, one pixel per row.
    for i in 0..fold {
        ctx.fill_rect(ox + w - fold + i, y + i, 1, 1, color);
    }
}

fn close(ctx: &mut PaintContext, x: i32, y: i32, s: i32, color: Color32) {
    let inset = s / 4;
    let span = s - inset * 2;
    for i in 0..span {
        ctx.fill_rect(x + inset + i, y + inset + i, 1, 1, color);
        ctx.fill_rect(x + inset + i, y + s - inset - 1 - i, 1, 1, color);
    }
}

fn search(ctx: &mut PaintContext, x: i32, y: i32, s: i32, color: Color32) {
    let r = (s / 3).max(3);
    let cx = x + r + 1;
    let cy = y + r + 1;
    ctx.draw_rounded_rect(cx - r, cy - r, r * 2, r * 2, r, color);
    for i in 0..(s / 4).max(2) {
        ctx.fill_rect(cx + r - 1 + i, cy + r - 1 + i, 2, 2, color);
    }
}
