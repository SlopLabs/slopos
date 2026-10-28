//! Glyphs for the grid. Only damaged cells repaint, and only their bounds are
//! presented, so a cursor blink sends the compositor one cell.

use slopos_abi::damage::DamageRect;
use slopos_abi::draw::Color32;

use slopos_windowing::{RenderSurface, Window};

use crate::gfx::DrawBuffer;
use crate::gfx::font;

use super::grid::{BG_COLOR, Cell, TerminalGrid};
use super::input::{Selection, cell_in_selection};

use slopos_terminal_core::damage::CellDamage;

const WINDOW_BG: Color32 = color_from_rgb(BG_COLOR);

pub const MARGIN: i32 = slopos_theme_core::space::SM;

/// The shell's selection blue.
const SELECTION_BG: Color32 = Color32::rgb(0x26, 0x4F, 0x78);

pub fn cells_in(pixels: i32, cell: i32, max: usize) -> u16 {
    ((pixels - 2 * MARGIN) / cell.max(1)).clamp(1, max as i32) as u16
}

pub fn cell_size() -> (i32, i32) {
    (font::cell_width().max(1), font::cell_height().max(1))
}

/// Grid colors are `0x00RRGGBB`.
const fn color_from_rgb(rgb: u32) -> Color32 {
    Color32(0xFF00_0000 | (rgb & 0x00FF_FFFF))
}

fn draw_cell_at(
    buf: &mut DrawBuffer,
    row: usize,
    col: usize,
    cell: &Cell,
    fg: Color32,
    bg: Color32,
) {
    let (cw, ch) = cell_size();
    let x = MARGIN + col as i32 * cw;
    let y = MARGIN + row as i32 * ch;
    font::draw_glyph(buf, x, y, cell.glyph(), fg, bg);
}

/// The partial cell below the last row is left to a full paint.
fn damage_bounds(damage: &CellDamage) -> Option<DamageRect> {
    let (cw, ch) = cell_size();
    let mut out: Option<DamageRect> = None;
    for row in 0..damage.rows() {
        let Some(span) = damage.span(row) else {
            continue;
        };
        let rect = DamageRect {
            x0: MARGIN + span.first as i32 * cw,
            y0: MARGIN + row as i32 * ch,
            x1: MARGIN + (span.last as i32 + 1) * cw - 1,
            y1: MARGIN + (row as i32 + 1) * ch - 1,
        };
        out = Some(match out {
            Some(acc) => acc.union(&rect),
            None => rect,
        });
    }
    out
}

/// A repaint that is dropped rather than presented leaves the compositor
/// showing stale cells forever, since the grid has forgotten they changed.
pub fn render_damage(
    win: &mut Window,
    grid: &TerminalGrid,
    selection: &Selection,
    cursor_on: bool,
    damage: &CellDamage,
) {
    let Some(bounds) = damage_bounds(damage) else {
        return;
    };

    if let Some(mut buf) = win.renderer_mut().frame() {
        // A glyph's atlas cell need not be the grid's, and could overhang.
        buf.set_scissor(Some(bounds));
        for row in 0..damage.rows() {
            let Some(span) = damage.span(row) else {
                continue;
            };
            draw_row_span(
                &mut buf, grid, selection, cursor_on, row, span.first, span.last,
            );
        }
        buf.set_scissor(None);
    }

    win.renderer_mut().present_region(
        bounds.x0,
        bounds.y0,
        bounds.x1 - bounds.x0 + 1,
        bounds.y1 - bounds.y0 + 1,
    );
}

pub fn render_full(win: &mut Window, grid: &TerminalGrid, selection: &Selection, cursor_on: bool) {
    let rows = grid.rows as usize;
    let cols = grid.cols as usize;

    if let Some(mut buf) = win.renderer_mut().frame() {
        let w = buf.width() as i32;
        let h = buf.height() as i32;
        crate::gfx::fill_rect(&mut buf, 0, 0, w, h, WINDOW_BG);

        if cols > 0 {
            for row in 0..rows {
                draw_row_span(
                    &mut buf,
                    grid,
                    selection,
                    cursor_on,
                    row,
                    0,
                    cols as u16 - 1,
                );
            }
        }
    }
    win.renderer_mut().present();
}

fn draw_row_span(
    buf: &mut DrawBuffer,
    grid: &TerminalGrid,
    selection: &Selection,
    cursor_on: bool,
    row: usize,
    first: u16,
    last: u16,
) {
    let cols = grid.cols as usize;
    if row >= grid.rows as usize || cols == 0 {
        return;
    }
    let sel_range = selection.ordered();
    let abs = grid.screen_to_abs(row);
    let last = (last as usize).min(cols - 1);

    for col in (first as usize)..=last {
        let cell = grid.visible_cell(row, col);
        let mut fg = color_from_rgb(cell.attrs.fg);
        let mut bg = color_from_rgb(cell.attrs.bg);

        if let Some((lo, hi)) = sel_range {
            if cell_in_selection(abs, col, lo, hi) {
                bg = SELECTION_BG;
            }
        }

        let is_cursor = !grid.viewing_history()
            && grid.cursor_visible
            && cursor_on
            && row == grid.cursor_row as usize
            && col == grid.cursor_col as usize;
        if is_cursor {
            core::mem::swap(&mut fg, &mut bg);
        }

        draw_cell_at(buf, row, col, &cell, fg, bg);
    }
}
