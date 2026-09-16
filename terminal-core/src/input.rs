//! Pure input model for the terminal emulator: key encoding, pointer
//! selection, paste sanitizing, and the compositor-event taxonomy.
//!
//! Keys become PTY-master byte sequences (printable / control passthrough plus
//! the kernel's baked 0x80-0x88 navigation codes mapped to CSI). Touches no
//! syscalls, no protocol wire types and no font globals — cell metrics arrive
//! as plain `i32` arguments, and the userland app owns the actual IO and the
//! `classify(ProtocolEvent)` bridge.

use slopos_abi::input::keycode;
use slopos_abi::input::{
    MODIFIER_ALT, MODIFIER_ALTGR, MODIFIER_CTRL, MODIFIER_NUM_LOCK, MODIFIER_SHIFT,
};
use slopos_vt::MouseTracking;

use super::grid::TerminalGrid;

// Kernel-baked navigation key codes: the compositor reports these as the
// "ascii" byte for non-text keys.
const KEY_PAGE_UP: u8 = 0x80;
const KEY_PAGE_DOWN: u8 = 0x81;
const KEY_UP: u8 = 0x82;
const KEY_DOWN: u8 = 0x83;
const KEY_LEFT: u8 = 0x84;
const KEY_RIGHT: u8 = 0x85;
const KEY_HOME: u8 = 0x86;
const KEY_END: u8 = 0x87;
const KEY_DELETE: u8 = 0x88;

const MOUSE_LEFT: u8 = 0x01;
const MOUSE_RIGHT: u8 = 0x02;
const MOUSE_MIDDLE: u8 = 0x04;

/// Scrollback lines a single Ctrl+Shift+PgUp / PgDn moves.
const SCROLLBACK_PAGE_LINES: usize = 10;

/// Scrollback lines moved per mouse-wheel / touchpad notch (a value120 axis
/// delta of ±120); three per notch is the common terminal default.
const SCROLLBACK_WHEEL_LINES: i32 = 3;

/// What the event loop should do after a compositor key event.
pub enum KeyAction {
    /// Write these bytes to the PTY master.
    ToMaster(KeyBytes),
    /// Scroll the local scrollback view (Ctrl+Shift+PgUp/PgDn).
    ScrollUp(usize),
    ScrollDown(usize),
    /// Ctrl+Shift+C: copy the pointer selection to the compositor clipboard.
    CopySelection,
    /// Ctrl+Shift+V: ask the compositor for the clipboard contents (the
    /// `PasteResult` reply feeds the paste writer).
    RequestPaste,
    None,
}

/// A short, owned byte sequence for one key or mouse report. Sized for the
/// longest either produces: an SGR report at the grid's maximum coordinates.
pub struct KeyBytes {
    buf: [u8; 16],
    len: usize,
}

impl KeyBytes {
    fn one(b: u8) -> Self {
        let mut out = Self::empty();
        out.push(b);
        out
    }

    fn seq(s: &[u8]) -> Self {
        let mut out = Self::empty();
        for &b in s {
            out.push(b);
        }
        out
    }

    const fn empty() -> Self {
        Self {
            buf: [0u8; 16],
            len: 0,
        }
    }

    fn push(&mut self, b: u8) {
        if self.len < self.buf.len() {
            self.buf[self.len] = b;
            self.len += 1;
        }
    }

    fn push_num(&mut self, n: u16) {
        let (digits, count) = crate::decimal(n);
        for &d in &digits[..count] {
            self.push(d);
        }
    }

    fn utf8(c: char) -> Self {
        let mut out = Self::empty();
        let mut scratch = [0u8; 4];
        for &b in c.encode_utf8(&mut scratch).as_bytes() {
            out.push(b);
        }
        out
    }

    fn prefixed_esc(self) -> Self {
        let mut out = Self::empty();
        out.push(0x1B);
        for &b in self.as_bytes() {
            out.push(b);
        }
        out
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

/// A selection endpoint anchored to a stable content coordinate: an absolute
/// line number (see [`TerminalGrid::screen_to_abs`]) plus a column. Anchoring
/// to content rather than a screen cell is what makes a copy survive scrolling.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    pub line: u64,
    pub col: usize,
}

impl Anchor {
    const ZERO: Self = Self { line: 0, col: 0 };

    #[inline]
    fn key(&self) -> (u64, usize) {
        (self.line, self.col)
    }
}

/// Pointer-driven cell selection over the terminal's content.
///
/// `anchor` is where the drag began, `head` the current drag point — both in
/// absolute content coordinates. `active` is false until a drag produces a
/// non-empty range.
pub struct Selection {
    pub anchor: Anchor,
    pub head: Anchor,
    pub active: bool,
}

impl Selection {
    pub const NONE: Self = Self {
        anchor: Anchor::ZERO,
        head: Anchor::ZERO,
        active: false,
    };

    pub fn clear(&mut self) {
        *self = Self::NONE;
    }

    pub fn is_active(&self) -> bool {
        self.active && self.anchor != self.head
    }

    /// The active selection endpoints as `(line, col)` content points for
    /// [`TerminalGrid::resize`](crate::grid::TerminalGrid::resize)'s anchor
    /// remap, or `[None; 2]` when inactive.
    pub fn endpoints(&self) -> [Option<(u64, usize)>; 2] {
        if self.is_active() {
            [
                Some((self.anchor.line, self.anchor.col)),
                Some((self.head.line, self.head.col)),
            ]
        } else {
            [None, None]
        }
    }

    /// Re-seat the endpoints from content points remapped by a width reflow.
    pub fn set_endpoints(&mut self, anchor: (u64, usize), head: (u64, usize)) {
        self.anchor = Anchor {
            line: anchor.0,
            col: anchor.1,
        };
        self.head = Anchor {
            line: head.0,
            col: head.1,
        };
    }

    /// Ordered `(lo, hi)` anchors with `hi` exclusive at cell granularity, or
    /// `None` when inactive. Ordering is lexicographic on `(line, col)`.
    pub fn ordered(&self) -> Option<(Anchor, Anchor)> {
        if !self.is_active() {
            return None;
        }
        Some(if self.anchor.key() <= self.head.key() {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        })
    }
}

/// Pointer tracking state used to drive selection during the event loop.
pub struct PointerState {
    pub last_x: i32,
    pub last_y: i32,
    pub has_focus: bool,
    pub button_state: u8,
    pub prev_left: bool,
    pub dragging: bool,
}

impl PointerState {
    pub const fn new() -> Self {
        Self {
            last_x: 0,
            last_y: 0,
            has_focus: false,
            button_state: 0,
            prev_left: false,
            dragging: false,
        }
    }

