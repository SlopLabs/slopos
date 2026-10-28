//! Windowing events as widget events. The kernel is the one keyboard-layout
//! authority, so text is the codepoint an event carries.

use slopos_abi::input::POINTER_AXIS_HORIZONTAL;

use super::event::{Key, Modifiers, WidgetEvent};
use slopos_keymap_core::{UiKey, mods_locks_from_raw, ui_classify};
use slopos_windowing::Event;

/// A dead key just pressed carries no codepoint and is [`Key::Unknown`].
/// Ctrl+letter's control code goes back to its letter, so a shortcut follows
/// the layout: on QWERTZ, Ctrl on the key labelled Z is Ctrl+Z.
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
        // The run loop fills in where the pointer is, and the modifiers.
        Event::PointerPress { button } => Some(WidgetEvent::PointerDown {
            x: 0,
            y: 0,
            button: pointer_button(*button),
            modifiers: Modifiers::default(),
        }),
        Event::PointerRelease { button } => Some(WidgetEvent::PointerUp {
            x: 0,
            y: 0,
            button: pointer_button(*button),
        }),
        Event::PointerAxis { axis, value_v120 } => {
            let px = wheel_px(*value_v120);
            let (delta_x, delta_y) = match *axis {
                POINTER_AXIS_HORIZONTAL => (px, 0),
                _ => (0, px),
            };
            Some(WidgetEvent::Scroll {
                x: 0,
                y: 0,
                delta_x,
                delta_y,
            })
        }
        Event::KeyPress {
            keycode,
            codepoint,
            modifiers,
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
                }),
            }
        }
        Event::KeyRelease {
            keycode,
            codepoint,
            modifiers,
        } => {
            let mods = Modifiers::from_raw(*modifiers);
            let key = classify_key(*keycode, *codepoint, *modifiers);
            Some(WidgetEvent::KeyUp {
                key,
                modifiers: mods,
            })
        }
        Event::PointerLeave
        | Event::Configure { .. }
        | Event::CloseRequest
        | Event::ClipboardOffer { .. }
        | Event::ClipboardData { .. } => None,
    }
}

/// The wire carries the PS/2 button mask: left 0x01, right 0x02, middle 0x04.
fn pointer_button(button: u8) -> super::event::PointerButton {
    match button {
        0x02 => super::event::PointerButton::Right,
        0x04 => super::event::PointerButton::Middle,
        _ => super::event::PointerButton::Left,
    }
}

/// How far a turn of the wheel, in 120ths of a notch, scrolls: a line a
/// notch.
pub(crate) fn wheel_px(value_v120: i32) -> i32 {
    value_v120 * slopos_theme_core::TextRole::Body.style().line_height / 120
}
