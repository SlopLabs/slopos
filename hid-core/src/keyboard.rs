//! What a keyboard's report says is held, and the key transitions between
//! two reports. Keys are Keyboard page ids; a modifier is one of the eight
//! from Left Control.

use crate::boot;
use crate::descriptor::{Descriptor, Field, Kind};
use crate::usage::{self, KEY_ERRORS, KEY_LEFT_CONTROL, KEY_RIGHT_META, id_of, page, page_of};

/// Keyboard page ids 0 to 255.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeySet([u64; 4]);

impl KeySet {
    pub const EMPTY: Self = Self([0; 4]);

    pub fn insert(&mut self, key: u16) {
        if let Some(word) = self.0.get_mut(usize::from(key >> 6)) {
            *word |= 1 << (key & 63);
        }
    }

    pub fn remove(&mut self, key: u16) {
        if let Some(word) = self.0.get_mut(usize::from(key >> 6)) {
            *word &= !(1 << (key & 63));
        }
    }

    pub fn contains(&self, key: u16) -> bool {
        self.0
            .get(usize::from(key >> 6))
            .is_some_and(|word| word >> (key & 63) & 1 != 0)
    }

    pub fn keys(&self) -> impl Iterator<Item = u16> + '_ {
        (0..256u16).filter(|&key| self.contains(key))
    }

    pub fn union(self, other: Self) -> Self {
        Self(core::array::from_fn(|i| self.0[i] | other.0[i]))
    }
}

/// Report IDs whose keys a keyboard can hold apart.
pub const KEY_REPORTS: usize = 4;

/// What each report ID last said is held: a keyboard whose keys span several
/// reports holds their union, and a report replaces only its own share.
#[derive(Clone, Copy, Debug, Default)]
pub struct HeldByReport {
    ids: [u8; KEY_REPORTS],
    sets: [KeySet; KEY_REPORTS],
    used: usize,
}

impl HeldByReport {
    pub const EMPTY: Self = Self {
        ids: [0; KEY_REPORTS],
        sets: [KeySet::EMPTY; KEY_REPORTS],
        used: 0,
    };

    /// The union before and after report `id` says `held`; `None`, changing
    /// nothing, when `id` is new and every slot is taken.
    pub fn replace(&mut self, id: u8, held: KeySet) -> Option<(KeySet, KeySet)> {
        let before = self.union();
        let at = match self.ids[..self.used].iter().position(|&known| known == id) {
            Some(at) => at,
            None if self.used < KEY_REPORTS => {
                self.ids[self.used] = id;
                self.used += 1;
                self.used - 1
            }
            None => return None,
        };
        self.sets[at] = held;
        Some((before, self.union()))
    }

    pub fn union(&self) -> KeySet {
        self.sets[..self.used]
            .iter()
            .fold(KeySet::EMPTY, |all, set| all.union(*set))
    }
}

