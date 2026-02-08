use slopos_abi::damage::DamageRect;
use slopos_abi::draw::{Canvas, Color32};

#[inline]
fn emit<T: Canvas>(target: &mut T, damage: Option<DamageRect>) -> Option<DamageRect> {
    if let Some(d) = damage {
        target.report_damage(d);
    }
    damage
}

pub fn line<T: Canvas>(
    target: &mut T,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: Color32,
) -> Option<DamageRect> {
    let w = target.width() as i32;
    let h = target.height() as i32;

    if (x0 < 0 && x1 < 0) || (y0 < 0 && y1 < 0) || (x0 >= w && x1 >= w) || (y0 >= h && y1 >= h) {
        return None;
    }

    let px = target.pixel_format().encode(color);

    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let mut cx = x0;
    let mut cy = y0;

    let mut min_x = x0.min(x1);
    let mut min_y = y0.min(y1);
    let mut max_x = x0.max(x1);
    let mut max_y = y0.max(y1);

    loop {
        target.put_pixel(cx, cy, px);
        if cx == x1 && cy == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            cx += sx;
        }
        if e2 <= dx {
            err += dx;
            cy += sy;
        }
    }

    min_x = min_x.max(0);
    min_y = min_y.max(0);
    max_x = max_x.min(w - 1);
    max_y = max_y.min(h - 1);

    let damage = if min_x <= max_x && min_y <= max_y {
        Some(DamageRect {
            x0: min_x,
            y0: min_y,
            x1: max_x,
            y1: max_y,
        })
    } else {
        None
    };
    emit(target, damage)
}

pub fn rect<T: Canvas>(
    target: &mut T,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: Color32,
) -> Option<DamageRect> {
    if w <= 0 || h <= 0 {
        return None;
    }
    let px = target.pixel_format().encode(color);
    target.hline(x, x + w - 1, y, px);
    target.hline(x, x + w - 1, y + h - 1, px);
    target.vline(x, y, y + h - 1, px);
    target.vline(x + w - 1, y, y + h - 1, px);

    let buf_w = target.width() as i32;
    let buf_h = target.height() as i32;
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + w - 1).min(buf_w - 1);
    let y1 = (y + h - 1).min(buf_h - 1);

    let damage = if x0 <= x1 && y0 <= y1 {
        Some(DamageRect { x0, y0, x1, y1 })
    } else {
        None
    };
    emit(target, damage)
}

pub fn fill_rect<T: Canvas>(
    target: &mut T,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: Color32,
) -> Option<DamageRect> {
    if w <= 0 || h <= 0 {
        return None;
    }
    let px = target.pixel_format().encode(color);
    target.fill_rect_encoded(x, y, w, h, px);

    let buf_w = target.width() as i32;
    let buf_h = target.height() as i32;
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + w - 1).min(buf_w - 1);
    let y1 = (y + h - 1).min(buf_h - 1);

    let damage = if x0 <= x1 && y0 <= y1 {
        Some(DamageRect { x0, y0, x1, y1 })
    } else {
        None
    };
    emit(target, damage)
}

pub fn fill_rect_clipped<T: Canvas>(
    target: &mut T,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: Color32,
    clip: &DamageRect,
) {
    let rx0 = x.max(clip.x0);
    let ry0 = y.max(clip.y0);
    let rx1 = (x + w - 1).min(clip.x1);
    let ry1 = (y + h - 1).min(clip.y1);
    if rx0 <= rx1 && ry0 <= ry1 {
        fill_rect(target, rx0, ry0, rx1 - rx0 + 1, ry1 - ry0 + 1, color);
    }
}

