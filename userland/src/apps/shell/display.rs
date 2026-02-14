//! Display state, scrollback buffer, and rendering functions.

use core::cell::Cell;

use slopos_abi::draw::Color32;

use crate::gfx::font::{FONT_CHAR_HEIGHT, FONT_CHAR_WIDTH};
use crate::gfx::{self, DrawBuffer};
use crate::syscall::{DisplayInfo, fs, window};

use super::SyncUnsafeCell;
use super::surface;

pub const SHELL_BG_COLOR: Color32 = Color32(0x1E1E_1EFF);
pub const SHELL_FG_COLOR: Color32 = Color32(0xE6E6_E6FF);

pub const SHELL_WINDOW_WIDTH: i32 = 640;
pub const SHELL_WINDOW_HEIGHT: i32 = 480;
pub const SHELL_TAB_WIDTH: i32 = 4;
pub const SHELL_SCROLLBACK_LINES: usize = 256;
pub const SHELL_SCROLLBACK_COLS: usize = 160;

// =============================================================================
// DisplayState: Cell-based state (no borrow conflicts)
// =============================================================================

pub struct DisplayState {
    pub enabled: Cell<bool>,
    pub width: Cell<i32>,
    pub height: Cell<i32>,
    pub pitch: Cell<usize>,
    pub bytes_pp: Cell<u8>,
    pub cols: Cell<i32>,
    pub rows: Cell<i32>,
    pub cursor_col: Cell<i32>,
    pub cursor_line: Cell<i32>,
    pub origin: Cell<i32>,
    pub total_lines: Cell<i32>,
    pub view_top: Cell<i32>,
    pub follow: Cell<bool>,
    pub fg: Cell<Color32>,
    pub bg: Cell<Color32>,
}

impl DisplayState {
    pub const fn new() -> Self {
        Self {
            enabled: Cell::new(false),
            width: Cell::new(0),
            height: Cell::new(0),
            pitch: Cell::new(0),
            bytes_pp: Cell::new(4),
            cols: Cell::new(0),
            rows: Cell::new(0),
            cursor_col: Cell::new(0),
            cursor_line: Cell::new(0),
            origin: Cell::new(0),
            total_lines: Cell::new(1),
            view_top: Cell::new(0),
            follow: Cell::new(true),
            fg: Cell::new(SHELL_FG_COLOR),
            bg: Cell::new(SHELL_BG_COLOR),
        }
    }

    pub fn line_slot(&self, logical: i32) -> usize {
        let max_lines = SHELL_SCROLLBACK_LINES as i32;
        ((self.origin.get() + logical).rem_euclid(max_lines)) as usize
    }

    pub fn cursor(&self) -> (i32, i32) {
        let row = (self.cursor_line.get() - self.view_top.get())
            .clamp(0, self.rows.get().saturating_sub(1));
        (self.cursor_col.get(), row)
    }

    pub fn reset(&self) {
        self.cursor_col.set(0);
        self.cursor_line.set(0);
        self.origin.set(0);
        self.total_lines.set(1);
        self.view_top.set(0);
        self.follow.set(true);
    }
}

// Safety: Userland is single-threaded with no preemption during shell code
unsafe impl Sync for DisplayState {}

pub static DISPLAY: DisplayState = DisplayState::new();
static OUTPUT_FD: SyncUnsafeCell<i32> = SyncUnsafeCell::new(-1);

// =============================================================================
// Scrollback module: safe accessors for large arrays
// =============================================================================

pub mod scrollback {
    use super::*;

    static DATA: SyncUnsafeCell<[u8; SHELL_SCROLLBACK_LINES * SHELL_SCROLLBACK_COLS]> =
        SyncUnsafeCell::new([0; SHELL_SCROLLBACK_LINES * SHELL_SCROLLBACK_COLS]);

    static LENS: SyncUnsafeCell<[u16; SHELL_SCROLLBACK_LINES]> =
        SyncUnsafeCell::new([0; SHELL_SCROLLBACK_LINES]);

    #[inline]
    pub fn with_line<R, F: FnOnce(&[u8]) -> R>(slot: usize, f: F) -> R {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        unsafe {
            let data = &*DATA.get();
            let start = slot * SHELL_SCROLLBACK_COLS;
            f(&data[start..start + SHELL_SCROLLBACK_COLS])
        }
    }

