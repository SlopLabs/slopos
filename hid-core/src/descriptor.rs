//! Report descriptors (§6.2.2). Items are read within the descriptor's bytes,
//! global state is pushed and popped, local usages take the usage page in
//! force at their main item (§6.2.2.8), and each data field is recorded with
//! its report, its bit position and its usages.

use crate::usage::{self, page_of};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// An item's data runs past the descriptor.
    Truncated,
    TooManyFields,
    TooManyUsages,
    /// Pop without Push, Collection without End Collection, or a delimiter
    /// left open.
    Unbalanced,
    TooDeep,
    /// Report ID 0 is reserved, and an ID is one byte.
    ReportId,
    /// Past [`MAX_REPORT_BITS`].
    ReportTooLong,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
    #[default]
    Input,
    Output,
    Feature,
}

/// Main item data bits (§6.2.2.5).
pub mod flag {
    pub const CONSTANT: u16 = 1 << 0;
    pub const VARIABLE: u16 = 1 << 1;
    pub const RELATIVE: u16 = 1 << 2;
    pub const NULL_STATE: u16 = 1 << 6;
}

/// A report no longer than a page.
pub const MAX_REPORT_BITS: u32 = 8 * 4096;
/// Distinct reports, counting each kind of each ID; fields of any past these
/// are not recorded.
pub const MAX_REPORTS: usize = 32;
/// Input and output elements over every data field, which bounds what
/// decoding one report costs; a field past them is left out.
pub const MAX_ELEMENTS: u32 = 1024;
const MAX_GLOBALS: usize = 4;
const MAX_COLLECTIONS: usize = 8;
const MAX_SEGMENTS: usize = 16;
const MAX_FIELD_BITS: u32 = 32;

/// One main item's data: `count` elements of `bit_size` bits from
/// `bit_offset`, counted from the first byte after the report ID.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Field {
    pub kind: Kind,
    pub report_id: u8,
    pub flags: u16,
    pub bit_offset: u32,
    pub bit_size: u8,
    pub count: u16,
    pub logical_min: i32,
    pub logical_max: i32,
    /// The innermost application collection's usage, 0 outside any.
    pub application: u32,
    usage_min: u32,
    usage_max: u32,
    /// Into the usage table; 0 when the usages are `usage_min..=usage_max`.
    list_len: u16,
    list_at: u16,
}

impl Field {
    pub fn is_variable(&self) -> bool {
        self.flags & flag::VARIABLE != 0
    }

    pub fn is_relative(&self) -> bool {
        self.flags & flag::RELATIVE != 0
    }

    pub fn is_signed(&self) -> bool {
        self.logical_min < 0
    }

    /// Element `index`'s raw value, sign-extended when the logical minimum is
    /// negative; `None` past the payload.
    pub fn value(&self, payload: &[u8], index: u16) -> Option<i32> {
        let size = u32::from(self.bit_size);
        let at = self
            .bit_offset
            .checked_add(u32::from(index).checked_mul(size)?)?;
        let raw = read_bits(payload, at, size)?;
        Some(if self.is_signed() {
            sign_extend(raw, size)
        } else {
            raw as i32
        })
    }

    /// Writes element `index`'s bits; `false` past `out`.
    pub fn write(&self, out: &mut [u8], index: u16, value: i32) -> bool {
        let size = u32::from(self.bit_size);
        let Some(at) = u32::from(index)
            .checked_mul(size)
            .and_then(|o| o.checked_add(self.bit_offset))
        else {
            return false;
        };
        write_bits(out, at, size, value as u32)
    }

    fn usage_at(&self, table: &[u32], index: u32) -> Option<u32> {
        if self.list_len == 0 {
            let at = self.usage_min.checked_add(index)?;
            return (at <= self.usage_max).then_some(at);
        }
        if index >= u32::from(self.list_len) {
            return None;
        }
        table
            .get(usize::from(self.list_at) + index as usize)
            .copied()
    }

