use std::vec::Vec;

use super::*;
use crate::build::{self, DATA_ARRAY, DATA_VAR, DATA_VAR_REL, Desc};
use crate::usage::{self as u, page};

pub(crate) struct Storage {
    pub fields: Vec<Field>,
    pub usages: Vec<u32>,
}

impl Storage {
    pub fn new(fields: usize, usages: usize) -> Self {
        Self {
            fields: std::vec![Field::default(); fields],
            usages: std::vec![0; usages],
        }
    }

    pub fn parse(&mut self, desc: &[u8]) -> Result<Parsed, Error> {
        parse(desc, &mut self.fields, &mut self.usages)
    }
}

#[test]
fn a_boot_keyboard_lays_out_its_eight_bytes() {
    let mut s = Storage::new(16, 16);
    let parsed = s.parse(&build::keyboard()).unwrap();
    let d = parsed.descriptor(&s.fields, &s.usages);
    assert!(!d.report_ids());
    assert_eq!(d.payload_bytes(Kind::Input, 0), 8);
    assert_eq!(d.payload_bytes(Kind::Output, 0), 1);
    assert_eq!(d.max_report_bytes(Kind::Input), 8);
    assert_eq!(parsed.fields, 3, "the constants are not fields");
    let modifiers: Vec<u32> = d.elements(Kind::Input, 0).map(|e| e.usage).collect();
    assert_eq!(
        modifiers,
        (0xe0..=0xe7)
            .map(|id| u::usage(page::KEYBOARD, id))
            .collect::<Vec<_>>()
    );
    let keys = d.arrays(Kind::Input, 0).next().unwrap();
    assert_eq!((keys.bit_offset, keys.bit_size, keys.count), (16, 8, 6));
    assert_eq!(keys.application, u::KEYBOARD);
    assert_eq!(d.array_usage(keys, 4), Some(u::usage(page::KEYBOARD, 4)));
    assert_eq!(d.array_page(keys), page::KEYBOARD);
    let leds: Vec<u32> = d.elements(Kind::Output, 0).map(|e| e.usage).collect();
    assert_eq!(leds[..3], [u::NUM_LOCK, u::CAPS_LOCK, u::SCROLL_LOCK]);
    let report = [0x02, 0, 0x04, 0x05, 0, 0, 0, 0];
    assert_eq!(keys.value(&report, 0), Some(4));
    assert_eq!(keys.value(&report, 5), Some(0));
    assert_eq!(keys.value(&report[..7], 5), None, "past the payload");
    let shift = d.elements(Kind::Input, 0).nth(1).unwrap();
    assert_eq!(shift.value(&report), Some(1));
}

#[test]
fn report_ids_prefix_reports_and_push_pop_restores_globals() {
    let desc = Desc::default()
        .page(0x01)
        .usage(0x02)
        .collection(1)
        .id(1)
        .logical(-127, 127)
        .size(8)
        .count(2)
        .push()
        .id(3)
        .size(16)
        .usage(0x30)
        .usage(0x31)
        .input(DATA_VAR_REL)
        .pop()
        .usage(0x30)
        .usage(0x31)
        .input(DATA_VAR_REL)
        .end()
        .0;
    let mut s = Storage::new(8, 8);
    let parsed = s.parse(&desc).unwrap();
    let d = parsed.descriptor(&s.fields, &s.usages);
    assert!(d.report_ids());
    assert_eq!(d.payload_bytes(Kind::Input, 3), 4);
    assert_eq!(d.payload_bytes(Kind::Input, 1), 2);
    assert_eq!(d.max_report_bytes(Kind::Input), 5);
    assert_eq!(d.split(&[3, 1, 2, 3, 4]), Some((3, &[1u8, 2, 3, 4][..])));
    assert_eq!(d.split(&[]), None);
    let one = d.fields_of(Kind::Input, 1).next().unwrap();
    assert_eq!((one.bit_offset, one.bit_size), (0, 8));
    assert!(one.is_relative() && one.is_signed());
    assert_eq!(one.value(&[0xff, 0x01], 0), Some(-1));
    assert_eq!(one.value(&[0xff, 0x01], 1), Some(1));
}