    #[inline]
    pub fn get_line_len(slot: usize) -> u16 {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        unsafe { (*LENS.get())[slot] }
    }

    #[inline]
    pub fn set_line_len(slot: usize, len: u16) {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        unsafe { (*LENS.get())[slot] = len }
    }

    #[inline]
    pub fn set_char(slot: usize, col: usize, ch: u8) {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        let col = col % SHELL_SCROLLBACK_COLS;
        unsafe {
            let data = &mut *DATA.get();
            data[slot * SHELL_SCROLLBACK_COLS + col] = ch;
        }
    }

    #[inline]
    pub fn get_char(slot: usize, col: usize) -> u8 {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        let col = col % SHELL_SCROLLBACK_COLS;
        unsafe {
            let data = &*DATA.get();
            data[slot * SHELL_SCROLLBACK_COLS + col]
        }
    }

    pub fn clear_line(slot: usize) {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        unsafe {
            let data = &mut *DATA.get();
            let start = slot * SHELL_SCROLLBACK_COLS;
            for i in start..start + SHELL_SCROLLBACK_COLS {
                data[i] = 0;
            }
            (*LENS.get())[slot] = 0;
        }
    }

    pub fn clear_all() {
        unsafe {
            let data = &mut *DATA.get();
            for byte in data.iter_mut() {
                *byte = 0;
            }
            let lens = &mut *LENS.get();
            for len in lens.iter_mut() {
                *len = 0;
            }
        }
    }

    /// Write a line to scrollback (for prompt rewriting)
    pub fn write_line(slot: usize, content: &[u8]) {
        let slot = slot % SHELL_SCROLLBACK_LINES;
        let len = content.len().min(SHELL_SCROLLBACK_COLS);
        unsafe {
            let data = &mut *DATA.get();
            let start = slot * SHELL_SCROLLBACK_COLS;
            // Clear first
            for i in start..start + SHELL_SCROLLBACK_COLS {
                data[i] = 0;
            }
            // Write content
            for (i, &b) in content.iter().take(len).enumerate() {
                data[start + i] = b;
            }
            (*LENS.get())[slot] = len as u16;
        }
    }
}

// =============================================================================
// Free drawing functions (no &mut self, explicit parameters)
// =============================================================================

fn draw_char_at(buf: &mut DrawBuffer, col: i32, row: i32, c: u8, fg: Color32, bg: Color32) {
    let x = col * FONT_CHAR_WIDTH;
    let y = row * FONT_CHAR_HEIGHT;
    gfx::font::draw_char(buf, x, y, c, fg, bg);
}

fn clear_row(buf: &mut DrawBuffer, row: i32, width: i32, bg: Color32) {
    gfx::fill_rect(buf, 0, row * FONT_CHAR_HEIGHT, width, FONT_CHAR_HEIGHT, bg);
}

fn draw_row_from_scrollback(buf: &mut DrawBuffer, display: &DisplayState, logical: i32, row: i32) {
    let bg = display.bg.get();
    let fg = display.fg.get();
    let width = display.width.get();
    let cols = display.cols.get();
    let total_lines = display.total_lines.get();

    // Clear the row first
    clear_row(buf, row, width, bg);

    if logical < 0 || logical >= total_lines {
        return;
    }

    let slot = display.line_slot(logical);
    let len = scrollback::get_line_len(slot) as usize;
    let draw_len = len.min(cols as usize);

    if draw_len == 0 {
        return;
    }

    scrollback::with_line(slot, |line| {
        for (col, &ch) in line.iter().take(draw_len).enumerate() {
            if ch != 0 {
                draw_char_at(buf, col as i32, row, ch, fg, bg);
            }
        }
    });
}

fn redraw_view(buf: &mut DrawBuffer, display: &DisplayState) {
    let bg = display.bg.get();
    let width = display.width.get();
    let height = display.height.get();
    let rows = display.rows.get();
    let view_top = display.view_top.get();

    // Clear entire view
    gfx::fill_rect(buf, 0, 0, width, height, bg);

    // Draw each row
    for row in 0..rows {
        draw_row_from_scrollback(buf, display, view_top + row, row);
    }
}

