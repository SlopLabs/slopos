use core::ffi::{c_char, c_void};
use slopos_abi::draw::Color32;
use slopos_abi::video_traits::{VideoError, VideoResult};
use slopos_drivers::pit::pit_poll_delay_ms;

use crate::graphics::GraphicsContext;
use crate::{font, framebuffer};
use slopos_gfx::canvas_ops;

const ROULETTE_BLANK_COLOR: Color32 = Color32(0x2A31_3BFF);
const ROULETTE_BLANK_HIGHLIGHT: Color32 = Color32(0x5660_70FF);
const ROULETTE_COLORED_HIGHLIGHT: Color32 = Color32(0x4DE3_CAFF);
const ROULETTE_POINTER_COLOR: Color32 = Color32(0xFFE0_87FF);
const ROULETTE_INFO_BG_COLOR: Color32 = Color32(0x0E14_1CFF);
const ROULETTE_CARD_BORDER: Color32 = Color32(0x6470_83FF);
const ROULETTE_CARD_TEXT: Color32 = Color32(0xF6F9_FDFF);
const ROULETTE_MUTED_TEXT: Color32 = Color32(0xAAB4_C5FF);
const ROULETTE_WIN_BG: Color32 = Color32(0x0D37_31FF);
const ROULETTE_LOSE_BG: Color32 = Color32(0x3A20_24FF);

const ROULETTE_SEGMENT_COUNT: i32 = 12;
const ROULETTE_TRIG_SCALE: i32 = 1024;
const ROULETTE_WHEEL_RADIUS: i32 = 120;
const ROULETTE_INNER_RADIUS: i32 = 36;
const ROULETTE_DEGREE_STEPS: i32 = 360;
const ROULETTE_SEGMENT_DEGREES: i32 = 360 / ROULETTE_SEGMENT_COUNT;
const ROULETTE_SPIN_LOOPS: i32 = 4;
const ROULETTE_SPIN_DURATION_MS: i32 = 7200;
const ROULETTE_SPIN_FRAME_DELAY_MS: i32 = 12;

// Public colors pulled from the legacy header.
pub const ROULETTE_BG_COLOR: Color32 = Color32(0x0000_0000);
pub const ROULETTE_WHEEL_COLOR: Color32 = Color32(0xD4DB_E6FF);
pub const ROULETTE_TEXT_COLOR: Color32 = Color32(0xF1F4_F9FF);
pub const ROULETTE_WIN_COLOR: Color32 = Color32(0x2DD4_B3FF);
pub const ROULETTE_LOSE_COLOR: Color32 = Color32(0xE35D_5BFF);
pub const ROULETTE_EVEN_COLOR: Color32 = Color32(0x3B45_54FF);
pub const ROULETTE_ODD_COLOR: Color32 = Color32(0x0C7A_68FF);
pub const ROULETTE_RESULT_DELAY_MS: u32 = 1700;

#[repr(C)]
pub struct RouletteBackend {
    pub ctx: *mut c_void,
    pub get_size: Option<fn(*mut c_void, *mut i32, *mut i32) -> VideoResult>,
    pub fill_rect: Option<fn(*mut c_void, i32, i32, i32, i32, u32) -> VideoResult>,
    pub draw_line: Option<fn(*mut c_void, i32, i32, i32, i32, u32) -> VideoResult>,
    pub draw_circle: Option<fn(*mut c_void, i32, i32, i32, u32) -> VideoResult>,
    pub draw_circle_filled: Option<fn(*mut c_void, i32, i32, i32, u32) -> VideoResult>,
    pub draw_text: Option<fn(*mut c_void, i32, i32, *const u8, u32, u32) -> VideoResult>,
    pub sleep_ms: Option<fn(*mut c_void, u32)>,
}

#[derive(Copy, Clone)]
struct RouletteSegment {
    is_colored: bool,
}

const SEGMENTS: [RouletteSegment; ROULETTE_SEGMENT_COUNT as usize] = [
    RouletteSegment { is_colored: true },
    RouletteSegment { is_colored: false },
    RouletteSegment { is_colored: true },
    RouletteSegment { is_colored: false },
    RouletteSegment { is_colored: true },
    RouletteSegment { is_colored: false },
    RouletteSegment { is_colored: true },
    RouletteSegment { is_colored: false },
    RouletteSegment { is_colored: true },
    RouletteSegment { is_colored: false },
    RouletteSegment { is_colored: true },
    RouletteSegment { is_colored: false },
];

const TEXT_UNKNOWN: &[u8] = b"Hidden\0";
const TEXT_WIN: &[u8] = b"Win\0";
const TEXT_WIN_SUB: &[u8] = b"Boot path unlocked\0";
const TEXT_LOSE: &[u8] = b"Lose\0";
const TEXT_LOSE_SUB: &[u8] = b"Retry after reboot\0";
const TEXT_CARD_LABEL: &[u8] = b"Number\0";
const TEXT_PARITY_ODD: &[u8] = b"Odd\0";
const TEXT_PARITY_EVEN: &[u8] = b"Even\0";
const TEXT_CURRENCY_WIN: &[u8] = b"+10 W\0";
const TEXT_CURRENCY_LOSE: &[u8] = b"-10 W\0";
const TEXT_BOOTING_TITLE: &[u8] = b"Starting SlopOS\0";
const TEXT_BOOTING_SUB: &[u8] = b"Loading shell and compositor\0";
const TEXT_BOOTING_DETAIL: &[u8] = b"Please wait\0";
const TEXT_REBOOT_TITLE: &[u8] = b"Rebooting\0";
const TEXT_REBOOT_SUB: &[u8] = b"Preparing next spin\0";
const TEXT_REBOOT_DETAIL: &[u8] = b"Please wait\0";
const SPINNER_FRAMES: [&[u8]; 4] = [b"|\0", b"/\0", b"-\0", b"\\\0"];

