//! Procedural box-drawing (U+2500..U+257F) and block-element (U+2580..U+259F)
//! glyphs.
//!
//! The atlas centres a rasterized glyph on the ASCII-derived advance and clips
//! it to the cell, and a font's box-drawing glyphs do not span that box — a
//! framed TUI drawn from them shows a seam between every pair of adjacent
//! cells. Drawing this block from cell geometry instead keeps the strokes edge
//! to edge, and gives the 8x16 bitmap console a block it has no glyphs for at
//! all.

const LINE_FIRST: u32 = 0x2500;
const BLOCK_FIRST: u32 = 0x2580;
const BLOCK_LAST: u32 = 0x259F;
const QUADRANT_FIRST: u32 = 0x2596;

const INK: u8 = 255;

const W_NONE: u8 = 0;
const W_LIGHT: u8 = 1;
const W_HEAVY: u8 = 2;
const W_DOUBLE: u8 = 3;

const UP: usize = 0;
const RIGHT: usize = 1;
const DOWN: usize = 2;
const LEFT: usize = 3;

/// Leg weights of U+2500..U+257F indexed by `cp - 0x2500`, two bits per leg:
/// up in bits 0-1, right in 2-3, down in 4-5, left in 6-7, each holding
/// `W_NONE` / `W_LIGHT` / `W_HEAVY` / `W_DOUBLE`. Transcribed from the Unicode
/// 16.0 character names, which name every leg's weight; the dash count and the
/// arc/diagonal shape come from the codepoint ranges in [`draw_line`], so the
/// diagonals (U+2571..U+2573, no legs) are zero here.
static LEG_WEIGHTS: [u8; 128] = [
    0x44, 0x88, 0x11, 0x22, 0x44, 0x88, 0x11, 0x22, 0x44, 0x88, 0x11, 0x22, 0x14, 0x18, 0x24, 0x28,
    0x50, 0x90, 0x60, 0xa0, 0x05, 0x09, 0x06, 0x0a, 0x41, 0x81, 0x42, 0x82, 0x15, 0x19, 0x16, 0x25,
    0x26, 0x1a, 0x29, 0x2a, 0x51, 0x91, 0x52, 0x61, 0x62, 0x92, 0xa1, 0xa2, 0x54, 0x94, 0x58, 0x98,
    0x64, 0xa4, 0x68, 0xa8, 0x45, 0x85, 0x49, 0x89, 0x46, 0x86, 0x4a, 0x8a, 0x55, 0x95, 0x59, 0x99,
    0x56, 0x65, 0x66, 0x96, 0x5a, 0xa5, 0x69, 0x9a, 0xa9, 0xa6, 0x6a, 0xaa, 0x44, 0x88, 0x11, 0x22,
    0xcc, 0x33, 0x1c, 0x34, 0x3c, 0xd0, 0x70, 0xf0, 0x0d, 0x07, 0x0f, 0xc1, 0x43, 0xc3, 0x1d, 0x37,
    0x3f, 0xd1, 0x73, 0xf3, 0xdc, 0x74, 0xfc, 0xcd, 0x47, 0xcf, 0xdd, 0x77, 0xff, 0x14, 0x50, 0x41,
    0x05, 0x00, 0x00, 0x00, 0x40, 0x01, 0x04, 0x10, 0x80, 0x02, 0x08, 0x20, 0x48, 0x21, 0x84, 0x12,
];

/// Quadrant occupancy of U+2596..U+259F indexed by `cp - 0x2596`: upper left 1,
/// upper right 2, lower left 4, lower right 8.
static QUADRANT_MASKS: [u8; 10] = [4, 8, 1, 13, 9, 7, 11, 2, 6, 14];

static BAYER: [u8; 16] = [0, 8, 2, 10, 12, 4, 14, 6, 3, 11, 1, 9, 15, 7, 13, 5];

