//! A miniature slopos_abi, documented by the real rustdoc so a JSON format
//! change shows here first: three private calls, a struct named by a
//! signature with a nested struct, a module rooted by a doc link, a family
//! rooted by a glob, a Linux-ish struct nothing reaches, and a kernel whose
//! Linux-numbered table answers `read`.

use std::sync::LazyLock;

use serde_json::Value;

use super::*;
use crate::fixture::Tree;

const LIB: &str = r#"#![no_std]
pub mod base_interface {
pub const BASE_INTERFACE: u32 = 1;
pub const BASE_INTERFACE_MIN: u32 = 1;
}
pub mod syscall {
pub mod numbers {
/// `read(fd, buf: *mut Nothing, len)`: Linux-numbered, never parsed.
pub const SYSCALL_READ: u64 = 0;
pub const SYSCALL_WRITE: u64 = 1;
pub const SYSCALL_PRIVATE_BASE: u64 = 1024;
pub const SYSCALL_PRIVATE_TABLE_SIZE: usize = 8;
pub const SYSCALL_PRIVATE_END: u64 = SYSCALL_PRIVATE_BASE + (SYSCALL_PRIVATE_TABLE_SIZE as u64);
/// `widget_get(out: *mut Widget,
/// max) -> entries written` — fills widgets.
pub const SYSCALL_WIDGET_GET: u64 = SYSCALL_PRIVATE_BASE + 0;
/// Drives the gadget: `gadget_ctl(op, arg: u64)`, `op` from [`crate::gadget`].
pub const SYSCALL_GADGET_CTL: u64 = SYSCALL_PRIVATE_BASE + 1;
/// `knob_set(value: Knob) -> 0`.
pub const SYSCALL_KNOB_SET: u64 = SYSCALL_PRIVATE_BASE + 2;
}
}
pub mod widget {
/// A widget.
#[repr(C)]
pub struct Widget {
pub id: u32,
pub inner: Inner,
pub word: Word,
}
#[repr(C)]
pub union Word {
pub raw: u32,
pub bytes: [u8; 4],
}
#[repr(C)]
pub struct Inner {
pub a: u16,
pub b: [u8; INNER_LEN],
}
pub const INNER_LEN: usize = 4;
impl Widget {
pub const SIZE: usize = 8 + INNER_LEN;
pub fn helper(&self) -> u32 { self.id }
}
}
pub mod gadget {
pub const GADGET_OP_A: u32 = 1;
#[repr(u8)]
pub enum Mode {
Off = 0,
On = 1,
}
pub fn encode(m: Mode) -> u8 { m as u8 }
pub static TABLE: [u8; 2] = [1, 2];
}
pub mod flags {
pub const FLAG_X: u32 = 1 << 0;
pub const FLAG_Y: u32 = 1 << 1;
pub const OTHER: u32 = 9;
}
pub type Knob = u32;
pub mod linuxish {
#[repr(C)]
pub struct Stat {
pub size: u64,
}
}
"#;

const THIN: &str = r#"pub mod base_interface {
pub const BASE_INTERFACE: u32 = 1;
pub const BASE_INTERFACE_MIN: u32 = 1;
}
pub mod syscall {
pub mod numbers {
pub const SYSCALL_PRIVATE_BASE: u64 = 1024;
pub const SYSCALL_PRIVATE_END: u64 = 1032;
/// `only_call()`.
pub const SYSCALL_ONLY_CALL: u64 = SYSCALL_PRIVATE_BASE + 0;
}
}
"#;

/// The private table names a constant nothing defines: only `SYSCALL_TABLE`
/// is read.
const HANDLERS: &str = r#"use slopos_abi::syscall::*;

/// Linux-numbered entry points.
static SYSCALL_TABLE: [SyscallEntry; SYSCALL_TABLE_SIZE] = syscall_table! {
    size: SYSCALL_TABLE_SIZE;
    base: 0;

    [SYSCALL_READ] => syscall_read, "read";
};