// Precomputed trig tables (scaled by 1024) carried over from the original C.
const COS_TABLE: [i16; (ROULETTE_SEGMENT_COUNT + 1) as usize] = [
    1024, 887, 512, 0, -512, -887, -1024, -887, -512, 0, 512, 887, 1024,
];
const SIN_TABLE: [i16; (ROULETTE_SEGMENT_COUNT + 1) as usize] = [
    0, 512, 887, 1024, 887, 512, 0, -512, -887, -1024, -887, -512, 0,
];

const COS360: [i16; ROULETTE_DEGREE_STEPS as usize] = [
    1024, 1024, 1023, 1023, 1022, 1020, 1018, 1016, 1014, 1011, 1008, 1005, 1002, 998, 994, 989,
    984, 979, 974, 968, 962, 956, 949, 943, 935, 928, 920, 912, 904, 896, 887, 878, 868, 859, 849,
    839, 828, 818, 807, 796, 784, 773, 761, 749, 737, 724, 711, 698, 685, 672, 658, 644, 630, 616,
    602, 587, 573, 558, 543, 527, 512, 496, 481, 465, 449, 433, 416, 400, 384, 367, 350, 333, 316,
    299, 282, 265, 248, 230, 213, 195, 178, 160, 143, 125, 107, 89, 71, 54, 36, 18, 0, -18, -36,
    -54, -71, -89, -107, -125, -143, -160, -178, -195, -213, -230, -248, -265, -282, -299, -316,
    -333, -350, -367, -384, -400, -416, -433, -449, -465, -481, -496, -512, -527, -543, -558, -573,
    -587, -602, -616, -630, -644, -658, -672, -685, -698, -711, -724, -737, -749, -761, -773, -784,
    -796, -807, -818, -828, -839, -849, -859, -868, -878, -887, -896, -904, -912, -920, -928, -935,
    -943, -949, -956, -962, -968, -974, -979, -984, -989, -994, -998, -1002, -1005, -1008, -1011,
    -1014, -1016, -1018, -1020, -1022, -1023, -1023, -1024, -1024, -1024, -1023, -1023, -1022,
    -1020, -1018, -1016, -1014, -1011, -1008, -1005, -1002, -998, -994, -989, -984, -979, -974,
    -968, -962, -956, -949, -943, -935, -928, -920, -912, -904, -896, -887, -878, -868, -859, -849,
    -839, -828, -818, -807, -796, -784, -773, -761, -749, -737, -724, -711, -698, -685, -672, -658,
    -644, -630, -616, -602, -587, -573, -558, -543, -527, -512, -496, -481, -465, -449, -433, -416,
    -400, -384, -367, -350, -333, -316, -299, -282, -265, -248, -230, -213, -195, -178, -160, -143,
    -125, -107, -89, -71, -54, -36, -18, 0, 18, 36, 54, 71, 89, 107, 125, 143, 160, 178, 195, 213,
    230, 248, 265, 282, 299, 316, 333, 350, 367, 384, 400, 416, 433, 449, 465, 481, 496, 512, 527,
    543, 558, 573, 587, 602, 616, 630, 644, 658, 672, 685, 698, 711, 724, 737, 749, 761, 773, 784,
    796, 807, 818, 828, 839, 849, 859, 868, 878, 887, 896, 904, 912, 920, 928, 935, 943, 949, 956,
    962, 968, 974, 979, 984, 989, 994, 998, 1002, 1005, 1008, 1011, 1014, 1016, 1018, 1020, 1022,
    1023, 1023, 1024,
];

