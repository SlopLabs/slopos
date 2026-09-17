//! Input translation: windowing events → widget events.
//!
//! The kernel is the single keyboard-layout authority, so text comes from the
//! codepoint the event already carries; only layout-independent action keys go
//! through `keymap-core`.

use super::event::{Key, Modifiers, WidgetEvent};
use slopos_keymap_core::{UiKey, mods_locks_from_raw, ui_classify};
use slopos_windowing::Event;

/// Classify a windowing key event into an appkit [`Key`].
///
/// A freshly-pressed dead key carries `codepoint == 0` and yields
/// [`Key::Unknown`]. Ctrl+letter control codes (0x01..=0x1A) are inverted back
/// to the letter so shortcuts follow the active layout — on QWERTZ, Ctrl on the
/// key labelled Z is Ctrl+Z.
fn classify_key(keycode: u16, codepoint: u32, modifiers: u8) -> Key {
    let (mods, locks) = mods_locks_from_raw(modifiers);

    if let UiKey::Named(nk) = ui_classify(keycode, mods, locks) {
        return Key::Named(nk);
    }
    if let Some(c) = char::from_u32(codepoint) {
        if !c.is_control() {
            return Key::Char(c);
        }
    }
    if mods.ctrl {
        if (0x01..=0x1A).contains(&codepoint) {
            let c = (b'a' + (codepoint as u8 - 1)) as char;
            return Key::Char(if mods.shift {
                c.to_ascii_uppercase()
            } else {
                c
            });
        }
        if let UiKey::Char(c) = ui_classify(keycode, mods, locks) {
            return Key::Char(c);
        }
    }
    Key::Unknown
}

pub fn translate_event(event: &Event) -> Option<WidgetEvent> {
    match event {
        Event::PointerMotion { x, y } => Some(WidgetEvent::PointerMove { x: *x, y: *y }),
        Event::PointerPress { button } => {
            let btn = pointer_button(*button);
            // Position and modifiers are filled in by the framework from the
            // pointer and keyboard state it tracks.
            Some(WidgetEvent::PointerDown {
                x: 0,
                y: 0,
                button: btn,
                modifiers: Modifiers::default(),
            })
        }
        Event::PointerRelease { button } => {
            let btn = pointer_button(*button);
            Some(WidgetEvent::PointerUp {
                x: 0,
                y: 0,
                button: btn,
            })
        }
        Event::PointerAxis { value_v120, .. } => {
            let delta_lines = *value_v120 / 120;
            let delta_px = delta_lines * 20; // ~line_height
            Some(WidgetEvent::Scroll {
                // Filled in by `fill_pointer_state`, which is the only place
                // that knows where the pointer is.
                x: 0,
                y: 0,
                delta_x: 0,
                delta_y: delta_px,
            })
        }
        Event::KeyPress {
            keycode,
            codepoint,
            modifiers,
            ..
        } => {
            let mods = Modifiers::from_raw(*modifiers);
            let key = classify_key(*keycode, *codepoint, *modifiers);
            match key {
                // AltGr-composed characters are text; Ctrl and plain Alt are shortcuts.
                Key::Char(c) if !mods.ctrl && !mods.plain_alt() => {
                    Some(WidgetEvent::TextInput { character: c })
                }
                _ => Some(WidgetEvent::KeyDown {
                    key,
                    modifiers: mods,
                    repeat: false,
                }),
            }
        }
        Event::KeyRelease {
            keycode,
            codepoint,
            modifiers,
            ..
        } => {
            let mods = Modifiers::from_raw(*modifiers);
            let key = classify_key(*keycode, *codepoint, *modifiers);
            Some(WidgetEvent::KeyUp {
                key,
                modifiers: mods,
            })
        }
        Event::Configure { width, height } => Some(WidgetEvent::Configure {
            width: *width,
            height: *height,
        }),
        // Handled at the appkit level, before translation.
        Event::CloseRequest | Event::ClipboardOffer { .. } | Event::ClipboardData { .. } => None,
        Event::Other => None,
    }
}

/// The wire carries the PS/2 button **mask** (left 0x01, right 0x02, middle
/// 0x04), not an index into them, so middle arrived as left and nothing ever
/// decoded as middle.
fn pointer_button(button: u8) -> super::event::PointerButton {
    match button {
        0x02 => super::event::PointerButton::Right,
        0x04 => super::event::PointerButton::Middle,
        _ => super::event::PointerButton::Left,
    }
}
