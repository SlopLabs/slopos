//! Keys as the compositor's own surfaces read them, and the presses those
//! surfaces took, whose repeats and releases no client may see.

use alloc::vec::Vec;

use slopos_abi::InputEvent;
use slopos_abi::input::MODIFIER_SHIFT;
use slopos_abi::input::keycode::{
    KEY_BACKSPACE, KEY_DOWN, KEY_ENTER, KEY_ESC, KEY_KP_ENTER, KEY_LEFT, KEY_RIGHT, KEY_TAB, KEY_UP,
};

/// A key as the compositor's own surfaces read it: Tab and Shift+Tab move
/// focus, Left and Right move along a row of buttons, Up and Down choose in
/// a list, Enter presses, Escape dismisses, Backspace erases. Space is text,
/// which presses a focused button or list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Move { back: bool },
    Arrow { right: bool },
    Choose { next: bool },
    Press,
    Dismiss,
    Erase,
    Text(char),
}

impl Key {
    /// The key `event` presses; a held key repeats only as Backspace.
    pub fn read(event: &InputEvent) -> Option<Key> {
        let keycode = event.key_keycode();
        if event.key_is_repeat() && keycode != KEY_BACKSPACE {
            return None;
        }
        Some(match keycode {
            KEY_TAB => Key::Move {
                back: event.key_modifiers() & MODIFIER_SHIFT != 0,
            },
            KEY_LEFT => Key::Arrow { right: false },
            KEY_RIGHT => Key::Arrow { right: true },
            KEY_UP => Key::Choose { next: false },
            KEY_DOWN => Key::Choose { next: true },
            KEY_ENTER | KEY_KP_ENTER => Key::Press,
            KEY_ESC => Key::Dismiss,
            KEY_BACKSPACE => Key::Erase,
            _ => Key::Text(text(event)?),
        })
    }
}

/// The text `event` types, from its canonical codepoint alone: the legacy
/// byte `key_codepoint` falls back to is the console's code for a navigation
/// key.
pub fn text(event: &InputEvent) -> Option<char> {
    char::from_u32(event.data.data1 >> 16).filter(|&ch| ch != '\0')
}

/// More keys than a hand holds down at once.
const TAKEN_MAX: usize = 16;

/// Keys pressed into one of the compositor's own surfaces, whose repeats and
/// releases no client may see.
#[derive(Default, Debug)]
pub struct Taken(Vec<u16>);

impl Taken {
    /// `event`, a press the surface took. A repeat is not kept: its key went
    /// down for whoever had the keyboard then, who must see it go up.
    pub fn take(&mut self, event: &InputEvent) {
        let keycode = event.key_keycode();
        if event.key_is_repeat() || self.0.contains(&keycode) {
            return;
        }
        if self.0.len() >= TAKEN_MAX {
            self.0.remove(0);
        }
        self.0.push(keycode);
    }

    /// Whether `event`, a press the surface does not take, repeats a key it
    /// took. A fresh press means the release was missed, so the key is
    /// forgotten.
    pub fn repeats(&mut self, event: &InputEvent) -> bool {
        let keycode = event.key_keycode();
        let held = self.0.contains(&keycode);
        if held && !event.key_is_repeat() {
            self.0.retain(|&k| k != keycode);
        }
        held && event.key_is_repeat()
    }

