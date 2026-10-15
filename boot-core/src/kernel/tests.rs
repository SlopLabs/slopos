use super::*;
use std::vec;
use std::vec::Vec;

fn note(owner: &[u8], kind: u32, desc: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for word in [owner.len() as u32, desc.len() as u32, kind] {
        out.extend_from_slice(&word.to_le_bytes());
    }
    out.extend_from_slice(owner);
    out.resize(out.len().next_multiple_of(4), 0);
    out.extend_from_slice(desc);
    out.resize(out.len().next_multiple_of(4), 0);
    out
}

fn served(min: u32, max: u32) -> Vec<u8> {
    let desc: Vec<u8> = [min, max].iter().flat_map(|w| w.to_le_bytes()).collect();
    note(b"SlopOS\0", NT_SLOPOS_BASE_INTERFACE, &desc)
}

/// A 64-bit ELF file whose sections are `sections`, `(name, type, bytes)`
/// after the null section, then the name table.
fn elf(sections: &[(&str, u32, &[u8])]) -> Vec<u8> {
    let mut names = vec![0u8];
    let mut file = vec![0u8; 64];
    file[..6].copy_from_slice(b"\x7fELF\x02\x01");
    let mut headers = vec![[0u8; SECTION_HEADER]];
    let mut add = |file: &mut Vec<u8>, name: &str, kind: u32, bytes: &[u8]| {
        let mut header = [0u8; SECTION_HEADER];
        header[0..4].copy_from_slice(&(names.len() as u32).to_le_bytes());
        names.extend_from_slice(name.as_bytes());
        names.push(0);
        header[4..8].copy_from_slice(&kind.to_le_bytes());
        header[24..32].copy_from_slice(&(file.len() as u64).to_le_bytes());
        header[32..40].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
        header[48..56].copy_from_slice(&4u64.to_le_bytes());
        file.extend_from_slice(bytes);
        header
    };
    for &(name, kind, bytes) in sections {
        let header = add(&mut file, name, kind, bytes);
        headers.push(header);
    }
    let table_name = names.len() as u32;
    names.extend_from_slice(b".shstrtab\0");
    let mut strtab = [0u8; SECTION_HEADER];
    strtab[0..4].copy_from_slice(&table_name.to_le_bytes());
    strtab[4..8].copy_from_slice(&3u32.to_le_bytes());
    strtab[24..32].copy_from_slice(&(file.len() as u64).to_le_bytes());
    strtab[32..40].copy_from_slice(&(names.len() as u64).to_le_bytes());
    file.extend_from_slice(&names);
    headers.push(strtab);
    file.resize(file.len().next_multiple_of(8), 0);
    let table = file.len() as u64;
    for header in &headers {
        file.extend_from_slice(header);
    }
    file[0x28..0x30].copy_from_slice(&table.to_le_bytes());
    file[0x3A..0x3C].copy_from_slice(&(SECTION_HEADER as u16).to_le_bytes());
    file[0x3C..0x3E].copy_from_slice(&(headers.len() as u16).to_le_bytes());
    file[0x3E..0x40].copy_from_slice(&(headers.len() as u16 - 1).to_le_bytes());
    file
}

#[test]
fn the_note_the_kernel_carries_states_its_range() {
    let file = elf(&[
        (".text", 1, b"\x90\x90"),
        (NOTE_SECTION, SHT_NOTE, &served(2, 5)),
    ]);
    assert_eq!(served_interfaces(&file), Ok(2..=5));
}

#[test]
fn the_kernel_s_own_note_reads_as_the_range_this_tree_serves() {
    let note = slopos_abi::base_interface::BaseInterfaceNote::SERVED;
    let mut bytes = Vec::new();
    for word in [note.namesz, note.descsz, note.kind] {
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    bytes.extend_from_slice(&note.name);
    bytes.extend_from_slice(&note.min.to_le_bytes());
    bytes.extend_from_slice(&note.max.to_le_bytes());
    let file = elf(&[(NOTE_SECTION, SHT_NOTE, &bytes)]);
    assert_eq!(
        served_interfaces(&file),
        Ok(slopos_abi::BASE_INTERFACE_MIN..=slopos_abi::BASE_INTERFACE)
    );
}

#[test]
fn another_owner_s_note_before_ours_is_passed_over() {
    let mut notes = note(b"GNU\0", 1, &[1, 2, 3]);
    notes.extend(note(b"SlopOS\0", 2, &[0; 8]));
    notes.extend(served(1, 1));
    let file = elf(&[(NOTE_SECTION, SHT_NOTE, &notes)]);
    assert_eq!(served_interfaces(&file), Ok(1..=1));
}

#[test]
fn a_kernel_without_the_note_or_with_a_bad_one_is_refused() {
    let cases: [(Vec<u8>, NoteError); 7] = [
        (b"MZ not an elf".to_vec(), NoteError::NotElf),
        (elf(&[(".text", 1, b"x")]), NoteError::NoNote),
        (elf(&[(NOTE_SECTION, 1, &served(1, 1))]), NoteError::NoNote),
        (
            elf(&[(".note.slopos.base-interfaces", SHT_NOTE, &served(1, 1))]),
            NoteError::NoNote,
        ),
        (
            elf(&[(NOTE_SECTION, SHT_NOTE, &served(3, 2))]),
            NoteError::BadNote,
        ),
        (
            elf(&[(NOTE_SECTION, SHT_NOTE, &served(0, 2))]),
            NoteError::BadNote,
        ),
        (
            elf(&[(
                NOTE_SECTION,
                SHT_NOTE,
                &note(b"SlopOS\0", NT_SLOPOS_BASE_INTERFACE, &[1, 0, 0, 0]),
            )]),
            NoteError::BadNote,
        ),
    ];
    for (file, want) in cases {
        assert_eq!(served_interfaces(&file), Err(want));
    }
}

#[test]
fn a_file_cut_or_flipped_anywhere_is_refused_without_a_panic() {
    let file = elf(&[
        (".text", 1, b"abc"),
        (NOTE_SECTION, SHT_NOTE, &served(1, 2)),
    ]);
    for len in 0..file.len() {
        assert!(served_interfaces(&file[..len]).is_err(), "cut at {len}");
    }
    for at in 0..file.len() {
        let mut flipped = file.clone();
        flipped[at] ^= 0xA5;
        let _ = served_interfaces(&flipped);
    }
}
