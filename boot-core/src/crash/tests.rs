use super::*;
use std::string::String;
use std::vec;
use std::vec::Vec;

fn record(text: &[u8], sequence: u64) -> Vec<u8> {
    let mut slot = vec![0u8; SLOT_BYTES];
    slot[HEADER_BYTES..HEADER_BYTES + text.len()].copy_from_slice(text);
    assert_eq!(
        seal(&mut slot, text.len(), sequence),
        Some(HEADER_BYTES + text.len())
    );
    slot
}

#[test]
fn a_sealed_record_parses_and_its_text_checks_out() {
    let slot = record(b"the report", 7);
    let header = Header::parse(&slot).expect("a sealed header");
    assert_eq!((header.sequence, header.text_len), (7, 10));
    assert!(header.seals(&slot[HEADER_BYTES..HEADER_BYTES + 10]));
}

#[test]
fn a_record_cut_short_or_changed_does_not_check_out() {
    let mut slot = record(b"the report", 7);
    let header = Header::parse(&slot).unwrap();
    assert!(!header.seals(&slot[HEADER_BYTES..HEADER_BYTES + 9]));
    slot[HEADER_BYTES + 3] ^= 1;
    assert!(!header.seals(&slot[HEADER_BYTES..HEADER_BYTES + 10]));
}

#[test]
fn the_header_covers_its_own_fields() {
    let slot = record(b"text", 3);
    let mut renumbered = slot.clone();
    renumbered[SEQUENCE_AT] = 4;
    let header = Header::parse(&renumbered).unwrap();
    assert!(!header.seals(b"text"));
}

#[test]
fn a_slot_holding_no_record_of_this_version_parses_to_none() {
    assert_eq!(Header::parse(&[0u8; HEADER_BYTES]), None);
    assert_eq!(Header::parse(&[0u8; 8]), None);
    let mut slot = record(b"x", 1);
    slot[VERSION_AT] = 2;
    assert_eq!(Header::parse(&slot), None);
    let mut slot = record(b"x", 1);
    slot[LEN_AT..LEN_AT + 4].copy_from_slice(&(TEXT_MAX as u32 + 1).to_le_bytes());
    assert_eq!(Header::parse(&slot), None);
    let mut slot = record(b"x", 1);
    slot[SEQUENCE_AT..SEQUENCE_AT + 8].fill(0);
    assert_eq!(Header::parse(&slot), None);
}

#[test]
fn seal_refuses_what_a_slot_cannot_hold() {
    let mut slot = vec![0u8; SLOT_BYTES];
    assert_eq!(seal(&mut slot, TEXT_MAX + 1, 1), None);
    assert_eq!(seal(&mut slot, 4, 0), None);
    assert_eq!(seal(&mut slot[..HEADER_BYTES + 3], 4, 1), None);
    assert_eq!(seal(&mut slot, TEXT_MAX, 1), Some(SLOT_BYTES));
}

#[test]
fn a_partition_holds_whole_slots_up_to_the_cap() {
    assert_eq!(slot_count(4 << 20), 64);
    assert_eq!(slot_count(SLOT_BYTES as u64 - 1), 0);
    assert_eq!(slot_count(1 << 40), MAX_SLOTS);
}

fn states(slots: &[SlotState]) -> impl Fn(usize) -> SlotState + '_ {
    |slot| slots[slot]
}

#[test]
fn an_empty_ring_starts_at_the_first_slot() {
    let slots = [SlotState::Empty; 4];
    assert_eq!(place(4, states(&slots)), Some(0));
}

#[test]
fn the_next_record_follows_the_newest() {
    use SlotState::{Empty, Holds};
    let slots = [Holds(4), Empty, Holds(3), Empty];
    assert_eq!(place(4, states(&slots)), Some(1));
    let slots = [Empty, Holds(2), Empty, Holds(9)];
    assert_eq!(place(4, states(&slots)), Some(0));
}

#[test]
fn a_full_ring_overwrites_the_oldest() {
    use SlotState::Holds;
    let slots = [Holds(6), Holds(7), Holds(4), Holds(5)];
    assert_eq!(place(4, states(&slots)), Some(2));
}

#[test]
fn a_busy_slot_is_never_chosen() {
    use SlotState::{Busy, Empty, Holds};
    let slots = [Holds(1), Busy, Empty];
    assert_eq!(place(3, states(&slots)), Some(2));
    let slots = [Holds(1), Busy, Holds(2)];
    assert_eq!(place(3, states(&slots)), Some(0));
    assert_eq!(place(2, states(&[Busy, Busy])), None);
    assert_eq!(place(0, states(&[])), None);
}

const SUMMARY: Summary<'static> = Summary {
    kernel: "/boot/bad/kernel.elf",
    cmdline: "slot=bad panic=reboot panic.boot=on",
    build: "guest-17",
    time: Some(1_759_320_000),
    uptime_ms: 12_345,
    cpu: 3,
    panic: "boot/src/early_init.rs:926:13: panic.boot=on",
};

fn rendered(summary: &Summary<'_>) -> String {
    let mut text = String::new();
    summary.write(&mut text).unwrap();
    text
}

#[test]
fn a_summary_reads_back_as_written() {
    let mut text = rendered(&SUMMARY);
    assert!(text.ends_with("\n\n"));
    text.push_str("=== KERNEL PANIC ===\n");
    assert_eq!(Summary::parse(&text), Some(SUMMARY));
    assert_eq!(SUMMARY.slot(), Some("bad"));
}

#[test]
fn a_summary_without_a_clock_or_a_slot_says_so() {
    let summary = Summary {
        kernel: "/boot/kernel.elf",
        time: None,
        ..SUMMARY
    };
    let text = rendered(&summary);
    assert!(text.contains("\ntime: -\n"));
    assert!(rendered(&SUMMARY).contains("\nuptime: 12345 ms\ncpu: 3\n"));
    let parsed = Summary::parse(&text).unwrap();
    assert_eq!((parsed.time, parsed.slot()), (None, None));
}

#[test]
fn a_line_break_inside_a_value_stays_inside_it() {
    let summary = Summary {
        panic: "first\nsecond\r\nthird",
        ..SUMMARY
    };
    let parsed_from = rendered(&summary);
    let parsed = Summary::parse(&parsed_from).unwrap();
    assert_eq!(parsed.panic, "first second  third");
    assert_eq!(parsed.uptime_ms, SUMMARY.uptime_ms);
}

#[test]
fn zero_is_written_as_a_digit() {
    let summary = Summary {
        time: Some(0),
        uptime_ms: 0,
        cpu: 0,
        ..SUMMARY
    };
    let parsed_from = rendered(&summary);
    let parsed = Summary::parse(&parsed_from).unwrap();
    assert_eq!((parsed.time, parsed.uptime_ms, parsed.cpu), (Some(0), 0, 0));
}

#[test]
fn a_summary_cut_short_or_absent_parses_to_none() {
    let text = rendered(&SUMMARY);
    assert_eq!(Summary::parse(&text[..text.len() - 1]), None);
    assert_eq!(Summary::parse("some other file\n\n"), None);
    assert_eq!(Summary::parse(""), None);
}