static SYSCALL_PRIVATE_TABLE: [SyscallEntry; SYSCALL_PRIVATE_TABLE_SIZE] = syscall_table! {
    size: SYSCALL_PRIVATE_TABLE_SIZE;
    base: SYSCALL_PRIVATE_BASE;

    [SYSCALL_WIDGET_GET] => syscall_widget_get, "widget_get";
    [SYSCALL_NOT_DEFINED] => syscall_not_defined, "not_defined";
};
"#;

const READ_ROW: &str = r#"    [SYSCALL_READ] => syscall_read, "read";"#;

const FLOORS: Floors = Floors {
    calls: 3,
    lines: 10,
    linux: 1,
};

const ROOTS: &str = "# fixture\nroot slopos_abi::flags::FLAG_*\n";

struct Fixture {
    tree: Tree,
    doc: Value,
}

/// `LIB` with each exact line `from` replaced by `to`, in order.
fn variant(edits: &[(&str, &str)]) -> String {
    let mut text = LIB.to_string();
    for (from, to) in edits {
        let mut hit = false;
        let lines: Vec<&str> = text
            .lines()
            .map(|l| {
                if l == *from {
                    hit = true;
                    to
                } else {
                    l
                }
            })
            .collect();
        assert!(hit, "variant: no line {from:?}");
        text = lines.join("\n") + "\n";
    }
    text
}

fn handlers(edits: &[(&str, &str)]) -> String {
    let mut text = HANDLERS.to_string();
    for (from, to) in edits {
        assert!(text.contains(from), "handlers: no {from:?}");
        text = text.replace(from, to);
    }
    text
}

fn document(lib: &str) -> Fixture {
    static CHANNEL: LazyLock<String> = LazyLock::new(|| {
        toolchain::channel(&Repo::at(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))).unwrap()
    });
    let tree = Tree::new(&[("lib.rs", lib)]);
    let out = toolchain::rustdoc(&CHANNEL)
        .current_dir(&tree.repo.root)
        .args([
            "--edition",
            "2024",
            "--crate-type",
            "lib",
            "--crate-name",
            "slopos_abi",
            "-Zunstable-options",
            "--output-format",
            "json",
            "--document-private-items",
            "--cap-lints",
            "allow",
            "-o",
            "out",
            "lib.rs",
        ])
        .output()
        .expect("run rustdoc");
    assert!(
        out.status.success(),
        "the fixture does not document:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json = tree.repo.read("out/slopos_abi.json").unwrap();
    Fixture {
        doc: serde_json::from_str(&json).unwrap(),
        tree,
    }
}

impl Fixture {
    fn scan_in(&self, gate: &str, handlers: &str) -> Result<Scan> {
        scan(&Inputs {
            doc: &self.doc,
            src_root: &self.tree.repo.root,
            gate: ("gate.txt", gate),
            handlers: ("handlers.rs", handlers),
            floors: &FLOORS,
        })
    }

    fn check_in(&self, gate: &str, handlers: &str) -> Scan {
        let mut scan = self.scan_in(gate, handlers).unwrap();
        scan.judge();
        scan
    }

    fn check(&self, gate: &str) -> Scan {
        self.check_in(gate, HANDLERS)
    }

    /// `--record` against a gate file holding `gate`; the file afterwards.
    fn record(&self, gate: &str) -> (bool, String) {
        self.tree.write("gate.txt", gate);
        let scan = self.scan_in(gate, HANDLERS).unwrap();
        let ok = record(&scan, &self.tree.repo.path("gate.txt")).unwrap();
        (ok, self.tree.repo.read("gate.txt").unwrap())
    }
}

/// The roots with the base fixture's surface recorded as interface 1.
static GATE: LazyLock<String> = LazyLock::new(|| {
    let (ok, text) = document(LIB).record(ROOTS);
    assert!(ok);
    text
});