    fn usages(&self, table: &[u32]) -> u32 {
        if self.list_len == 0 {
            (self.usage_max - self.usage_min).saturating_add(1)
        } else {
            u32::from(self.list_len).min(table.len() as u32)
        }
    }

    fn first_usage(&self, table: &[u32]) -> u32 {
        self.usage_at(table, 0).unwrap_or(self.usage_min)
    }
}

/// A variable element: a field's element and the usage it reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Element<'a> {
    pub field: &'a Field,
    pub index: u16,
    pub usage: u32,
}

impl Element<'_> {
    pub fn value(&self, payload: &[u8]) -> Option<i32> {
        self.field.value(payload, self.index)
    }

    /// The value, unless the field has a null state and the value is outside
    /// the logical range (§6.2.2.5).
    pub fn reading(&self, payload: &[u8]) -> Option<i32> {
        let value = self.value(payload)?;
        let f = self.field;
        let null =
            f.flags & flag::NULL_STATE != 0 && (value < f.logical_min || value > f.logical_max);
        (!null).then_some(value)
    }

    pub fn write(&self, out: &mut [u8], value: i32) -> bool {
        self.field.write(out, self.index, value)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Report {
    kind: Kind,
    id: u8,
    /// [`PAST_LIMIT`] once a feature report ran past [`MAX_REPORT_BITS`].
    bits: u16,
}

/// A feature report past the limit leaves out its later fields rather than
/// refusing the descriptor: features are never decoded where reports arrive.
const PAST_LIMIT: u16 = u16::MAX;

/// What [`parse`] recorded, beside the fields and usages it wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Parsed {
    pub fields: usize,
    pub usages: usize,
    /// Every report starts with its ID byte.
    pub report_ids: bool,
    reports: [Report; MAX_REPORTS],
    report_count: usize,
}

impl Parsed {
    pub fn descriptor<'a>(&'a self, fields: &'a [Field], usages: &'a [u32]) -> Descriptor<'a> {
        Descriptor {
            parsed: self,
            fields: &fields[..self.fields.min(fields.len())],
            usages: &usages[..self.usages.min(usages.len())],
        }
    }
}

/// A parsed descriptor over the storage it was parsed into.
#[derive(Clone, Copy, Debug)]
pub struct Descriptor<'a> {
    parsed: &'a Parsed,
    fields: &'a [Field],
    usages: &'a [u32],
}