/// Procedurally draw the box-drawing / block-element glyph for `cp` into
/// `cell` (`cell_w * cell_h` coverage bytes, row-major, 0 = background,
/// 255 = full ink). Returns false and leaves `cell` untouched when `cp` is
/// not one this module owns.
pub fn draw(cp: u32, cell: &mut [u8], cell_w: usize, cell_h: usize) -> bool {
    if !owns(cp) || cell_w == 0 || cell_h == 0 {
        return false;
    }
    let Some(len) = cell_w.checked_mul(cell_h) else {
        return false;
    };
    if cell.len() < len {
        return false;
    }

    let cell = &mut cell[..len];
    cell.fill(0);
    let geom = Geom::new(cell_w, cell_h);
    if cp >= BLOCK_FIRST {
        draw_block(&geom, cell, cp);
    } else {
        draw_line(&geom, cell, cp);
    }
    true
}

/// Whether [`draw`] owns `cp`.
pub fn owns(cp: u32) -> bool {
    (LINE_FIRST..=BLOCK_LAST).contains(&cp)
}

/// Every stroke position in the block derives from these six numbers, so a
/// stroke of one weight lands on the same rows (or columns) in every glyph
/// that has one and adjacent cells join.
struct Geom {
    w: usize,
    h: usize,
    mid_x: usize,
    mid_y: usize,
    light_x: usize,
    light_y: usize,
}

impl Geom {
    fn new(w: usize, h: usize) -> Self {
        Self {
            w,
            h,
            mid_x: (w - 1) / 2,
            mid_y: (h - 1) / 2,
            light_x: (w / 8).max(1),
            light_y: (h / 10).max(1),
        }
    }

    fn v_band(&self, weight: u8) -> (usize, usize) {
        band(self.mid_x, thickness(self.light_x, weight), self.w)
    }

    fn h_band(&self, weight: u8) -> (usize, usize) {
        band(self.mid_y, thickness(self.light_y, weight), self.h)
    }

    fn fill(&self, cell: &mut [u8], x: (usize, usize), y: (usize, usize)) {
        for row in y.0..y.1 {
            let base = row * self.w;
            cell[base + x.0..base + x.1].fill(INK);
        }
    }

    fn blob(&self, cell: &mut [u8], x: usize, y: usize) {
        self.fill(
            cell,
            band(x, self.light_x, self.w),
            band(y, self.light_y, self.h),
        );
    }
}

fn thickness(light: usize, weight: u8) -> usize {
    match weight {
        W_LIGHT => light,
        W_HEAVY => (light * 2).max(light + 1),
        W_DOUBLE => light * 3,
        _ => 0,
    }
}

fn band(mid: usize, thick: usize, limit: usize) -> (usize, usize) {
    let thick = thick.clamp(1, limit);
    let start = mid.saturating_sub(thick / 2).min(limit - thick);
    (start, start + thick)
}

#[derive(Clone, Copy)]
struct Rect {
    x: (usize, usize),
    y: (usize, usize),
}

impl Rect {
    const EMPTY: Self = Self {
        x: (0, 0),
        y: (0, 0),
    };

    fn holds(&self, x: usize, y: usize) -> bool {
        x >= self.x.0 && x < self.x.1 && y >= self.y.0 && y < self.y.1
    }
}

fn draw_line(g: &Geom, cell: &mut [u8], cp: u32) {
    let packed = LEG_WEIGHTS[(cp - LINE_FIRST) as usize];
    let weights = [
        packed & 3,
        (packed >> 2) & 3,
        (packed >> 4) & 3,
        (packed >> 6) & 3,
    ];
    match cp {
        0x2504..=0x2507 => draw_dashed(g, cell, weights, 3),
        0x2508..=0x250B => draw_dashed(g, cell, weights, 4),
        0x254C..=0x254F => draw_dashed(g, cell, weights, 2),
        0x256D..=0x2570 => draw_arc(g, cell, weights),
        0x2571..=0x2573 => draw_diagonal(g, cell, cp),
        _ => draw_legs(g, cell, weights),
    }
}

/// Spans covered by the two legs of one axis: `band` is their union, `core` the
/// union of the gaps inside the double-weight ones.
struct AxisSpans {
    band: Option<(usize, usize)>,
    core: Option<(usize, usize)>,
}

fn axis_spans(
    weights: &[u8; 4],
    perp: &[(usize, usize); 4],
    perp_core: &[(usize, usize); 4],
    legs: [usize; 2],
) -> AxisSpans {
    let mut spans = AxisSpans {
        band: None,
        core: None,
    };
    for leg in legs {
        if weights[leg] == W_NONE {
            continue;
        }
        spans.band = Some(match spans.band {
            None => perp[leg],
            Some(b) => (b.0.min(perp[leg].0), b.1.max(perp[leg].1)),
        });
        if weights[leg] == W_DOUBLE {
            spans.core = Some(match spans.core {
                None => perp_core[leg],
                Some(c) => (c.0.min(perp_core[leg].0), c.1.max(perp_core[leg].1)),
            });
        }
    }
    spans
}