fn gate() -> &'static str {
    &GATE
}

fn count(scan: &Scan, tag: Tag) -> usize {
    scan.hits.iter().filter(|h| h.tag == tag).count()
}

fn assert_only(scan: &Scan, tag: Tag, n: usize) {
    assert_eq!(count(scan, tag), n, "{tag} findings: {:#?}", scan.hits);
    assert_eq!(scan.hits.len(), n, "other findings: {:#?}", scan.hits);
}

fn assert_silent(scan: &Scan) {
    assert!(scan.hits.is_empty(), "findings: {:#?}", scan.hits);
}

fn rows(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|l| l.starts_with("interface "))
        .collect()
}

#[test]
fn record_appends_the_next_row_once_and_print_shows_the_surface() {
    let base = document(LIB);
    let (ok, text) = base.record(ROOTS);
    assert!(ok);
    let r = rows(&text);
    assert_eq!(r.len(), 1);
    let digest = r[0].strip_prefix("interface 1 ").unwrap();
    assert!(digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()));
    let (ok, again) = base.record(&text);
    assert!(ok);
    assert_eq!(
        again, text,
        "recording an already-recorded surface changed the ledger"
    );

    let scan = base.scan_in(&text, HANDLERS).unwrap();
    assert!(scan.lines.iter().any(|l| l
        == "call 1024 SYSCALL_WIDGET_GET widget_get(out: *mut Widget, max) -> entries written"));
    let linux: Vec<&String> = scan
        .lines
        .iter()
        .filter(|l| l.starts_with("linux "))
        .collect();
    assert_eq!(linux, ["linux 0 SYSCALL_READ"]);
    assert_eq!(scan.calls, 3);
    assert_eq!(scan.linux, 1);
}

#[test]
fn the_recorded_surface_passes() {
    assert_silent(&document(LIB).check(gate()));
}

#[test]
fn prose_formatting_and_order_leave_the_digest() {
    let cases: [&[(&str, &str)]; 4] = [
        &[
            (
                "/// `knob_set(value: Knob) -> 0`.",
                "/// Sets the `knob` — `knob_set(value: Knob) -> 0` — and says more about it.",
            ),
            (
                "/// A widget.",
                "/// A widget, described at greater length.",
            ),
        ],
        &[
            (
                "pub fn encode(m: Mode) -> u8 { m as u8 }",
                "pub fn encode(m: Mode)->u8 {\n    // why it is a cast\n    m as u8 /* a block comment */\n}",
            ),
            ("pub a: u16,", "pub a: u16, // a comment"),
            (
                "pub const FLAG_Y: u32 = 1 << 1;",
                "pub const FLAG_Y: u32 =\n    1<<1;",
            ),
            (
                "pub const SIZE: usize = 8 + INNER_LEN;",
                "pub const SIZE: usize = 8+INNER_LEN; // why",
            ),
        ],
        &[
            ("pub const FLAG_X: u32 = 1 << 0;", "@@"),
            (
                "pub const FLAG_Y: u32 = 1 << 1;",
                "pub const FLAG_X: u32 = 1 << 0;",
            ),
            ("@@", "pub const FLAG_Y: u32 = 1 << 1;"),
            ("pub const INNER_LEN: usize = 4;", ""),
            (
                "pub mod widget {",
                "pub mod widget {\npub const INNER_LEN: usize = 4;",
            ),
        ],
        &[("On = 1,", "On,")],
    ];
    for edits in cases {
        assert_silent(&document(&variant(edits)).check(gate()));
    }
}

