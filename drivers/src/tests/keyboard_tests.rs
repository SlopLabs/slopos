//! Keyboard integration tests: synthetic set-1 scancodes into the sink
//! holder's queue as `(keycode, codepoint, modifiers)`, and into the console
//! TTY as the bytes a VT sends while nothing holds the sink.

use slopos_abi::input::{InputEventType, KEY_FLAG_FROM_KEYPAD, MODIFIER_SHIFT};
use slopos_keymap_core::keycode;
use slopos_testing::{TestResult, fail, pass};

use crate::input_event::{
    input_cleanup_task, input_compositor_task_id, input_poll, input_register_compositor,
};
use crate::ps2::keyboard::{handle_scancode, reset_state_for_test};

const DUMMY_TASK: u32 = 9001;

/// Makes [`DUMMY_TASK`] the sink's holder; what held it before, for
/// [`teardown`].
fn setup() -> u32 {
    reset_state_for_test();
    let restore = input_compositor_task_id();
    input_cleanup_task(DUMMY_TASK);
    input_register_compositor(DUMMY_TASK);
    while input_poll(DUMMY_TASK).is_some() {}
    restore
}

fn teardown(restore: u32) {
    input_cleanup_task(DUMMY_TASK);
    input_register_compositor(restore);
    reset_state_for_test();
}

/// KP7 with Num Lock on (the default) produces the digit '7'.
pub fn test_keypad_numlock_digit() -> TestResult {
    let restore = setup();
    handle_scancode(0x47); // KP7 make code
    let ev = input_poll(DUMMY_TASK);
    teardown(restore);

    let ev = match ev {
        Some(e) => e,
        None => return fail!("keypad 7 produced no event"),
    };
    if ev.key_keycode() != keycode::KEY_KP_7 {
        return fail!("keypad 7 decoded to the wrong keycode");
    }
    if ev.key_codepoint() != b'7' as u32 {
        return fail!("keypad 7 did not produce digit '7' with Num Lock on");
    }
    if ev.key_flags() & KEY_FLAG_FROM_KEYPAD == 0 {
        return fail!("keypad 7 missing FROM_KEYPAD flag");
    }
    pass!()
}

pub fn test_letter_canonical_payload() -> TestResult {
    let restore = setup();
    handle_scancode(0x1E); // 'a' make code
    let ev = input_poll(DUMMY_TASK);
    teardown(restore);

    let ev = match ev {
        Some(e) => e,
        None => return fail!("letter 'a' produced no event"),
    };
    if ev.key_keycode() != keycode::KEY_A {
        return fail!("letter 'a' wrong keycode");
    }
    if ev.key_codepoint() != b'a' as u32 {
        return fail!("letter 'a' wrong codepoint");
    }
    if ev.key_flags() != 0 {
        return fail!("letter 'a' carries flags {:#x}", ev.key_flags());
    }
    pass!()
}

pub fn test_shift_letter_uppercase_and_modifier() -> TestResult {
    let restore = setup();
    handle_scancode(0x2A); // Left Shift press
    handle_scancode(0x1E); // 'a' -> 'A'
    let _shift_ev = input_poll(DUMMY_TASK);
    let ev = input_poll(DUMMY_TASK);
    teardown(restore);

    let ev = match ev {
        Some(e) => e,
        None => return fail!("shift+'a' produced no letter event"),
    };
    if ev.key_keycode() != keycode::KEY_A {
        return fail!("shift+'a' wrong keycode");
    }
    if ev.key_codepoint() != b'A' as u32 {
        return fail!("shift+'a' did not produce uppercase 'A'");
    }
    if ev.key_modifiers() & MODIFIER_SHIFT == 0 {
        return fail!("shift+'a' missing Shift in modifier snapshot");
    }
    pass!()
}

/// An extended arrow is its keycode and no text.
pub fn test_extended_arrow_canonical_payload() -> TestResult {
    let restore = setup();
    handle_scancode(0xE0); // extended prefix
    handle_scancode(0x48); // Up arrow
    let ev = input_poll(DUMMY_TASK);
    teardown(restore);

    let ev = match ev {
        Some(e) => e,
        None => return fail!("Up arrow produced no event"),
    };
    if ev.key_keycode() != keycode::KEY_UP {
        return fail!("Up arrow wrong keycode");
    }
    if ev.key_codepoint() != 0 {
        return fail!("Up arrow carries text {:#x}", ev.key_codepoint());
    }
    pass!()
}

pub fn test_press_then_release_events() -> TestResult {
    let restore = setup();
    handle_scancode(0x1E); // 'a' press
    handle_scancode(0x9E); // 'a' release
    let press = input_poll(DUMMY_TASK);
    let release = input_poll(DUMMY_TASK);
    teardown(restore);

    let press = match press {
        Some(e) => e,
        None => return fail!("missing press event"),
    };
    if press.event_type != InputEventType::KeyPress || press.key_codepoint() != b'a' as u32 {
        return fail!("press event malformed");
    }
    let release = match release {
        Some(e) => e,
        None => return fail!("missing release event"),
    };
    if release.event_type != InputEventType::KeyRelease {
        return fail!("expected KeyRelease");
    }
    if release.key_keycode() != keycode::KEY_A {
        return fail!("release missing keycode");
    }
    if release.key_codepoint() != 0 {
        return fail!("release should carry no text");
    }
    pass!()
}