/// Where a corridor coming from the low side of the axis stops: at the crossing
/// corridor when the perpendicular legs are double (the two corridors join), at
/// the near edge of a single or heavy perpendicular stroke (which the corridor
/// cannot cross), else at the midline.
fn corridor_end(axis: &AxisSpans, midline: usize) -> usize {
    match (axis.core, axis.band) {
        (Some(core), _) => core.1,
        (None, Some(band)) => band.0,
        (None, None) => midline,
    }
}

fn corridor_start(axis: &AxisSpans, midline: usize) -> usize {
    match (axis.core, axis.band) {
        (Some(core), _) => core.0,
        (None, Some(band)) => band.1,
        (None, None) => midline,
    }
}

/// Draws up to four legs. Each leg is a band running from the cell edge to the
/// far side of the perpendicular legs; a double leg additionally carries a
/// corridor — the gap between its two strokes — and ink is band minus corridor
/// over the whole glyph, which is what turns the corners and breaks the lines
/// of the double-line junctions.
fn draw_legs(g: &Geom, cell: &mut [u8], weights: [u8; 4]) {
    let mut perp = [(0usize, 0usize); 4];
    let mut perp_core = [(0usize, 0usize); 4];
    for (leg, &weight) in weights.iter().enumerate() {
        if weight == W_NONE {
            continue;
        }
        let vertical = leg == UP || leg == DOWN;
        let b = if vertical {
            g.v_band(weight)
        } else {
            g.h_band(weight)
        };
        perp[leg] = b;
        if weight == W_DOUBLE {
            let light = if vertical { g.light_x } else { g.light_y };
            let start = (b.0 + light).min(b.1);
            perp_core[leg] = (start, (start + light).min(b.1));
        }
    }

    let rows = axis_spans(&weights, &perp, &perp_core, [LEFT, RIGHT]);
    let cols = axis_spans(&weights, &perp, &perp_core, [UP, DOWN]);

    let up_end = rows.band.map_or(g.mid_y + 1, |b| b.1.max(g.mid_y + 1));
    let down_start = rows.band.map_or(g.mid_y, |b| b.0.min(g.mid_y));
    let left_end = cols.band.map_or(g.mid_x + 1, |b| b.1.max(g.mid_x + 1));
    let right_start = cols.band.map_or(g.mid_x, |b| b.0.min(g.mid_x));

    let bands = [
        rect_if(weights[UP] != W_NONE, perp[UP], (0, up_end)),
        rect_if(weights[RIGHT] != W_NONE, (right_start, g.w), perp[RIGHT]),
        rect_if(weights[DOWN] != W_NONE, perp[DOWN], (down_start, g.h)),
        rect_if(weights[LEFT] != W_NONE, (0, left_end), perp[LEFT]),
    ];
    let cores = [
        rect_if(
            weights[UP] == W_DOUBLE,
            perp_core[UP],
            (0, corridor_end(&rows, g.mid_y + 1)),
        ),
        rect_if(
            weights[RIGHT] == W_DOUBLE,
            (corridor_start(&cols, g.mid_x), g.w),
            perp_core[RIGHT],
        ),
        rect_if(
            weights[DOWN] == W_DOUBLE,
            perp_core[DOWN],
            (corridor_start(&rows, g.mid_y), g.h),
        ),
        rect_if(
            weights[LEFT] == W_DOUBLE,
            (0, corridor_end(&cols, g.mid_x + 1)),
            perp_core[LEFT],
        ),
    ];

    for y in 0..g.h {
        for x in 0..g.w {
            if bands.iter().any(|r| r.holds(x, y)) && !cores.iter().any(|r| r.holds(x, y)) {
                cell[y * g.w + x] = INK;
            }
        }
    }
}

fn rect_if(present: bool, x: (usize, usize), y: (usize, usize)) -> Rect {
    if present { Rect { x, y } } else { Rect::EMPTY }
}