#[test]
fn what_the_surface_does_not_reach_is_free_until_a_root_names_it() {
    let outside = document(&variant(&[
        ("pub size: u64,", "pub size: u32,"),
        ("pub const OTHER: u32 = 9;", "pub const OTHER: u32 = 10;"),
        (
            "pub fn helper(&self) -> u32 { self.id }",
            "pub fn helper(&self) -> u32 { self.id + 1 }",
        ),
    ]));
    assert_silent(&outside.check(gate()));

    let method = format!("root slopos_abi::widget::Widget::helper\n{ROOTS}");
    let (ok, method) = document(LIB).record(&method);
    assert!(ok);
    assert_only(&outside.check(&method), Tag::Bump, 1);
}

#[test]
fn every_reached_shape_change_is_a_bump() {
    let cases: [(&str, &[(&str, &str)]); 13] = [
        ("a nested field's type", &[("pub a: u16,", "pub a: u32,")]),
        (
            "two fields swapped",
            &[
                ("pub id: u32,", "@@"),
                ("pub inner: Inner,", "pub id: u32,"),
                ("@@", "pub inner: Inner,"),
            ],
        ),
        (
            "a new private call",
            &[(
                "pub const SYSCALL_KNOB_SET: u64 = SYSCALL_PRIVATE_BASE + 2;",
                "pub const SYSCALL_KNOB_SET: u64 = SYSCALL_PRIVATE_BASE + 2;\n/// `dial_set(level)`.\npub const SYSCALL_DIAL_SET: u64 = SYSCALL_PRIVATE_BASE + 3;",
            )],
        ),
        (
            "a signature gained an argument",
            &[(
                "/// `knob_set(value: Knob) -> 0`.",
                "/// `knob_set(value: Knob, extra) -> 0`.",
            )],
        ),
        (
            "a discriminant in a linked module",
            &[("On = 1,", "On = 2,")],
        ),
        (
            "a glob-rooted constant",
            &[(
                "pub const FLAG_X: u32 = 1 << 0;",
                "pub const FLAG_X: u32 = 1 << 2;",
            )],
        ),
        (
            "a reached function's body",
            &[(
                "pub fn encode(m: Mode) -> u8 { m as u8 }",
                "pub fn encode(m: Mode) -> u8 { m as u8 ^ 1 }",
            )],
        ),
        (
            "a reached static's value",
            &[(
                "pub static TABLE: [u8; 2] = [1, 2];",
                "pub static TABLE: [u8; 2] = [1, 3];",
            )],
        ),
        (
            "an alias retargeted",
            &[("pub type Knob = u32;", "pub type Knob = u64;")],
        ),
        ("an enum's repr", &[("#[repr(u8)]", "#[repr(u16)]")]),
        (
            "a union field reached through a struct",
            &[("pub bytes: [u8; 4],", "pub bytes: [u8; 2],")],
        ),
        (
            "an inherent constant of a reached type",
            &[(
                "pub const SIZE: usize = 8 + INNER_LEN;",
                "pub const SIZE: usize = 9 + INNER_LEN;",
            )],
        ),
        (
            "an implicit discriminant following an explicit one",
            &[("Off = 0,", "Off = 3,"), ("On = 1,", "On,")],
        ),
    ];
    for (what, edits) in cases {
        let scan = document(&variant(edits)).check(gate());
        assert_eq!(count(&scan, Tag::Bump), 1, "{what}: {:#?}", scan.hits);
        assert_eq!(scan.hits.len(), 1, "{what}: {:#?}", scan.hits);
    }
}

#[test]
fn record_never_rewrites_a_row() {
    let nested = document(&variant(&[("pub a: u16,", "pub a: u32,")]));
    let (ok, after) = nested.record(gate());
    assert!(!ok);
    assert_eq!(after, gate());
}