impl<'a> Descriptor<'a> {
    pub fn fields(&self) -> &'a [Field] {
        self.fields
    }

    pub fn report_ids(&self) -> bool {
        self.parsed.report_ids
    }

    /// The report's ID and the payload after it.
    pub fn split<'r>(&self, report: &'r [u8]) -> Option<(u8, &'r [u8])> {
        if self.parsed.report_ids {
            let (&id, payload) = report.split_first()?;
            Some((id, payload))
        } else {
            Some((0, report))
        }
    }

    /// Bytes of a report after its ID; 0 for one the descriptor never
    /// declares.
    pub fn payload_bytes(&self, kind: Kind, id: u8) -> usize {
        self.parsed.reports[..self.parsed.report_count]
            .iter()
            .find(|r| r.kind == kind && r.id == id)
            .map_or(0, |r| usize::from(r.bits).div_ceil(8))
    }

    /// The longest report of a kind, its ID byte included.
    pub fn max_report_bytes(&self, kind: Kind) -> usize {
        let payload = self.parsed.reports[..self.parsed.report_count]
            .iter()
            .filter(|r| r.kind == kind)
            .map(|r| usize::from(r.bits).div_ceil(8))
            .max()
            .unwrap_or(0);
        payload + usize::from(self.parsed.report_ids)
    }

    pub fn fields_of(&self, kind: Kind, id: u8) -> impl Iterator<Item = &'a Field> + 'a {
        self.fields
            .iter()
            .filter(move |f| f.kind == kind && f.report_id == id)
    }

    /// Every element of every variable field of the report, with its usage:
    /// past the last usage, the last one repeats (§6.2.2.8).
    pub fn elements(&self, kind: Kind, id: u8) -> impl Iterator<Item = Element<'a>> + 'a {
        self.expand(self.fields_of(kind, id))
    }

    /// [`elements`](Self::elements) of every report of a kind.
    pub fn every_element(&self, kind: Kind) -> impl Iterator<Item = Element<'a>> + 'a {
        self.expand(self.fields.iter().filter(move |f| f.kind == kind))
    }

    fn expand(
        &self,
        fields: impl Iterator<Item = &'a Field> + 'a,
    ) -> impl Iterator<Item = Element<'a>> + 'a {
        let usages = self.usages;
        fields.filter(|f| f.is_variable()).flat_map(move |field| {
            let last = field.usages(usages).saturating_sub(1);
            (0..field.count).map(move |index| Element {
                field,
                index,
                usage: field
                    .usage_at(usages, u32::from(index).min(last))
                    .unwrap_or(field.usage_min),
            })
        })
    }

    /// The array fields of the report.
    pub fn arrays(&self, kind: Kind, id: u8) -> impl Iterator<Item = &'a Field> + 'a {
        self.fields_of(kind, id).filter(|f| !f.is_variable())
    }

    /// The usage an array element's value selects; `None` for a value outside
    /// the logical range or past the usages, which reports nothing.
    pub fn array_usage(&self, field: &Field, value: i32) -> Option<u32> {
        if value < field.logical_min || value > field.logical_max {
            return None;
        }
        let index = (i64::from(value) - i64::from(field.logical_min)) as u32;
        field.usage_at(self.usages, index)
    }

    /// The page of the usages an array field selects among.
    pub fn array_page(&self, field: &Field) -> u16 {
        page_of(field.first_usage(self.usages))
    }
}

#[derive(Clone, Copy, Default)]
struct Globals {
    page: u16,
    logical_min: i32,
    logical_max: u32,
    logical_max_size: u8,
    report_size: u32,
    report_count: u32,
    report_id: u8,
}

impl Globals {
    /// Logical Maximum is unsigned unless Logical Minimum is negative.
    fn logical_range(&self) -> (i32, i32) {
        let max = if self.logical_min < 0 {
            sign_extend(self.logical_max, u32::from(self.logical_max_size) * 8)
        } else {
            self.logical_max.min(i32::MAX as u32) as i32
        };
        (self.logical_min, max)
    }
}

#[derive(Clone, Copy, Default)]
struct Segment {
    min: u32,
    max: u32,
    min_extended: bool,
    max_extended: bool,
}

impl Segment {
    fn resolve(&self, page: u16) -> (u32, u32) {
        let full = |id: u32, extended: bool| {
            if extended {
                id
            } else {
                usage::usage(page, id as u16)
            }
        };
        (
            full(self.min, self.min_extended),
            full(self.max, self.max_extended),
        )
    }
}

#[derive(Clone, Copy, Default)]
struct Locals {
    segments: [Segment; MAX_SEGMENTS],
    len: usize,
    min: Option<(u32, bool)>,
    max: Option<(u32, bool)>,
    delimiter: bool,
    taken_in_set: bool,
}

impl Locals {
    fn push(&mut self, segment: Segment) -> Result<(), Error> {
        if self.delimiter {
            if self.taken_in_set {
                return Ok(());
            }
            self.taken_in_set = true;
        }
        if let Some(slot) = self.segments.get_mut(self.len) {
            *slot = segment;
            self.len += 1;
        }
        Ok(())
    }

    fn range_ends(&mut self) -> Result<(), Error> {
        if let (Some((min, min_extended)), Some((max, max_extended))) = (self.min, self.max) {
            self.min = None;
            self.max = None;
            self.push(Segment {
                min,
                max,
                min_extended,
                max_extended,
            })?;
        }
        Ok(())
    }

