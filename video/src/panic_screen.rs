//! Kernel panic screen display.
//!
//! Renders a full-screen panic message through the pre-rasterized glyph atlas,
//! so nothing allocates at render time, and only tries the framebuffer's
//! locks, which a CPU the panic stopped may hold.

use slopos_abi::draw::{Canvas, Color32};
use slopos_font::atlas::GlyphAtlas;
use slopos_ostd::numfmt;

use crate::framebuffer;
use crate::graphics::GraphicsContext;
use crate::kernel_font;

const PANIC_BG_COLOR: Color32 = Color32(0xFF8B0000);
const PANIC_FG_COLOR: Color32 = Color32(0xFFFFFFFF);
const PANIC_HEADER_COLOR: Color32 = Color32(0xFFFF4444);

fn draw_register_line(
    ctx: &mut GraphicsContext,
    atlas: &GlyphAtlas,
    x: i32,
    y: i32,
    label: &[u8],
    value: u64,
) {
    atlas.draw_bytes(ctx, x, y, label, PANIC_FG_COLOR, PANIC_BG_COLOR);

    let mut hex_buf = numfmt::NumBuf::<19>::new();
    let hex_text = hex_buf.format_hex_u64(value);
    let label_width = atlas.bytes_width(label);
    atlas.draw_bytes(
        ctx,
        x + label_width,
        y,
        hex_text,
        PANIC_FG_COLOR,
        PANIC_BG_COLOR,
    );
}

fn draw_symbol_text(ctx: &mut GraphicsContext, atlas: &GlyphAtlas, mut x: i32, y: i32, rip: u64) {
    let Some(sym) = slopos_ostd::ksym::lookup(rip) else {
        return;
    };

    let max_x = ctx.width() as i32 - 40;
    let char_width = atlas.cell_width();
    let mut plus = numfmt::NumBuf::<19>::new();
    let offset = plus.format_hex_u64(sym.offset);

    for &byte in b" " {
        if x + char_width > max_x {
            return;
        }
        atlas.draw_char(ctx, x, y, byte as u32, PANIC_FG_COLOR, PANIC_BG_COLOR);
        x += char_width;
    }

    for &byte in sym.symbol.as_bytes() {
        if x + char_width > max_x {
            return;
        }
        atlas.draw_char(ctx, x, y, byte as u32, PANIC_FG_COLOR, PANIC_BG_COLOR);
        x += char_width;
    }

    for &byte in b"+" {
        if x + char_width > max_x {
            return;
        }
        atlas.draw_char(ctx, x, y, byte as u32, PANIC_FG_COLOR, PANIC_BG_COLOR);
        x += char_width;
    }

    for &byte in offset {
        if x + char_width > max_x {
            return;
        }
        atlas.draw_char(ctx, x, y, byte as u32, PANIC_FG_COLOR, PANIC_BG_COLOR);
        x += char_width;
    }
}

/// As many trailing log lines as fit between `y` and `limit_y`. The report says
/// where the machine stopped; these say why, and on a machine with no serial
/// port they exist nowhere else.
fn draw_log_tail(
    ctx: &mut GraphicsContext,
    atlas: &GlyphAtlas,
    tail: &[u8],
    mut y: i32,
    limit_y: i32,
) -> i32 {
    if tail.is_empty() {
        return y;
    }
    let char_height = atlas.cell_height();
    let char_width = atlas.cell_width();
    let line_pitch = char_height + 2;

    let header_room = char_height + 8;
    if y + header_room + line_pitch > limit_y {
        return y;
    }
    let max_lines = ((limit_y - y - header_room) / line_pitch).max(0) as usize;
    if max_lines == 0 {
        return y;
    }

    // The capture ends mid-stream, so the last byte is usually a newline.
    let mut end = tail.len();
    while end > 0 && (tail[end - 1] == b'\n' || tail[end - 1] == b'\r') {
        end -= 1;
    }
    let mut start = end;
    let mut lines = 0usize;
    while start > 0 {
        if tail[start - 1] == b'\n' {
            lines += 1;
            if lines == max_lines {
                break;
            }
        }
        start -= 1;
    }

    atlas.draw_bytes(
        ctx,
        40,
        y,
        b"Kernel log (oldest first):\0",
        PANIC_HEADER_COLOR,
        PANIC_BG_COLOR,
    );
    y += header_room;

    let max_x = ctx.width() as i32 - 40;
    let mut x = 60;
    slopos_ostd::fblog::for_each_log_char(&tail[start..end], |unit| {
        if y + char_height > limit_y {
            return;
        }
        match unit {
            slopos_ostd::fblog::LogChar::Newline => {
                y += line_pitch;
                x = 60;
            }
            slopos_ostd::fblog::LogChar::Char(byte) => {
                if x + char_width <= max_x {
                    atlas.draw_char(ctx, x, y, byte as u32, PANIC_FG_COLOR, PANIC_BG_COLOR);
                    x += char_width;
                }
            }
        }
    });
    y + line_pitch
}

pub struct PanicView<'a> {
    pub message: &'a str,
    pub rip: Option<u64>,
    pub rsp: u64,
    pub cr0: u64,
    pub cr2: u64,
    pub cr3: u64,
    pub cr4: u64,
    /// Return addresses, most recent call first.
    pub backtrace: &'a [u64],
    /// What became of the crash record, above the prompt.
    pub status: &'a str,
    pub prompt: &'a str,
}

