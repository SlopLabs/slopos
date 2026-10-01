//! The crash partition: a ring of [`SLOT_BYTES`] slots, each holding at most
//! one record of a fatal panic for a later boot to collect.
//!
//! A record is a [`HEADER_BYTES`] header and the text after it. The header
//! holds the magic, the version, the text's length, the record's sequence
//! number and a CRC-32 over those and the text, which tells a record a reset
//! cut short from a whole one. The text opens with a [`Summary`], then the
//! panic report, then the tail of the kernel log under [`LOG_HEADING`].

use core::fmt::{self, Write};

use crate::crc32;
use crate::layout;

pub const SLOT_BYTES: usize = 64 * 1024;
pub const HEADER_BYTES: usize = 64;
pub const TEXT_MAX: usize = SLOT_BYTES - HEADER_BYTES;
/// A larger partition keeps its records in the first this many slots.
pub const MAX_SLOTS: usize = 256;
/// No record carries `u64::MAX`, so a store may keep it as a marker.
pub const SEQUENCE_MAX: u64 = u64::MAX - 1;

const MAGIC: [u8; 8] = *b"SLOPCRSH";
const VERSION: u32 = 1;
const VERSION_AT: usize = 8;
const LEN_AT: usize = 12;
const SEQUENCE_AT: usize = 16;
const CRC_AT: usize = 24;

pub const TITLE: &str = "SlopOS crash record";
pub const LOG_HEADING: &str = "--- kernel log ---";

pub fn slot_count(partition_bytes: u64) -> usize {
    usize::try_from(partition_bytes / SLOT_BYTES as u64)
        .unwrap_or(usize::MAX)
        .min(MAX_SLOTS)
}

fn le_u32(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0u8; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(word)
}

fn le_u64(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(word)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub sequence: u64,
    pub text_len: usize,
    crc: u32,
}

impl Header {
    /// The record whose header `head` begins with, if it is one of this
    /// version.
    pub fn parse(head: &[u8]) -> Option<Header> {
        let head = head.get(..HEADER_BYTES)?;
        if head[..MAGIC.len()] != MAGIC || le_u32(head, VERSION_AT) != VERSION {
            return None;
        }
        let header = Header {
            sequence: le_u64(head, SEQUENCE_AT),
            text_len: le_u32(head, LEN_AT) as usize,
            crc: le_u32(head, CRC_AT),
        };
        ((1..=SEQUENCE_MAX).contains(&header.sequence) && header.text_len <= TEXT_MAX)
            .then_some(header)
    }

    fn fields(sequence: u64, text_len: usize) -> [u8; CRC_AT] {
        let mut fields = [0u8; CRC_AT];
        fields[..MAGIC.len()].copy_from_slice(&MAGIC);
        fields[VERSION_AT..LEN_AT].copy_from_slice(&VERSION.to_le_bytes());
        fields[LEN_AT..SEQUENCE_AT].copy_from_slice(&(text_len as u32).to_le_bytes());
        fields[SEQUENCE_AT..CRC_AT].copy_from_slice(&sequence.to_le_bytes());
        fields
    }

    /// Whether `text` is the whole of the text this header was sealed over.
    pub fn seals(&self, text: &[u8]) -> bool {
        text.len() == self.text_len && checksum(self.sequence, text) == self.crc
    }
}

fn checksum(sequence: u64, text: &[u8]) -> u32 {
    let state = crc32::feed(crc32::INIT, &Header::fields(sequence, text.len()));
    crc32::finish(crc32::feed(state, text))
}