#[test]
fn logical_maximum_is_unsigned_unless_the_minimum_is_negative() {
    let unsigned = Desc::default()
        .page(0x07)
        .logical(0, 0)
        .logical_max_raw(&[0xff])
        .size(8)
        .count(1)
        .range(0, 0xff)
        .input(DATA_ARRAY)
        .0;
    let mut s = Storage::new(4, 4);
    let parsed = s.parse(&unsigned).unwrap();
    let field = s.fields[0];
    assert_eq!((field.logical_min, field.logical_max), (0, 255));
    assert_eq!(field.value(&[0xfe], 0), Some(254));
    let d = parsed.descriptor(&s.fields, &s.usages);
    assert_eq!(d.array_usage(&field, 0xfe), Some(u::usage(7, 0xfe)));
    let signed = Desc::default()
        .page(0x01)
        .logical(-1, 0)
        .logical_max_raw(&[0x7f])
        .size(8)
        .count(1)
        .usage(0x30)
        .input(DATA_VAR_REL)
        .0;
    s.parse(&signed).unwrap();
    assert_eq!(
        (s.fields[0].logical_min, s.fields[0].logical_max),
        (-1, 127)
    );
}

#[test]
fn usages_take_the_page_in_force_at_the_main_item() {
    let desc = Desc::default()
        .page(0x01)
        .usage(0x30)
        .page(0x09)
        .usage_extended(u::X)
        .logical(0, 1)
        .size(1)
        .count(2)
        .input(DATA_VAR)
        .0;
    let mut s = Storage::new(4, 4);
    let parsed = s.parse(&desc).unwrap();
    let d = parsed.descriptor(&s.fields, &s.usages);
    let usages: Vec<u32> = d.elements(Kind::Input, 0).map(|e| e.usage).collect();
    assert_eq!(usages, [u::usage(page::BUTTON, 0x30), u::X]);
}

#[test]
fn the_last_usage_repeats_and_lists_fill_the_table() {
    let desc = Desc::default()
        .page(0x0c)
        .usage(0xe9)
        .usage(0xea)
        .usage(0xe2)
        .logical(0, 3)
        .size(2)
        .count(2)
        .input(DATA_ARRAY)
        .page(0x09)
        .usage(1)
        .logical(0, 1)
        .size(1)
        .count(3)
        .input(DATA_VAR)
        .0;
    let mut s = Storage::new(4, 8);
    let parsed = s.parse(&desc).unwrap();
    assert_eq!(parsed.usages, 3);
    let d = parsed.descriptor(&s.fields, &s.usages);
    let consumer = d.arrays(Kind::Input, 0).next().unwrap();
    assert_eq!(d.array_usage(consumer, 0), Some(u::usage(0x0c, 0xe9)));
    assert_eq!(d.array_usage(consumer, 2), Some(u::usage(0x0c, 0xe2)));
    assert_eq!(d.array_usage(consumer, 3), None, "past the list");
    assert_eq!(d.array_usage(consumer, -1), None);
    assert_eq!(d.array_page(consumer), 0x0c);
    let buttons: Vec<u32> = d.elements(Kind::Input, 0).map(|e| e.usage).collect();
    assert_eq!(buttons, [u::usage(9, 1); 3]);
}

#[test]
fn only_a_delimited_sets_first_usage_counts_and_long_items_are_skipped() {
    let desc = Desc::default()
        .page(0x01)
        .delimiter(true)
        .usage(0x30)
        .usage(0x31)
        .delimiter(false)
        .raw(&[0xfe, 2, 0xf0, 0xaa, 0xbb])
        .logical(0, 1)
        .size(1)
        .count(1)
        .input(DATA_VAR)
        .0;
    let mut s = Storage::new(4, 4);
    let parsed = s.parse(&desc).unwrap();
    let d = parsed.descriptor(&s.fields, &s.usages);
    assert_eq!(d.elements(Kind::Input, 0).next().unwrap().usage, u::X);
}

#[test]
fn null_states_read_as_nothing() {
    let desc = Desc::default()
        .page(0x01)
        .usage(0x39)
        .logical(0, 7)
        .size(4)
        .count(1)
        .input(DATA_VAR | flag::NULL_STATE)
        .0;
    let mut s = Storage::new(4, 4);
    let parsed = s.parse(&desc).unwrap();
    let d = parsed.descriptor(&s.fields, &s.usages);
    let hat = d.elements(Kind::Input, 0).next().unwrap();
    assert_eq!(hat.reading(&[0x03]), Some(3));
    assert_eq!(hat.reading(&[0x0f]), None);
}

#[test]
fn fields_write_their_bits_and_nothing_else() {
    let mut s = Storage::new(16, 16);
    let parsed = s.parse(&build::keyboard()).unwrap();
    let d = parsed.descriptor(&s.fields, &s.usages);
    let mut out = [0xe0u8];
    for led in d.elements(Kind::Output, 0) {
        assert!(led.write(&mut out, i32::from(led.usage == u::CAPS_LOCK)));
    }
    assert_eq!(out, [0xe2], "the padding is untouched");
    assert!(!s.fields[0].write(&mut [], 0, 1));
}