    pub fn left_pressed(&self) -> bool {
        self.has_focus && (self.button_state & MOUSE_LEFT) != 0
    }
}

/// Convert a pixel coordinate to a clamped `(screen_row, col)` cell. `cell_w`/
/// `cell_h` are the caller's font metrics; the core stays font-agnostic.
/// Clamping is what lets a drag past the window edge still name a cell.
pub fn pixel_to_cell(
    px: i32,
    py: i32,
    cell_w: i32,
    cell_h: i32,
    grid: &TerminalGrid,
) -> (usize, usize) {
    let cw = cell_w.max(1);
    let ch = cell_h.max(1);
    let col = (px / cw).clamp(0, grid.cols as i32 - 1) as usize;
    let row = (py / ch).clamp(0, grid.rows as i32 - 1) as usize;
    (row, col)
}

/// Capture the content anchor under a pixel coordinate (screen row resolved to
/// an absolute line via the grid's current view).
fn pixel_to_anchor(px: i32, py: i32, cell_w: i32, cell_h: i32, grid: &TerminalGrid) -> Anchor {
    let (row, col) = pixel_to_cell(px, py, cell_w, cell_h, grid);
    Anchor {
        line: grid.screen_to_abs(row),
        col,
    }
}

/// One compositor key press, as the terminal receives it.
#[derive(Clone, Copy)]
pub struct KeyPress {
    /// Legacy single-byte code: ASCII text, or one of the kernel's baked
    /// navigation pseudo-codes (0x80..=0x88).
    pub ascii: u8,
    /// Canonical HID usage; 0 for an event that carries none (a dead-key
    /// accent flush).
    pub keycode: u16,
    /// Layout-resolved text codepoint; 0 for a key that produces no text.
    pub codepoint: u32,
    /// `MODIFIER_*` snapshot at the time of the press.
    pub mods: u8,
}

/// The escape-sequence family a non-text key belongs to, in xterm's PC-style
/// encoding.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Special {
    /// `CSI <final>`, `SS3 <final>` under DECCKM, `CSI 1 ; <mod> <final>` when
    /// modified. Home and End ride it too.
    Cursor(u8),
    /// `CSI <n> ~`, `CSI <n> ; <mod> ~` when modified.
    Tilde(u16),
    /// F1–F4: `SS3 <final>` unmodified regardless of DECCKM,
    /// `CSI 1 ; <mod> <final>` when modified.
    Pf(u8),
}

/// xterm's modifier parameter: `1 + shift + 2*alt + 4*ctrl`.
///
/// AltGr is reported with `MODIFIER_ALT` set too, but it selects a layout
/// level rather than a chord — counting it would turn `AltGr+2` into a
/// modified key instead of the `@` the layout resolved.
fn modifier_param(mods: u8) -> u16 {
    let mut m = 1u16;
    if mods & MODIFIER_SHIFT != 0 {
        m += 1;
    }
    if mods & MODIFIER_ALT != 0 && mods & MODIFIER_ALTGR == 0 {
        m += 2;
    }
    if mods & MODIFIER_CTRL != 0 {
        m += 4;
    }
    m
}

fn alt_chord(mods: u8) -> bool {
    mods & MODIFIER_ALT != 0 && mods & MODIFIER_ALTGR == 0
}

fn encode_special(special: Special, mods: u8, app_cursor: bool) -> KeyBytes {
    let param = modifier_param(mods);
    let mut out = KeyBytes::empty();
    out.push(0x1B);
    match special {
        Special::Cursor(final_byte) => {
            if param == 1 {
                out.push(if app_cursor { b'O' } else { b'[' });
                out.push(final_byte);
            } else {
                out.push(b'[');
                out.push(b'1');
                out.push(b';');
                out.push_num(param);
                out.push(final_byte);
            }
        }
        Special::Tilde(n) => {
            out.push(b'[');
            out.push_num(n);
            if param != 1 {
                out.push(b';');
                out.push_num(param);
            }
            out.push(b'~');
        }
        Special::Pf(final_byte) => {
            if param == 1 {
                out.push(b'O');
                out.push(final_byte);
            } else {
                out.push(b'[');
                out.push(b'1');
                out.push(b';');
                out.push_num(param);
                out.push(final_byte);
            }
        }
    }
    out
}

/// The family a baked navigation pseudo-code belongs to. Also how keypad
/// navigation arrives.
fn special_for_ascii(ascii: u8) -> Option<Special> {
    Some(match ascii {
        KEY_UP => Special::Cursor(b'A'),
        KEY_DOWN => Special::Cursor(b'B'),
        KEY_RIGHT => Special::Cursor(b'C'),
        KEY_LEFT => Special::Cursor(b'D'),
        KEY_HOME => Special::Cursor(b'H'),
        KEY_END => Special::Cursor(b'F'),
        KEY_DELETE => Special::Tilde(3),
        KEY_PAGE_UP => Special::Tilde(5),
        KEY_PAGE_DOWN => Special::Tilde(6),
        _ => return None,
    })
}