fn scroll_up_fast(buf: &mut DrawBuffer, display: &DisplayState) -> bool {
    let width = display.width.get();
    let height = display.height.get();
    let bg = display.bg.get();

    if height <= FONT_CHAR_HEIGHT {
        return false;
    }

    buf.blit(0, FONT_CHAR_HEIGHT, 0, 0, width, height - FONT_CHAR_HEIGHT);

    gfx::fill_rect(
        buf,
        0,
        height - FONT_CHAR_HEIGHT,
        width,
        FONT_CHAR_HEIGHT,
        bg,
    );

    true
}

// =============================================================================
// State update functions (no drawing)
// =============================================================================

fn update_new_line(display: &DisplayState) {
    display.cursor_col.set(0);
    let cursor_line = display.cursor_line.get() + 1;
    display.cursor_line.set(cursor_line);

    let total_lines = display.total_lines.get();
    if cursor_line >= total_lines {
        if total_lines < SHELL_SCROLLBACK_LINES as i32 {
            display.total_lines.set(total_lines + 1);
        } else {
            let origin = (display.origin.get() + 1) % SHELL_SCROLLBACK_LINES as i32;
            display.origin.set(origin);
            display.cursor_line.set(total_lines - 1);
            let view_top = display.view_top.get();
            if view_top > 0 {
                display.view_top.set(view_top - 1);
            }
        }
        let slot = display.line_slot(display.cursor_line.get());
        scrollback::clear_line(slot);
    }
}

fn update_char_state(display: &DisplayState, c: u8) {
    let cursor_col = display.cursor_col.get();
    let cursor_line = display.cursor_line.get();
    let cols = display.cols.get();
    let slot = display.line_slot(cursor_line);

    if (cursor_col as usize) < SHELL_SCROLLBACK_COLS {
        scrollback::set_char(slot, cursor_col as usize, c);
        let len = scrollback::get_line_len(slot) as i32;
        if cursor_col + 1 > len {
            scrollback::set_line_len(slot, (cursor_col + 1) as u16);
        }
    }

    display.cursor_col.set(cursor_col + 1);
    if display.cursor_col.get() >= cols {
        update_new_line(display);
    }
}

fn update_backspace_state(display: &DisplayState) {
    let mut cursor_col = display.cursor_col.get();
    let mut cursor_line = display.cursor_line.get();

    if cursor_col > 0 {
        cursor_col -= 1;
    } else if cursor_line > 0 {
        cursor_line -= 1;
        let slot = display.line_slot(cursor_line);
        let len = scrollback::get_line_len(slot) as i32;
        cursor_col = if len > 0 {
            (len - 1).clamp(0, display.cols.get().saturating_sub(1))
        } else {
            0
        };
    } else {
        return;
    }

    display.cursor_col.set(cursor_col);
    display.cursor_line.set(cursor_line);

    // Clear character in scrollback
    let slot = display.line_slot(cursor_line);
    if (cursor_col as usize) < SHELL_SCROLLBACK_COLS {
        scrollback::set_char(slot, cursor_col as usize, 0);
        // Trim trailing zeros from line length
        let mut len = scrollback::get_line_len(slot) as i32;
        while len > 0 {
            if scrollback::get_char(slot, (len - 1) as usize) != 0 {
                break;
            }
            len -= 1;
        }
        scrollback::set_line_len(slot, len as u16);
    }
}

// =============================================================================
// Console operations (combined state + render)
// =============================================================================