#[test]
fn bits_are_read_little_endian_across_bytes() {
    assert_eq!(
        read_bits(&[0b1010_0000, 0b0000_0111], 5, 6),
        Some(0b111_101)
    );
    assert_eq!(read_bits(&[0xff; 4], 0, 32), Some(u32::MAX));
    assert_eq!(read_bits(&[0xff; 4], 1, 32), None);
    assert_eq!(read_bits(&[0xff; 4], u32::MAX, 2), None);
    assert_eq!(sign_extend(0x1f, 5), -1);
    let mut out = [0u8; 2];
    assert!(write_bits(&mut out, 5, 6, 0b111_101));
    assert_eq!(out, [0b1010_0000, 0b0000_0111]);
}

#[test]
fn malformed_descriptors_are_refused() {
    let mut s = Storage::new(4, 4);
    let truncated = Desc::default().page(0x01).0;
    assert_eq!(s.parse(&truncated[..1]), Err(Error::Truncated));
    assert_eq!(s.parse(&[0xfe, 4, 0, 1]), Err(Error::Truncated));
    assert_eq!(s.parse(&Desc::default().id(0).0), Err(Error::ReportId));
    assert_eq!(s.parse(&Desc::default().id(256).0), Err(Error::ReportId));
    assert_eq!(
        s.parse(&Desc::default().collection(1).0),
        Err(Error::Unbalanced)
    );
    assert_eq!(s.parse(&Desc::default().end().0), Err(Error::Unbalanced));
    assert_eq!(s.parse(&Desc::default().pop().0), Err(Error::Unbalanced));
    assert_eq!(
        s.parse(&Desc::default().delimiter(true).0),
        Err(Error::Unbalanced)
    );
    assert_eq!(
        s.parse(&Desc::default().delimiter(false).0),
        Err(Error::Unbalanced)
    );
    let deep = (0..=MAX_GLOBALS).fold(Desc::default(), |d, _| d.push());
    assert_eq!(s.parse(&deep.0), Err(Error::TooDeep));
    let nested = (0..=MAX_COLLECTIONS).fold(Desc::default(), |d, _| d.collection(2));
    assert_eq!(s.parse(&nested.0), Err(Error::TooDeep));
    let long = Desc::default().size(8).count(4097).input(DATA_VAR).0;
    assert_eq!(s.parse(&long), Err(Error::ReportTooLong));
    let overflow = Desc::default().size(0xffff_ffff).count(2).input(DATA_VAR).0;
    assert_eq!(s.parse(&overflow), Err(Error::ReportTooLong));
    let feature = Desc::default()
        .logical(0, 1)
        .size(8)
        .count(4097)
        .feature(DATA_VAR)
        .size(1)
        .count(1)
        .feature(DATA_VAR)
        .count(8)
        .input(DATA_VAR)
        .0;
    let parsed = s.parse(&feature).unwrap();
    assert_eq!(
        parsed.fields, 1,
        "a feature report past the limit leaves its fields out, the input stays"
    );
}

#[test]
fn storage_bounds_what_is_recorded() {
    let mut fields = Storage::new(2, 8);
    assert_eq!(fields.parse(&build::keyboard()), Err(Error::TooManyFields));
    let list = Desc::default()
        .usage(1)
        .usage(3)
        .usage(5)
        .logical(0, 1)
        .size(1)
        .count(3)
        .input(DATA_VAR)
        .0;
    let mut usages = Storage::new(4, 2);
    assert_eq!(usages.parse(&list), Err(Error::TooManyUsages));
    let elements = Desc::default()
        .logical(0, 1)
        .size(1)
        .count(MAX_ELEMENTS)
        .input(DATA_VAR)
        .count(1)
        .input(DATA_VAR)
        .count(MAX_ELEMENTS)
        .feature(DATA_VAR)
        .0;
    let mut budget = Storage::new(4, 4);
    let parsed = budget.parse(&elements).unwrap();
    assert_eq!(
        parsed.fields, 2,
        "the input past the budget is left out and the feature counts nothing"
    );
    let d = parsed.descriptor(&budget.fields, &budget.usages);
    assert_eq!(
        d.payload_bytes(Kind::Input, 0),
        (MAX_ELEMENTS as usize + 1).div_ceil(8),
        "what is left out still takes its bits"
    );
}

