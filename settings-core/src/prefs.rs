//! Each person's preferences, which `/bin/prefs` and Settings write and the
//! desktop reads: a few typed keys, each with a default, kept as `key =
//! value` lines in [`path`] for only those set otherwise. Reading never
//! fails: a line it does not understand is passed over and named, so a file
//! a person edited by hand costs at most that line.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use slopos_theme_core::GRADIENTS;

use crate::background::{Background, Fit};

/// Below a person's home, where every reader looks, init among them, which
/// has no person's environment to find another configuration directory in.
pub const FILE: &str = ".config/slopos/prefs";

/// The longest path a picture may have.
pub const PATH_MAX: usize = 4096;

/// The most a preferences file is read of.
pub const FILE_MAX: usize = 64 * 1024;

pub fn path(home: &str) -> String {
    format!("{}/{FILE}", home.trim_end_matches('/'))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    BackgroundGradient,
    BackgroundPicture,
    BackgroundFit,
    ClockFormat,
    KeyboardLayout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Choice(&'static [&'static str]),
    /// An absolute path, or nothing.
    Path,
    /// A layout's name in `/usr/share/keymaps`, or nothing.
    Layout,
}

const FITS: [&str; 5] = ["fill", "fit", "center", "stretch", "tile"];
const CLOCKS: [&str; 2] = ["24h", "12h"];

impl Key {
    pub const ALL: [Key; 5] = [
        Key::BackgroundGradient,
        Key::BackgroundPicture,
        Key::BackgroundFit,
        Key::ClockFormat,
        Key::KeyboardLayout,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Key::BackgroundGradient => "background.gradient",
            Key::BackgroundPicture => "background.picture",
            Key::BackgroundFit => "background.fit",
            Key::ClockFormat => "clock.format",
            Key::KeyboardLayout => "keyboard.layout",
        }
    }

    pub fn named(name: &str) -> Option<Key> {
        Key::ALL.into_iter().find(|key| key.name() == name)
    }

    pub fn kind(self) -> Kind {
        match self {
            Key::BackgroundGradient => Kind::Choice(&slopos_theme_core::GRADIENT_IDS),
            Key::BackgroundPicture => Kind::Path,
            Key::BackgroundFit => Kind::Choice(&FITS),
            Key::ClockFormat => Kind::Choice(&CLOCKS),
            Key::KeyboardLayout => Kind::Layout,
        }
    }

    pub fn default_value(self) -> &'static str {
        match self {
            Key::BackgroundGradient => GRADIENTS[0].id,
            Key::BackgroundPicture | Key::KeyboardLayout => "",
            Key::BackgroundFit => FITS[0],
            Key::ClockFormat => CLOCKS[0],
        }
    }

    pub const fn summary(self) -> &'static str {
        match self {
            Key::BackgroundGradient => {
                "The gradient behind the desktop, which shows around a picture that does not cover the screen."
            }
            Key::BackgroundPicture => {
                "The picture on the desktop, a PNG file named by its whole path; nothing leaves the gradient alone."
            }
            Key::BackgroundFit => {
                "How the picture meets the screen: fill it, fit inside it, sit at its center, stretch to it or tile it."
            }
            Key::ClockFormat => "Whether the clock counts 24 hours or 12 hours with AM and PM.",
            Key::KeyboardLayout => {
                "The keyboard layout while your desktop is in front; nothing uses the computer's."
            }
        }
    }

    const fn index(self) -> usize {
        match self {
            Key::BackgroundGradient => 0,
            Key::BackgroundPicture => 1,
            Key::BackgroundFit => 2,
            Key::ClockFormat => 3,
            Key::KeyboardLayout => 4,
        }
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Why a value is not one a key takes, as a sentence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    NoSuchKey,
    NotAChoice(&'static [&'static str]),
    NotAbsolute,
    TooLong,
    LineBreak,
    NotALayout,
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::NoSuchKey => f.write_str("There is no such preference; `prefs` lists them."),
            Refused::NotAChoice(choices) => {
                write!(f, "The value is one of {}.", choices.join(", "))
            }
            Refused::NotAbsolute => {
                f.write_str("A picture is named by its whole path, starting with a slash.")
            }
            Refused::TooLong => f.write_str("That path is longer than any a picture may have."),
            Refused::LineBreak => f.write_str("A picture's path can't hold a line break."),
            Refused::NotALayout => {
                f.write_str("A layout is named as in the list of layouts, such as de_CH.")
            }
        }
    }
}

