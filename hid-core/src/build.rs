//! Report descriptors written item by item, for the tests.

use std::vec::Vec;

#[derive(Default)]
pub struct Desc(pub Vec<u8>);

impl Desc {
    fn short(mut self, kind: u8, tag: u8, data: &[u8]) -> Self {
        let size = match data.len() {
            0 => 0,
            1 => 1,
            2 => 2,
            4 => 3,
            n => panic!("no short item carries {n} bytes"),
        };
        self.0.push(tag << 4 | kind << 2 | size);
        self.0.extend_from_slice(data);
        self
    }

    fn sized(value: i64) -> Vec<u8> {
        if (-128..=127).contains(&value) {
            Vec::from([value as u8])
        } else if (-32768..=32767).contains(&value) {
            (value as i16).to_le_bytes().to_vec()
        } else {
            (value as i32).to_le_bytes().to_vec()
        }
    }

    fn unsigned(value: u32) -> Vec<u8> {
        if value <= 0xff {
            Vec::from([value as u8])
        } else if value <= 0xffff {
            (value as u16).to_le_bytes().to_vec()
        } else {
            value.to_le_bytes().to_vec()
        }
    }

    pub fn page(self, page: u16) -> Self {
        self.short(1, 0x0, &Self::unsigned(page.into()))
    }
    pub fn logical(self, min: i64, max: i64) -> Self {
        let min = Self::sized(min);
        let max = Self::sized(max);
        self.short(1, 0x1, &min).short(1, 0x2, &max)
    }
    pub fn logical_max_raw(self, data: &[u8]) -> Self {
        self.short(1, 0x2, data)
    }
    pub fn size(self, bits: u32) -> Self {
        self.short(1, 0x7, &Self::unsigned(bits))
    }
    pub fn count(self, count: u32) -> Self {
        self.short(1, 0x9, &Self::unsigned(count))
    }
    pub fn id(self, id: u32) -> Self {
        self.short(1, 0x8, &Self::unsigned(id))
    }
    pub fn push(self) -> Self {
        self.short(1, 0xa, &[])
    }
    pub fn pop(self) -> Self {
        self.short(1, 0xb, &[])
    }
    pub fn usage(self, id: u16) -> Self {
        self.short(2, 0x0, &Self::unsigned(id.into()))
    }
    pub fn usage_extended(self, usage: u32) -> Self {
        self.short(2, 0x0, &usage.to_le_bytes())
    }
    pub fn range(self, min: u16, max: u16) -> Self {
        self.short(2, 0x1, &Self::unsigned(min.into()))
            .short(2, 0x2, &Self::unsigned(max.into()))
    }
    pub fn delimiter(self, open: bool) -> Self {
        self.short(2, 0xa, &[open as u8])
    }
    pub fn input(self, flags: u16) -> Self {
        self.short(0, 0x8, &Self::unsigned(flags.into()))
    }
    pub fn output(self, flags: u16) -> Self {
        self.short(0, 0x9, &Self::unsigned(flags.into()))
    }
    pub fn feature(self, flags: u16) -> Self {
        self.short(0, 0xb, &Self::unsigned(flags.into()))
    }
    pub fn collection(self, kind: u8) -> Self {
        self.short(0, 0xa, &[kind])
    }
    pub fn end(self) -> Self {
        self.short(0, 0xc, &[])
    }
    pub fn raw(mut self, bytes: &[u8]) -> Self {
        self.0.extend_from_slice(bytes);
        self
    }
}

pub const DATA_VAR: u16 = 0x02;
pub const DATA_VAR_REL: u16 = 0x06;
pub const DATA_ARRAY: u16 = 0x00;
pub const CONSTANT: u16 = 0x01;

/// A keyboard in the boot report's layout, with LED outputs.
pub fn keyboard() -> Vec<u8> {
    Desc::default()
        .page(0x01)
        .usage(0x06)
        .collection(1)
        .page(0x07)
        .range(0xe0, 0xe7)
        .logical(0, 1)
        .size(1)
        .count(8)
        .input(DATA_VAR)
        .count(1)
        .size(8)
        .input(CONSTANT)
        .page(0x08)
        .range(1, 5)
        .count(5)
        .size(1)
        .output(DATA_VAR)
        .count(1)
        .size(3)
        .output(CONSTANT)
        .page(0x07)
        .range(0, 0xff)
        .logical(0, 0xff)
        .size(8)
        .count(6)
        .input(DATA_ARRAY)
        .end()
        .0
}

/// A relative mouse with report ID 2, three buttons, X, Y and a wheel.
pub fn mouse() -> Vec<u8> {
    Desc::default()
        .page(0x01)
        .usage(0x02)
        .collection(1)
        .id(2)
        .usage(0x01)
        .collection(0)
        .page(0x09)
        .range(1, 3)
        .logical(0, 1)
        .size(1)
        .count(3)
        .input(DATA_VAR)
        .count(1)
        .size(5)
        .input(CONSTANT)
        .page(0x01)
        .usage(0x30)
        .usage(0x31)
        .usage(0x38)
        .logical(-127, 127)
        .size(8)
        .count(3)
        .input(DATA_VAR_REL)
        .end()
        .end()
        .0
}

/// An absolute tablet over 0..=0x7fff, three buttons and a wheel.
pub fn tablet() -> Vec<u8> {
    Desc::default()
        .page(0x01)
        .usage(0x02)
        .collection(1)
        .usage(0x01)
        .collection(0)
        .page(0x09)
        .range(1, 3)
        .logical(0, 1)
        .size(1)
        .count(3)
        .input(DATA_VAR)
        .count(1)
        .size(5)
        .input(CONSTANT)
        .page(0x01)
        .usage(0x30)
        .usage(0x31)
        .logical(0, 0x7fff)
        .size(16)
        .count(2)
        .input(DATA_VAR)
        .usage(0x38)
        .logical(-127, 127)
        .size(8)
        .count(1)
        .input(DATA_VAR_REL)
        .end()
        .end()
        .0
}
