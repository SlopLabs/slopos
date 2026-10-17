use super::*;

#[test]
fn what_build_writes_reads_back() {
    let program = build(
        Kind::Executable,
        Some("/lib/ld-slopos.so.1"),
        &["libc.so", "libx.so"],
        &["puts", "x_hello"],
        &["main"],
    );
    let linking = read(&program).unwrap();
    assert_eq!(linking.kind, Kind::Executable);
    assert!(linking.dynamic);
    assert_eq!(linking.interpreter, Some(&b"/lib/ld-slopos.so.1"[..]));
    assert_eq!(linking.needed, [&b"libc.so"[..], b"libx.so"]);
    assert_eq!(linking.imports, [&b"puts"[..], b"x_hello"]);
    assert_eq!(linking.exports, [&b"main"[..]]);
    let library = build(Kind::Shared, None, &[], &[], &["x_hello"]);
    let linking = read(&library).unwrap();
    assert_eq!((linking.kind, linking.interpreter), (Kind::Shared, None));
    assert_eq!(linking.exports, [&b"x_hello"[..]]);
}

#[test]
fn a_file_cut_short_or_foreign_is_an_error_never_a_panic() {
    let program = build(Kind::Executable, Some("/i"), &["libc.so"], &["a"], &["b"]);
    for cut in 0..program.len() {
        let _ = read(&program[..cut]);
    }
    for at in 0..program.len() {
        let mut bent = program.clone();
        bent[at] ^= 0xff;
        let _ = read(&bent);
    }
    let mut elf32 = program.clone();
    elf32[4] = 1;
    assert_eq!(read(&elf32).map(|_| ()), Err(ElfError::Foreign));
    let mut arm = program.clone();
    arm[18] = 183;
    assert_eq!(read(&arm).map(|_| ()), Err(ElfError::Foreign));
    let mut stripped = program;
    stripped[40..48].fill(0);
    assert_eq!(read(&stripped).map(|_| ()), Err(ElfError::NoSections));
    assert!(!is_elf(b"#!/bin/sh\n"));
}