fn draw_dashed(g: &Geom, cell: &mut [u8], weights: [u8; 4], dashes: usize) {
    let vertical = weights[UP] != W_NONE;
    let weight = if vertical {
        weights[UP]
    } else {
        weights[RIGHT]
    };
    if vertical {
        let x = g.v_band(weight);
        for y in 0..g.h {
            if dash_ink(y, g.h, dashes) {
                g.fill(cell, x, (y, y + 1));
            }
        }
    } else {
        let y = g.h_band(weight);
        for x in 0..g.w {
            if dash_ink(x, g.w, dashes) {
                g.fill(cell, (x, x + 1), y);
            }
        }
    }
}

/// Dash periods of `len / dashes` pixels with the last pixel of each period
/// blank, computed in units of `dashes` so the period is identical in every
/// cell of a run whether or not `dashes` divides `len`.
fn dash_ink(i: usize, len: usize, dashes: usize) -> bool {
    if len <= dashes {
        return true;
    }
    (i * dashes) % len < len - dashes
}

fn draw_arc(g: &Geom, cell: &mut [u8], weights: [u8; 4]) {
    let right = weights[RIGHT] != W_NONE;
    let down = weights[DOWN] != W_NONE;
    // One radius for all four corners, so the arcs of a frame match.
    let radius = g
        .mid_x
        .min(g.w - 1 - g.mid_x)
        .min(g.mid_y)
        .min(g.h - 1 - g.mid_y);
    if radius == 0 {
        // No room to curve in a cell this thin; the square corner still joins.
        draw_legs(g, cell, weights);
        return;
    }

    let cx = if right {
        g.mid_x + radius
    } else {
        g.mid_x - radius
    };
    let cy = if down {
        g.mid_y + radius
    } else {
        g.mid_y - radius
    };
    let r2 = radius * radius;
    for step in 0..=radius {
        let off = isqrt(r2 - step * step);
        for (dx, dy) in [(step, off), (off, step)] {
            let x = if right { cx - dx } else { cx + dx };
            let y = if down { cy - dy } else { cy + dy };
            g.blob(cell, x, y);
        }
    }

    let tail_x = band(g.mid_x, g.light_x, g.w);
    let tail_y = band(g.mid_y, g.light_y, g.h);
    if down {
        g.fill(cell, tail_x, (cy, g.h));
    } else {
        g.fill(cell, tail_x, (0, cy + 1));
    }
    if right {
        g.fill(cell, (cx, g.w), tail_y);
    } else {
        g.fill(cell, (0, cx + 1), tail_y);
    }
}

/// Sweeping the dominant axis keeps the stroke connected; both sweeps of the
/// arc do the same for its quarter circle.
fn draw_diagonal(g: &Geom, cell: &mut [u8], cp: u32) {
    if cp != 0x2571 {
        sweep_diagonal(g, cell, true);
    }
    if cp != 0x2572 {
        sweep_diagonal(g, cell, false);
    }
}

fn sweep_diagonal(g: &Geom, cell: &mut [u8], descending: bool) {
    let (w1, h1) = (g.w - 1, g.h - 1);
    if w1 >= h1 {
        for x in 0..g.w {
            let step = if w1 == 0 { 0 } else { (x * h1 + w1 / 2) / w1 };
            let y = if descending { step } else { h1 - step };
            g.blob(cell, x, y);
        }
    } else {
        for y in 0..g.h {
            let step = (y * w1 + h1 / 2) / h1;
            let x = if descending { step } else { w1 - step };
            g.blob(cell, x, y);
        }
    }
}

fn isqrt(v: usize) -> usize {
    if v < 2 {
        return v;
    }
    let mut x = v;
    let mut next = (x + 1) / 2;
    while next < x {
        x = next;
        next = (x + v / x) / 2;
    }
    x
}