/// Whether the screen was drawn; its flush is tried, and skipped while the
/// display backend's hook is held.
pub fn display_panic_screen(view: &PanicView<'_>) -> bool {
    let &PanicView {
        message,
        rip,
        rsp,
        cr0,
        cr2,
        cr3,
        cr4,
        backtrace,
        status,
        prompt,
    } = view;
    let Some(fb) = framebuffer::try_snapshot() else {
        return false;
    };
    let mut ctx = GraphicsContext::from_state(fb);

    let atlas = match kernel_font::atlas() {
        Some(a) => a,
        None => return false,
    };

    let bg_px = ctx.pixel_format().encode(PANIC_BG_COLOR);
    ctx.clear_canvas(bg_px);

    let width = ctx.width() as i32;
    let height = ctx.height() as i32;

    let char_height = atlas.cell_height();
    let char_width = atlas.cell_width();

    let mut y = 60;

    let header = b"=== KERNEL PANIC ===\0";
    let header_width = atlas.bytes_width(header);
    let header_x = (width - header_width) / 2;
    atlas.draw_bytes(
        &mut ctx,
        header_x,
        y,
        header,
        PANIC_HEADER_COLOR,
        PANIC_BG_COLOR,
    );
    y += char_height * 2;

    let subtitle = b"An unrecoverable error has occurred\0";
    let subtitle_width = atlas.bytes_width(subtitle);
    let subtitle_x = (width - subtitle_width) / 2;
    atlas.draw_bytes(
        &mut ctx,
        subtitle_x,
        y,
        subtitle,
        PANIC_FG_COLOR,
        PANIC_BG_COLOR,
    );
    y += char_height * 2;

    y += char_height;

    let msg_label = b"Reason: \0";
    atlas.draw_bytes(&mut ctx, 40, y, msg_label, PANIC_FG_COLOR, PANIC_BG_COLOR);
    let mut x = 40 + 8 * char_width;
    let max_x = width - 40;
    for &byte in message.as_bytes() {
        if byte == 0 {
            break;
        }
        if x + char_width > max_x {
            y += char_height;
            x = 40 + 8 * char_width;
            if y > height - 120 {
                break;
            }
        }
        atlas.draw_char(&mut ctx, x, y, byte as u32, PANIC_FG_COLOR, PANIC_BG_COLOR);
        x += char_width;
    }
    y += char_height * 2;

    y += char_height;
    let reg_header = b"CPU State:\0";
    atlas.draw_bytes(
        &mut ctx,
        40,
        y,
        reg_header,
        PANIC_HEADER_COLOR,
        PANIC_BG_COLOR,
    );
    y += char_height + 8;

    if let Some(rip_val) = rip {
        draw_register_line(&mut ctx, &atlas, 60, y, b"RIP: \0", rip_val);
        y += char_height + 4;
    }

    draw_register_line(&mut ctx, &atlas, 60, y, b"RSP: \0", rsp);
    y += char_height + 4;

    draw_register_line(&mut ctx, &atlas, 60, y, b"CR0: \0", cr0);
    y += char_height + 4;

    draw_register_line(&mut ctx, &atlas, 60, y, b"CR2: \0", cr2);
    y += char_height + 4;

    draw_register_line(&mut ctx, &atlas, 60, y, b"CR3: \0", cr3);
    y += char_height + 4;

    draw_register_line(&mut ctx, &atlas, 60, y, b"CR4: \0", cr4);
    y += char_height + 4;

    if !backtrace.is_empty() {
        y += char_height;
        let label = b"Frame-pointer backtrace:\0";
        atlas.draw_bytes(&mut ctx, 40, y, label, PANIC_HEADER_COLOR, PANIC_BG_COLOR);
        y += char_height + 8;
        for (i, &ra) in backtrace.iter().enumerate() {
            let label: &[u8] = match i {
                0 => b"#0:  \0",
                1 => b"#1:  \0",
                2 => b"#2:  \0",
                3 => b"#3:  \0",
                4 => b"#4:  \0",
                5 => b"#5:  \0",
                6 => b"#6:  \0",
                _ => b"#7:  \0",
            };
            draw_register_line(&mut ctx, &atlas, 60, y, label, ra);
            draw_symbol_text(
                &mut ctx,
                &atlas,
                60 + atlas.bytes_width(label) + 19 * char_width,
                y,
                ra,
            );
            y += char_height + 4;
        }
    }

    let prompt_y = height - 60;
    slopos_ostd::fblog::with_panic_tail(|tail| {
        draw_log_tail(
            &mut ctx,
            &atlas,
            tail,
            y + char_height,
            prompt_y - char_height * 3,
        );
    });

    for (line, y) in [(status, prompt_y - char_height - 4), (prompt, prompt_y)] {
        let x = (width - atlas.bytes_width(line.as_bytes())) / 2;
        atlas.draw_bytes(
            &mut ctx,
            x,
            y,
            line.as_bytes(),
            PANIC_FG_COLOR,
            PANIC_BG_COLOR,
        );
    }

    let serial_note = b"(Debug output also available on serial console)\0";
    let note_width = atlas.bytes_width(serial_note);
    let note_x = (width - note_width) / 2;
    let note_y = height - 40;
    atlas.draw_bytes(
        &mut ctx,
        note_x,
        note_y,
        serial_note,
        Color32(0xFF888888),
        PANIC_BG_COLOR,
    );

    framebuffer::try_flush();
    true
}