#[allow(clippy::unreadable_literal)]
const SIN360: [i16; ROULETTE_DEGREE_STEPS as usize] = [
    0, 18, 36, 54, 71, 89, 107, 125, 143, 160, 178, 195, 213, 230, 248, 265, 282, 299, 316, 333,
    350, 367, 384, 400, 416, 433, 449, 465, 481, 496, 512, 527, 543, 558, 573, 587, 602, 616, 630,
    644, 658, 672, 685, 698, 711, 724, 737, 749, 761, 773, 784, 796, 807, 818, 828, 839, 849, 859,
    868, 878, 887, 896, 904, 912, 920, 928, 935, 943, 949, 956, 962, 968, 974, 979, 984, 989, 994,
    998, 1002, 1005, 1008, 1011, 1014, 1016, 1018, 1020, 1022, 1023, 1023, 1024, 1024, 1024, 1023,
    1023, 1022, 1020, 1018, 1016, 1014, 1011, 1008, 1005, 1002, 998, 994, 989, 984, 979, 974, 968,
    962, 956, 949, 943, 935, 928, 920, 912, 904, 896, 887, 878, 868, 859, 849, 839, 828, 818, 807,
    796, 784, 773, 761, 749, 737, 724, 711, 698, 685, 672, 658, 644, 630, 616, 602, 587, 573, 558,
    543, 527, 512, 496, 481, 465, 449, 433, 416, 400, 384, 367, 350, 333, 316, 299, 282, 265, 248,
    230, 213, 195, 178, 160, 143, 125, 107, 89, 71, 54, 36, 18, 0, -18, -36, -54, -71, -89, -107,
    -125, -143, -160, -178, -195, -213, -230, -248, -265, -282, -299, -316, -333, -350, -367, -384,
    -400, -416, -433, -449, -465, -481, -496, -512, -527, -543, -558, -573, -587, -602, -616, -630,
    -644, -658, -672, -685, -698, -711, -724, -737, -749, -761, -773, -784, -796, -807, -818, -828,
    -839, -849, -859, -868, -878, -887, -896, -904, -912, -920, -928, -935, -943, -949, -956, -962,
    -968, -974, -979, -984, -989, -994, -998, -1002, -1005, -1008, -1011, -1014, -1016, -1018,
    -1020, -1022, -1023, -1023, -1024, -1024, -1024, -1023, -1023, -1022, -1020, -1018, -1016,
    -1014, -1011, -1008, -1005, -1002, -998, -994, -989, -984, -979, -974, -968, -962, -956, -949,
    -943, -935, -928, -920, -912, -904, -896, -887, -878, -868, -859, -849, -839, -828, -818, -807,
    -796, -784, -773, -761, -749, -737, -724, -711, -698, -685, -672, -658, -644, -630, -616, -602,
    -587, -573, -558, -543, -527, -512, -496, -481, -465, -449, -433, -416, -400, -384, -367, -350,
    -333, -316, -299, -282, -265, -248, -230, -213, -195, -178, -160, -143, -125, -107, -89, -71,
    -54, -36, -18,
];

fn normalize_angle(degrees: i32) -> i32 {
    let mut angle = degrees % ROULETTE_DEGREE_STEPS;
    if angle < 0 {
        angle += ROULETTE_DEGREE_STEPS;
    }
    angle
}

fn cos_deg(degrees: i32) -> i16 {
    COS360[normalize_angle(degrees) as usize]
}

fn sin_deg(degrees: i32) -> i16 {
    SIN360[normalize_angle(degrees) as usize]
}

fn scale(value: i16, radius: i32) -> i32 {
    (value as i32 * radius) / ROULETTE_TRIG_SCALE
}

fn backend_get_size(b: &RouletteBackend, w: &mut i32, h: &mut i32) -> VideoResult {
    match b.get_size {
        Some(f) => f(b.ctx, w as *mut i32, h as *mut i32),
        None => Err(VideoError::NoFramebuffer),
    }
}

fn backend_fill_rect(
    b: &RouletteBackend,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: Color32,
) -> VideoResult {
    match b.fill_rect {
        Some(f) => f(b.ctx, x, y, w, h, color.to_u32()),
        None => Err(VideoError::NoFramebuffer),
    }
}

fn backend_draw_line(
    b: &RouletteBackend,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: Color32,
) -> VideoResult {
    match b.draw_line {
        Some(f) => f(b.ctx, x0, y0, x1, y1, color.to_u32()),
        None => Err(VideoError::NoFramebuffer),
    }
}

fn backend_draw_circle(
    b: &RouletteBackend,
    cx: i32,
    cy: i32,
    radius: i32,
    color: Color32,
) -> VideoResult {
    match b.draw_circle {
        Some(f) => f(b.ctx, cx, cy, radius, color.to_u32()),
        None => Err(VideoError::NoFramebuffer),
    }
}

fn backend_draw_circle_filled(
    b: &RouletteBackend,
    cx: i32,
    cy: i32,
    radius: i32,
    color: Color32,
) -> VideoResult {
    match b.draw_circle_filled {
        Some(f) => f(b.ctx, cx, cy, radius, color.to_u32()),
        None => Err(VideoError::NoFramebuffer),
    }
}

fn backend_draw_text(
    b: &RouletteBackend,
    x: i32,
    y: i32,
    text: &[u8],
    fg: Color32,
    bg: Color32,
) -> VideoResult {
    match b.draw_text {
        Some(f) => f(b.ctx, x, y, text.as_ptr(), fg.to_u32(), bg.to_u32()),
        None => Err(VideoError::NoFramebuffer),
    }
}

fn backend_sleep_ms(b: &RouletteBackend, ms: u32) {
    if let Some(f) = b.sleep_ms {
        f(b.ctx, ms);
    }
}

fn segment_center_angle(segment_index: i32) -> i32 {
    segment_index * ROULETTE_SEGMENT_DEGREES + (ROULETTE_SEGMENT_DEGREES / 2)
}

struct RouletteLayout {
    radius: i32,
    inner_radius: i32,
    pointer_width: i32,
    pointer_tip_radius: i32,
    pointer_base_radius: i32,
}

