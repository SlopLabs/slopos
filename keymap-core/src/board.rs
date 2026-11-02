//! What a layout types on each key of a keyboard's main block.

use crate::keycode::*;
use crate::layout_table::{CellKind, LayoutTable, NUM_KEYS};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Ansi,
    Iso,
}

/// A key whose job no layout changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fixed {
    Backspace,
    Tab,
    CapsLock,
    Enter,
    Shift,
    Control,
    Super,
    Alt,
    AltGr,
    Menu,
    Space,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// By HID usage.
    Typing(u16),
    Fixed(Fixed),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    pub key: Key,
    /// In quarters of a letter key's width.
    pub quarters: u8,
    /// The lower part of an ISO Enter, joining the key above.
    pub joins_above: bool,
}

/// A dead key shows its accent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Legend {
    pub base: Option<char>,
    pub shift: Option<char>,
    pub alt_gr: Option<char>,
}

impl Legend {
    /// The capital a letter key prints alone.
    pub fn letter(&self) -> Option<char> {
        let base = self.base.filter(|c| c.is_alphabetic())?;
        let shift = self.shift?;
        let mut upper = base.to_uppercase();
        (upper.next() == Some(shift) && upper.next().is_none()).then_some(shift)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Board {
    shape: Shape,
    legends: [Legend; NUM_KEYS],
}

impl Board {
    pub fn of(table: &LayoutTable) -> Board {
        let mut legends = [Legend::default(); NUM_KEYS];
        for (legend, levels) in legends.iter_mut().zip(table.levels.iter()) {
            let show = |level: usize| match levels[level].kind() {
                CellKind::Literal(cp) => char::from_u32(cp),
                CellKind::Dead(idx) => table
                    .dead_accent
                    .get(idx as usize)
                    .filter(|&&a| a != 0)
                    .and_then(|&a| char::from_u32(a)),
                CellKind::None => None,
            };
            *legend = Legend {
                base: show(0),
                shift: show(1),
                alt_gr: show(2),
            };
        }
        let iso_key = legends[KEY_NONUS_BACKSLASH as usize];
        let backslash = legends[KEY_BACKSLASH as usize];
        let empty = iso_key.base.is_none() && iso_key.shift.is_none();
        // An ANSI layout copies backslash onto the ISO key so ISO keyboards type it.
        let copied = iso_key.base == backslash.base && iso_key.shift == backslash.shift;
        let shape = if empty || copied {
            Shape::Ansi
        } else {
            Shape::Iso
        };
        Board { shape, legends }
    }

    pub fn shape(&self) -> Shape {
        self.shape
    }

    pub fn rows(&self) -> [&'static [Place]; 5] {
        rows(self.shape)
    }

    /// `KEY_NONUS_HASH` falls back to `KEY_BACKSLASH`: PS/2 sends one scancode
    /// for both.
    pub fn legend(&self, usage: u16) -> Legend {
        let Some(&own) = self.legends.get(usage as usize) else {
            return Legend::default();
        };
        if usage == KEY_NONUS_HASH && own == Legend::default() {
            return self.legends[KEY_BACKSLASH as usize];
        }
        own
    }
}

pub fn rows(shape: Shape) -> [&'static [Place]; 5] {
    match shape {
        Shape::Ansi => [&DIGITS, &ANSI_TOP, &ANSI_HOME, &ANSI_BOTTOM, &SPACE_ROW],
        Shape::Iso => [&DIGITS, &ISO_TOP, &ISO_HOME, &ISO_BOTTOM, &SPACE_ROW],
    }
}

const fn t(usage: u16) -> Place {
    Place {
        key: Key::Typing(usage),
        quarters: 4,
        joins_above: false,
    }
}

const fn tw(usage: u16, quarters: u8) -> Place {
    Place {
        key: Key::Typing(usage),
        quarters,
        joins_above: false,
    }
}

const fn f(key: Fixed, quarters: u8) -> Place {
    Place {
        key: Key::Fixed(key),
        quarters,
        joins_above: false,
    }
}

static DIGITS: [Place; 14] = [
    t(KEY_GRAVE),
    t(KEY_1),
    t(KEY_2),
    t(KEY_3),
    t(KEY_4),
    t(KEY_5),
    t(KEY_6),
    t(KEY_7),
    t(KEY_8),
    t(KEY_9),
    t(KEY_0),
    t(KEY_MINUS),
    t(KEY_EQUAL),
    f(Fixed::Backspace, 8),
];

static ANSI_TOP: [Place; 14] = [
    f(Fixed::Tab, 6),
    t(KEY_Q),
    t(KEY_W),
    t(KEY_E),
    t(KEY_R),
    t(KEY_T),
    t(KEY_Y),
    t(KEY_U),
    t(KEY_I),
    t(KEY_O),
    t(KEY_P),
    t(KEY_LEFTBRACE),
    t(KEY_RIGHTBRACE),
    tw(KEY_BACKSLASH, 6),
];

static ISO_TOP: [Place; 14] = [
    f(Fixed::Tab, 6),
    t(KEY_Q),
    t(KEY_W),
    t(KEY_E),
    t(KEY_R),
    t(KEY_T),
    t(KEY_Y),
    t(KEY_U),
    t(KEY_I),
    t(KEY_O),
    t(KEY_P),
    t(KEY_LEFTBRACE),
    t(KEY_RIGHTBRACE),
    f(Fixed::Enter, 6),
];

static ANSI_HOME: [Place; 13] = [
    f(Fixed::CapsLock, 7),
    t(KEY_A),
    t(KEY_S),
    t(KEY_D),
    t(KEY_F),
    t(KEY_G),
    t(KEY_H),
    t(KEY_J),
    t(KEY_K),
    t(KEY_L),
    t(KEY_SEMICOLON),
    t(KEY_APOSTROPHE),
    f(Fixed::Enter, 9),
];

static ISO_HOME: [Place; 14] = [
    f(Fixed::CapsLock, 7),
    t(KEY_A),
    t(KEY_S),
    t(KEY_D),
    t(KEY_F),
    t(KEY_G),
    t(KEY_H),
    t(KEY_J),
    t(KEY_K),
    t(KEY_L),
    t(KEY_SEMICOLON),
    t(KEY_APOSTROPHE),
    t(KEY_NONUS_HASH),
    Place {
        key: Key::Fixed(Fixed::Enter),
        quarters: 5,
        joins_above: true,
    },
];

static ANSI_BOTTOM: [Place; 12] = [
    f(Fixed::Shift, 9),
    t(KEY_Z),
    t(KEY_X),
    t(KEY_C),
    t(KEY_V),
    t(KEY_B),
    t(KEY_N),
    t(KEY_M),
    t(KEY_COMMA),
    t(KEY_DOT),
    t(KEY_SLASH),
    f(Fixed::Shift, 11),
];

static ISO_BOTTOM: [Place; 13] = [
    f(Fixed::Shift, 5),
    t(KEY_NONUS_BACKSLASH),
    t(KEY_Z),
    t(KEY_X),
    t(KEY_C),
    t(KEY_V),
    t(KEY_B),
    t(KEY_N),
    t(KEY_M),
    t(KEY_COMMA),
    t(KEY_DOT),
    t(KEY_SLASH),
    f(Fixed::Shift, 11),
];

static SPACE_ROW: [Place; 8] = [
    f(Fixed::Control, 5),
    f(Fixed::Super, 5),
    f(Fixed::Alt, 5),
    f(Fixed::Space, 25),
    f(Fixed::AltGr, 5),
    f(Fixed::Super, 5),
    f(Fixed::Menu, 5),
    f(Fixed::Control, 5),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    fn board(src: &[u8]) -> Board {
        let mut t = LayoutTable::empty();
        parse(src, &mut t).expect("layout should parse");
        Board::of(&t)
    }

    fn de_ch() -> Board {
        board(include_bytes!("../../assets/keymaps/de_CH.layout"))
    }

    #[test]
    fn every_row_is_sixty_quarters_wide() {
        for shape in [Shape::Ansi, Shape::Iso] {
            for row in rows(shape) {
                let width: u32 = row.iter().map(|p| p.quarters as u32).sum();
                assert_eq!(width, 60, "{shape:?}");
            }
        }
    }

    #[test]
    fn each_shipped_layout_has_its_shape() {
        let cases: [(&[u8], Shape); 7] = [
            (
                include_bytes!("../../assets/keymaps/us.layout"),
                Shape::Ansi,
            ),
            (
                include_bytes!("../../assets/keymaps/colemak.layout"),
                Shape::Ansi,
            ),
            (
                include_bytes!("../../assets/keymaps/dvorak.layout"),
                Shape::Ansi,
            ),
            (include_bytes!("../../assets/keymaps/de.layout"), Shape::Iso),
            (
                include_bytes!("../../assets/keymaps/de_CH.layout"),
                Shape::Iso,
            ),
            (include_bytes!("../../assets/keymaps/fr.layout"), Shape::Iso),
            (include_bytes!("../../assets/keymaps/gb.layout"), Shape::Iso),
        ];
        for (src, shape) in cases {
            assert_eq!(board(src).shape(), shape);
        }
    }

    #[test]
    fn de_ch_keys_print_what_they_type() {
        let b = de_ch();
        let y = b.legend(KEY_Y);
        assert_eq!(
            (y.base, y.shift, y.letter()),
            (Some('z'), Some('Z'), Some('Z'))
        );
        let two = b.legend(KEY_2);
        assert_eq!(
            (two.base, two.shift, two.alt_gr),
            (Some('2'), Some('"'), Some('@'))
        );
        assert_eq!(two.letter(), None);
        let equal = b.legend(KEY_EQUAL);
        assert_eq!(
            (equal.base, equal.shift, equal.alt_gr),
            (Some('^'), Some('`'), Some('~'))
        );
    }

    #[test]
    fn the_key_beside_iso_enter_falls_back_to_backslash() {
        let hash = de_ch().legend(KEY_NONUS_HASH);
        assert_eq!((hash.base, hash.shift), (Some('$'), Some('£')));
    }

    #[test]
    fn fr_and_us_keys() {
        let fr = board(include_bytes!("../../assets/keymaps/fr.layout"));
        assert_eq!(fr.legend(KEY_Q).base, Some('a'));
        let us = board(include_bytes!("../../assets/keymaps/us.layout"));
        assert_eq!(us.legend(KEY_1).letter(), None);
        assert_eq!(us.legend(NUM_KEYS as u16), Legend::default());
    }
}