    fn resolved(&self, page: u16) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.segments[..self.len]
            .iter()
            .map(move |s| s.resolve(page))
            .filter(|(min, max)| min <= max)
    }
}

#[derive(Clone, Copy, Default)]
struct Collection {
    usage: u32,
    application: bool,
}

struct Parser<'s> {
    fields: &'s mut [Field],
    field_count: usize,
    usages: &'s mut [u32],
    usage_count: usize,
    globals: Globals,
    stack: [Globals; MAX_GLOBALS],
    depth: usize,
    locals: Locals,
    collections: [Collection; MAX_COLLECTIONS],
    nesting: usize,
    reports: [Report; MAX_REPORTS],
    report_count: usize,
    report_ids: bool,
    elements: u32,
}

const MAIN: u8 = 0;
const GLOBAL: u8 = 1;
const LOCAL: u8 = 2;
const LONG_ITEM: u8 = 0xfe;

/// Parses `desc` into `fields` and `usages`, which bound what it may record.
pub fn parse(desc: &[u8], fields: &mut [Field], usages: &mut [u32]) -> Result<Parsed, Error> {
    let mut parser = Parser {
        fields,
        field_count: 0,
        usages,
        usage_count: 0,
        globals: Globals::default(),
        stack: [Globals::default(); MAX_GLOBALS],
        depth: 0,
        locals: Locals::default(),
        collections: [Collection::default(); MAX_COLLECTIONS],
        nesting: 0,
        reports: [Report::default(); MAX_REPORTS],
        report_count: 0,
        report_ids: false,
        elements: 0,
    };
    let mut at = 0;
    while at < desc.len() {
        let prefix = desc[at];
        if prefix == LONG_ITEM {
            let size = usize::from(*desc.get(at + 1).ok_or(Error::Truncated)?);
            at = at
                .checked_add(3 + size)
                .filter(|end| *end <= desc.len())
                .ok_or(Error::Truncated)?;
            continue;
        }
        let size = match prefix & 3 {
            3 => 4,
            n => usize::from(n),
        };
        let data = desc.get(at + 1..at + 1 + size).ok_or(Error::Truncated)?;
        parser.item((prefix >> 2) & 3, prefix >> 4, data)?;
        at += 1 + size;
    }
    if parser.nesting != 0 || parser.locals.delimiter {
        return Err(Error::Unbalanced);
    }
    Ok(Parsed {
        fields: parser.field_count,
        usages: parser.usage_count,
        report_ids: parser.report_ids,
        reports: parser.reports,
        report_count: parser.report_count,
    })
}

fn unsigned(data: &[u8]) -> u32 {
    data.iter()
        .rev()
        .fold(0, |value, &byte| value << 8 | u32::from(byte))
}

fn signed(data: &[u8]) -> i32 {
    sign_extend(unsigned(data), data.len() as u32 * 8)
}

fn sign_extend(raw: u32, bits: u32) -> i32 {
    if bits == 0 || bits >= 32 {
        return raw as i32;
    }
    let shift = 32 - bits;
    ((raw << shift) as i32) >> shift
}