fn console_write(display: &DisplayState, text: &[u8]) {
    if !display.enabled.get() {
        return;
    }

    let follow = display.follow.get();
    let mut needs_scroll = false;
    let old_view_top = display.view_top.get();
    let start_line = display.cursor_line.get(); // Track first line modified

    // Phase 1: Update state
    for &b in text {
        match b {
            b'\n' => {
                let old_total = display.total_lines.get();
                update_new_line(display);

                // Check if we need to scroll view
                if follow {
                    let rows = display.rows.get();
                    let total = display.total_lines.get();
                    let max_top = (total - rows).max(0);
                    if display.view_top.get() != max_top {
                        display.view_top.set(max_top);
                        if total > old_total || display.view_top.get() != old_view_top {
                            needs_scroll = true;
                        }
                    }
                }
            }
            b'\r' => display.cursor_col.set(0),
            b'\t' => {
                for _ in 0..SHELL_TAB_WIDTH {
                    update_char_state(display, b' ');
                }
            }
            b'\x08' => update_backspace_state(display),
            0x20..=0x7E => update_char_state(display, b),
            _ => {}
        }
    }

    // Phase 2: Render if following
    if follow {
        surface::draw(|buf| {
            let view_top = display.view_top.get();
            let rows = display.rows.get();
            let cursor_line = display.cursor_line.get();

            if needs_scroll {
                let view_diff = display.view_top.get() - old_view_top;
                if view_diff == 1 && scroll_up_fast(buf, display) {
                    for line in start_line..=cursor_line {
                        let row = line - view_top;
                        if row >= 0 && row < rows {
                            draw_row_from_scrollback(buf, display, line, row);
                        }
                    }
                } else {
                    redraw_view(buf, display);
                }
            } else {
                for line in start_line..=cursor_line {
                    let row = line - view_top;
                    if row >= 0 && row < rows {
                        draw_row_from_scrollback(buf, display, line, row);
                    }
                }
            }
        });
    }
}

fn console_clear(display: &DisplayState) {
    if !display.enabled.get() {
        return;
    }

    display.reset();
    scrollback::clear_all();

    surface::draw(|buf| {
        let bg = display.bg.get();
        let width = display.width.get();
        let height = display.height.get();
        gfx::fill_rect(buf, 0, 0, width, height, bg);
    });
}

fn console_page_up(display: &DisplayState) {
    if display.total_lines.get() <= display.rows.get() {
        return;
    }

    let step = display.rows.get().max(1);
    let new_top = (display.view_top.get() - step).max(0);
    let delta = (display.view_top.get() - new_top).max(0);

    if delta == 0 {
        return;
    }

    display.view_top.set(new_top);
    display.follow.set(false);

    surface::draw(|buf| {
        let rows = display.rows.get();
        if delta < rows {
            let shift = delta * FONT_CHAR_HEIGHT;
            let width = display.width.get();
            let height = display.height.get();
            buf.blit(0, 0, 0, shift, width, height - shift);

            let view_top = display.view_top.get();
            for row in 0..delta {
                draw_row_from_scrollback(buf, display, view_top + row, row);
            }
        } else {
            redraw_view(buf, display);
        }
    });
}

fn console_page_down(display: &DisplayState) {
    let rows = display.rows.get();
    let total_lines = display.total_lines.get();

    if total_lines <= rows {
        return;
    }

    let max_top = (total_lines - rows).max(0);
    let step = rows.max(1);
    let new_top = (display.view_top.get() + step).min(max_top);
    let delta = (new_top - display.view_top.get()).max(0);

    if delta == 0 {
        return;
    }

    display.view_top.set(new_top);
    display.follow.set(new_top == max_top);

    surface::draw(|buf| {
        if delta < rows {
            let shift = delta * FONT_CHAR_HEIGHT;
            let width = display.width.get();
            let height = display.height.get();
            buf.blit(0, shift, 0, 0, width, height - shift);

            let start = rows - delta;
            let view_top = display.view_top.get();
            for row in start..rows {
                draw_row_from_scrollback(buf, display, view_top + row, row);
            }
        } else {
            redraw_view(buf, display);
        }
    });
}

fn console_ensure_follow(display: &DisplayState) {
    let max_top = (display.total_lines.get() - display.rows.get()).max(0);
    display.view_top.set(max_top);
    display.follow.set(true);

    surface::draw(|buf| {
        redraw_view(buf, display);
    });
}

fn console_rewrite_input(display: &DisplayState, prompt: &[u8], input: &[u8], cursor_pos: usize) {
    if !display.enabled.get() {
        return;
    }

    let cursor_line = display.cursor_line.get();
    let slot = display.line_slot(cursor_line);
    let cols = display.cols.get() as usize;

    let total_len = prompt.len() + input.len();
    let write_len = total_len.min(cols);

    let mut combined = [0u8; SHELL_SCROLLBACK_COLS];
    let mut idx = 0;
    for &b in prompt.iter().chain(input.iter()).take(write_len) {
        combined[idx] = b;
        idx += 1;
    }
    scrollback::write_line(slot, &combined[..idx]);
    display.cursor_col.set(idx as i32);

    if display.follow.get() {
        let view_top = display.view_top.get();
        let row = cursor_line - view_top;
        if row >= 0 && row < display.rows.get() {
            surface::draw(|buf| {
                draw_row_from_scrollback(buf, display, cursor_line, row);

                let cursor_col = (prompt.len() + cursor_pos) as i32;
                if cursor_col < display.cols.get() {
                    let ch = if cursor_pos < input.len() {
                        input[cursor_pos]
                    } else {
                        b' '
                    };
                    draw_char_at(buf, cursor_col, row, ch, display.bg.get(), display.fg.get());
                }
            });
        }
    }
}