pub fn is_modifier(key: u16) -> bool {
    (KEY_LEFT_CONTROL..=KEY_RIGHT_META).contains(&key)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keys {
    Held(KeySet),
    /// More keys are down than the report holds, or the keyboard reports an
    /// error in place of keys; what is held is unknown.
    Rollover,
}

fn held(modifiers: u8, keys: impl Iterator<Item = u16>) -> Keys {
    let mut set = KeySet::EMPTY;
    for bit in 0..8 {
        if modifiers >> bit & 1 != 0 {
            set.insert(KEY_LEFT_CONTROL + bit);
        }
    }
    for key in keys {
        if KEY_ERRORS.contains(&key) {
            return Keys::Rollover;
        }
        if key != 0 {
            set.insert(key);
        }
    }
    Keys::Held(set)
}

/// A boot-protocol report; `None` when it is shorter than the boot layout.
pub fn boot(report: &[u8]) -> Option<Keys> {
    let report = boot::Keyboard::parse(report)?;
    Some(held(
        report.modifiers,
        report.keys.iter().map(|&key| u16::from(key)),
    ))
}

fn in_keyboard(field: &Field) -> bool {
    matches!(field.application, usage::KEYBOARD | usage::KEYPAD)
}

/// Whether any input report carries keys [`decode`] reads.
pub fn carries_keys(desc: &Descriptor<'_>) -> bool {
    desc.every_element(Kind::Input)
        .any(|e| in_keyboard(e.field) && page_of(e.usage) == page::KEYBOARD)
        || desc.fields().iter().any(|f| {
            f.kind == Kind::Input
                && !f.is_variable()
                && in_keyboard(f)
                && desc.array_page(f) == page::KEYBOARD
        })
}

/// A report-protocol report: the Keyboard page variables and arrays of its
/// keyboard and keypad collections. `None` when report `id` carries no keys
/// or the payload is shorter than it.
pub fn decode(desc: &Descriptor<'_>, id: u8, payload: &[u8]) -> Option<Keys> {
    if payload.len() < desc.payload_bytes(Kind::Input, id) {
        return None;
    }
    let mut any = false;
    let mut set = KeySet::EMPTY;
    for element in desc
        .elements(Kind::Input, id)
        .filter(|e| in_keyboard(e.field) && page_of(e.usage) == page::KEYBOARD)
    {
        any = true;
        let key = id_of(element.usage);
        if key != 0 && element.reading(payload).is_some_and(|v| v != 0) {
            if KEY_ERRORS.contains(&key) {
                return Some(Keys::Rollover);
            }
            set.insert(key);
        }
    }
    for field in desc
        .arrays(Kind::Input, id)
        .filter(|f| in_keyboard(f) && desc.array_page(f) == page::KEYBOARD)
    {
        any = true;
        for index in 0..field.count {
            let selected = field
                .value(payload, index)
                .and_then(|value| desc.array_usage(field, value))
                .filter(|&usage| page_of(usage) == page::KEYBOARD);
            if let Some(usage) = selected {
                let key = id_of(usage);
                if KEY_ERRORS.contains(&key) {
                    return Some(Keys::Rollover);
                }
                if key != 0 {
                    set.insert(key);
                }
            }
        }
    }
    any.then_some(Keys::Held(set))
}

/// The transitions from `old` to `new`: releases before presses, so a key
/// released with a modifier is released under it, and modifiers pressed
/// before the keys they shift.
pub fn steps(old: KeySet, new: KeySet) -> Steps {
    Steps {
        old,
        new,
        phase: 0,
        at: 0,
    }
}

pub struct Steps {
    old: KeySet,
    new: KeySet,
    phase: u8,
    at: u16,
}

impl Iterator for Steps {
    type Item = (u16, bool);

    fn next(&mut self) -> Option<(u16, bool)> {
        while self.phase < 4 {
            let pressed = self.phase >= 2;
            let modifiers = self.phase == 1 || self.phase == 2;
            let (from, to) = if pressed {
                (&self.new, &self.old)
            } else {
                (&self.old, &self.new)
            };
            while self.at < 256 {
                let key = self.at;
                self.at += 1;
                if is_modifier(key) == modifiers && from.contains(key) && !to.contains(key) {
                    return Some((key, pressed));
                }
            }
            self.phase += 1;
            self.at = 0;
        }
        None
    }
}

/// Lock LEDs as the keyboard's output report sets them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Leds {
    pub num: bool,
    pub caps: bool,
    pub scroll: bool,
}

impl Leds {
    pub fn boot(self) -> u8 {
        u8::from(self.num) * boot::led::NUM_LOCK
            | u8::from(self.caps) * boot::led::CAPS_LOCK
            | u8::from(self.scroll) * boot::led::SCROLL_LOCK
    }

    fn of(self, usage: u32) -> Option<bool> {
        match usage {
            usage::NUM_LOCK => Some(self.num),
            usage::CAPS_LOCK => Some(self.caps),
            usage::SCROLL_LOCK => Some(self.scroll),
            _ => None,
        }
    }
}