fn draw_filled_triangle(
    b: &RouletteBackend,
    mut x0: i32,
    mut y0: i32,
    mut x1: i32,
    mut y1: i32,
    mut x2: i32,
    mut y2: i32,
    color: Color32,
) -> VideoResult {
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
        return Ok(());
    }

    for y in y0..=y2 {
        let second_half = y > y1 || y1 == y0;
        let segment_height = if second_half { y2 - y1 } else { y1 - y0 };
        if segment_height == 0 {
            continue;
        }
        let dy = y - if second_half { y1 } else { y0 };
        let alpha = (y - y0) as i64 * 65536 / total_height as i64;
        let beta = dy as i64 * 65536 / segment_height as i64;

        let ax = x0 + (((x2 - x0) as i64 * alpha) >> 16) as i32;
        let bx = if second_half {
            x1 + (((x2 - x1) as i64 * beta) >> 16) as i32
        } else {
            x0 + (((x1 - x0) as i64 * beta) >> 16) as i32
        };

        let (xa, xb) = if ax < bx { (ax, bx) } else { (bx, ax) };
        backend_draw_line(b, xa, y, xb, y, color)?;
    }
    Ok(())
}

fn draw_segment_wedge(
    b: &RouletteBackend,
    cx: i32,
    cy: i32,
    start_idx: usize,
    inner_radius: i32,
    radius: i32,
    color: Color32,
) -> VideoResult {
    let inner = inner_radius;
    let start_deg = (start_idx as i32) * ROULETTE_SEGMENT_DEGREES;
    let end_deg = start_deg + ROULETTE_SEGMENT_DEGREES;

    let start_x = cx + scale(cos_deg(start_deg), radius);
    let start_y = cy + scale(sin_deg(start_deg), radius);
    let end_x = cx + scale(cos_deg(end_deg), radius);
    let end_y = cy + scale(sin_deg(end_deg), radius);

    draw_filled_triangle(b, cx, cy, start_x, start_y, end_x, end_y, color)?;

    if inner > 0 {
        let inner_start_x = cx + scale(cos_deg(start_deg), inner);
        let inner_start_y = cy + scale(sin_deg(start_deg), inner);
        let inner_end_x = cx + scale(cos_deg(end_deg), inner);
        let inner_end_y = cy + scale(sin_deg(end_deg), inner);
        draw_filled_triangle(
            b,
            cx,
            cy,
            inner_start_x,
            inner_start_y,
            inner_end_x,
            inner_end_y,
            ROULETTE_BG_COLOR,
        )?;
    }
    Ok(())
}

fn draw_segment_divider(
    b: &RouletteBackend,
    cx: i32,
    cy: i32,
    idx: usize,
    radius: i32,
) -> VideoResult {
    let x_outer = cx + scale(COS_TABLE[idx], radius + 2);
    let y_outer = cy + scale(SIN_TABLE[idx], radius + 2);
    backend_draw_line(b, cx, cy, x_outer, y_outer, ROULETTE_WHEEL_COLOR)
}

fn draw_roulette_wheel(
    b: &RouletteBackend,
    cx: i32,
    cy: i32,
    inner_radius: i32,
    radius: i32,
    highlight_segment: i32,
) -> VideoResult {
    backend_draw_circle_filled(b, cx, cy, radius + 8, ROULETTE_BG_COLOR)?;
    backend_draw_circle(b, cx, cy, radius + 8, ROULETTE_WHEEL_COLOR)?;

    for i in 0..ROULETTE_SEGMENT_COUNT {
        let is_colored = SEGMENTS[i as usize].is_colored;
        let mut base_color = if is_colored {
            ROULETTE_ODD_COLOR
        } else {
            ROULETTE_BLANK_COLOR
        };
        if i == highlight_segment {
            base_color = if is_colored {
                ROULETTE_COLORED_HIGHLIGHT
            } else {
                ROULETTE_BLANK_HIGHLIGHT
            };
        }
        draw_segment_wedge(b, cx, cy, i as usize, inner_radius, radius, base_color)?;
        draw_segment_divider(b, cx, cy, i as usize, radius)?;
    }
    draw_segment_divider(b, cx, cy, ROULETTE_SEGMENT_COUNT as usize, radius)?;

    backend_draw_circle_filled(b, cx, cy, inner_radius + 6, ROULETTE_WHEEL_COLOR)?;
    backend_draw_circle_filled(b, cx, cy, inner_radius, ROULETTE_BG_COLOR)?;
    Ok(())
}

fn draw_pointer_for_angle(
    b: &RouletteBackend,
    cx: i32,
    cy: i32,
    pointer_width: i32,
    tip_radius: i32,
    base_radius: i32,
    angle_deg: i32,
    color: Color32,
) -> VideoResult {
    let dir_x = cos_deg(angle_deg);
    let dir_y = sin_deg(angle_deg);
    let perp_x = -dir_y;
    let perp_y = dir_x;

    let tip_x = cx + scale(dir_x, tip_radius);
    let tip_y = cy + scale(dir_y, tip_radius);
    let base_x = cx + scale(dir_x, base_radius);
    let base_y = cy + scale(dir_y, base_radius);

    let offset_x = scale(perp_x, pointer_width);
    let offset_y = scale(perp_y, pointer_width);

    let left_x = base_x + offset_x;
    let left_y = base_y + offset_y;
    let right_x = base_x - offset_x;
    let right_y = base_y - offset_y;

    backend_draw_line(b, tip_x, tip_y, left_x, left_y, color)?;
    backend_draw_line(b, tip_x, tip_y, right_x, right_y, color)?;
    backend_draw_line(b, left_x, left_y, right_x, right_y, color)?;
    Ok(())
}