/// The family a canonical HID usage belongs to. The driver bakes no legacy
/// byte for F1–F12 or Insert, so this is their only source.
fn special_for_keycode(code: u16, mods: u8) -> Option<Special> {
    // The keymap resolves a keypad key to text when `numlock ^ shift`; the
    // complement is its navigation meaning.
    let keypad_nav = (mods & MODIFIER_NUM_LOCK != 0) == (mods & MODIFIER_SHIFT != 0);
    Some(match code {
        keycode::KEY_UP => Special::Cursor(b'A'),
        keycode::KEY_DOWN => Special::Cursor(b'B'),
        keycode::KEY_RIGHT => Special::Cursor(b'C'),
        keycode::KEY_LEFT => Special::Cursor(b'D'),
        keycode::KEY_HOME => Special::Cursor(b'H'),
        keycode::KEY_END => Special::Cursor(b'F'),
        keycode::KEY_INSERT => Special::Tilde(2),
        keycode::KEY_DELETE => Special::Tilde(3),
        keycode::KEY_PAGEUP => Special::Tilde(5),
        keycode::KEY_PAGEDOWN => Special::Tilde(6),
        keycode::KEY_F1 => Special::Pf(b'P'),
        keycode::KEY_F2 => Special::Pf(b'Q'),
        keycode::KEY_F3 => Special::Pf(b'R'),
        keycode::KEY_F4 => Special::Pf(b'S'),
        keycode::KEY_F5 => Special::Tilde(15),
        keycode::KEY_F6 => Special::Tilde(17),
        keycode::KEY_F7 => Special::Tilde(18),
        keycode::KEY_F8 => Special::Tilde(19),
        keycode::KEY_F9 => Special::Tilde(20),
        keycode::KEY_F10 => Special::Tilde(21),
        keycode::KEY_F11 => Special::Tilde(23),
        keycode::KEY_F12 => Special::Tilde(24),
        keycode::KEY_KP_7 if keypad_nav => Special::Cursor(b'H'),
        keycode::KEY_KP_8 if keypad_nav => Special::Cursor(b'A'),
        keycode::KEY_KP_9 if keypad_nav => Special::Tilde(5),
        keycode::KEY_KP_4 if keypad_nav => Special::Cursor(b'D'),
        keycode::KEY_KP_6 if keypad_nav => Special::Cursor(b'C'),
        keycode::KEY_KP_1 if keypad_nav => Special::Cursor(b'F'),
        keycode::KEY_KP_2 if keypad_nav => Special::Cursor(b'B'),
        keycode::KEY_KP_3 if keypad_nav => Special::Tilde(6),
        keycode::KEY_KP_0 if keypad_nav => Special::Tilde(2),
        keycode::KEY_KP_DOT if keypad_nav => Special::Tilde(3),
        _ => return None,
    })
}

/// Encode a compositor key press into a master action.
///
/// `app_cursor` is DECCKM: an application that set it expects `SS3 A` rather
/// than `CSI A`.
///
/// Ctrl+Shift chords are terminal commands, never PTY input: the kernel bakes
/// the same control byte for Ctrl+C and Ctrl+Shift+C, so `mods` is the only
/// way to tell them apart. Ctrl+Shift+PgUp/PgDn pages the local scrollback for
/// the same reason — plain PgUp/PgDn belongs to the application.
pub fn encode_key(key: KeyPress, app_cursor: bool) -> KeyAction {
    const CHORD: u8 = MODIFIER_CTRL | MODIFIER_SHIFT;
    let chorded = key.mods & CHORD == CHORD;
    if chorded {
        match key.ascii {
            0x03 => return KeyAction::CopySelection, // Ctrl+Shift+C
            0x16 => return KeyAction::RequestPaste,  // Ctrl+Shift+V
            _ => {}
        }
        if key.ascii == KEY_PAGE_UP || key.keycode == keycode::KEY_PAGEUP {
            return KeyAction::ScrollUp(SCROLLBACK_PAGE_LINES);
        }
        if key.ascii == KEY_PAGE_DOWN || key.keycode == keycode::KEY_PAGEDOWN {
            return KeyAction::ScrollDown(SCROLLBACK_PAGE_LINES);
        }
    }

    if let Some(special) = special_for_ascii(key.ascii) {
        return KeyAction::ToMaster(encode_special(special, key.mods, app_cursor));
    }

    if key.ascii != 0 {
        // Shift+Tab is CBT, not a tab: the keymap folds both to 0x09, so the
        // modifier snapshot is the only thing that distinguishes them.
        if key.ascii == b'\t' && modifier_param(key.mods) == 2 {
            return KeyAction::ToMaster(KeyBytes::seq(b"\x1b[Z"));
        }
        let bytes = KeyBytes::one(key.ascii);
        return KeyAction::ToMaster(if alt_chord(key.mods) {
            bytes.prefixed_esc()
        } else {
            bytes
        });
    }

    if key.codepoint > 0x7F {
        if let Some(c) = char::from_u32(key.codepoint) {
            let bytes = KeyBytes::utf8(c);
            return KeyAction::ToMaster(if alt_chord(key.mods) {
                bytes.prefixed_esc()
            } else {
                bytes
            });
        }
    }

    match special_for_keycode(key.keycode, key.mods) {
        Some(special) => KeyAction::ToMaster(encode_special(special, key.mods, app_cursor)),
        None => KeyAction::None,
    }
}

/// What a (non-key) compositor event resolved to.
pub enum CompositorEvent {
    /// Key press, with everything the encoder needs to resolve it.
    Key(KeyPress),
    /// Keyboard modifier state changed (bitfield of `MODIFIER_*`).
    Modifiers(u8),
    /// Configure: new pixel dimensions.
    Resize(i32, i32),
    Close,
    PointerMotion(i32, i32),
    PointerEnter(i32, i32),
    PointerLeave,
    PointerButton {
        pressed: bool,
        code: u8,
    },
    /// Mouse-wheel / touchpad scroll on the vertical axis, carrying the raw
    /// value120 delta (±120 = one notch). Resolve it with [`wheel_scroll_lines`].
    Scroll(i32),
    /// The compositor reports the clipboard holds this many bytes; the app
    /// should provide a destination memfd of that size (0 = empty, no-op).
    PasteReady(u32),
    /// The destination memfd handed to the compositor now holds this many
    /// valid clipboard bytes, ready to write to the PTY master.
    PasteResult(u32),
    Ignored,
}

/// Resolve a pointer-axis value120 delta (±120 per notch) into a signed number
/// of scrollback lines: negative scrolls up into history, matching the rest of
/// the system's axis convention. Sub-notch deltas round toward zero.
pub fn wheel_scroll_lines(value_v120: i32) -> i32 {
    (value_v120 / 120) * SCROLLBACK_WHEEL_LINES
}

/// What a pointer event means to a mouse-tracking application.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MouseEventKind {
    Press,
    Release,
    /// The pointer moved into a different cell.
    Motion,
    WheelUp,
    WheelDown,
}

/// xterm's button number. The wire value is a bitmask, so a multi-button state
/// resolves to its lowest set bit.
fn mouse_button_number(code: u8) -> Option<u16> {
    if code & MOUSE_LEFT != 0 {
        Some(0)
    } else if code & MOUSE_MIDDLE != 0 {
        Some(1)
    } else if code & MOUSE_RIGHT != 0 {
        Some(2)
    } else {
        None
    }
}