/// The output report carrying the lock LEDs, its ID byte first when the
/// descriptor uses IDs: its ID and length in `out`. `None` when no output
/// report has a lock LED, or `out` cannot hold it.
pub fn led_report(desc: &Descriptor<'_>, leds: Leds, out: &mut [u8]) -> Option<(u8, usize)> {
    let id = desc
        .fields()
        .iter()
        .filter(|f| f.kind == Kind::Output)
        .map(|f| f.report_id)
        .find(|&id| {
            desc.elements(Kind::Output, id)
                .any(|e| leds.of(e.usage).is_some())
        })?;
    let skip = usize::from(desc.report_ids());
    let len = skip + desc.payload_bytes(Kind::Output, id);
    let report = out.get_mut(..len)?;
    report.fill(0);
    if skip == 1 {
        report[0] = id;
    }
    for element in desc.elements(Kind::Output, id) {
        if let Some(on) = leds.of(element.usage) {
            element.write(&mut report[skip..], i32::from(on));
        }
    }
    Some((id, len))
}

#[cfg(test)]
mod tests {
    use std::vec::Vec;

    use super::*;
    use crate::build::{self, DATA_ARRAY, DATA_VAR, Desc};
    use crate::descriptor::tests::{Storage, mutate};

    fn set(keys: &[u16]) -> KeySet {
        let mut set = KeySet::EMPTY;
        for &key in keys {
            set.insert(key);
        }
        set
    }

    #[test]
    fn a_boot_report_holds_its_modifiers_and_keys() {
        let keys = boot(&[0x02, 0, 0x04, 0x05, 0, 0, 0, 0]).unwrap();
        assert_eq!(keys, Keys::Held(set(&[0xe1, 0x04, 0x05])));
        assert_eq!(boot(&[0, 0, 1, 1, 1, 1, 1, 1]), Some(Keys::Rollover));
        assert_eq!(boot(&[0, 0, 2, 0, 0, 0, 0, 0]), Some(Keys::Rollover));
        assert_eq!(boot(&[0; 4]), None);
    }

    #[test]
    fn steps_release_then_press_with_modifiers_around_keys() {
        let old = set(&[0xe1, 0x04]);
        let new = set(&[0xe0, 0x05]);
        let steps: Vec<_> = steps(old, new).collect();
        assert_eq!(
            steps,
            [(0x04, false), (0xe1, false), (0xe0, true), (0x05, true)]
        );
        assert_eq!(super::steps(new, new).count(), 0);
    }

    #[test]
    fn report_protocol_reads_arrays_and_bitmaps() {
        let desc = Desc::default()
            .page(0x01)
            .usage(0x06)
            .collection(1)
            .id(1)
            .page(0x07)
            .range(0xe0, 0xe7)
            .logical(0, 1)
            .size(1)
            .count(8)
            .input(DATA_VAR)
            .range(0x04, 0x13)
            .count(16)
            .input(DATA_VAR)
            .range(0, 0x65)
            .logical(0, 0x65)
            .size(8)
            .count(2)
            .input(DATA_ARRAY)
            .end()
            .page(0x01)
            .usage(0x02)
            .collection(1)
            .id(2)
            .usage(0x30)
            .logical(-127, 127)
            .size(8)
            .count(1)
            .input(DATA_VAR | 4)
            .end()
            .page(0x01)
            .usage(0x05)
            .collection(1)
            .id(3)
            .page(0x07)
            .range(0x04, 0x0b)
            .logical(0, 1)
            .size(1)
            .count(8)
            .input(DATA_VAR)
            .end()
            .0;
        let mut s = Storage::new(8, 8);
        let parsed = s.parse(&desc).unwrap();
        let d = parsed.descriptor(&s.fields, &s.usages);
        let keys = decode(&d, 1, &[0x10, 0x01, 0x00, 0x2c, 0x00]).unwrap();
        assert_eq!(keys, Keys::Held(set(&[0xe4, 0x04, 0x2c])));
        assert_eq!(decode(&d, 1, &[0, 0, 0, 0x01, 0x01]), Some(Keys::Rollover));
        assert_eq!(decode(&d, 2, &[5]), None, "report 2 carries no keys");
        assert_eq!(
            decode(&d, 3, &[0xff]),
            None,
            "a game pad's buttons are no keys"
        );
        assert_eq!(decode(&d, 1, &[0x10]), None, "a short report says nothing");
        assert!(carries_keys(&d));
    }