fn draw_pointer_ticks(
    b: &RouletteBackend,
    cx: i32,
    cy: i32,
    pointer_width: i32,
    tip_radius: i32,
    base_radius: i32,
    angle_deg: i32,
    color: Color32,
) {
    let _ = draw_pointer_for_angle(
        b,
        cx,
        cy,
        pointer_width,
        tip_radius,
        base_radius,
        angle_deg,
        color,
    );
    let _ = draw_pointer_for_angle(
        b,
        cx,
        cy,
        pointer_width,
        tip_radius,
        base_radius,
        angle_deg + 180,
        color,
    );
}

fn draw_fate_number(b: &RouletteBackend, cx: i32, y_pos: i32, fate_number: u32, revealed: bool) {
    let card_x = cx - 136;
    let card_w = 272;
    let card_h = 88;
    let number_y = y_pos + 46;

    let _ = draw_panel(
        b,
        card_x,
        y_pos,
        card_w,
        card_h,
        ROULETTE_INFO_BG_COLOR,
        ROULETTE_CARD_BORDER,
    );
    draw_text_centered(b, cx, y_pos + 14, TEXT_CARD_LABEL, ROULETTE_MUTED_TEXT);

    if !revealed {
        draw_text_centered(b, cx, number_y, TEXT_UNKNOWN, ROULETTE_CARD_TEXT);
        return;
    }

    let box_color = if fate_number & 1 == 1 {
        ROULETTE_ODD_COLOR
    } else {
        ROULETTE_EVEN_COLOR
    };
    let _ = backend_fill_rect(b, card_x + 8, y_pos + 34, card_w - 16, 44, box_color);

    let mut num_str = [0u8; 21];
    let mut len = 0usize;
    if fate_number == 0 {
        num_str[len] = b'0';
        len += 1;
    } else {
        let mut n = fate_number;
        let mut tmp = [0u8; 21];
        let mut t = 0usize;
        while n != 0 && t < tmp.len() {
            tmp[t] = b'0' + (n % 10) as u8;
            n /= 10;
            t += 1;
        }
        while t > 0 {
            len += 1;
            num_str[len - 1] = tmp[t - 1];
            t -= 1;
        }
    }
    let text_x = cx - (len as i32 * font::FONT_CHAR_WIDTH) / 2;
    let mut text_len = len;
    if len < num_str.len() {
        num_str[len] = 0;
        text_len = len + 1;
    }
    let _ = backend_draw_text(
        b,
        text_x,
        number_y,
        &num_str[..text_len],
        ROULETTE_CARD_TEXT,
        Color32(0),
    );
}

fn draw_result_banner(b: &RouletteBackend, cx: i32, y_pos: i32, fate_number: u32) {
    let (result_text, sub_text, parity_text, currency_text, banner_color) = if fate_number & 1 == 1
    {
        (
            TEXT_WIN,
            TEXT_WIN_SUB,
            TEXT_PARITY_ODD,
            TEXT_CURRENCY_WIN,
            ROULETTE_WIN_BG,
        )
    } else {
        (
            TEXT_LOSE,
            TEXT_LOSE_SUB,
            TEXT_PARITY_EVEN,
            TEXT_CURRENCY_LOSE,
            ROULETTE_LOSE_BG,
        )
    };

    let _ = draw_panel(
        b,
        cx - 220,
        y_pos,
        440,
        108,
        banner_color,
        ROULETTE_CARD_BORDER,
    );
    draw_text_centered(b, cx, y_pos + 16, result_text, ROULETTE_CARD_TEXT);
    draw_text_centered(b, cx, y_pos + 41, sub_text, ROULETTE_MUTED_TEXT);

    let _ = draw_panel(
        b,
        cx - 190,
        y_pos + 64,
        160,
        32,
        ROULETTE_INFO_BG_COLOR,
        ROULETTE_CARD_BORDER,
    );
    let _ = draw_panel(
        b,
        cx + 30,
        y_pos + 64,
        160,
        32,
        ROULETTE_INFO_BG_COLOR,
        ROULETTE_CARD_BORDER,
    );
    draw_text_centered(b, cx - 110, y_pos + 74, parity_text, ROULETTE_TEXT_COLOR);
    draw_text_centered(b, cx + 110, y_pos + 74, currency_text, ROULETTE_TEXT_COLOR);
}