fn draw_block(g: &Geom, cell: &mut [u8], cp: u32) {
    let full_w = (0, g.w);
    let full_h = (0, g.h);
    match cp {
        0x2580 => g.fill(cell, full_w, (0, eighths(g.h, 4))),
        0x2581..=0x2587 => {
            let e = eighths(g.h, (cp - 0x2580) as usize);
            g.fill(cell, full_w, (g.h - e, g.h));
        }
        0x2588 => g.fill(cell, full_w, full_h),
        0x2589..=0x258F => {
            let e = eighths(g.w, (0x2590 - cp) as usize);
            g.fill(cell, (0, e), full_h);
        }
        0x2590 => {
            let e = eighths(g.w, 4);
            g.fill(cell, (g.w - e, g.w), full_h);
        }
        0x2591 => shade(g, cell, 4),
        0x2592 => shade(g, cell, 8),
        0x2593 => shade(g, cell, 12),
        0x2594 => g.fill(cell, full_w, (0, eighths(g.h, 1))),
        0x2595 => {
            let e = eighths(g.w, 1);
            g.fill(cell, (g.w - e, g.w), full_h);
        }
        _ => quadrants(g, cell, QUADRANT_MASKS[(cp - QUADRANT_FIRST) as usize]),
    }
}

fn eighths(total: usize, n: usize) -> usize {
    ((n * total + 4) / 8).clamp(1, total)
}

/// Ordered 4x4 dither rather than a flat coverage value, so a shaded run reads
/// as texture at any cell size and at any foreground colour.
fn shade(g: &Geom, cell: &mut [u8], level: u8) {
    for y in 0..g.h {
        for x in 0..g.w {
            if BAYER[(y % 4) * 4 + (x % 4)] < level {
                cell[y * g.w + x] = INK;
            }
        }
    }
}

