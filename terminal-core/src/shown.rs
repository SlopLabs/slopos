//! The look each cell was last drawn with, so a frame repaints the cells that
//! look different, not the cells that were written.

use alloc::vec::Vec;

use crate::damage::CellDamage;
use crate::grid::TerminalGrid;
use crate::input::{Selection, cell_in_selection};

pub const SELECTION_BG: u32 = 0x0026_4F78;

/// A cell as drawn: `0x00RRGGBB` colours, selection and cursor applied.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Look {
    pub glyph: u32,
    pub fg: u32,
    pub bg: u32,
}

impl Look {
    /// Matches no drawn cell, so it always repaints.
    const UNDRAWN: Look = Look {
        glyph: u32::MAX,
        fg: 0,
        bg: 0,
    };
}

pub struct Shown {
    cols: usize,
    looks: Vec<Look>,
}

impl Shown {
    pub fn new(rows: usize, cols: usize) -> Self {
        let mut shown = Self {
            cols: 0,
            looks: Vec::new(),
        };
        shown.reshape(rows, cols);
        shown
    }

    pub fn reshape(&mut self, rows: usize, cols: usize) {
        self.cols = cols;
        self.looks.clear();
        self.looks.resize(rows * cols, Look::UNDRAWN);
    }

    #[inline]
    pub fn rows(&self) -> usize {
        self.looks.len().checked_div(self.cols).unwrap_or(0)
    }

    #[inline]
    pub fn cols(&self) -> usize {
        self.cols
    }

    #[inline]
    pub fn look(&self, row: usize, col: usize) -> Look {
        self.looks[row * self.cols + col]
    }

    /// Returns the `touched` cells whose look changed. A look that changes
    /// outside `touched` is missed.
    pub fn update(
        &mut self,
        touched: &CellDamage,
        grid: &TerminalGrid,
        selection: &Selection,
        cursor_on: bool,
    ) -> CellDamage {
        let mut changed = CellDamage::new();
        changed.set_rows(touched.rows());
        let rows = self.rows().min(grid.rows as usize);
        let cols = self.cols.min(grid.cols as usize);
        let selected = selection.ordered();
        let cursor = grid.cursor_cell().filter(|_| cursor_on);

        for row in 0..rows {
            let Some(span) = touched.span(row) else {
                continue;
            };
            let abs = grid.screen_to_abs(row);
            for col in span.first as usize..cols.min(span.last as usize + 1) {
                let cell = grid.visible_cell(row, col);
                let mut look = Look {
                    glyph: cell.glyph(),
                    fg: cell.attrs.fg,
                    bg: cell.attrs.bg,
                };
                if selected.is_some_and(|(lo, hi)| cell_in_selection(abs, col, lo, hi)) {
                    look.bg = SELECTION_BG;
                }
                if cursor == Some((row, col)) {
                    core::mem::swap(&mut look.fg, &mut look.bg);
                }
                let drawn = &mut self.looks[row * self.cols + col];
                if *drawn != look {
                    *drawn = look;
                    changed.add(row, col as u16, col as u16);
                }
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::damage::Span;
    use crate::input::Anchor;

    const ROWS: usize = 4;
    const COLS: usize = 20;

    fn feed(g: &mut TerminalGrid, bytes: &[u8]) {
        for &b in bytes {
            g.process_byte(b);
        }
    }

    fn drawn(bytes: &[u8]) -> (TerminalGrid, Shown) {
        let mut g = TerminalGrid::new(ROWS as u16, COLS as u16);
        feed(&mut g, bytes);
        let mut shown = Shown::new(ROWS, COLS);
        let _ = shown.update(&g.take_damage(), &g, &Selection::NONE, true);
        (g, shown)
    }

    fn spans(d: &CellDamage) -> Vec<(usize, u16, u16)> {
        (0..d.rows())
            .filter_map(|row| d.span(row).map(|s| (row, s.first, s.last)))
            .collect()
    }

    #[test]
    fn a_line_written_again_repaints_only_what_differs() {
        let (mut g, mut shown) = drawn(b"\x1b[32m[/]\x1b[0m # ec");
        feed(&mut g, b"\r\x1b[J\x1b[32m[/]\x1b[0m # ech\r\x1b[9C");

        let touched = g.take_damage();
        assert_eq!(
            touched.span(0),
            Some(Span {
                first: 0,
                last: COLS as u16 - 1
            })
        );
        assert!(touched.span(ROWS - 1).is_some());

        let changed = shown.update(&touched, &g, &Selection::NONE, true);
        assert_eq!(spans(&changed), [(0, 8, 9)]);
        assert_eq!(shown.look(0, 8).glyph, u32::from(b'h'));
    }

    #[test]
    fn the_cursor_blinking_off_changes_its_cell_alone() {
        let (mut g, mut shown) = drawn(b"ab");
        g.damage_cursor();
        let changed = shown.update(&g.take_damage(), &g, &Selection::NONE, false);
        assert_eq!(spans(&changed), [(0, 2, 2)]);
    }

    #[test]
    fn a_selection_changes_the_cells_it_covers() {
        let (g, mut shown) = drawn(b"abcdef");
        let selection = Selection {
            anchor: Anchor { line: 0, col: 1 },
            head: Anchor { line: 0, col: 4 },
            active: true,
        };
        let mut all = CellDamage::new();
        all.set_rows(ROWS);
        all.add_all(COLS as u16);
        let changed = shown.update(&all, &g, &selection, true);
        assert_eq!(spans(&changed), [(0, 1, 3)]);
        assert_eq!(shown.look(0, 1).bg, SELECTION_BG);
    }

    #[test]
    fn a_reshape_repaints_every_cell() {
        let (mut g, mut shown) = drawn(b"ab");
        g.resize(ROWS as u16, COLS as u16 - 1, &mut []);
        shown.reshape(ROWS, COLS - 1);
        let changed = shown.update(&g.take_damage(), &g, &Selection::NONE, true);
        let whole = (0..ROWS).map(|row| (row, 0, COLS as u16 - 2));
        assert_eq!(spans(&changed), whole.collect::<Vec<_>>());
    }
}