fn render_wheel_frame(
    b: &RouletteBackend,
    screen_width: i32,
    screen_height: i32,
    cx: i32,
    cy: i32,
    inner_radius: i32,
    radius: i32,
    pointer_width: i32,
    pointer_tip_radius: i32,
    pointer_base_radius: i32,
    highlight_segment: i32,
    pointer_angle_deg: i32,
    last_pointer_angle: &mut i32,
    fate_number: u32,
    reveal_number: bool,
    clear_background: bool,
    draw_wheel: bool,
) -> VideoResult {
    let region = radius + 80;
    let mut region_x = cx - region;
    let mut region_y = cy - region;
    let mut region_w = region * 2;
    let mut region_h = region * 2;

    if !clear_background && *last_pointer_angle >= 0 {
        draw_pointer_ticks(
            b,
            cx,
            cy,
            pointer_width,
            pointer_tip_radius,
            pointer_base_radius,
            *last_pointer_angle,
            ROULETTE_BG_COLOR,
        );
    }

    if clear_background {
        if region_x < 0 {
            region_w += region_x;
            region_x = 0;
        }
        if region_y < 0 {
            region_h += region_y;
            region_y = 0;
        }
        if region_x + region_w > screen_width {
            region_w = screen_width - region_x;
        }
        if region_y + region_h > screen_height {
            region_h = screen_height - region_y;
        }
        backend_fill_rect(b, region_x, region_y, region_w, region_h, ROULETTE_BG_COLOR)?;
    }

    if draw_wheel {
        draw_roulette_wheel(b, cx, cy, inner_radius, radius, highlight_segment)?;
    }
    draw_pointer_ticks(
        b,
        cx,
        cy,
        pointer_width,
        pointer_tip_radius,
        pointer_base_radius,
        pointer_angle_deg,
        ROULETTE_POINTER_COLOR,
    );
    draw_fate_number(b, cx, cy + radius + 30, fate_number, reveal_number);

    *last_pointer_angle = pointer_angle_deg;
    Ok(())
}

fn segment_matches_parity(segment_index: i32, need_colored: bool) -> bool {
    let is_colored = SEGMENTS[(segment_index % ROULETTE_SEGMENT_COUNT) as usize].is_colored;
    if need_colored {
        is_colored
    } else {
        !is_colored
    }
}

fn choose_segment_for_parity(fate_number: u32, need_colored: bool) -> i32 {
    let start = (fate_number % ROULETTE_SEGMENT_COUNT as u32) as i32;
    for tries in 0..ROULETTE_SEGMENT_COUNT {
        let idx = (start + tries) % ROULETTE_SEGMENT_COUNT;
        if segment_matches_parity(idx, need_colored) {
            return idx;
        }
    }
    start
}

fn text_width_px(text: &[u8]) -> i32 {
    let mut len = 0usize;
    for &b in text {
        if b == 0 {
            break;
        }
        len += 1;
    }
    len as i32 * font::FONT_CHAR_WIDTH
}

fn even_px(x: i32) -> i32 {
    x & !1
}

fn draw_text_centered(b: &RouletteBackend, cx: i32, y: i32, text: &[u8], fg: Color32) {
    let x = even_px(cx - text_width_px(text) / 2);
    let _ = backend_draw_text(b, x, y, text, fg, Color32(0));
}

fn draw_panel(
    b: &RouletteBackend,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    fill: Color32,
    border: Color32,
) -> VideoResult {
    backend_fill_rect(b, x, y, w, h, fill)?;
    backend_draw_line(b, x, y, x + w, y, border)?;
    backend_draw_line(b, x, y + h, x + w, y + h, border)?;
    backend_draw_line(b, x, y, x, y + h, border)?;
    backend_draw_line(b, x + w, y, x + w, y + h, border)?;
    Ok(())
}

fn draw_transition_spinner(
    b: &RouletteBackend,
    screen_width: i32,
    screen_height: i32,
    is_win: bool,
) -> VideoResult {
    let mut panel_w = (screen_width - 120).max(260);
    if panel_w > 520 {
        panel_w = 520;
    }
    if panel_w > screen_width - 20 {
        panel_w = (screen_width - 20).max(220);
    }
    let panel_h = 152;
    let panel_x = (screen_width - panel_w) / 2;
    let panel_y = (screen_height - panel_h) / 2;

    let panel_bg = if is_win {
        ROULETTE_WIN_BG
    } else {
        ROULETTE_LOSE_BG
    };
    let title = if is_win {
        TEXT_BOOTING_TITLE
    } else {
        TEXT_REBOOT_TITLE
    };
    let subtitle = if is_win {
        TEXT_BOOTING_SUB
    } else {
        TEXT_REBOOT_SUB
    };
    let detail = if is_win {
        TEXT_BOOTING_DETAIL
    } else {
        TEXT_REBOOT_DETAIL
    };

    for tick in 0..16 {
        draw_panel(
            b,
            panel_x,
            panel_y,
            panel_w,
            panel_h,
            panel_bg,
            ROULETTE_CARD_BORDER,
        )?;

        draw_text_centered(
            b,
            panel_x + panel_w / 2,
            panel_y + 26,
            title,
            ROULETTE_CARD_TEXT,
        );
        draw_text_centered(
            b,
            panel_x + panel_w / 2,
            panel_y + 56,
            subtitle,
            ROULETTE_MUTED_TEXT,
        );

        let spinner_box_x = panel_x + (panel_w - 120) / 2;
        let _ = backend_fill_rect(
            b,
            spinner_box_x,
            panel_y + 86,
            120,
            36,
            ROULETTE_INFO_BG_COLOR,
        );
        let _ = backend_draw_line(
            b,
            spinner_box_x,
            panel_y + 86,
            spinner_box_x + 120,
            panel_y + 86,
            ROULETTE_CARD_BORDER,
        );
        let _ = backend_draw_line(
            b,
            spinner_box_x,
            panel_y + 122,
            spinner_box_x + 120,
            panel_y + 122,
            ROULETTE_CARD_BORDER,
        );
        draw_text_centered(
            b,
            panel_x + panel_w / 2,
            panel_y + 96,
            SPINNER_FRAMES[(tick % SPINNER_FRAMES.len() as i32) as usize],
            ROULETTE_POINTER_COLOR,
        );
        draw_text_centered(
            b,
            panel_x + panel_w / 2,
            panel_y + 132,
            detail,
            ROULETTE_TEXT_COLOR,
        );
        backend_sleep_ms(b, 90);
    }

    Ok(())
}