fn quadrants(g: &Geom, cell: &mut [u8], mask: u8) {
    let left = (0, eighths(g.w, 4));
    let right = (g.w - eighths(g.w, 4), g.w);
    let top = (0, eighths(g.h, 4));
    let bottom = (g.h - eighths(g.h, 4), g.h);
    for (bit, x, y) in [
        (1, left, top),
        (2, right, top),
        (4, left, bottom),
        (8, right, bottom),
    ] {
        if mask & bit != 0 {
            g.fill(cell, x, y);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slopos_ostd::KVec;

    /// 8x16 is the bitmap console, 10x22 the measured JetBrains Mono 16 px
    /// cell, 10x21 an odd height so the midlines land differently.
    const SIZES: [(usize, usize); 3] = [(8, 16), (10, 21), (10, 22)];

    fn render(cp: u32, w: usize, h: usize) -> KVec<u8> {
        let mut cell = KVec::<u8>::zeroed(w * h).expect("test alloc");
        assert!(draw(cp, &mut cell, w, h), "draw rejected {cp:#x}");
        cell
    }

    fn rows_at_column(cell: &[u8], w: usize, h: usize, x: usize) -> u64 {
        let mut mask = 0u64;
        for y in 0..h {
            if cell[y * w + x] != 0 {
                mask |= 1 << y;
            }
        }
        mask
    }

    fn columns_at_row(cell: &[u8], w: usize, y: usize) -> u64 {
        let mut mask = 0u64;
        for x in 0..w {
            if cell[y * w + x] != 0 {
                mask |= 1 << x;
            }
        }
        mask
    }

    fn inked(cell: &[u8]) -> usize {
        cell.iter().filter(|&&v| v != 0).count()
    }

    #[test]
    fn owns_exactly_the_box_and_block_ranges() {
        assert!(!owns(0x24FF));
        assert!(owns(0x2500));
        assert!(owns(0x257F));
        assert!(owns(0x2580));
        assert!(owns(0x259F));
        assert!(!owns(0x25A0));
        assert!(!owns(0x41));
    }

    #[test]
    fn every_owned_codepoint_draws_ink() {
        for (w, h) in SIZES {
            for cp in 0x2500..=0x259F {
                let cell = render(cp, w, h);
                assert!(inked(&cell) > 0, "{cp:#x} blank at {w}x{h}");
            }
        }
    }

    #[test]
    fn through_lines_reach_both_edges() {
        for (w, h) in SIZES {
            let horizontal = render(0x2500, w, h);
            let first = rows_at_column(&horizontal, w, h, 0);
            assert_ne!(first, 0);
            assert_eq!(first, rows_at_column(&horizontal, w, h, w - 1));

            let vertical = render(0x2502, w, h);
            let top = columns_at_row(&vertical, w, 0);
            assert_ne!(top, 0);
            assert_eq!(top, columns_at_row(&vertical, w, h - 1));
        }
    }

    #[test]
    fn stubs_reach_their_own_edges() {
        for (w, h) in SIZES {
            let corner = render(0x250C, w, h);
            let line = render(0x2500, w, h);
            assert_eq!(
                rows_at_column(&corner, w, h, w - 1),
                rows_at_column(&line, w, h, 0)
            );
            assert_eq!(
                columns_at_row(&corner, w, h - 1),
                columns_at_row(&render(0x2502, w, h), w, 0)
            );
            assert_eq!(rows_at_column(&corner, w, h, 0), 0);
        }
    }

    #[test]
    fn junction_legs_join_through_lines() {
        for (w, h) in SIZES {
            let horizontal = rows_at_column(&render(0x2500, w, h), w, h, 0);
            for cp in [0x252C, 0x253C] {
                let cell = render(cp, w, h);
                assert_eq!(rows_at_column(&cell, w, h, 0), horizontal, "{cp:#x} left");
                assert_eq!(
                    rows_at_column(&cell, w, h, w - 1),
                    horizontal,
                    "{cp:#x} right"
                );
            }

            let vertical = columns_at_row(&render(0x2502, w, h), w, 0);
            for cp in [0x251C, 0x253C] {
                let cell = render(cp, w, h);
                assert_eq!(columns_at_row(&cell, w, h - 1), vertical, "{cp:#x} bottom");
            }
            assert_eq!(columns_at_row(&render(0x253C, w, h), w, 0), vertical);
        }
    }

    #[test]
    fn halves_tile_into_the_full_block() {
        for (w, h) in SIZES {
            let full = render(0x2588, w, h);
            let upper = render(0x2580, w, h);
            let lower = render(0x2584, w, h);
            for i in 0..w * h {
                assert_eq!(full[i], upper[i].max(lower[i]), "byte {i} at {w}x{h}");
            }
        }
    }

    #[test]
    fn shades_are_increasing_dithers() {
        for (w, h) in SIZES {
            let mut previous = 0;
            for cp in [0x2591, 0x2592, 0x2593] {
                let cell = render(cp, w, h);
                let count = inked(&cell);
                assert!(count > previous, "{cp:#x} not denser at {w}x{h}");
                assert!(count < w * h, "{cp:#x} is a flat fill at {w}x{h}");
                previous = count;
            }
        }
    }

    #[test]
    fn heavy_strokes_are_thicker_than_light() {
        for (w, h) in SIZES {
            let light_rows = rows_at_column(&render(0x2500, w, h), w, h, 0).count_ones();
            let heavy_rows = rows_at_column(&render(0x2501, w, h), w, h, 0).count_ones();
            assert!(heavy_rows > light_rows, "{w}x{h}");

            let light_cols = columns_at_row(&render(0x2502, w, h), w, 0).count_ones();
            let heavy_cols = columns_at_row(&render(0x2503, w, h), w, 0).count_ones();
            assert!(heavy_cols > light_cols, "{w}x{h}");
        }
    }

    #[test]
    fn double_lines_carry_a_corridor() {
        let cell = render(0x2550, 8, 16);
        let rows = rows_at_column(&cell, 8, 16, 0);
        assert_eq!(rows.count_ones(), 2, "double horizontal is two strokes");
        let single = rows_at_column(&render(0x2500, 8, 16), 8, 16, 0);
        assert_eq!(rows & single, 0, "corridor sits on the light stroke's row");
    }

    #[test]
    fn unowned_codepoints_leave_the_cell_untouched() {
        let mut cell = KVec::<u8>::zeroed(8 * 16).expect("test alloc");
        cell.fill(7);
        for cp in [0x41, 0x25A0, 0x24FF] {
            assert!(!draw(cp, &mut cell, 8, 16));
        }
        assert!(cell.iter().all(|&v| v == 7));
    }

    #[test]
    fn bad_geometry_is_refused() {
        let mut cell = KVec::<u8>::zeroed(8 * 16).expect("test alloc");
        assert!(!draw(0x2500, &mut cell, 8, 17));
        assert!(!draw(0x2500, &mut cell, 0, 16));
        assert!(!draw(0x2500, &mut cell, 8, 0));
        assert!(!draw(0x2588, &mut cell, usize::MAX, usize::MAX));
        assert!(cell.iter().all(|&v| v == 0));
        assert!(draw(0x2500, &mut cell, 8, 16));
    }
}
