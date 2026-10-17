use super::*;
use crate::elf::build;
use alloc::vec::Vec;

fn libc() -> Vec<u8> {
    build(Kind::Shared, None, &[], &[], &["puts", "malloc"])
}

fn program(needed: &[&str], imports: &[&str]) -> Vec<u8> {
    build(Kind::Executable, Some(INTERPRETER), needed, imports, &[])
}

fn check_files(files: &[(&str, Vec<u8>)]) -> Result<(), SymbolError> {
    let files: Vec<(&str, &[u8])> = files.iter().map(|(p, b)| (*p, b.as_slice())).collect();
    check(&files, &libc())
}

#[test]
fn what_libc_and_its_own_libraries_export_binds() {
    let files = [
        (
            "/bin/hello",
            program(&["libc.so", "libx.so"], &["puts", "x_hello", "y_deep"]),
        ),
        (
            "/lib/libx.so",
            build(Kind::Shared, None, &["liby.so"], &["malloc"], &["x_hello"]),
        ),
        (
            "/lib/liby.so",
            build(Kind::Shared, None, &[], &[], &["y_deep"]),
        ),
        ("/share/readme", b"not a program".to_vec()),
        (
            "/share/a.o",
            build(Kind::Other, None, &[], &["anything"], &[]),
        ),
    ];
    assert_eq!(check_files(&files), Ok(()));
}

#[test]
fn what_binds_nothing_it_needs_is_refused() {
    for (files, why) in [
        (
            [("/bin/a", program(&["libc.so"], &["puts", "lacking"]))].to_vec(),
            SymbolError::Unbound("/bin/a".into(), "lacking".into()),
        ),
        (
            [("/bin/a", program(&["libc.so", "libz.so"], &["puts"]))].to_vec(),
            SymbolError::Needs("/bin/a".into(), "libz.so".into()),
        ),
        (
            [
                ("/bin/a", program(&["libc.so", "libz.so"], &["z"])),
                (
                    "/share/libz.so",
                    build(Kind::Shared, None, &[], &[], &["z"]),
                ),
            ]
            .to_vec(),
            SymbolError::Needs("/bin/a".into(), "libz.so".into()),
        ),
        (
            [("/bin/a", program(&["libx.so"], &[]))].to_vec(),
            SymbolError::NoLibc("/bin/a".into()),
        ),
        (
            [(
                "/bin/a",
                build(
                    Kind::Executable,
                    Some("/lib/ld-linux.so"),
                    &["libc.so"],
                    &[],
                    &[],
                ),
            )]
            .to_vec(),
            SymbolError::Interpreter("/bin/a".into()),
        ),
        (
            [("/bin/a", {
                let mut static_program = program(&["libc.so"], &[]);
                static_program[64..68].fill(0);
                static_program[64 + 56..68 + 56].fill(0);
                static_program
            })]
            .to_vec(),
            SymbolError::Static("/bin/a".into()),
        ),
    ] {
        assert_eq!(check_files(&files), Err(why));
    }
    assert!(matches!(
        check(&[], b"not an elf"),
        Err(SymbolError::Libc(_))
    ));
}