    #[test]
    fn a_game_pad_carries_no_keys_and_moves_no_cursor() {
        let desc = Desc::default()
            .page(0x01)
            .usage(0x05)
            .collection(1)
            .usage(0x30)
            .logical(0, 255)
            .size(8)
            .count(1)
            .input(DATA_VAR)
            .page(0x09)
            .range(1, 8)
            .logical(0, 1)
            .size(1)
            .count(8)
            .input(DATA_VAR)
            .page(0x07)
            .range(0x04, 0x0b)
            .input(DATA_VAR)
            .end()
            .0;
        let mut s = Storage::new(8, 8);
        let parsed = s.parse(&desc).unwrap();
        let d = parsed.descriptor(&s.fields, &s.usages);
        assert!(!carries_keys(&d));
        assert!(!crate::pointer::carries_pointer(&d));
    }

    #[test]
    fn each_report_replaces_only_its_own_keys() {
        let mut held = HeldByReport::EMPTY;
        assert_eq!(
            held.replace(1, set(&[0x04])),
            Some((set(&[]), set(&[0x04])))
        );
        assert_eq!(
            held.replace(2, set(&[0x59])),
            Some((set(&[0x04]), set(&[0x04, 0x59])))
        );
        assert_eq!(
            held.replace(1, set(&[])),
            Some((set(&[0x04, 0x59]), set(&[0x59]))),
            "report 1 lets go of its key and report 2 keeps its own"
        );
        for id in 3..=KEY_REPORTS as u8 {
            assert!(held.replace(id, set(&[])).is_some());
        }
        assert_eq!(held.replace(0xf0, set(&[0x05])), None, "no slot is left");
        assert_eq!(held.union(), set(&[0x59]));
    }

    #[test]
    fn the_led_report_sets_only_its_lock_bits() {
        let mut s = Storage::new(16, 16);
        let parsed = s.parse(&build::keyboard()).unwrap();
        let d = parsed.descriptor(&s.fields, &s.usages);
        let mut out = [0xffu8; 4];
        let leds = Leds {
            num: true,
            caps: true,
            scroll: false,
        };
        assert_eq!(led_report(&d, leds, &mut out), Some((0, 1)));
        assert_eq!(out[0], 0x03);
        assert_eq!(leds.boot(), 0x03);
        assert_eq!(led_report(&d, leds, &mut []), None);
        let with_id = Desc::default()
            .id(5)
            .page(0x08)
            .range(1, 3)
            .logical(0, 1)
            .size(1)
            .count(3)
            .output(DATA_VAR)
            .0;
        let parsed = s.parse(&with_id).unwrap();
        let d = parsed.descriptor(&s.fields, &s.usages);
        assert_eq!(led_report(&d, Leds::default(), &mut out), Some((5, 2)));
        assert_eq!(out[..2], [5, 0]);
        let parsed = s.parse(&build::mouse()).unwrap();
        let d = parsed.descriptor(&s.fields, &s.usages);
        assert_eq!(led_report(&d, leds, &mut out), None);
    }

    #[test]
    fn mutated_reports_never_panic_or_read_past_their_bytes() {
        let mut s = Storage::new(16, 16);
        let parsed = s.parse(&build::keyboard()).unwrap();
        let d = parsed.descriptor(&s.fields, &s.usages);
        mutate(&[0x22, 0, 4, 5, 6, 7, 8, 9], |report| {
            let _ = boot(report);
            if let Some(Keys::Held(keys)) = decode(&d, 0, report) {
                let _ = steps(KeySet::EMPTY, keys).count();
            }
        });
    }
}