// =============================================================================
// Public API functions
// =============================================================================

pub fn shell_console_init() {
    let width = SHELL_WINDOW_WIDTH;
    let height = SHELL_WINDOW_HEIGHT;

    if !surface::init(width, height) {
        DISPLAY.enabled.set(false);
        return;
    }

    let mut info = DisplayInfo::default();
    let _ = window::fb_info(&mut info);

    DISPLAY.width.set(width);
    DISPLAY.height.set(height);
    let bytes_pp = info.bytes_per_pixel();
    DISPLAY.bytes_pp.set(bytes_pp);
    DISPLAY.pitch.set((width as usize) * (bytes_pp as usize));

    let cols = width / FONT_CHAR_WIDTH;
    let rows = height / FONT_CHAR_HEIGHT;
    DISPLAY
        .cols
        .set(cols.clamp(1, SHELL_SCROLLBACK_COLS as i32));
    DISPLAY
        .rows
        .set(rows.clamp(1, SHELL_SCROLLBACK_LINES as i32));

    if DISPLAY.cols.get() <= 0 || DISPLAY.rows.get() <= 0 {
        DISPLAY.enabled.set(false);
        return;
    }

    DISPLAY.enabled.set(true);
    DISPLAY.reset();
    DISPLAY.fg.set(SHELL_FG_COLOR);
    DISPLAY.bg.set(SHELL_BG_COLOR);
}

pub fn shell_console_clear() {
    if DISPLAY.enabled.get() {
        console_clear(&DISPLAY);
        shell_console_commit();
    }
}

pub fn shell_console_write(buf: &[u8]) {
    if DISPLAY.enabled.get() {
        console_write(&DISPLAY, buf);
    }
}

pub fn shell_write(buf: &[u8]) {
    let redirected_fd = unsafe { *OUTPUT_FD.get() };
    if redirected_fd >= 0 {
        let _ = fs::write_slice(redirected_fd, buf);
        return;
    }
    let _ = crate::syscall::tty::write(buf);
    shell_console_write(buf);
    shell_console_commit();
}

pub fn shell_set_output_fd(fd: i32) {
    unsafe {
        *OUTPUT_FD.get() = fd;
    }
}

pub fn shell_clear_output_fd() {
    unsafe {
        *OUTPUT_FD.get() = -1;
    }
}

pub fn shell_echo_char(c: u8) {
    let buf = [c];
    let _ = crate::syscall::tty::write(&buf);
    shell_console_write(&buf);
    shell_console_commit();
}

pub fn shell_console_get_cursor() -> (i32, i32) {
    if DISPLAY.enabled.get() {
        DISPLAY.cursor()
    } else {
        (0, 0)
    }
}

pub fn shell_console_page_up() {
    if DISPLAY.enabled.get() {
        console_page_up(&DISPLAY);
        shell_console_commit();
    }
}

pub fn shell_console_page_down() {
    if DISPLAY.enabled.get() {
        console_page_down(&DISPLAY);
        shell_console_commit();
    }
}

pub fn shell_console_commit() {
    if DISPLAY.enabled.get() {
        surface::present_full();
    }
}

pub fn shell_console_follow_bottom() {
    if DISPLAY.enabled.get() {
        console_ensure_follow(&DISPLAY);
        shell_console_commit();
    }
}

pub fn shell_redraw_input(_line_row: i32, prompt: &[u8], input: &[u8], cursor_pos: usize) {
    if DISPLAY.enabled.get() {
        console_rewrite_input(&DISPLAY, prompt, input, cursor_pos);
        shell_console_commit();
    }
}