/// Encode a pointer event as a mouse report, or `None` when the tracking mode
/// does not ask for this event.
///
/// `col`/`row` are 0-based; the wire form is 1-based. `buttons_held` is what
/// distinguishes a drag from a bare move under button-event tracking.
pub fn encode_mouse(
    tracking: MouseTracking,
    sgr: bool,
    kind: MouseEventKind,
    code: u8,
    buttons_held: u8,
    col: usize,
    row: usize,
    mods: u8,
) -> Option<KeyBytes> {
    if tracking == MouseTracking::Off {
        return None;
    }

    let mut cb = match kind {
        MouseEventKind::WheelUp => 64,
        MouseEventKind::WheelDown => 65,
        MouseEventKind::Press | MouseEventKind::Release => mouse_button_number(code)?,
        MouseEventKind::Motion => {
            match tracking {
                MouseTracking::Normal => return None,
                MouseTracking::ButtonEvent if buttons_held == 0 => return None,
                _ => {}
            }
            // A drag reports the held button; a bare move under any-event
            // tracking reports 3, xterm's "no button" value.
            32 + mouse_button_number(buttons_held).unwrap_or(3)
        }
    };

    if mods & MODIFIER_SHIFT != 0 {
        cb += 4;
    }
    if mods & MODIFIER_ALT != 0 && mods & MODIFIER_ALTGR == 0 {
        cb += 8;
    }
    if mods & MODIFIER_CTRL != 0 {
        cb += 16;
    }

    let cx = u16::try_from(col.saturating_add(1)).ok()?;
    let cy = u16::try_from(row.saturating_add(1)).ok()?;

    let mut out = KeyBytes::empty();
    out.push(0x1B);
    out.push(b'[');
    if sgr {
        out.push(b'<');
        out.push_num(cb);
        out.push(b';');
        out.push_num(cx);
        out.push(b';');
        out.push_num(cy);
        out.push(if kind == MouseEventKind::Release {
            b'm'
        } else {
            b'M'
        });
    } else {
        // One byte per field, so a coordinate past 223 has no representation
        // at all; 1006 is the encoding without the limit.
        if cb > 223 || cx > 223 || cy > 223 {
            return None;
        }
        if kind == MouseEventKind::Release {
            // X10 names no button on release, but keeps the press's modifiers.
            cb = (cb & !3) | 3;
        }
        out.push(b'M');
        out.push(32 + cb as u8);
        out.push(32 + cx as u8);
        out.push(32 + cy as u8);
    }
    Some(out)
}

/// True when the application asked for mouse reports and Shift is not held.
/// Shift is xterm's override back to a local selection.
pub fn mouse_reporting(grid: &TerminalGrid, mods: u8) -> bool {
    grid.mouse_tracking() != MouseTracking::Off && mods & MODIFIER_SHIFT == 0
}

/// Encode a pointer event at a pixel position. The single place pixel-to-cell
/// and [`encode_mouse`]'s `(col, row)` order meet: both are `usize`, so a
/// transposed call site compiles.
#[allow(clippy::too_many_arguments)]
pub fn mouse_report_at(
    grid: &TerminalGrid,
    kind: MouseEventKind,
    code: u8,
    buttons_held: u8,
    px: i32,
    py: i32,
    cell_w: i32,
    cell_h: i32,
    mods: u8,
) -> Option<KeyBytes> {
    let (row, col) = pixel_to_cell(px, py, cell_w, cell_h, grid);
    encode_mouse(
        grid.mouse_tracking(),
        grid.mouse_sgr(),
        kind,
        code,
        buttons_held,
        col,
        row,
        mods,
    )
}

/// Update pointer-driven selection from a button/motion change. Returns true
/// when the selection changed, so the caller re-renders.
pub fn update_selection(
    ptr: &mut PointerState,
    selection: &mut Selection,
    grid: &TerminalGrid,
    cell_w: i32,
    cell_h: i32,
) -> bool {
    let left = ptr.left_pressed();
    let newly_pressed = left && !ptr.prev_left;
    let newly_released = !left && ptr.prev_left;
    let mut changed = false;

    if newly_pressed {
        let a = pixel_to_anchor(ptr.last_x, ptr.last_y, cell_w, cell_h, grid);
        selection.anchor = a;
        selection.head = a;
        selection.active = true;
        ptr.dragging = true;
        changed = true;
    } else if ptr.dragging && left {
        let h = pixel_to_anchor(ptr.last_x, ptr.last_y, cell_w, cell_h, grid);
        if h != selection.head {
            selection.head = h;
            changed = true;
        }
    }

    if newly_released && ptr.dragging {
        ptr.dragging = false;
        if selection.anchor == selection.head {
            selection.clear();
            changed = true;
        }
    }

    ptr.prev_left = left;
    changed
}

/// Sanitize a clipboard payload so it can only ever act as typed text, for
/// bracketed and plain pastes alike: `\r\n` and `\n` normalize to `\r`, `\t`
/// passes, every other C0 control and DEL is dropped, bytes above 0x7F pass
/// through. Returns the sanitized length in `out`.
///
/// Dropping ESC outright is what makes bracketed paste injection-proof (the
/// xterm CVE-2022-45063 class): with no ESC byte in the payload the `\x1b[201~`
/// end marker cannot appear — not literally, and not spliced together from
/// fragments around a stripped inner marker.
pub fn sanitize_paste(data: &[u8], out: &mut [u8]) -> usize {
    let mut n = 0usize;
    let mut i = 0usize;
    while i < data.len() && n < out.len() {
        let b = data[i];
        match b {
            b'\r' => {
                out[n] = b'\r';
                n += 1;
                // Swallow the \n of a \r\n pair.
                if data.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
            }
            b'\n' => {
                out[n] = b'\r';
                n += 1;
            }
            b'\t' | 0x20..=0x7E | 0x80.. => {
                out[n] = b;
                n += 1;
            }
            // Remaining C0 controls (incl. ESC) and DEL: dropped.
            _ => {}
        }
        i += 1;
    }
    n
}

/// Upper bound on the byte length [`collect_selection`] can produce for this
/// selection: one byte per cell across every selected line plus a newline per
/// line. The caller sizes its clipboard memfd to this before collecting.
pub fn selection_byte_bound(grid: &TerminalGrid, selection: &Selection) -> usize {
    let cols = grid.cols as usize;
    match selection.ordered() {
        Some((lo, hi)) => {
            let lines = (hi.line - lo.line + 1) as usize;
            lines.saturating_mul(cols + 1)
        }
        None => 0,
    }
}