pub fn circle<T: Canvas>(
    target: &mut T,
    cx: i32,
    cy: i32,
    radius: i32,
    color: Color32,
) -> Option<DamageRect> {
    if radius <= 0 {
        return None;
    }
    let px = target.pixel_format().encode(color);

    let mut x = 0i32;
    let mut y = radius;
    let mut d = 1 - radius;

    while x <= y {
        target.put_pixel(cx + x, cy + y, px);
        target.put_pixel(cx - x, cy + y, px);
        target.put_pixel(cx + x, cy - y, px);
        target.put_pixel(cx - x, cy - y, px);
        target.put_pixel(cx + y, cy + x, px);
        target.put_pixel(cx - y, cy + x, px);
        target.put_pixel(cx + y, cy - x, px);
        target.put_pixel(cx - y, cy - x, px);

        x += 1;
        if d < 0 {
            d += 2 * x + 1;
        } else {
            y -= 1;
            d += 2 * (x - y) + 1;
        }
    }

    let buf_w = target.width() as i32;
    let buf_h = target.height() as i32;
    let x0 = (cx - radius).max(0);
    let y0 = (cy - radius).max(0);
    let x1 = (cx + radius).min(buf_w - 1);
    let y1 = (cy + radius).min(buf_h - 1);

    let damage = if x0 <= x1 && y0 <= y1 {
        Some(DamageRect { x0, y0, x1, y1 })
    } else {
        None
    };
    emit(target, damage)
}

pub fn circle_filled<T: Canvas>(
    target: &mut T,
    cx: i32,
    cy: i32,
    radius: i32,
    color: Color32,
) -> Option<DamageRect> {
    if radius <= 0 {
        return None;
    }
    let px = target.pixel_format().encode(color);

    let mut x = 0i32;
    let mut y = radius;
    let mut d = 1 - radius;

    target.hline(cx - radius, cx + radius, cy, px);

    while x < y {
        x += 1;
        if d < 0 {
            d += 2 * x + 1;
        } else {
            target.hline(cx - x + 1, cx + x - 1, cy + y, px);
            target.hline(cx - x + 1, cx + x - 1, cy - y, px);
            y -= 1;
            d += 2 * (x - y) + 1;
        }

        target.hline(cx - y, cx + y, cy + x, px);
        target.hline(cx - y, cx + y, cy - x, px);
    }

    let buf_w = target.width() as i32;
    let buf_h = target.height() as i32;
    let x0 = (cx - radius).max(0);
    let y0 = (cy - radius).max(0);
    let x1 = (cx + radius).min(buf_w - 1);
    let y1 = (cy + radius).min(buf_h - 1);

    let damage = if x0 <= x1 && y0 <= y1 {
        Some(DamageRect { x0, y0, x1, y1 })
    } else {
        None
    };
    emit(target, damage)
}

pub fn triangle_filled<T: Canvas>(
    target: &mut T,
    mut x0: i32,
    mut y0: i32,
    mut x1: i32,
    mut y1: i32,
    mut x2: i32,
    mut y2: i32,
    color: Color32,
) -> Option<DamageRect> {
    let px = target.pixel_format().encode(color);

    if y0 > y1 {
        core::mem::swap(&mut y0, &mut y1);
        core::mem::swap(&mut x0, &mut x1);
    }
    if y1 > y2 {
        core::mem::swap(&mut y1, &mut y2);
        core::mem::swap(&mut x1, &mut x2);
    }
    if y0 > y1 {
        core::mem::swap(&mut y0, &mut y1);
        core::mem::swap(&mut x0, &mut x1);
    }

    let total_height = y2 - y0;
    if total_height == 0 {
        return None;
    }

    for y in y0..=y2 {
        let second_half = y > y1 || y1 == y0;
        let segment_height = if second_half { y2 - y1 } else { y1 - y0 };
        if segment_height == 0 {
            continue;
        }

        let dy = y - if second_half { y1 } else { y0 };
        let alpha = ((y - y0) as i64 * 65536) / total_height as i64;
        let beta = (dy as i64 * 65536) / segment_height as i64;

        let ax = x0 + (((x2 - x0) as i64 * alpha) >> 16) as i32;
        let bx = if second_half {
            x1 + (((x2 - x1) as i64 * beta) >> 16) as i32
        } else {
            x0 + (((x1 - x0) as i64 * beta) >> 16) as i32
        };

        let (xa, xb) = if ax < bx { (ax, bx) } else { (bx, ax) };
        target.hline(xa, xb, y, px);
    }

    let buf_w = target.width() as i32;
    let buf_h = target.height() as i32;
    let min_x = x0.min(x1).min(x2).max(0);
    let min_y = y0.max(0);
    let max_x = x0.max(x1).max(x2).min(buf_w - 1);
    let max_y = y2.min(buf_h - 1);

    let damage = if min_x <= max_x && min_y <= max_y {
        Some(DamageRect {
            x0: min_x,
            y0: min_y,
            x1: max_x,
            y1: max_y,
        })
    } else {
        None
    };
    emit(target, damage)
}