/// Frame the `text_len` bytes of text that follow `record`'s header as record
/// `sequence`. Answers the record's length.
pub fn seal(record: &mut [u8], text_len: usize, sequence: u64) -> Option<usize> {
    let len = HEADER_BYTES + text_len;
    if text_len > TEXT_MAX || !(1..=SEQUENCE_MAX).contains(&sequence) || record.len() < len {
        return None;
    }
    let crc = checksum(sequence, &record[HEADER_BYTES..len]);
    let head = &mut record[..HEADER_BYTES];
    head.fill(0);
    head[..CRC_AT].copy_from_slice(&Header::fields(sequence, text_len));
    head[CRC_AT..CRC_AT + 4].copy_from_slice(&crc.to_le_bytes());
    Some(len)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotState {
    Empty,
    Holds(u64),
    /// Being written or erased.
    Busy,
}

/// The slot the next record goes to: the first empty slot after the newest
/// record's, wrapping, else the oldest record's. A busy slot is never chosen.
pub fn place(count: usize, state: impl Fn(usize) -> SlotState) -> Option<usize> {
    let mut newest: Option<(usize, u64)> = None;
    let mut oldest: Option<(usize, u64)> = None;
    for slot in 0..count {
        if let SlotState::Holds(sequence) = state(slot) {
            if newest.is_none_or(|(_, n)| sequence > n) {
                newest = Some((slot, sequence));
            }
            if oldest.is_none_or(|(_, o)| sequence < o) {
                oldest = Some((slot, sequence));
            }
        }
    }
    let after = newest.map_or(0, |(slot, _)| slot + 1);
    (0..count)
        .map(|k| (after + k) % count)
        .find(|&slot| state(slot) == SlotState::Empty)
        .or(oldest.map(|(slot, _)| slot))
}

/// What a record's text opens with: [`TITLE`], `key: value` lines and a
/// blank line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Summary<'a> {
    /// The path the loader read the kernel from.
    pub kernel: &'a str,
    pub cmdline: &'a str,
    pub build: &'a str,
    /// Unix seconds at the panic, when a wall clock was set.
    pub time: Option<u64>,
    pub uptime_ms: u64,
    pub cpu: u32,
    /// The panic's location and message.
    pub panic: &'a str,
}

/// A line break would end the value early.
fn field(out: &mut impl Write, key: &str, value: &str) -> fmt::Result {
    write!(out, "{key}: ")?;
    for c in value.chars() {
        out.write_char(if matches!(c, '\n' | '\r') { ' ' } else { c })?;
    }
    out.write_char('\n')
}

impl<'a> Summary<'a> {
    /// The slot whose kernel panicked, when it came from one.
    pub fn slot(&self) -> Option<&'a str> {
        layout::slot_of_kernel(self.kernel)
    }

    pub fn write(&self, out: &mut impl Write) -> fmt::Result {
        writeln!(out, "{TITLE}")?;
        field(out, "kernel", self.kernel)?;
        field(out, "cmdline", self.cmdline)?;
        field(out, "build", self.build)?;
        match self.time {
            Some(time) => writeln!(out, "time: {time}")?,
            None => writeln!(out, "time: -")?,
        }
        writeln!(out, "uptime: {} ms", self.uptime_ms)?;
        writeln!(out, "cpu: {}", self.cpu)?;
        field(out, "panic", self.panic)?;
        writeln!(out)
    }

    /// The summary `text` opens with; `None` when it opens with none, or one
    /// cut short before its blank line.
    pub fn parse(text: &'a str) -> Option<Summary<'a>> {
        let mut lines = text[..text.find("\n\n")?].split('\n');
        if lines.next()? != TITLE {
            return None;
        }
        let mut summary = Summary {
            kernel: "",
            cmdline: "",
            build: "",
            time: None,
            uptime_ms: 0,
            cpu: 0,
            panic: "",
        };
        for line in lines {
            let (key, value) = line.split_once(": ")?;
            match key {
                "kernel" => summary.kernel = value,
                "cmdline" => summary.cmdline = value,
                "build" => summary.build = value,
                "time" => summary.time = value.parse().ok(),
                "uptime" => summary.uptime_ms = value.strip_suffix(" ms")?.parse().ok()?,
                "cpu" => summary.cpu = value.parse().ok()?,
                "panic" => summary.panic = value,
                _ => {}
            }
        }
        Some(summary)
    }
}

#[cfg(test)]
mod tests;
