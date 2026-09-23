//! A DER reader (ITU-T X.690) over the subset X.509 uses.

pub const BOOLEAN: u8 = 0x01;
pub const INTEGER: u8 = 0x02;
pub const BIT_STRING: u8 = 0x03;
pub const OCTET_STRING: u8 = 0x04;
pub const NULL: u8 = 0x05;
pub const OID: u8 = 0x06;
pub const UTF8_STRING: u8 = 0x0c;
pub const PRINTABLE_STRING: u8 = 0x13;
pub const IA5_STRING: u8 = 0x16;
pub const UTC_TIME: u8 = 0x17;
pub const GENERALIZED_TIME: u8 = 0x18;
pub const SEQUENCE: u8 = 0x30;
pub const SET: u8 = 0x31;

pub const fn context(n: u8) -> u8 {
    0xa0 | n
}

pub const fn context_primitive(n: u8) -> u8 {
    0x80 | n
}

#[derive(Clone, Copy)]
pub struct Reader<'a> {
    data: &'a [u8],
}

/// One element: its tag, its contents, and the whole encoding.
#[derive(Clone, Copy)]
pub struct Tlv<'a> {
    pub tag: u8,
    pub value: &'a [u8],
    pub raw: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn finish(&self) -> Option<()> {
        self.is_empty().then_some(())
    }

    pub fn peek_tag(&self) -> Option<u8> {
        self.data.first().copied()
    }

    pub fn tlv(&mut self) -> Option<Tlv<'a>> {
        let tag = *self.data.first()?;
        if tag & 0x1f == 0x1f {
            return None;
        }
        let first = *self.data.get(1)? as usize;
        let (len, header) = if first < 0x80 {
            (first, 2)
        } else {
            let n = first & 0x7f;
            if n == 0 || n > 4 {
                return None;
            }
            let bytes = self.data.get(2..2 + n)?;
            if bytes[0] == 0 {
                return None;
            }
            let len = bytes.iter().fold(0usize, |acc, &b| (acc << 8) | b as usize);
            if len < 0x80 {
                return None;
            }
            (len, 2 + n)
        };
        let end = header.checked_add(len)?;
        let raw = self.data.get(..end)?;
        self.data = &self.data[end..];
        Some(Tlv {
            tag,
            value: &raw[header..],
            raw,
        })
    }

    pub fn expect(&mut self, tag: u8) -> Option<&'a [u8]> {
        let tlv = self.tlv()?;
        (tlv.tag == tag).then_some(tlv.value)
    }

    /// The contents of the next element if it carries `tag`; otherwise
    /// nothing is consumed.
    pub fn optional(&mut self, tag: u8) -> Option<&'a [u8]> {
        if self.peek_tag() == Some(tag) {
            self.expect(tag)
        } else {
            None
        }
    }

    pub fn sequence(&mut self) -> Option<Reader<'a>> {
        self.expect(SEQUENCE).map(Reader::new)
    }

    /// A non-negative INTEGER's magnitude, without the sign octet.
    pub fn unsigned_integer(&mut self) -> Option<&'a [u8]> {
        let v = self.expect(INTEGER)?;
        match v {
            [] => None,
            [b, ..] if b & 0x80 != 0 => None,
            [0, rest @ ..] if !rest.is_empty() && rest[0] & 0x80 == 0 => None,
            [0, rest @ ..] if !rest.is_empty() => Some(rest),
            _ => Some(v),
        }
    }

    pub fn small_unsigned(&mut self) -> Option<u64> {
        let v = self.unsigned_integer()?;
        (v.len() <= 8).then(|| v.iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b)))
    }

    pub fn oid(&mut self) -> Option<&'a [u8]> {
        self.expect(OID)
    }

    /// A BIT STRING with no unused bits.
    pub fn bit_string(&mut self) -> Option<&'a [u8]> {
        match self.expect(BIT_STRING)? {
            [0, rest @ ..] => Some(rest),
            _ => None,
        }
    }

    pub fn boolean(&mut self) -> Option<bool> {
        match self.expect(BOOLEAN)? {
            [0x00] => Some(false),
            [0xff] => Some(true),
            _ => None,
        }
    }
}
