//! The bytes a VT sends for a named key: xterm's PC-style encoding, the
//! modifiers riding the second parameter as `1 + shift + 2*alt + 4*ctrl`.

use crate::keycode::NamedKey;
use crate::keymap::Mods;

/// The longest sequence, `CSI 2 4 ; 8 ~`.
pub const SEQUENCE_MAX: usize = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sequence {
    bytes: [u8; SEQUENCE_MAX],
    len: u8,
}

impl Sequence {
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    fn push(&mut self, byte: u8) {
        self.bytes[usize::from(self.len)] = byte;
        self.len += 1;
    }

    fn push_num(&mut self, n: u8) {
        if n >= 10 {
            self.push(b'0' + n / 10);
        }
        self.push(b'0' + n % 10);
    }
}

enum Family {
    /// `CSI <final>`, `SS3 <final>` in application cursor mode (DECCKM),
    /// `CSI 1 ; <mod> <final>` when modified.
    Cursor(u8),
    /// `CSI <n> ~`, `CSI <n> ; <mod> ~` when modified.
    Tilde(u8),
    /// F1–F4: `SS3 <final>`, `CSI 1 ; <mod> <final>` when modified.
    Pf(u8),
}

fn family(key: NamedKey) -> Option<Family> {
    Some(match key {
        NamedKey::Up => Family::Cursor(b'A'),
        NamedKey::Down => Family::Cursor(b'B'),
        NamedKey::Right => Family::Cursor(b'C'),
        NamedKey::Left => Family::Cursor(b'D'),
        NamedKey::Home => Family::Cursor(b'H'),
        NamedKey::End => Family::Cursor(b'F'),
        NamedKey::Insert => Family::Tilde(2),
        NamedKey::Delete => Family::Tilde(3),
        NamedKey::PageUp => Family::Tilde(5),
        NamedKey::PageDown => Family::Tilde(6),
        NamedKey::F1 => Family::Pf(b'P'),
        NamedKey::F2 => Family::Pf(b'Q'),
        NamedKey::F3 => Family::Pf(b'R'),
        NamedKey::F4 => Family::Pf(b'S'),
        NamedKey::F5 => Family::Tilde(15),
        NamedKey::F6 => Family::Tilde(17),
        NamedKey::F7 => Family::Tilde(18),
        NamedKey::F8 => Family::Tilde(19),
        NamedKey::F9 => Family::Tilde(20),
        NamedKey::F10 => Family::Tilde(21),
        NamedKey::F11 => Family::Tilde(23),
        NamedKey::F12 => Family::Tilde(24),
        _ => return None,
    })
}

/// xterm's modifier parameter, `1 + shift + 2*alt + 4*ctrl`.
pub fn modifier_param(mods: Mods) -> u8 {
    1 + u8::from(mods.shift) + 2 * u8::from(mods.alt_chord()) + 4 * u8::from(mods.ctrl)
}