/// Whether `value` is one `key` takes.
pub fn check(key: Key, value: &str) -> Result<(), Refused> {
    match key.kind() {
        Kind::Choice(choices) => match choices.contains(&value) {
            true => Ok(()),
            false => Err(Refused::NotAChoice(choices)),
        },
        Kind::Path if value.is_empty() => Ok(()),
        Kind::Path if !value.starts_with('/') => Err(Refused::NotAbsolute),
        Kind::Path if value.contains(['\n', '\r', '\0']) => Err(Refused::LineBreak),
        Kind::Path if value.len() > PATH_MAX => Err(Refused::TooLong),
        Kind::Path => Ok(()),
        Kind::Layout if value.is_empty() || layout_name(value) => Ok(()),
        Kind::Layout => Err(Refused::NotALayout),
    }
}

/// A name `/usr/share/keymaps/<name>.layout` may have, as the kernel stores
/// one.
pub fn layout_name(name: &str) -> bool {
    (1..16).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// A line [`Prefs::parse`] passed over, counted from 1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    pub line: usize,
    pub why: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Prefs {
    set: [Option<String>; Key::ALL.len()],
}

const HEADER: &str = "\
# Your preferences, which Settings and `prefs` change. `prefs describe`
# says what each one does and the values it takes.
";

impl Prefs {
    pub fn parse(text: &str) -> (Prefs, Vec<Skipped>) {
        let mut prefs = Prefs::default();
        let mut skipped = Vec::new();
        for (at, line) in text.lines().enumerate() {
            let line_no = at + 1;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut skip = |why: String| {
                skipped.push(Skipped { line: line_no, why });
            };
            let Some((name, value)) = line.split_once('=') else {
                skip(String::from(
                    "A line is a name, an equals sign and a value.",
                ));
                continue;
            };
            let (name, value) = (name.trim(), value.trim());
            let Some(key) = Key::named(name) else {
                skip(format!("{name}: {}", Refused::NoSuchKey));
                continue;
            };
            match prefs.set(key, value) {
                Ok(()) => {}
                Err(why) => skip(format!("{name}: {why}")),
            }
        }
        (prefs, skipped)
    }

    pub fn render(&self) -> String {
        let mut out = String::from(HEADER);
        for key in Key::ALL {
            if let Some(value) = &self.set[key.index()] {
                out.push_str(&format!("{} = {value}\n", key.name()));
            }
        }
        out
    }

    pub fn get(&self, key: Key) -> &str {
        self.set[key.index()]
            .as_deref()
            .unwrap_or(key.default_value())
    }

    pub fn is_set(&self, key: Key) -> bool {
        self.set[key.index()].is_some()
    }

    /// The default is no setting at all, so a later default takes its place.
    pub fn set(&mut self, key: Key, value: &str) -> Result<(), Refused> {
        check(key, value)?;
        self.set[key.index()] = (value != key.default_value()).then(|| value.to_string());
        Ok(())
    }

    pub fn reset(&mut self, key: Key) {
        self.set[key.index()] = None;
    }

    pub fn background(&self) -> Background<'_> {
        let gradient = GRADIENTS
            .iter()
            .find(|g| g.id == self.get(Key::BackgroundGradient))
            .unwrap_or(&GRADIENTS[0]);
        let picture = Some(self.get(Key::BackgroundPicture)).filter(|p| !p.is_empty());
        let fit = Fit::named(self.get(Key::BackgroundFit)).unwrap_or(Fit::Fill);
        Background {
            gradient,
            picture,
            fit,
        }
    }

    pub fn twelve_hour_clock(&self) -> bool {
        self.get(Key::ClockFormat) == CLOCKS[1]
    }

    pub fn keyboard_layout(&self) -> Option<&str> {
        Some(self.get(Key::KeyboardLayout)).filter(|name| !name.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn the_file_is_in_the_home() {
        assert_eq!(path("/home/ada"), "/home/ada/.config/slopos/prefs");
        assert_eq!(path("/home/ada/"), "/home/ada/.config/slopos/prefs");
    }

    #[test]
    fn an_empty_file_is_every_default() {
        let (prefs, skipped) = Prefs::parse("");
        assert!(skipped.is_empty());
        for key in Key::ALL {
            assert_eq!(prefs.get(key), key.default_value());
            assert_eq!(check(key, key.default_value()), Ok(()), "{key}");
        }
        let background = prefs.background();
        assert_eq!(background.gradient.id, GRADIENTS[0].id);
        assert_eq!((background.picture, background.fit), (None, Fit::Fill));
        assert!(!prefs.twelve_hour_clock());
        assert_eq!(prefs.keyboard_layout(), None);
    }

    #[test]
    fn what_is_set_reads_back_and_a_default_is_not_kept() {
        let mut prefs = Prefs::default();
        prefs
            .set(Key::BackgroundPicture, "/home/ada/beach.png")
            .unwrap();
        prefs.set(Key::BackgroundFit, "tile").unwrap();
        prefs.set(Key::ClockFormat, "24h").unwrap();
        prefs.set(Key::KeyboardLayout, "de_CH").unwrap();
        assert!(!prefs.is_set(Key::ClockFormat));
        let text = prefs.render();
        assert!(text.contains("background.picture = /home/ada/beach.png\n"));
        assert!(!text.contains("clock.format"));
        let (back, skipped) = Prefs::parse(&text);
        assert!(skipped.is_empty());
        assert_eq!(back, prefs);
        assert_eq!(back.background().fit, Fit::Tile);
        assert_eq!(back.keyboard_layout(), Some("de_CH"));
        prefs.reset(Key::BackgroundFit);
        assert_eq!(prefs.background().fit, Fit::Fill);
    }

    #[test]
    fn a_line_not_understood_is_passed_over_and_named() {
        let text = "\
background.fit = sideways
volume = 11
just words
clock.format = 12h
background.picture = beach.png
";
        let (prefs, skipped) = Prefs::parse(text);
        assert!(prefs.twelve_hour_clock());
        assert_eq!(
            skipped.iter().map(|s| s.line).collect::<Vec<_>>(),
            vec![1, 2, 3, 5]
        );
        assert!(skipped[0].why.contains("fill, fit, center, stretch, tile"));
    }

    #[test]
    fn a_value_is_held_to_its_kind() {
        assert_eq!(
            check(Key::BackgroundGradient, "plaid"),
            Err(Refused::NotAChoice(&slopos_theme_core::GRADIENT_IDS))
        );
        assert_eq!(
            check(Key::BackgroundPicture, "x.png"),
            Err(Refused::NotAbsolute)
        );
        assert_eq!(
            check(Key::BackgroundPicture, "/a\nb"),
            Err(Refused::LineBreak)
        );
        assert_eq!(
            check(Key::KeyboardLayout, "../us"),
            Err(Refused::NotALayout)
        );
        assert_eq!(check(Key::KeyboardLayout, "de_CH"), Ok(()));
        assert_eq!(Key::named("clock.format"), Some(Key::ClockFormat));
        assert_eq!(Key::named("clock"), None);
    }

    #[test]
    fn every_summary_and_refusal_is_a_sentence() {
        let refusals = [
            Refused::NoSuchKey,
            Refused::NotAChoice(&FITS),
            Refused::NotAbsolute,
            Refused::TooLong,
            Refused::LineBreak,
            Refused::NotALayout,
        ];
        let said = Key::ALL
            .iter()
            .map(|k| k.summary().to_string())
            .chain(refusals.iter().map(|r| r.to_string()));
        for text in said {
            slopos_theme_core::words::sentence(&text, &[])
                .unwrap_or_else(|e| panic!("{text}: {e}"));
        }
    }
}
