use slopos_abi::damage::DamageRect;
use slopos_abi::draw::{Canvas, Color32};
use slopos_abi::font::{FONT_CHAR_HEIGHT, FONT_CHAR_WIDTH, get_glyph_or_space};

pub fn draw_char<T: Canvas>(
    target: &mut T,
    x: i32,
    y: i32,
    ch: u8,
    fg: Color32,
    bg: Color32,
) -> Option<DamageRect> {
    let fmt = target.pixel_format();
    let fg_px = fmt.encode(fg);
    let bg_px = fmt.encode(bg);
    let glyph = get_glyph_or_space(ch);
    let has_bg = bg.0 != 0;

    for (row_idx, &row_bits) in glyph.iter().enumerate() {
        let py = y + row_idx as i32;
        for col in 0..FONT_CHAR_WIDTH {
            let px = x + col;
            let is_fg = (row_bits & (0x80 >> col)) != 0;
            if is_fg {
                target.put_pixel(px, py, fg_px);
            } else if has_bg {
                target.put_pixel(px, py, bg_px);
            }
        }
    }

    let buf_w = target.width() as i32;
    let buf_h = target.height() as i32;
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + FONT_CHAR_WIDTH - 1).min(buf_w - 1);
    let y1 = (y + FONT_CHAR_HEIGHT - 1).min(buf_h - 1);

    if x0 <= x1 && y0 <= y1 {
        Some(DamageRect { x0, y0, x1, y1 })
    } else {
        None
    }
}

pub fn draw_string<T: Canvas>(
    target: &mut T,
    x: i32,
    y: i32,
    text: &[u8],
    fg: Color32,
    bg: Color32,
) -> Option<DamageRect> {
    let w = target.width() as i32;
    let h = target.height() as i32;
    let mut cx = x;
    let mut cy = y;
    let mut damage: Option<DamageRect> = None;

    for &ch in text {
        match ch {
            0 => break,
            b'\n' => {
                cx = x;
                cy += FONT_CHAR_HEIGHT;
            }
            b'\r' => {
                cx = x;
            }
            b'\t' => {
                let tab_width = 4 * FONT_CHAR_WIDTH;
                cx = ((cx - x + tab_width) / tab_width) * tab_width + x;
            }
            _ => {
                if let Some(d) = draw_char(target, cx, cy, ch, fg, bg) {
                    damage = Some(match damage {
                        Some(prev) => prev.union(&d),
                        None => d,
                    });
                }
                cx += FONT_CHAR_WIDTH;
                if cx + FONT_CHAR_WIDTH > w {
                    cx = x;
                    cy += FONT_CHAR_HEIGHT;
                }
            }
        }
        if cy >= h {
            break;
        }
    }

    damage
}

#[inline]
pub fn draw_str<T: Canvas>(
    target: &mut T,
    x: i32,
    y: i32,
    text: &str,
    fg: Color32,
    bg: Color32,
) -> Option<DamageRect> {
    draw_string(target, x, y, text.as_bytes(), fg, bg)
}

pub fn string_width(text: &[u8]) -> i32 {
    let mut width = 0i32;
    for &ch in text {
        match ch {
            0 | b'\n' => break,
            b'\t' => {
                let tab_width = 4 * FONT_CHAR_WIDTH;
                width = ((width + tab_width - 1) / tab_width) * tab_width;
            }
            _ => width += FONT_CHAR_WIDTH,
        }
    }
    width
}

pub fn string_lines(text: &[u8]) -> i32 {
    let mut lines = 1i32;
    for &ch in text {
        if ch == 0 {
            break;
        }
        if ch == b'\n' {
            lines += 1;
        }
    }
    lines
}

#[inline]
pub fn str_width(text: &str) -> i32 {
    string_width(text.as_bytes())
}

#[inline]
pub fn str_lines(text: &str) -> i32 {
    string_lines(text.as_bytes())
}