#[test]
fn a_bump_without_a_row_is_unrecorded_and_record_appends_it() {
    let bumped = document(&variant(&[(
        "pub const BASE_INTERFACE: u32 = 1;",
        "pub const BASE_INTERFACE: u32 = 2;",
    )]));
    assert_only(&bumped.check(gate()), Tag::Unrecorded, 1);

    let next = document(&variant(&[
        ("pub a: u16,", "pub a: u32,"),
        (
            "pub const BASE_INTERFACE: u32 = 1;",
            "pub const BASE_INTERFACE: u32 = 2;",
        ),
    ]));
    let (ok, after) = next.record(gate());
    assert!(ok);
    let r = rows(&after);
    assert_eq!(r.len(), 2);
    assert_eq!(after.strip_suffix(&format!("{}\n", r[1])), Some(gate()));
    assert!(r[1].starts_with("interface 2 "));
    assert_silent(&next.check(&after));
}

#[test]
fn a_call_without_a_signature_is_found_and_suspends_the_digest() {
    let nosig = document(&variant(&[(
        "/// `knob_set(value: Knob) -> 0`.",
        "/// Sets the knob.",
    )]));
    assert_only(&nosig.check(gate()), Tag::Signature, 1);
}

#[test]
fn a_root_that_renders_nothing_is_dead() {
    let base = document(LIB);
    for root in [
        "root slopos_abi::nothing::*",
        "root slopos_abi::gadget::Mode::On",
        "root slopos_abi::base_interface::*",
    ] {
        let scan = base.check(&format!("{root}\n{}", gate()));
        assert_only(&scan, Tag::Dead, 1);
        assert_eq!(scan.hits[0].location, "gate.txt:1");
    }
}

#[test]
fn a_signature_type_must_name_one_item() {
    let scan = document(&variant(&[
        (
            "/// `knob_set(value: Knob) -> 0`.",
            "/// `knob_set(level, value: Knobb) -> Modee`.",
        ),
        (
            "pub size: u64,",
            "pub size: u64,\n}\npub struct Widget {\npub other: u8,",
        ),
    ]))
    .check(gate());
    assert_eq!(count(&scan, Tag::Unresolved), 3, "{:#?}", scan.hits);
    assert_eq!(count(&scan, Tag::Bump), 0);
}

#[test]
fn the_ledger_runs_from_one_without_gaps() {
    let base = document(LIB);
    let badmin = document(&variant(&[(
        "pub const BASE_INTERFACE_MIN: u32 = 1;",
        "pub const BASE_INTERFACE_MIN: u32 = 2;",
    )]));
    assert_only(&badmin.check(gate()), Tag::Ledger, 1);

    let zeros = "0".repeat(64);
    let bad = format!("{}interface 3 {zeros}\ninterface two\n", gate());
    assert_only(&base.check(&bad), Tag::Ledger, 2);
    let dup = format!("{}interface 1 {zeros}\n", gate());
    assert_only(&base.check(&dup), Tag::Ledger, 1);
    let ahead = format!("{}interface 2 {zeros}\n", gate());
    assert_only(&base.check(&ahead), Tag::Ledger, 1);

    let skip = document(&variant(&[(
        "pub const BASE_INTERFACE: u32 = 1;",
        "pub const BASE_INTERFACE: u32 = 3;",
    )]));
    assert_only(&skip.check(gate()), Tag::Ledger, 1);
    let badbump = document(&variant(&[
        ("/// `knob_set(value: Knob) -> 0`.", "/// Sets the knob."),
        (
            "pub const BASE_INTERFACE: u32 = 1;",
            "pub const BASE_INTERFACE: u32 = 2;",
        ),
    ]));
    for refused in [&skip, &badbump] {
        let (ok, after) = refused.record(gate());
        assert!(!ok);
        assert_eq!(after, gate());
    }
}

#[test]
fn unreadable_interface_constants_are_meta() {
    let nometa = document(&variant(&[
        (
            "pub const SYSCALL_PRIVATE_END: u64 = SYSCALL_PRIVATE_BASE + (SYSCALL_PRIVATE_TABLE_SIZE as u64);",
            "",
        ),
        ("pub const BASE_INTERFACE_MIN: u32 = 1;", ""),
    ]));
    let scan = nometa.check(gate());
    assert_eq!(count(&scan, Tag::Meta), 2, "{:#?}", scan.hits);
}