#[test]
fn reports_and_usages_past_the_tables_are_left_out() {
    let reports = (1..=MAX_REPORTS as u32 + 1).fold(
        Desc::default()
            .page(0x09)
            .usage(1)
            .logical(0, 1)
            .size(1)
            .count(1),
        |d, id| d.id(id).usage(1).input(DATA_VAR),
    );
    let mut s = Storage::new(64, 4);
    let parsed = s.parse(&reports.0).unwrap();
    assert_eq!(
        parsed.fields, MAX_REPORTS,
        "the last report is not laid out"
    );
    let d = parsed.descriptor(&s.fields, &s.usages);
    assert_eq!(d.payload_bytes(Kind::Input, MAX_REPORTS as u8 + 1), 0);
    let segments = (1..=MAX_SEGMENTS as u16 + 4)
        .fold(Desc::default().page(0x0c), |d, id| d.usage(id))
        .logical(1, MAX_SEGMENTS as i64 + 4)
        .size(8)
        .count(1)
        .input(DATA_ARRAY)
        .0;
    let mut s = Storage::new(4, 64);
    let parsed = s.parse(&segments).unwrap();
    let d = parsed.descriptor(&s.fields, &s.usages);
    let consumer = d.arrays(Kind::Input, 0).next().unwrap();
    let last = MAX_SEGMENTS as i32;
    assert_eq!(
        d.array_usage(consumer, last),
        Some(u::usage(0x0c, last as u16))
    );
    assert_eq!(
        d.array_usage(consumer, last + 1),
        None,
        "a usage past the table"
    );
}

#[test]
fn a_report_with_no_usages_reports_usage_zero_of_its_page() {
    let desc = Desc::default()
        .page(0xff00)
        .logical(0, 255)
        .size(8)
        .count(4)
        .input(DATA_VAR)
        .0;
    let mut s = Storage::new(4, 4);
    let parsed = s.parse(&desc).unwrap();
    let d = parsed.descriptor(&s.fields, &s.usages);
    assert!(d.elements(Kind::Input, 0).all(|e| e.usage == 0xff00_0000));
}

pub(crate) fn mutate(bytes: &[u8], mut exercise: impl FnMut(&[u8])) {
    for len in 0..=bytes.len() {
        exercise(&bytes[..len]);
    }
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    for at in 0..bytes.len() {
        for _ in 0..24 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let mut mutated = bytes.to_vec();
            mutated[at] = state as u8;
            exercise(&mutated);
            let cut = (state >> 8) as usize % (bytes.len() + 1);
            exercise(&mutated[..cut]);
        }
    }
}

#[test]
fn mutated_descriptors_never_panic_or_read_past_their_bytes() {
    let mut s = Storage::new(32, 64);
    let mut corpus = build::keyboard();
    corpus.extend(build::mouse());
    corpus.extend(build::tablet());
    mutate(&corpus, |bytes| {
        let Ok(parsed) = parse(bytes, &mut s.fields, &mut s.usages) else {
            return;
        };
        let d = parsed.descriptor(&s.fields, &s.usages);
        let report = [0xa5u8; 16];
        for kind in [Kind::Input, Kind::Output, Kind::Feature] {
            let _ = d.max_report_bytes(kind);
            for id in 0..4 {
                let _ = d.payload_bytes(kind, id);
                for e in d.elements(kind, id) {
                    let _ = e.reading(&report);
                    let _ = e.write(&mut [0u8; 4], -1);
                }
                for f in d.arrays(kind, id) {
                    for i in 0..f.count.min(64) {
                        if let Some(v) = f.value(&report, i) {
                            let _ = d.array_usage(f, v);
                        }
                    }
                    let _ = d.array_page(f);
                }
            }
        }
    });
}

#[test]
fn each_kind_of_report_counts_its_own_bits() {
    let desc = Desc::default()
        .id(4)
        .page(0x0d)
        .usage(0x42)
        .logical(0, 1)
        .size(1)
        .count(1)
        .input(DATA_VAR)
        .usage(0x52)
        .logical(0, 10)
        .size(8)
        .feature(DATA_VAR)
        .0;
    let mut s = Storage::new(4, 4);
    let parsed = s.parse(&desc).unwrap();
    let d = parsed.descriptor(&s.fields, &s.usages);
    let mode = d.elements(Kind::Feature, 4).next().unwrap();
    assert_eq!((mode.usage, mode.field.bit_offset), (u::INPUT_MODE, 0));
    assert_eq!(d.payload_bytes(Kind::Feature, 4), 1);
    assert_eq!(d.payload_bytes(Kind::Input, 4), 1);
    assert_eq!(d.payload_bytes(Kind::Output, 4), 0);
}