pub fn roulette_run(backend: *const RouletteBackend, fate_number: u32) -> i32 {
    if backend.is_null() {
        return -1;
    }
    let backend = unsafe { &*backend };

    let mut width = 0;
    let mut height = 0;
    if backend_get_size(backend, &mut width, &mut height).is_err() || width <= 0 || height <= 0 {
        return -2;
    }

    if backend_fill_rect(backend, 0, 0, width, height, ROULETTE_BG_COLOR).is_err() {
        return -3;
    }

    let mut radius = ROULETTE_WHEEL_RADIUS;
    let max_radius = (width.min(height) / 2) - 60;
    if radius > max_radius {
        radius = max_radius;
    }
    if radius < ROULETTE_INNER_RADIUS + 20 {
        radius = ROULETTE_INNER_RADIUS + 20;
    }

    let inner_radius = (radius * 3 / 10).clamp(ROULETTE_INNER_RADIUS, radius - 24);
    let pointer_width = (radius / 6).clamp(10, 18);
    let pointer_base_radius = radius + (radius / 8).clamp(6, 18);
    let pointer_tip_radius = radius + (radius / 3).clamp(18, 40);
    let layout = RouletteLayout {
        radius,
        inner_radius,
        pointer_width,
        pointer_tip_radius,
        pointer_base_radius,
    };

    let want_colored = (fate_number & 1) != 0;
    let mut start_segment = (fate_number % ROULETTE_SEGMENT_COUNT as u32) as i32;
    let target_segment = choose_segment_for_parity(fate_number, want_colored);
    if start_segment == target_segment {
        start_segment = (start_segment + 3) % ROULETTE_SEGMENT_COUNT;
    }

    backend_sleep_ms(backend, 300);

    let center_x = width / 2;
    let center_y = height / 2;
    let start_angle = segment_center_angle(start_segment);
    let target_angle = segment_center_angle(target_segment);
    let rotation_to_target = normalize_angle(target_angle - start_angle);
    let mut total_rotation = ROULETTE_SPIN_LOOPS * ROULETTE_DEGREE_STEPS + rotation_to_target;
    if total_rotation <= 0 {
        total_rotation += ROULETTE_DEGREE_STEPS;
    }

    let mut last_pointer_angle = -1;
    let _ = render_wheel_frame(
        backend,
        width,
        height,
        center_x,
        center_y,
        layout.inner_radius,
        layout.radius,
        layout.pointer_width,
        layout.pointer_tip_radius,
        layout.pointer_base_radius,
        -1,
        start_angle,
        &mut last_pointer_angle,
        fate_number,
        false,
        true,
        true,
    );

    let mut total_frames = ROULETTE_SPIN_DURATION_MS / ROULETTE_SPIN_FRAME_DELAY_MS;
    if total_frames < 1 {
        total_frames = 1;
    }

    for frame in 1..=total_frames {
        let p_q16 = ((frame as u32) << 16) / (total_frames as u32);
        let eased_q16 = (((p_q16 as u64) * (131072u64 - p_q16 as u64)) >> 16) as u32; // p * (2 - p)
        let pointer_angle_frame =
            start_angle + ((total_rotation as i64 * eased_q16 as i64) >> 16) as i32;
        let _ = render_wheel_frame(
            backend,
            width,
            height,
            center_x,
            center_y,
            layout.inner_radius,
            layout.radius,
            layout.pointer_width,
            layout.pointer_tip_radius,
            layout.pointer_base_radius,
            -1,
            pointer_angle_frame,
            &mut last_pointer_angle,
            fate_number,
            false,
            false,
            false,
        );
        backend_sleep_ms(backend, ROULETTE_SPIN_FRAME_DELAY_MS as u32);
    }

    let pointer_angle = start_angle + total_rotation;
    let landing_segment = target_segment;
    let _ = render_wheel_frame(
        backend,
        width,
        height,
        center_x,
        center_y,
        layout.inner_radius,
        layout.radius,
        layout.pointer_width,
        layout.pointer_tip_radius,
        layout.pointer_base_radius,
        landing_segment,
        pointer_angle,
        &mut last_pointer_angle,
        fate_number,
        false,
        true,
        true,
    );
    backend_sleep_ms(backend, 500);
    backend_sleep_ms(backend, 400);

    for flash in 0..5 {
        let _ = render_wheel_frame(
            backend,
            width,
            height,
            center_x,
            center_y,
            layout.inner_radius,
            layout.radius,
            layout.pointer_width,
            layout.pointer_tip_radius,
            layout.pointer_base_radius,
            landing_segment,
            pointer_angle,
            &mut last_pointer_angle,
            fate_number,
            true,
            false,
            false,
        );
        backend_sleep_ms(backend, 250);
        if flash < 4 {
            let _ = render_wheel_frame(
                backend,
                width,
                height,
                center_x,
                center_y,
                layout.inner_radius,
                layout.radius,
                layout.pointer_width,
                layout.pointer_tip_radius,
                layout.pointer_base_radius,
                landing_segment,
                pointer_angle,
                &mut last_pointer_angle,
                fate_number,
                false,
                false,
                false,
            );
            backend_sleep_ms(backend, 150);
        }
    }

    let _ = render_wheel_frame(
        backend,
        width,
        height,
        center_x,
        center_y,
        layout.inner_radius,
        layout.radius,
        layout.pointer_width,
        layout.pointer_tip_radius,
        layout.pointer_base_radius,
        landing_segment,
        pointer_angle,
        &mut last_pointer_angle,
        fate_number,
        true,
        false,
        true,
    );
    backend_sleep_ms(backend, 600);

    let fate_box_y = center_y + layout.radius + 22;
    let fate_box_h = 88;
    let mut info_y = fate_box_y + fate_box_h + 10;
    if info_y < 0 {
        info_y = 0;
    }
    if info_y > height {
        info_y = height;
    }
    let _ = backend_fill_rect(
        backend,
        0,
        info_y,
        width,
        height - info_y,
        ROULETTE_BG_COLOR,
    );
    let banner_y = info_y + 12;
    draw_result_banner(backend, center_x, banner_y, fate_number);

    backend_sleep_ms(backend, ROULETTE_RESULT_DELAY_MS);

    let _ = draw_transition_spinner(backend, width, height, (fate_number & 1) == 1);

    0
}