/// What `key` sends under `mods`, `app_cursor` being DECCKM; `None` for a key
/// that sends nothing.
pub fn sequence(key: NamedKey, mods: Mods, app_cursor: bool) -> Option<Sequence> {
    let family = family(key)?;
    let param = modifier_param(mods);
    let mut seq = Sequence {
        bytes: [0; SEQUENCE_MAX],
        len: 0,
    };
    seq.push(0x1B);
    match family {
        Family::Cursor(last) | Family::Pf(last) if param != 1 => {
            seq.push(b'[');
            seq.push(b'1');
            seq.push(b';');
            seq.push_num(param);
            seq.push(last);
        }
        Family::Cursor(last) => {
            seq.push(if app_cursor { b'O' } else { b'[' });
            seq.push(last);
        }
        Family::Pf(last) => {
            seq.push(b'O');
            seq.push(last);
        }
        Family::Tilde(n) => {
            seq.push(b'[');
            seq.push_num(n);
            if param != 1 {
                seq.push(b';');
                seq.push_num(param);
            }
            seq.push(b'~');
        }
    }
    Some(seq)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: Mods = Mods {
        shift: false,
        ctrl: false,
        alt: false,
        meta: false,
        altgr: false,
    };
    const SHIFT: Mods = Mods {
        shift: true,
        ..PLAIN
    };
    const CTRL: Mods = Mods {
        ctrl: true,
        ..PLAIN
    };
    const ALT: Mods = Mods { alt: true, ..PLAIN };
    const ALTGR: Mods = Mods {
        alt: true,
        altgr: true,
        ..PLAIN
    };

    fn sends(key: NamedKey, mods: Mods, bytes: &[u8]) {
        let seq = sequence(key, mods, false).expect("a key that sends a sequence");
        assert_eq!(seq.as_bytes(), bytes, "{key:?}");
    }

    #[test]
    fn navigation_keys_send_csi() {
        sends(NamedKey::Up, PLAIN, b"\x1b[A");
        sends(NamedKey::Down, PLAIN, b"\x1b[B");
        sends(NamedKey::Right, PLAIN, b"\x1b[C");
        sends(NamedKey::Left, PLAIN, b"\x1b[D");
        sends(NamedKey::Home, PLAIN, b"\x1b[H");
        sends(NamedKey::End, PLAIN, b"\x1b[F");
        sends(NamedKey::Insert, PLAIN, b"\x1b[2~");
        sends(NamedKey::Delete, PLAIN, b"\x1b[3~");
        sends(NamedKey::PageUp, PLAIN, b"\x1b[5~");
        sends(NamedKey::PageDown, PLAIN, b"\x1b[6~");
    }

    #[test]
    fn application_cursor_mode_moves_cursor_keys_to_ss3() {
        let ss3 = |key| sequence(key, PLAIN, true).unwrap();
        assert_eq!(ss3(NamedKey::Up).as_bytes(), b"\x1bOA");
        assert_eq!(ss3(NamedKey::Home).as_bytes(), b"\x1bOH");
        assert_eq!(ss3(NamedKey::PageUp).as_bytes(), b"\x1b[5~");
        assert_eq!(
            sequence(NamedKey::Up, CTRL, true).unwrap().as_bytes(),
            b"\x1b[1;5A"
        );
    }

    #[test]
    fn function_keys_use_the_pc_style_encoding() {
        sends(NamedKey::F1, PLAIN, b"\x1bOP");
        sends(NamedKey::F4, PLAIN, b"\x1bOS");
        sends(NamedKey::F5, PLAIN, b"\x1b[15~");
        sends(NamedKey::F6, PLAIN, b"\x1b[17~");
        sends(NamedKey::F10, PLAIN, b"\x1b[21~");
        sends(NamedKey::F11, PLAIN, b"\x1b[23~");
        sends(NamedKey::F12, PLAIN, b"\x1b[24~");
    }

    #[test]
    fn modifiers_ride_the_second_parameter() {
        sends(NamedKey::Left, CTRL, b"\x1b[1;5D");
        sends(NamedKey::Right, SHIFT, b"\x1b[1;2C");
        sends(NamedKey::Home, ALT, b"\x1b[1;3H");
        sends(NamedKey::Delete, CTRL, b"\x1b[3;5~");
        sends(NamedKey::F1, CTRL, b"\x1b[1;5P");
        sends(NamedKey::F5, SHIFT, b"\x1b[15;2~");
        let all = Mods {
            shift: true,
            ctrl: true,
            alt: true,
            ..PLAIN
        };
        sends(NamedKey::F12, all, b"\x1b[24;8~");
    }

    #[test]
    fn altgr_and_super_make_no_chord() {
        sends(NamedKey::Left, ALTGR, b"\x1b[D");
        sends(
            NamedKey::Up,
            Mods {
                meta: true,
                ..PLAIN
            },
            b"\x1b[A",
        );
    }

    #[test]
    fn keys_without_a_sequence_send_nothing() {
        for key in [
            NamedKey::CapsLock,
            NamedKey::NumLock,
            NamedKey::ScrollLock,
            NamedKey::PrintScreen,
            NamedKey::Pause,
            NamedKey::Menu,
        ] {
            assert_eq!(sequence(key, PLAIN, false), None, "{key:?}");
        }
    }
}