/// Extract the selected text into `out`, returning the bytes captured (bounded
/// by `out.len()`). Reads by absolute content line via
/// [`TerminalGrid::abs_cell`], so the text is the originally-selected content
/// regardless of the current scroll position. Trailing blanks on each row are
/// trimmed, multi-row selections join with `\n`, and lines evicted from
/// scrollback yield blanks rather than panicking.
pub fn collect_selection(grid: &TerminalGrid, selection: &Selection, out: &mut [u8]) -> usize {
    let cols = grid.cols as usize;
    let Some((lo, hi)) = selection.ordered() else {
        return 0;
    };
    let cap = out.len();

    // `hi` is exclusive: when it sits at column 0 the final line contributes
    // nothing, so the last line with content is `hi.line - 1`. `is_active`
    // guarantees `hi.col > 0` whenever `hi.line == lo.line`.
    let last_line = if hi.col == 0 {
        hi.line.wrapping_sub(1)
    } else {
        hi.line
    };

    let mut n = 0usize;
    let mut line = lo.line;
    let mut first = true;
    while line <= last_line && n < cap {
        if !first {
            out[n] = b'\n';
            n += 1;
            if n >= cap {
                break;
            }
        }
        first = false;

        let start_col = if line == lo.line { lo.col } else { 0 };
        let end_col = if line == hi.line { hi.col } else { cols };
        let mut last_nonblank = n;
        let mut col = start_col;
        while col < end_col && n < cap {
            let cp = grid.abs_cell(line, col).glyph();
            let byte = if (0x20..=0x7E).contains(&cp) {
                cp as u8
            } else if cp == b'\t' as u32 {
                b'\t'
            } else {
                b' '
            };
            out[n] = byte;
            n += 1;
            if byte != b' ' {
                last_nonblank = n;
            }
            col += 1;
        }
        // Trim trailing blanks on this row.
        n = last_nonblank;
        line = line.wrapping_add(1);
    }
    n
}