fn kernel_get_size(ctx: *mut c_void, w: *mut i32, h: *mut i32) -> VideoResult {
    if let Some(ctx) = unsafe { (ctx as *const GraphicsContext).as_ref() } {
        unsafe {
            if !w.is_null() {
                *w = ctx.width() as i32;
            }
            if !h.is_null() {
                *h = ctx.height() as i32;
            }
        }
        return Ok(());
    }

    let fb = match framebuffer::snapshot() {
        Some(fb) => fb,
        None => return Err(VideoError::NoFramebuffer),
    };
    unsafe {
        if !w.is_null() {
            *w = fb.width() as i32;
        }
        if !h.is_null() {
            *h = fb.height() as i32;
        }
    }
    Ok(())
}

fn kernel_fill_rect(ctx: *mut c_void, x: i32, y: i32, w: i32, h: i32, color: u32) -> VideoResult {
    let ctx = unsafe { (ctx as *mut GraphicsContext).as_mut() }.ok_or(VideoError::Invalid)?;
    canvas_ops::fill_rect(ctx, x, y, w, h, Color32(color));
    Ok(())
}

fn kernel_draw_line(
    ctx: *mut c_void,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: u32,
) -> VideoResult {
    let ctx = unsafe { (ctx as *mut GraphicsContext).as_mut() }.ok_or(VideoError::Invalid)?;
    canvas_ops::line(ctx, x0, y0, x1, y1, Color32(color));
    Ok(())
}

fn kernel_draw_circle(ctx: *mut c_void, cx: i32, cy: i32, radius: i32, color: u32) -> VideoResult {
    let ctx = unsafe { (ctx as *mut GraphicsContext).as_mut() }.ok_or(VideoError::Invalid)?;
    canvas_ops::circle(ctx, cx, cy, radius, Color32(color));
    Ok(())
}

fn kernel_draw_circle_filled(
    ctx: *mut c_void,
    cx: i32,
    cy: i32,
    radius: i32,
    color: u32,
) -> VideoResult {
    let ctx = unsafe { (ctx as *mut GraphicsContext).as_mut() }.ok_or(VideoError::Invalid)?;
    canvas_ops::circle_filled(ctx, cx, cy, radius, Color32(color));
    Ok(())
}

fn kernel_draw_text(
    ctx: *mut c_void,
    x: i32,
    y: i32,
    text: *const u8,
    fg: u32,
    bg: u32,
) -> VideoResult {
    let ctx = unsafe { (ctx as *const GraphicsContext).as_ref() }.ok_or(VideoError::Invalid)?;
    if text.is_null() {
        return Err(VideoError::Invalid);
    }
    let rc = font::font_draw_string_ctx(ctx, x, y, text as *const c_char, Color32(fg), Color32(bg));
    if rc == 0 {
        Ok(())
    } else {
        Err(VideoError::Invalid)
    }
}

fn kernel_sleep_ms(_ctx: *mut c_void, ms: u32) {
    crate::framebuffer::framebuffer_flush();
    pit_poll_delay_ms(ms);
}

pub fn roulette_draw_kernel(fate_number: u32) -> VideoResult {
    let mut gfx_ctx = GraphicsContext::new()?;
    let backend = RouletteBackend {
        ctx: &mut gfx_ctx as *mut GraphicsContext as *mut c_void,
        get_size: Some(kernel_get_size),
        fill_rect: Some(kernel_fill_rect),
        draw_line: Some(kernel_draw_line),
        draw_circle: Some(kernel_draw_circle),
        draw_circle_filled: Some(kernel_draw_circle_filled),
        draw_text: Some(kernel_draw_text),
        sleep_ms: Some(kernel_sleep_ms),
    };

    let result = roulette_run(&backend as *const RouletteBackend, fate_number);

    if result == 0 {
        Ok(())
    } else {
        Err(VideoError::Invalid)
    }
}