#[test]
fn a_thin_crate_and_an_empty_table_are_under_the_floors() {
    let thin = document(THIN);
    let scan = thin.check_in("# empty\n", &handlers(&[(READ_ROW, "")]));
    assert_eq!(count(&scan, Tag::Floor), 3, "{:#?}", scan.hits);
}

#[test]
fn an_unknown_format_or_shape_cannot_run() {
    let mut base = document(LIB);
    base.doc["format_version"] = Value::from(1);
    let err = base.scan_in(gate(), HANDLERS).err().unwrap();
    assert!(err.to_string().contains("format_version is 1"), "{err}");

    let mut base = document(LIB);
    for item in base.doc["index"].as_object_mut().unwrap().values_mut() {
        item.as_object_mut().unwrap().remove("attrs");
    }
    assert!(base.scan_in(gate(), HANDLERS).is_err());
}

#[test]
fn a_call_the_kernel_gains_moves_the_digest() {
    let table = handlers(&[(
        READ_ROW,
        "    [SYSCALL_READ] => syscall_read, \"read\";\n    [SYSCALL_WRITE] => syscall_write, \"write\";",
    )]);
    let base = document(LIB);
    let scan = base.check_in(gate(), &table);
    assert_only(&scan, Tag::Bump, 1);
    assert!(scan.lines.iter().any(|l| l == "linux 1 SYSCALL_WRITE"));

    let renamed = handlers(&[(READ_ROW, "    [SYSCALL_READ] => sys_read_v2, \"read2\";")]);
    assert_silent(&base.check_in(gate(), &renamed));
}

#[test]
fn a_linux_row_must_name_an_evaluated_linux_number() {
    let base = document(LIB);
    let unknown = handlers(&[(
        READ_ROW,
        "    [SYSCALL_READ] => syscall_read, \"read\";\n    [SYSCALL_NOPE] => syscall_nope, \"nope\";",
    )]);
    let scan = base.check_in(gate(), &unknown);
    assert_only(&scan, Tag::Unresolved, 1);
    assert_eq!(scan.hits[0].location, "handlers.rs:9");

    let private = handlers(&[(
        READ_ROW,
        "    [SYSCALL_READ] => syscall_read, \"read\";\n    [SYSCALL_KNOB_SET] => syscall_knob_set, \"knob_set\";",
    )]);
    assert_only(&base.check_in(gate(), &private), Tag::Range, 1);

    let mut unevaluated = document(LIB);
    let index = unevaluated.doc["index"].as_object_mut().unwrap();
    for item in index.values_mut() {
        if item["name"] == "SYSCALL_READ" {
            item["inner"]["constant"]["const"]["value"] = Value::Null;
        }
    }
    let scan = unevaluated.check(gate());
    assert_eq!(count(&scan, Tag::Unresolved), 1, "{:#?}", scan.hits);
}

#[test]
fn a_linux_table_not_read_as_one_is_meta() {
    let base = document(LIB);
    let cases = [
        handlers(&[("static SYSCALL_TABLE:", "static SYSCALL_TABLE_OLD:")]),
        handlers(&[("base: 0;", "base: 1;")]),
        handlers(&[(READ_ROW, "    [SYSCALL_READ] => syscall_read;")]),
        handlers(&[(
            "static SYSCALL_TABLE:",
            "#[cfg(not(test))]\nstatic SYSCALL_TABLE:",
        )]),
        handlers(&[("static SYSCALL_PRIVATE_TABLE:", "static SYSCALL_TABLE:")]),
    ];
    for table in cases {
        let scan = base.check_in(gate(), &table);
        assert_eq!(count(&scan, Tag::Meta), 1, "{:#?}", scan.hits);
        assert_eq!(count(&scan, Tag::Bump), 0);
    }
}