/// Whether absolute cell `(abs_line, col)` lies within the ordered selection
/// range `[lo, hi)` (lexicographic on `(line, col)`, `hi` exclusive).
pub fn cell_in_selection(abs_line: u64, col: usize, lo: Anchor, hi: Anchor) -> bool {
    let pos = (abs_line, col);
    pos >= lo.key() && pos < hi.key()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    const CTRL_SHIFT: u8 = MODIFIER_CTRL | MODIFIER_SHIFT;

    fn master_bytes(action: KeyAction) -> Vec<u8> {
        match action {
            KeyAction::ToMaster(b) => b.as_bytes().to_vec(),
            _ => panic!("expected ToMaster"),
        }
    }

    fn text(ascii: u8, mods: u8) -> KeyPress {
        KeyPress {
            ascii,
            keycode: 0,
            codepoint: ascii as u32,
            mods,
        }
    }

    fn baked(ascii: u8, mods: u8) -> KeyPress {
        KeyPress {
            ascii,
            keycode: 0,
            codepoint: 0,
            mods,
        }
    }

    fn named(code: u16, mods: u8) -> KeyPress {
        KeyPress {
            ascii: 0,
            keycode: code,
            codepoint: 0,
            mods,
        }
    }

    fn bytes_of(key: KeyPress) -> Vec<u8> {
        master_bytes(encode_key(key, false))
    }

    #[test]
    fn ctrl_shift_c_copies_instead_of_sigint() {
        assert!(matches!(
            encode_key(text(0x03, CTRL_SHIFT), false),
            KeyAction::CopySelection
        ));
    }

    /// Plain Ctrl+C (no Shift) must reach the PTY master so the ldisc can
    /// raise SIGINT.
    #[test]
    fn ctrl_only_c_reaches_master_as_sigint_byte() {
        assert_eq!(bytes_of(text(0x03, MODIFIER_CTRL)), alloc::vec![0x03]);
    }

    #[test]
    fn ctrl_shift_v_requests_paste() {
        assert!(matches!(
            encode_key(text(0x16, CTRL_SHIFT), false),
            KeyAction::RequestPaste
        ));
    }

    #[test]
    fn shift_only_c_is_plain_text() {
        assert_eq!(bytes_of(text(b'C', MODIFIER_SHIFT)), [b'C']);
    }

    #[test]
    fn ctrl_shift_other_keys_still_reach_master() {
        // Ctrl+Shift+A (0x01) is not a clipboard chord; the ldisc gets it.
        assert_eq!(bytes_of(text(0x01, CTRL_SHIFT)), [0x01]);
    }

    #[test]
    fn baked_navigation_codes_map_to_csi() {
        assert_eq!(bytes_of(baked(KEY_UP, 0)), b"\x1b[A");
        assert_eq!(bytes_of(baked(KEY_DOWN, 0)), b"\x1b[B");
        assert_eq!(bytes_of(baked(KEY_LEFT, 0)), b"\x1b[D");
        assert_eq!(bytes_of(baked(KEY_RIGHT, 0)), b"\x1b[C");
        assert_eq!(bytes_of(baked(KEY_HOME, 0)), b"\x1b[H");
        assert_eq!(bytes_of(baked(KEY_END, 0)), b"\x1b[F");
        assert_eq!(bytes_of(baked(KEY_DELETE, 0)), b"\x1b[3~");
    }

    /// The editing keys the driver bakes no legacy byte for, so the canonical
    /// keycode is their only source.
    #[test]
    fn function_keys_use_the_pc_style_encoding() {
        assert_eq!(bytes_of(named(keycode::KEY_F1, 0)), b"\x1bOP");
        assert_eq!(bytes_of(named(keycode::KEY_F4, 0)), b"\x1bOS");
        assert_eq!(bytes_of(named(keycode::KEY_F5, 0)), b"\x1b[15~");
        assert_eq!(bytes_of(named(keycode::KEY_F6, 0)), b"\x1b[17~");
        assert_eq!(bytes_of(named(keycode::KEY_F10, 0)), b"\x1b[21~");
        assert_eq!(bytes_of(named(keycode::KEY_F11, 0)), b"\x1b[23~");
        assert_eq!(bytes_of(named(keycode::KEY_F12, 0)), b"\x1b[24~");
        assert_eq!(bytes_of(named(keycode::KEY_INSERT, 0)), b"\x1b[2~");
    }

    #[test]
    fn modifiers_ride_the_second_csi_parameter() {
        assert_eq!(bytes_of(baked(KEY_LEFT, MODIFIER_CTRL)), b"\x1b[1;5D");
        assert_eq!(bytes_of(baked(KEY_RIGHT, MODIFIER_SHIFT)), b"\x1b[1;2C");
        assert_eq!(bytes_of(baked(KEY_HOME, MODIFIER_ALT)), b"\x1b[1;3H");
        assert_eq!(bytes_of(baked(KEY_DELETE, MODIFIER_CTRL)), b"\x1b[3;5~");
        assert_eq!(
            bytes_of(named(keycode::KEY_F5, MODIFIER_SHIFT)),
            b"\x1b[15;2~"
        );
        // F1–F4 leave SS3 for CSI the moment they are modified.
        assert_eq!(
            bytes_of(named(keycode::KEY_F1, MODIFIER_CTRL)),
            b"\x1b[1;5P"
        );
        // Every modifier at once is the widest sequence the encoder produces.
        assert_eq!(
            bytes_of(named(
                keycode::KEY_F12,
                MODIFIER_CTRL | MODIFIER_SHIFT | MODIFIER_ALT
            )),
            b"\x1b[24;8~"
        );
    }

    /// AltGr sets Alt too, but it selected a layout level — the codepoint it
    /// produced must not come back as a modified key.
    #[test]
    fn altgr_is_not_an_alt_chord() {
        let mut key = text(b'@', MODIFIER_ALT | MODIFIER_ALTGR);
        key.codepoint = b'@' as u32;
        assert_eq!(bytes_of(key), [b'@']);
        assert_eq!(
            bytes_of(baked(KEY_LEFT, MODIFIER_ALT | MODIFIER_ALTGR)),
            b"\x1b[D"
        );
    }

    #[test]
    fn alt_prefixes_text_with_escape() {
        assert_eq!(bytes_of(text(b'x', MODIFIER_ALT)), b"\x1bx");
        let mut key = named(0, MODIFIER_ALT);
        key.codepoint = 0x00E4;
        assert_eq!(bytes_of(key), b"\x1b\xc3\xa4");
    }

    #[test]
    fn application_cursor_keys_use_ss3() {
        assert_eq!(master_bytes(encode_key(baked(KEY_UP, 0), true)), b"\x1bOA");
        assert_eq!(
            master_bytes(encode_key(baked(KEY_HOME, 0), true)),
            b"\x1bOH"
        );
        // A modified cursor key is CSI even under DECCKM, as xterm has it.
        assert_eq!(
            master_bytes(encode_key(baked(KEY_UP, MODIFIER_CTRL), true)),
            b"\x1b[1;5A"
        );
    }

    #[test]
    fn shift_tab_is_cbt() {
        assert_eq!(bytes_of(text(b'\t', MODIFIER_SHIFT)), b"\x1b[Z");
        assert_eq!(bytes_of(text(b'\t', 0)), b"\t");
    }

    /// An editor needs PgUp/PgDn, so the local scrollback moved to the chord
    /// that already means "terminal command" here.
    #[test]
    fn page_keys_reach_the_application_and_the_chord_scrolls() {
        assert_eq!(bytes_of(baked(KEY_PAGE_UP, 0)), b"\x1b[5~");
        assert_eq!(bytes_of(baked(KEY_PAGE_DOWN, 0)), b"\x1b[6~");
        assert_eq!(bytes_of(baked(KEY_PAGE_UP, MODIFIER_SHIFT)), b"\x1b[5;2~");
        assert!(matches!(
            encode_key(baked(KEY_PAGE_UP, CTRL_SHIFT), false),
            KeyAction::ScrollUp(SCROLLBACK_PAGE_LINES)
        ));
        assert!(matches!(
            encode_key(baked(KEY_PAGE_DOWN, CTRL_SHIFT), false),
            KeyAction::ScrollDown(SCROLLBACK_PAGE_LINES)
        ));
    }

    /// Keypad navigation arrives as a baked byte for every key but KP-0, whose
    /// Insert meaning has no legacy code.
    #[test]
    fn keypad_navigation_resolves_without_a_baked_byte() {
        assert_eq!(bytes_of(named(keycode::KEY_KP_0, 0)), b"\x1b[2~");
        assert_eq!(bytes_of(named(keycode::KEY_KP_DOT, 0)), b"\x1b[3~");
        // NumLock on means the keypad is digits; the keymap resolved text, so
        // the nav mapping must stand down.
        assert!(matches!(
            encode_key(named(keycode::KEY_KP_0, MODIFIER_NUM_LOCK), false),
            KeyAction::None
        ));
    }

    #[test]
    fn wheel_axis_resolves_to_signed_scrollback_lines() {
        assert_eq!(wheel_scroll_lines(-120), -SCROLLBACK_WHEEL_LINES);
        assert_eq!(wheel_scroll_lines(120), SCROLLBACK_WHEEL_LINES);
        assert_eq!(wheel_scroll_lines(-240), -2 * SCROLLBACK_WHEEL_LINES);
        assert_eq!(wheel_scroll_lines(0), 0);
        assert_eq!(wheel_scroll_lines(60), 0);
    }

    #[test]
    fn an_unmapped_key_produces_nothing() {
        assert!(matches!(
            encode_key(named(keycode::KEY_PRINTSCREEN, 0), false),
            KeyAction::None
        ));
    }

    #[test]
    fn non_ascii_codepoint_encodes_as_utf8() {
        let mut key = named(0, 0);
        key.codepoint = 0x00E4;
        assert_eq!(bytes_of(key), "ä".as_bytes());
        key.codepoint = 0x20AC;
        assert_eq!(bytes_of(key), "€".as_bytes());
        // The dead-key accent flush carries no keycode at all.
        key.codepoint = 0x00B4;
        assert_eq!(bytes_of(key), "´".as_bytes());
        // ASCII still rides the legacy byte, not double-encoded.
        assert_eq!(bytes_of(text(b'a', 0)), [b'a']);
    }

    #[test]
    fn sgr_mouse_reports_are_one_based_with_a_release_marker() {
        let press = encode_mouse(
            MouseTracking::Normal,
            true,
            MouseEventKind::Press,
            MOUSE_LEFT,
            MOUSE_LEFT,
            9,
            4,
            0,
        )
        .expect("press reported");
        assert_eq!(press.as_bytes(), b"\x1b[<0;10;5M");

        let release = encode_mouse(
            MouseTracking::Normal,
            true,
            MouseEventKind::Release,
            MOUSE_LEFT,
            0,
            9,
            4,
            0,
        )
        .expect("release reported");
        assert_eq!(release.as_bytes(), b"\x1b[<0;10;5m");

        let ctrl_right = encode_mouse(
            MouseTracking::Normal,
            true,
            MouseEventKind::Press,
            MOUSE_RIGHT,
            MOUSE_RIGHT,
            299,
            99,
            MODIFIER_CTRL,
        )
        .expect("modified press reported");
        assert_eq!(ctrl_right.as_bytes(), b"\x1b[<18;300;100M");
    }

    #[test]
    fn x10_mouse_reports_bias_by_32_and_refuse_what_they_cannot_encode() {
        let press = encode_mouse(
            MouseTracking::Normal,
            false,
            MouseEventKind::Press,
            MOUSE_MIDDLE,
            MOUSE_MIDDLE,
            0,
            0,
            0,
        )
        .expect("press reported");
        assert_eq!(press.as_bytes(), &[0x1B, b'[', b'M', 33, 33, 33]);

        // X10 has one byte per field, so past column 223 there is nothing to
        // send; 1006 is the encoding that has no such limit.
        assert!(
            encode_mouse(
                MouseTracking::Normal,
                false,
                MouseEventKind::Press,
                MOUSE_LEFT,
                MOUSE_LEFT,
                230,
                0,
                0,
            )
            .is_none()
        );

        // X10 names no button on release, but the press's modifier bits are
        // the only way an application can pair the two.
        let release = encode_mouse(
            MouseTracking::Normal,
            false,
            MouseEventKind::Release,
            MOUSE_MIDDLE,
            0,
            0,
            0,
            MODIFIER_CTRL,
        )
        .expect("release reported");
        assert_eq!(release.as_bytes(), &[0x1B, b'[', b'M', 32 + 3 + 16, 33, 33]);
    }

    #[test]
    fn motion_reporting_follows_the_tracking_mode() {
        let motion =
            |tracking, held| encode_mouse(tracking, true, MouseEventKind::Motion, 0, held, 1, 1, 0);
        assert!(motion(MouseTracking::Off, MOUSE_LEFT).is_none());
        assert!(motion(MouseTracking::Normal, MOUSE_LEFT).is_none());
        assert!(motion(MouseTracking::ButtonEvent, 0).is_none());
        assert_eq!(
            motion(MouseTracking::ButtonEvent, MOUSE_LEFT)
                .expect("drag reported")
                .as_bytes(),
            b"\x1b[<32;2;2M"
        );
        // Any-event tracking reports a bare move as button 3, xterm's "none".
        assert_eq!(
            motion(MouseTracking::AnyEvent, 0)
                .expect("move reported")
                .as_bytes(),
            b"\x1b[<35;2;2M"
        );
    }

    #[test]
    fn wheel_reports_use_buttons_64_and_65() {
        let up = encode_mouse(
            MouseTracking::Normal,
            true,
            MouseEventKind::WheelUp,
            0,
            0,
            0,
            0,
            0,
        )
        .expect("wheel reported");
        assert_eq!(up.as_bytes(), b"\x1b[<64;1;1M");
        let down = encode_mouse(
            MouseTracking::Normal,
            true,
            MouseEventKind::WheelDown,
            0,
            0,
            0,
            0,
            0,
        )
        .expect("wheel reported");
        assert_eq!(down.as_bytes(), b"\x1b[<65;1;1M");
    }

    /// Both coordinates are `usize`, so a transposition in `mouse_report_at`
    /// compiles and reports the wrong cell.
    #[test]
    fn a_report_names_the_cell_under_the_pointer() {
        let mut grid = TerminalGrid::new(24, 80);
        for &b in b"\x1b[?1000h\x1b[?1006h" {
            grid.process_byte(b);
        }
        // 8x16 cells: x=24 is column 3, y=64 is row 4, so the wire form is
        // column 4 and row 5 — and they are not interchangeable.
        let report = mouse_report_at(
            &grid,
            MouseEventKind::Press,
            MOUSE_LEFT,
            MOUSE_LEFT,
            24,
            64,
            8,
            16,
            0,
        )
        .expect("press reported");
        assert_eq!(report.as_bytes(), b"\x1b[<0;4;5M");
    }

    #[test]
    fn shift_is_the_way_out_of_mouse_reporting() {
        let mut grid = TerminalGrid::new(24, 80);
        assert!(!mouse_reporting(&grid, 0));
        for &b in b"\x1b[?1000h" {
            grid.process_byte(b);
        }
        assert!(mouse_reporting(&grid, 0));
        assert!(!mouse_reporting(&grid, MODIFIER_SHIFT));
        for &b in b"\x1b[?1000l" {
            grid.process_byte(b);
        }
        assert!(!mouse_reporting(&grid, 0));
    }

    #[test]
    fn tracking_off_reports_nothing() {
        assert!(
            encode_mouse(
                MouseTracking::Off,
                true,
                MouseEventKind::Press,
                MOUSE_LEFT,
                MOUSE_LEFT,
                0,
                0,
                0,
            )
            .is_none()
        );
    }

    #[test]
    fn paste_cannot_inject_bracket_end_marker() {
        let mut out = [0u8; 64];
        // Literal end marker: the ESC is dropped, leaving inert text.
        let n = sanitize_paste(b"safe\x1b[201~rm -rf /\r", &mut out);
        assert_eq!(&out[..n], b"safe[201~rm -rf /\r");
        // Splice attack: no ESC survives, so no marker can reassemble.
        let n = sanitize_paste(b"\x1b\x1b[201~[201~x", &mut out);
        assert_eq!(&out[..n], b"[201~[201~x");
        assert!(!out[..n].windows(6).any(|w| w == b"\x1b[201~"));
    }

    #[test]
    fn paste_normalizes_newlines_and_drops_controls() {
        let mut out = [0u8; 64];
        let n = sanitize_paste(b"one\r\ntwo\nthree", &mut out);
        assert_eq!(&out[..n], b"one\rtwo\rthree");
        // Ctrl bytes, ESC, and DEL must not be typeable from a clipboard.
        let n = sanitize_paste(b"a\x03b\x1b[Ac\x7fd\te", &mut out);
        assert_eq!(&out[..n], b"ab[Acd\te");
    }
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    use crate::grid::TerminalGrid;
    use alloc::vec::Vec;

    const CW: i32 = 8;
    const CH: i32 = 16;

    fn feed(g: &mut TerminalGrid, s: &[u8]) {
        for &b in s {
            g.process_byte(b);
        }
    }

    /// Simulate a press at screen `(r0,c0)`, drag to `(r1,c1)`, release.
    fn drag_select(
        g: &TerminalGrid,
        sel: &mut Selection,
        r0: usize,
        c0: usize,
        r1: usize,
        c1: usize,
    ) {
        let mut ptr = PointerState::new();
        ptr.has_focus = true;
        ptr.button_state = MOUSE_LEFT;
        ptr.last_x = c0 as i32 * CW;
        ptr.last_y = r0 as i32 * CH;
        update_selection(&mut ptr, sel, g, CW, CH);
        ptr.last_x = c1 as i32 * CW;
        ptr.last_y = r1 as i32 * CH;
        update_selection(&mut ptr, sel, g, CW, CH);
        ptr.button_state = 0;
        update_selection(&mut ptr, sel, g, CW, CH);
    }

    fn copied(g: &TerminalGrid, sel: &Selection) -> Vec<u8> {
        let mut buf = [0u8; 256];
        let n = collect_selection(g, sel, &mut buf);
        buf[..n].to_vec()
    }

    #[test]
    fn screen_abs_round_trip_at_view_zero() {
        let mut g = TerminalGrid::new(5, 10);
        feed(&mut g, b"a\r\nb\r\nc\r\nd");
        for r in 0..5 {
            assert_eq!(g.abs_to_screen(g.screen_to_abs(r)), Some(r));
        }
        assert_eq!(g.screen_to_abs(0), 0);
    }

    #[test]
    fn screen_abs_round_trip_while_scrolled() {
        let mut g = TerminalGrid::new(3, 8);
        // Force several evictions so there is history to page into.
        for i in 0..10 {
            feed(&mut g, alloc::format!("L{i}\r\n").as_bytes());
        }
        g.scroll_view_up(2);
        for r in 0..3 {
            let abs = g.screen_to_abs(r);
            assert_eq!(g.abs_to_screen(abs), Some(r), "row {r} must round-trip");
        }
    }

    #[test]
    fn total_scrolled_tracks_evictions() {
        let mut g = TerminalGrid::new(3, 8);
        // Seven CRLF-terminated lines push the cursor past the bottom of a
        // 3-row grid five times, so five lines reach history.
        for i in 0..7 {
            feed(&mut g, alloc::format!("L{i}\r\n").as_bytes());
        }
        assert_eq!(g.screen_to_abs(0), 5);
        let mut buf = [0u8; 16];
        let n = {
            let cell_line = g.screen_to_abs(0);
            let mut k = 0;
            for col in 0..8 {
                let cp = g.abs_cell(cell_line, col).glyph();
                if (0x21..=0x7e).contains(&cp) {
                    buf[k] = cp as u8;
                    k += 1;
                }
            }
            k
        };
        assert_eq!(&buf[..n], b"L5");
    }

    #[test]
    fn copy_survives_live_output_scroll() {
        let mut g = TerminalGrid::new(5, 10);
        feed(&mut g, b"AAAA\r\nBBBB\r\nCCCC");
        let mut sel = Selection::NONE;
        drag_select(&g, &mut sel, 0, 0, 0, 4);
        assert_eq!(copied(&g, &sel), b"AAAA");

        // Flood output so AAAA scrolls off the live region into history.
        for i in 0..8 {
            feed(&mut g, alloc::format!("X{i}\r\n").as_bytes());
        }
        assert_eq!(copied(&g, &sel), b"AAAA");
    }

    #[test]
    fn copy_survives_view_scroll() {
        let mut g = TerminalGrid::new(4, 10);
        feed(&mut g, b"FIRST\r\nSECOND\r\nTHIRD");
        let mut sel = Selection::NONE;
        drag_select(&g, &mut sel, 0, 0, 0, 5);
        assert_eq!(copied(&g, &sel), b"FIRST");

        // Push FIRST into history, then page the view up to look at it.
        for i in 0..6 {
            feed(&mut g, alloc::format!("Y{i}\r\n").as_bytes());
        }
        g.scroll_view_up(3);
        assert_eq!(copied(&g, &sel), b"FIRST");
    }

    #[test]
    fn multi_row_selection_joins_and_trims() {
        let mut g = TerminalGrid::new(5, 10);
        feed(&mut g, b"hello\r\nworld");
        let mut sel = Selection::NONE;
        // Col 5 on row 1 is the end of "world".
        drag_select(&g, &mut sel, 0, 0, 1, 5);
        assert_eq!(copied(&g, &sel), b"hello\nworld");
    }

    #[test]
    fn capture_stores_absolute_line() {
        let mut g = TerminalGrid::new(3, 8);
        for i in 0..6 {
            feed(&mut g, alloc::format!("L{i}\r\n").as_bytes());
        }
        let origin = g.screen_to_abs(0);
        let mut sel = Selection::NONE;
        drag_select(&g, &mut sel, 1, 0, 1, 2);
        let (lo, _hi) = sel.ordered().unwrap();
        assert_eq!(lo.line, origin + 1);
    }

    #[test]
    fn evicted_selection_degrades_without_panic() {
        let mut g = TerminalGrid::new(2, 6);
        feed(&mut g, b"keep\r\n");
        let mut sel = Selection::NONE;
        drag_select(&g, &mut sel, 0, 0, 0, 4);
        assert_eq!(copied(&g, &sel), b"keep");
        // Evict far beyond the 1000-line ring so abs 0 is gone.
        for _ in 0..1100 {
            feed(&mut g, b"\r\n");
        }
        assert!(copied(&g, &sel).is_empty());
    }

    #[test]
    fn cell_in_selection_is_half_open() {
        let lo = Anchor { line: 2, col: 3 };
        let hi = Anchor { line: 4, col: 1 };
        assert!(!cell_in_selection(2, 2, lo, hi)); // before lo
        assert!(cell_in_selection(2, 3, lo, hi)); // at lo (inclusive)
        assert!(cell_in_selection(3, 9, lo, hi)); // interior line
        assert!(cell_in_selection(4, 0, lo, hi)); // up to hi.col
        assert!(!cell_in_selection(4, 1, lo, hi)); // at hi (exclusive)
    }
}