/// While nothing holds the sink, a named key reaches the console as the
/// escape sequence a VT sends, its modifiers in the second parameter.
pub fn test_named_keys_reach_the_console_as_vt_sequences() -> TestResult {
    use crate::tty::{self, TtyIndex};
    use slopos_abi::syscall::LocalFlags;

    const CONSOLE: TtyIndex = TtyIndex(0);
    const PRESSES: &[&[u8]] = &[
        &[0xE0, 0x48, 0xE0, 0xC8],             // Up
        &[0xE0, 0x53, 0xE0, 0xD3],             // Delete
        &[0xE0, 0x51, 0xE0, 0xD1],             // Page Down
        &[0x1D, 0xE0, 0x4B, 0xE0, 0xCB, 0x9D], // Ctrl+Left
        &[0x3F, 0xBF],                         // F5
        &[0x1E, 0x9E],                         // 'a'
    ];
    const SENT: &[u8] = b"\x1b[A\x1b[3~\x1b[6~\x1b[1;5D\x1b[15~a";

    reset_state_for_test();
    tty::table::tty_table_init();
    let restore = input_compositor_task_id();
    input_register_compositor(0);
    let active = tty::active_tty();
    tty::set_active_tty(CONSOLE);
    let Ok(saved) = tty::get_termios(CONSOLE) else {
        tty::set_active_tty(active);
        input_register_compositor(restore);
        return fail!("the console has no termios");
    };
    let mut raw = saved;
    raw.c_lflag &= !(LocalFlags::ICANON | LocalFlags::ECHO);
    let _ = tty::set_termios(CONSOLE, &raw);
    let mut scratch = [0u8; 64];
    while matches!(tty::read(CONSOLE, &mut scratch, true), Ok(n) if n > 0) {}

    for press in PRESSES {
        for &byte in *press {
            handle_scancode(byte);
        }
    }
    let mut got = [0u8; 64];
    let mut len = 0;
    while let Ok(n) = tty::read(CONSOLE, &mut got[len..], true) {
        if n == 0 || len + n == got.len() {
            len += n;
            break;
        }
        len += n;
    }

    let _ = tty::set_termios(CONSOLE, &saved);
    tty::set_active_tty(active);
    input_register_compositor(restore);
    reset_state_for_test();

    if &got[..len] != SENT {
        return fail!("the console read {:x?}, want {:x?}", &got[..len], SENT);
    }
    pass!()
}

/// `handle_scancode` resolves against the layout loaded at runtime, not the
/// hardcoded default.
pub fn test_runtime_layout_swap() -> TestResult {
    use crate::keyboard::set_layout;
    use slopos_keymap_core::{LayoutTable, parse};
    use slopos_ostd::KBox;

    const DE_CH: &str = include_str!("../../../assets/keymaps/de_CH.layout");

    let restore = setup();

    let mut table = match KBox::<LayoutTable>::zeroed() {
        Ok(t) => t,
        Err(_) => {
            teardown(restore);
            return fail!("layout alloc failed");
        }
    };
    if parse(DE_CH.as_bytes(), &mut table).is_err() {
        teardown(restore);
        return fail!("de_CH layout failed to parse");
    }
    set_layout(table);

    // Physical-Y key (set-1 make 0x15) → 'z' under QWERTZ.
    handle_scancode(0x15);
    let z_ev = input_poll(DUMMY_TASK);

    // AltGr (right Alt = E0 38) + '2' (set-1 0x03) → '@'.
    handle_scancode(0xE0);
    handle_scancode(0x38);
    let _altgr_press = input_poll(DUMMY_TASK);
    handle_scancode(0x03);
    let at_ev = input_poll(DUMMY_TASK);
    handle_scancode(0xE0);
    handle_scancode(0xB8); // right Alt release

    teardown(restore);

    let z = match z_ev {
        Some(e) => e,
        None => return fail!("de_CH physical-Y produced no event"),
    };
    if z.key_codepoint() != 'z' as u32 {
        return fail!("de_CH physical-Y did not produce 'z' (QWERTZ swap)");
    }
    let at = match at_ev {
        Some(e) => e,
        None => return fail!("de_CH AltGr+2 produced no event"),
    };
    if at.key_codepoint() != '@' as u32 {
        return fail!("de_CH AltGr+2 did not produce '@'");
    }
    pass!()
}

slopos_testing::stest!(name = test_runtime_layout_swap, suite = keyboard);
slopos_testing::stest!(name = test_keypad_numlock_digit, suite = keyboard);
slopos_testing::stest!(name = test_letter_canonical_payload, suite = keyboard);
slopos_testing::stest!(
    name = test_shift_letter_uppercase_and_modifier,
    suite = keyboard
);
slopos_testing::stest!(
    name = test_extended_arrow_canonical_payload,
    suite = keyboard
);
slopos_testing::stest!(name = test_press_then_release_events, suite = keyboard);
slopos_testing::stest!(
    name = test_named_keys_reach_the_console_as_vt_sequences,
    suite = keyboard
);