    /// Whether the release of `keycode` ends a press the surface took.
    pub fn release(&mut self, keycode: u16) -> bool {
        let Some(at) = self.0.iter().position(|&k| k == keycode) else {
            return false;
        };
        self.0.remove(at);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slopos_abi::InputEventType;
    use slopos_abi::input::keycode::{
        KEY_A, KEY_DELETE, KEY_END, KEY_HOME, KEY_PAGEDOWN, KEY_PAGEUP,
    };
    use slopos_abi::input::{KEY_FLAG_HAS_CANONICAL, KEY_FLAG_IS_REPEAT};

    fn press(keycode: u16, codepoint: char, modifiers: u8, repeat: bool) -> InputEvent {
        let flags = match repeat {
            true => KEY_FLAG_IS_REPEAT,
            false => 0,
        };
        InputEvent::key_full(
            InputEventType::KeyPress,
            0,
            0,
            keycode,
            codepoint as u32,
            modifiers,
            flags,
            0,
        )
    }

    #[test]
    fn each_key_reads_as_what_a_form_does_with_it() {
        let read = |keycode, ch, modifiers| Key::read(&press(keycode, ch, modifiers, false));
        assert_eq!(read(KEY_TAB, '\t', 0), Some(Key::Move { back: false }));
        assert_eq!(
            read(KEY_TAB, '\t', MODIFIER_SHIFT),
            Some(Key::Move { back: true })
        );
        assert_eq!(read(KEY_LEFT, '\0', 0), Some(Key::Arrow { right: false }));
        assert_eq!(read(KEY_RIGHT, '\0', 0), Some(Key::Arrow { right: true }));
        assert_eq!(read(KEY_UP, '\0', 0), Some(Key::Choose { next: false }));
        assert_eq!(read(KEY_DOWN, '\0', 0), Some(Key::Choose { next: true }));
        assert_eq!(read(KEY_KP_ENTER, '\n', 0), Some(Key::Press));
        assert_eq!(read(KEY_ESC, '\0', 0), Some(Key::Dismiss));
        assert_eq!(read(KEY_A, 'a', 0), Some(Key::Text('a')));
        assert_eq!(read(KEY_A, '\0', 0), None);
    }

    #[test]
    fn a_navigation_key_is_no_text() {
        for (keycode, legacy) in [
            (KEY_PAGEUP, 0x80),
            (KEY_PAGEDOWN, 0x81),
            (KEY_HOME, 0x86),
            (KEY_END, 0x87),
            (KEY_DELETE, 0x88),
        ] {
            let event = InputEvent::key_full(
                InputEventType::KeyPress,
                0,
                legacy,
                keycode,
                0,
                0,
                KEY_FLAG_HAS_CANONICAL,
                0,
            );
            assert_eq!(text(&event), None);
            assert_eq!(Key::read(&event), None);
        }
    }

    #[test]
    fn a_held_key_repeats_only_as_backspace() {
        assert_eq!(Key::read(&press(KEY_A, 'a', 0, true)), None);
        assert_eq!(Key::read(&press(KEY_ENTER, '\n', 0, true)), None);
        assert_eq!(
            Key::read(&press(KEY_BACKSPACE, '\0', 0, true)),
            Some(Key::Erase)
        );
    }

    #[test]
    fn a_key_taken_keeps_its_repeats_and_release_from_clients() {
        let mut taken = Taken::default();
        taken.take(&press(KEY_ENTER, '\n', 0, false));
        assert!(taken.repeats(&press(KEY_ENTER, '\n', 0, true)));
        assert!(taken.release(KEY_ENTER));
        assert!(!taken.release(KEY_ENTER));
        assert!(!taken.repeats(&press(KEY_ENTER, '\n', 0, true)));
    }

    #[test]
    fn a_key_held_down_for_a_client_is_released_to_it() {
        let mut taken = Taken::default();
        taken.take(&press(KEY_A, 'a', 0, true));
        assert!(!taken.release(KEY_A));
    }

    #[test]
    fn a_fresh_press_forgets_a_release_that_was_missed() {
        let mut taken = Taken::default();
        taken.take(&press(KEY_A, 'a', 0, false));
        assert!(!taken.repeats(&press(KEY_A, 'a', 0, false)));
        assert!(!taken.release(KEY_A));
    }

    #[test]
    fn no_more_keys_are_kept_than_a_hand_holds() {
        let mut taken = Taken::default();
        for keycode in 0..TAKEN_MAX as u16 + 4 {
            taken.take(&press(KEY_A + keycode, 'a', 0, false));
        }
        assert!(!taken.release(KEY_A));
        assert!(taken.release(KEY_A + TAKEN_MAX as u16 + 3));
    }
}