impl Parser<'_> {
    fn item(&mut self, kind: u8, tag: u8, data: &[u8]) -> Result<(), Error> {
        match kind {
            MAIN => self.main(tag, unsigned(data)),
            GLOBAL => self.global(tag, data),
            LOCAL => self.local(tag, data),
            _ => Ok(()),
        }
    }

    #[inline(never)]
    fn global(&mut self, tag: u8, data: &[u8]) -> Result<(), Error> {
        let g = &mut self.globals;
        match tag {
            0x0 => g.page = unsigned(data) as u16,
            0x1 => g.logical_min = signed(data),
            0x2 => {
                g.logical_max = unsigned(data);
                g.logical_max_size = data.len() as u8;
            }
            0x7 => g.report_size = unsigned(data),
            0x8 => {
                let id = unsigned(data);
                if id == 0 || id > 0xff {
                    return Err(Error::ReportId);
                }
                g.report_id = id as u8;
                self.report_ids = true;
            }
            0x9 => g.report_count = unsigned(data),
            0xa => {
                let slot = self.stack.get_mut(self.depth).ok_or(Error::TooDeep)?;
                *slot = *g;
                self.depth += 1;
            }
            0xb => {
                self.depth = self.depth.checked_sub(1).ok_or(Error::Unbalanced)?;
                *g = self.stack[self.depth];
            }
            _ => {}
        }
        Ok(())
    }

    #[inline(never)]
    fn local(&mut self, tag: u8, data: &[u8]) -> Result<(), Error> {
        let value = unsigned(data);
        let extended = data.len() == 4;
        let l = &mut self.locals;
        match tag {
            0x0 => l.push(Segment {
                min: value,
                max: value,
                min_extended: extended,
                max_extended: extended,
            })?,
            0x1 => {
                l.min = Some((value, extended));
                l.range_ends()?;
            }
            0x2 => {
                l.max = Some((value, extended));
                l.range_ends()?;
            }
            0xa => match value {
                1 if !l.delimiter => {
                    l.delimiter = true;
                    l.taken_in_set = false;
                }
                0 if l.delimiter => l.delimiter = false,
                _ => return Err(Error::Unbalanced),
            },
            _ => {}
        }
        Ok(())
    }

    #[inline(never)]
    fn main(&mut self, tag: u8, data: u32) -> Result<(), Error> {
        let result = match tag {
            0x8 => self.data(Kind::Input, data as u16),
            0x9 => self.data(Kind::Output, data as u16),
            0xb => self.data(Kind::Feature, data as u16),
            0xa => self.open(data),
            0xc => {
                self.nesting = self.nesting.checked_sub(1).ok_or(Error::Unbalanced)?;
                Ok(())
            }
            _ => Ok(()),
        };
        let l = &mut self.locals;
        l.len = 0;
        l.min = None;
        l.max = None;
        l.taken_in_set = false;
        result
    }

    #[inline(never)]
    fn open(&mut self, kind: u32) -> Result<(), Error> {
        let usage = self
            .locals
            .resolved(self.globals.page)
            .next()
            .map_or(0, |(min, _)| min);
        let slot = self
            .collections
            .get_mut(self.nesting)
            .ok_or(Error::TooDeep)?;
        *slot = Collection {
            usage,
            application: kind == 1,
        };
        self.nesting += 1;
        Ok(())
    }

    fn application(&self) -> u32 {
        self.collections[..self.nesting]
            .iter()
            .rev()
            .find(|c| c.application)
            .map_or(0, |c| c.usage)
    }

    fn report(&mut self, kind: Kind, id: u8) -> Option<&mut Report> {
        let at = match self.reports[..self.report_count]
            .iter()
            .position(|r| r.kind == kind && r.id == id)
        {
            Some(at) => at,
            None => {
                *self.reports.get_mut(self.report_count)? = Report { kind, id, bits: 0 };
                self.report_count += 1;
                self.report_count - 1
            }
        };
        Some(&mut self.reports[at])
    }

    #[inline(never)]
    fn data(&mut self, kind: Kind, flags: u16) -> Result<(), Error> {
        let g = self.globals;
        let Some(report) = self.report(kind, g.report_id) else {
            return Ok(());
        };
        let bit_offset = u32::from(report.bits);
        let end = g
            .report_size
            .checked_mul(g.report_count)
            .and_then(|bits| bit_offset.checked_add(bits))
            .filter(|end| *end <= MAX_REPORT_BITS);
        let Some(end) = end else {
            if kind != Kind::Feature {
                return Err(Error::ReportTooLong);
            }
            report.bits = PAST_LIMIT;
            return Ok(());
        };
        report.bits = end as u16;
        let bits = end - bit_offset;
        if flags & flag::CONSTANT != 0 || bits == 0 || g.report_size > MAX_FIELD_BITS {
            return Ok(());
        }
        if kind != Kind::Feature {
            match self
                .elements
                .checked_add(g.report_count)
                .filter(|n| *n <= MAX_ELEMENTS)
            {
                Some(elements) => self.elements = elements,
                None => return Ok(()),
            }
        }
        let (logical_min, logical_max) = g.logical_range();
        let variable = flags & flag::VARIABLE != 0;
        let addressed = if variable {
            g.report_count
        } else if logical_max < logical_min {
            0
        } else {
            (i64::from(logical_max) - i64::from(logical_min) + 1).min(i64::from(u32::MAX)) as u32
        };
        let mut field = Field {
            kind,
            report_id: g.report_id,
            flags,
            bit_offset,
            bit_size: g.report_size as u8,
            count: g.report_count as u16,
            logical_min,
            logical_max,
            application: self.application(),
            ..Field::default()
        };
        self.assign_usages(&mut field, addressed)?;
        let slot = self
            .fields
            .get_mut(self.field_count)
            .ok_or(Error::TooManyFields)?;
        *slot = field;
        self.field_count += 1;
        Ok(())
    }

    /// One range in the field itself; anything else copied into the usage
    /// table, as far as `addressed` reaches.
    #[inline(never)]
    fn assign_usages(&mut self, field: &mut Field, addressed: u32) -> Result<(), Error> {
        let page = self.globals.page;
        let mut ranges = self.locals.resolved(page);
        let Some(first) = ranges.next() else {
            field.usage_min = usage::usage(page, 0);
            field.usage_max = field.usage_min;
            return Ok(());
        };
        if ranges.next().is_none() {
            (field.usage_min, field.usage_max) = first;
            return Ok(());
        }
        let start = self.usage_count;
        let mut written = 0u32;
        for (min, max) in self.locals.resolved(page) {
            for usage in min..=max {
                if written == addressed {
                    break;
                }
                let slot = self
                    .usages
                    .get_mut(self.usage_count)
                    .ok_or(Error::TooManyUsages)?;
                *slot = usage;
                self.usage_count += 1;
                written += 1;
            }
        }
        if written == 0 {
            (field.usage_min, field.usage_max) = first;
            return Ok(());
        }
        field.list_at = u16::try_from(start).map_err(|_| Error::TooManyUsages)?;
        field.list_len = u16::try_from(written).map_err(|_| Error::TooManyUsages)?;
        field.usage_min = first.0;
        Ok(())
    }
}

fn read_bits(bytes: &[u8], at: u32, size: u32) -> Option<u32> {
    if size == 0 || size > 32 {
        return None;
    }
    let end = at.checked_add(size)?;
    if end.div_ceil(8) as usize > bytes.len() {
        return None;
    }
    let mut value = 0u64;
    let first = (at / 8) as usize;
    let last = ((end - 1) / 8) as usize;
    for (i, &byte) in bytes[first..=last].iter().enumerate() {
        value |= u64::from(byte) << (8 * i);
    }
    value >>= at % 8;
    Some((value & ((1u64 << size) - 1)) as u32)
}

fn write_bits(bytes: &mut [u8], at: u32, size: u32, value: u32) -> bool {
    let Some(end) = at.checked_add(size) else {
        return false;
    };
    if size == 0 || size > 32 || end.div_ceil(8) as usize > bytes.len() {
        return false;
    }
    for bit in 0..size {
        let position = at + bit;
        let byte = &mut bytes[(position / 8) as usize];
        let mask = 1u8 << (position % 8);
        if value >> bit & 1 != 0 {
            *byte |= mask;
        } else {
            *byte &= !mask;
        }
    }
    true
}

#[cfg(test)]
pub(crate) mod tests;
