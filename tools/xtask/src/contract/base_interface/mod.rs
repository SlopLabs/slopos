//! The call surface, held to `slopos_abi::BASE_INTERFACE`.
//!
//! A base is built against one interface number and a kernel serves a range
//! of them; the number is the only thing that says whether the two can meet.
//! That promise is worth nothing if the private calls can change shape while
//! the number stays: the base and the kernel would agree on 12 and disagree
//! on what 12 means, and nothing in either build would notice.
//!
//! So the surface is digested from the compiler's own view of slopos-abi
//! (rustdoc JSON): every SlopOS-private syscall's number and the signature its
//! docs give in a `name(...)` code span; every abi item a signature names or a
//! call's docs link; the `root` globs in `scripts/gates/base-interface.txt`
//! for what a call reads or writes behind a byte buffer or an op word; and
//! every item those reach through field, variant, alias and constant types.
//! Each item is rendered without docs or rustdoc ids (structs with repr and
//! fields in order, enums with discriminants, constants with their evaluated
//! value, functions and statics as their source tokens), so prose and
//! formatting never move the digest. `slopos_abi::base_interface` is never
//! rendered: the number cannot digest itself. A glob is xtask's (`*`, `?`,
//! `[...]`, `[!...]` and `[^...]` negating, `\` escaping).
//!
//! The surface also holds the Linux-numbered calls the kernel answers, one
//! `linux <number> <SYSCALL_NAME>` line each: the rows of the `SYSCALL_TABLE`
//! static in `core/src/syscall/handlers.rs` (its `syscall_table!`, not the
//! private table), each constant evaluated through
//! `slopos_abi::syscall::numbers`. A base that starts using a call the kernel
//! has just gained would otherwise install live on a kernel without it. The
//! handler and its capability are the kernel's business and stay out.
//!
//! The ledger in the gate file records `interface <n> <sha256>` for every
//! number ever issued.
//!
//! The checks:
//!
//! - bump: the ledger's row for `BASE_INTERFACE` has another digest: the
//!   surface changed without a new number.
//! - unrecorded: `BASE_INTERFACE` has no row: bumped, not yet recorded.
//! - ledger: a malformed or out-of-order row or `root` line, a row past
//!   `BASE_INTERFACE`, a number skipped, or `BASE_INTERFACE_MIN` outside
//!   `1..=BASE_INTERFACE`.
//! - signature: a private call whose docs carry no `name(...)` span, or one
//!   whose parentheses do not balance.
//! - unresolved: a capitalised identifier in a signature that names no abi
//!   item, or more than one; a Linux table row whose constant `numbers` does
//!   not define or does not evaluate.
//! - range: a Linux table row at or above `SYSCALL_PRIVATE_BASE`.
//! - dead: a `root` glob that reaches nothing the surface renders.
//! - meta: `SYSCALL_PRIVATE_BASE`/`END` or `BASE_INTERFACE(_MIN)` unreadable,
//!   or no `SYSCALL_TABLE` read as a Linux-numbered `syscall_table!`.
//! - floor: fewer than 20 private calls, 200 surface lines besides the Linux
//!   ones, or 100 answered Linux calls: a parse that read nothing would
//!   otherwise digest an empty surface forever.
//!
//! bump and unrecorded are judged only on a surface with no other finding. A
//! rustdoc JSON `format_version` other than [`FORMAT_VERSION`], or a shape
//! the reader does not expect, cannot run (exit 2): a toolchain bump
//! re-validates the reader rather than shifting digests.
//!
//! `--record` appends `interface <BASE_INTERFACE> <digest>` when that is the
//! ledger's next row and refuses anything else (a recorded row is never
//! rewritten); `--print` writes the surface lines, to diff before and after a
//! change, and passes with any finding as a warning on stderr.

mod linux;
mod rustdoc;
mod signature;
mod surface;
mod text;
mod tokens;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use serde_json::Value;

use self::rustdoc::{CRATE, Crate};
use self::surface::{Root, Surface};
use self::text::Int;
use crate::Result;
use crate::repo::Repo;
use crate::report::Report;
use crate::toolchain;

const GATE: &str = "base-interface";
const GATE_FILE: &str = "scripts/gates/base-interface.txt";
const HANDLERS_RS: &str = "core/src/syscall/handlers.rs";
const TARGET_DIR: &str = "builddir/gates/base-interface";
const JSON: &str = "builddir/gates/base-interface/doc/slopos_abi.json";

/// The rustdoc JSON format the reader was validated against.
pub const FORMAT_VERSION: u64 = 61;

pub struct Floors {
    pub calls: usize,
    pub lines: usize,
    pub linux: usize,
}

const FLOORS: Floors = Floors {
    calls: 20,
    lines: 200,
    linux: 100,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    Bump,
    Unrecorded,
    Ledger,
    Signature,
    Unresolved,
    Range,
    Dead,
    Meta,
    Floor,
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Tag::Bump => "bump",
            Tag::Unrecorded => "unrecorded",
            Tag::Ledger => "ledger",
            Tag::Signature => "signature",
            Tag::Unresolved => "unresolved",
            Tag::Range => "range",
            Tag::Dead => "dead",
            Tag::Meta => "meta",
            Tag::Floor => "floor",
        })
    }
}

#[derive(Debug)]
pub struct Hit {
    pub tag: Tag,
    pub location: String,
    pub message: String,
}

/// What the gate reads: rustdoc's JSON of slopos-abi, the checkout the JSON's
/// spans are relative to, the gate file and the kernel's dispatch tables, each
/// as `(path, text)`.
pub struct Inputs<'a> {
    pub doc: &'a Value,
    pub src_root: &'a Path,
    pub gate: (&'a str, &'a str),
    pub handlers: (&'a str, &'a str),
    pub floors: &'a Floors,
}

pub struct Scan {
    pub hits: Vec<Hit>,
    pub lines: Vec<String>,
    pub calls: usize,
    pub linux: usize,
    pub digest: String,
    pub current: Option<Int>,
    pub recorded: BTreeMap<Int, String>,
    pub rows: usize,
}

impl Scan {
    /// bump and unrecorded, judged only on a surface read in full.
    pub fn judge(&mut self) {
        if !self.hits.is_empty() {
            return;
        }
        let Some(current) = &self.current else {
            return;
        };
        let (tag, message) = match self.recorded.get(current) {
            Some(d) if *d == self.digest => return,
            Some(d) => (
                Tag::Bump,
                format!(
                    "interface {current} is recorded as {d}, but the surface now digests to {}",
                    self.digest
                ),
            ),
            None => (
                Tag::Unrecorded,
                format!(
                    "BASE_INTERFACE = {current} has no ledger row; the surface digests to {}",
                    self.digest
                ),
            ),
        };
        self.hits.push(Hit {
            tag,
            location: GATE_FILE.to_string(),
            message,
        });
    }

    fn report(&self) -> Report {
        let mut report = Report::new();
        for hit in &self.hits {
            report.finding(
                hit.location.clone(),
                format!("{}: {}", hit.tag, hit.message),
            );
        }
        let current = self
            .current
            .as_ref()
            .map_or_else(|| "?".to_string(), Int::to_string);
        report.summary(format!(
            "{} private calls, {} Linux calls, {} surface lines in all, digest {}…, the ledger's row for \
             interface {current}",
            self.calls,
            self.linux,
            self.lines.len(),
            &self.digest[..16]
        ));
        report
    }
}

pub fn scan(inputs: &Inputs) -> Result<Scan> {
    let doc = inputs.doc;
    let version = doc.get("format_version");
    if version.and_then(Value::as_u64) != Some(FORMAT_VERSION) {
        return Err(format!(
            "rustdoc JSON format_version is {}, the reader was written against {FORMAT_VERSION}. \
             A toolchain bump must re-validate tools/xtask/src/contract/base_interface (its \
             tests, then --print before and after) and update FORMAT_VERSION; otherwise the \
             digest could change, or stop seeing changes, silently",
            version.map_or_else(|| "missing".to_string(), Value::to_string)
        )
        .into());
    }
    let root = rustdoc::field(doc, "index")?.get(rustdoc::field(doc, "root")?.to_string());
    let root_name = root.and_then(|r| r.get("name")).and_then(Value::as_str);
    if root_name != Some(CRATE) {
        return Err(format!("the rustdoc JSON documents {root_name:?}, not {CRATE}").into());
    }
    let crate_ = Crate::new(doc, inputs.src_root)?;
    let (gate_path, gate_text) = inputs.gate;
    let (roots, rows, mut hits) = read_gate(gate_path, gate_text);
    let mut surface = Surface::read(&crate_, gate_path, &roots)?;
    hits.append(&mut surface.hits);

    if surface.calls < inputs.floors.calls {
        hits.push(Hit {
            tag: Tag::Floor,
            location: CRATE.to_string(),
            message: format!(
                "{} private calls, fewer than {}: the parse missed numbers.rs",
                surface.calls, inputs.floors.calls
            ),
        });
    }
    if surface.lines.len() < inputs.floors.lines {
        hits.push(Hit {
            tag: Tag::Floor,
            location: CRATE.to_string(),
            message: format!(
                "{} surface lines besides the Linux calls, fewer than {}: the closure reached \
                 almost nothing",
                surface.lines.len(),
                inputs.floors.lines
            ),
        });
    }
    let linux = linux_lines(inputs.handlers, &surface, &crate_, inputs.floors, &mut hits)?;
    ledger(
        gate_path,
        &rows,
        surface.base_interface.as_ref(),
        surface.base_interface_min.as_ref(),
        &mut hits,
    );

    let linux_calls = linux.len();
    let mut lines = surface.lines;
    lines.extend(linux);
    let lines: Vec<String> = lines.into_iter().collect();
    let digest = hex(&slopos_verity_core::sha256::digest(
        lines.join("\n").as_bytes(),
    ));
    Ok(Scan {
        hits,
        lines,
        calls: surface.calls,
        linux: linux_calls,
        digest,
        current: surface.base_interface,
        recorded: rows
            .iter()
            .map(|r| (r.number.clone(), r.digest.clone()))
            .collect(),
        rows: rows.len(),
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `linux <number> <SYSCALL_NAME>` for each call the Linux-numbered table
/// answers.
fn linux_lines(
    (path, source): (&str, &str),
    surface: &Surface,
    crate_: &Crate,
    floors: &Floors,
    hits: &mut Vec<Hit>,
) -> Result<BTreeSet<String>> {
    let mut hit = |tag, location: String, message: String| {
        hits.push(Hit {
            tag,
            location,
            message,
        })
    };
    let mut lines = BTreeSet::new();
    match linux::rows(path, source)? {
        Err(why) => hit(
            Tag::Meta,
            path.to_string(),
            format!("{why}: the Linux-numbered dispatch table could not be read"),
        ),
        Ok(rows) => {
            let numbers = surface::NUMBERS.join("::");
            for row in rows {
                let at = format!("{path}:{}", row.line);
                let name = &row.constant;
                let Some(&ident) = surface.numbers.get(name) else {
                    hit(
                        Tag::Unresolved,
                        at,
                        format!("`[{name}]` names no constant in {numbers}"),
                    );
                    continue;
                };
                let Some(number) = surface::const_value(crate_.require(ident)?)? else {
                    hit(
                        Tag::Unresolved,
                        at,
                        format!("`[{name}]`: {numbers}::{name} does not evaluate to an integer"),
                    );
                    continue;
                };
                if let Some(base) = &surface.private_base
                    && number >= *base
                {
                    hit(
                        Tag::Range,
                        at,
                        format!(
                            "`[{name}]` = {number} is at or above SYSCALL_PRIVATE_BASE = {base}: \
                             a private call answered from the Linux-numbered table"
                        ),
                    );
                    continue;
                }
                lines.insert(format!("linux {number} {name}"));
            }
        }
    }
    if lines.len() < floors.linux {
        hit(
            Tag::Floor,
            path.to_string(),
            format!(
                "{} Linux calls answered, fewer than {}: the read missed {}",
                lines.len(),
                floors.linux,
                linux::TABLE
            ),
        );
    }
    Ok(lines)
}

struct Row {
    line: usize,
    number: Int,
    digest: String,
}

fn read_gate(path: &str, raw: &str) -> (Vec<Root>, Vec<Row>, Vec<Hit>) {
    let (mut roots, mut rows, mut hits) = (Vec::new(), Vec::new(), Vec::new());
    let mut bad = |line: usize, message: String| {
        hits.push(Hit {
            tag: Tag::Ledger,
            location: format!("{path}:{line}"),
            message,
        })
    };
    for (i, line) in text::universal_newlines(raw).split('\n').enumerate() {
        let lineno = i + 1;
        let t = text::strip(line);
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some(rest) = t.strip_prefix("root ") {
            let glob = text::strip(rest);
            if glob.is_empty() || glob.contains(' ') {
                bad(lineno, format!("malformed root line: {t}"));
            } else {
                roots.push(Root {
                    line: lineno,
                    glob: glob.to_string(),
                });
            }
            continue;
        }
        let row = t.strip_prefix("interface ").and_then(|rest| {
            let (n, d) = rest.split_once(' ')?;
            let digest_ok = d.len() == 64
                && d.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            digest_ok.then_some(())?;
            Some((Int::digits(n)?, d.to_string()))
        });
        match row {
            Some((number, digest)) => rows.push(Row {
                line: lineno,
                number,
                digest,
            }),
            None => bad(
                lineno,
                format!("not a `root <glob>` or `interface <n> <sha256>` line: {t}"),
            ),
        }
    }
    (roots, rows, hits)
}

fn ledger(path: &str, rows: &[Row], bi: Option<&Int>, min: Option<&Int>, hits: &mut Vec<Hit>) {
    let mut bad = |location: String, message: String| {
        hits.push(Hit {
            tag: Tag::Ledger,
            location,
            message,
        })
    };
    for (i, row) in rows.iter().enumerate() {
        let expect = Int::from_u64(i as u64 + 1);
        let at = format!("{path}:{}", row.line);
        if row.number != expect {
            bad(
                at,
                format!(
                    "`interface {}` where the ledger's next row is {expect} — rows run 1, 2, 3 … \
                     oldest first",
                    row.number
                ),
            );
        } else if let Some(bi) = bi
            && row.number > *bi
        {
            bad(
                at,
                format!("`interface {}` is past BASE_INTERFACE = {bi}", row.number),
            );
        }
    }
    if let (Some(bi), Some(min)) = (bi, min)
        && !(Int::from_u64(1) <= *min && min <= bi)
    {
        bad(
            CRATE.to_string(),
            format!("BASE_INTERFACE_MIN = {min} is outside 1..={bi}"),
        );
    }
    let next = Int::from_u64(rows.len() as u64 + 1);
    if let Some(bi) = bi
        && *bi > next
    {
        bad(
            CRATE.to_string(),
            format!("BASE_INTERFACE = {bi} skips past the ledger's next number {next}"),
        );
    }
}

/// Append the ledger's row for `BASE_INTERFACE` to `gate`, the file `scan`
/// read; true when the ledger holds this surface afterwards.
pub fn record(scan: &Scan, gate: &Path) -> Result<bool> {
    if !scan.hits.is_empty() {
        print!("{}", scan.report().render(GATE));
        eprintln!("{GATE}: refusing to record a surface the gate does not accept");
        return Ok(false);
    }
    let Some(current) = &scan.current else {
        return Err("BASE_INTERFACE unread on a surface with no finding".into());
    };
    match scan.recorded.get(current) {
        Some(d) if *d == scan.digest => {
            eprintln!(
                "{GATE}: interface {current} is already recorded with this surface; nothing to do"
            );
            Ok(true)
        }
        Some(d) => {
            eprintln!(
                "{GATE}: interface {current} is already recorded with another surface ({}…, now \
                 {}…). A recorded row is never rewritten: this surface needs BASE_INTERFACE = {}",
                &d[..12],
                &scan.digest[..12],
                scan.rows + 1
            );
            Ok(false)
        }
        None => {
            let at = |e: std::io::Error| format!("{}: {e}", gate.display());
            let data = std::fs::read(gate).map_err(at)?;
            let lead = if data.is_empty() || data.ends_with(b"\n") {
                ""
            } else {
                "\n"
            };
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(gate)
                .map_err(at)?;
            std::io::Write::write_all(
                &mut file,
                format!("{lead}interface {current} {}\n", scan.digest).as_bytes(),
            )
            .map_err(at)?;
            eprintln!("{GATE}: recorded interface {current} {}", scan.digest);
            Ok(true)
        }
    }
}

enum Mode {
    Check,
    Record,
    Print,
}

/// `xtask base-interface [--record | --print]`.
pub fn main(repo: &Repo, args: &[String]) -> Result<bool> {
    let mode = match args {
        [] => Mode::Check,
        [a] if a == "--record" => Mode::Record,
        [a] if a == "--print" => Mode::Print,
        _ => return Err("usage: xtask base-interface [--record | --print]".into()),
    };
    let gate_text = repo.read(GATE_FILE)?;
    let handlers = repo.read(HANDLERS_RS)?;
    if gate_text.trim().is_empty() {
        return Err(format!("{GATE_FILE} is empty").into());
    }
    let doc = rustdoc_json(repo)?;
    let mut scan = scan(&Inputs {
        doc: &doc,
        src_root: &repo.root,
        gate: (GATE_FILE, &gate_text),
        handlers: (HANDLERS_RS, &handlers),
        floors: &FLOORS,
    })?;
    match mode {
        Mode::Print => {
            let mut out = scan.lines.join("\n");
            if !out.is_empty() {
                out.push('\n');
            }
            print!("{out}");
            for hit in &scan.hits {
                eprintln!(
                    "{GATE}: warning: {}: {}: {}",
                    hit.location, hit.tag, hit.message
                );
            }
            Ok(true)
        }
        Mode::Record => record(&scan, &repo.path(GATE_FILE)),
        Mode::Check => {
            scan.judge();
            let report = scan.report();
            print!("{}", report.render(GATE));
            if scan.hits.iter().any(|h| h.tag == Tag::Bump) {
                eprintln!(
                    "\n  A base built against BASE_INTERFACE trusts every call in the surface \
                     to keep\n  the shape it had when that number was recorded. To change the \
                     surface: bump\n  BASE_INTERFACE in abi/src/base_interface.rs, then run\n  \
                     `just base-interface --record`. Before v1 BASE_INTERFACE_MIN follows the\n  \
                     number, so the change weighs no older base. To see what moved, compare\n  \
                     `just base-interface --print` before and after it."
                );
            } else if scan.hits.iter().any(|h| h.tag == Tag::Unrecorded) {
                eprintln!(
                    "\n  If the bump is deliberate, run `just base-interface --record` and \
                     commit\n  the new row."
                );
            }
            Ok(report.passed())
        }
    }
}

/// slopos-abi's rustdoc JSON, built into the gate's own target directory.
fn rustdoc_json(repo: &Repo) -> Result<Value> {
    let channel = toolchain::channel(repo)?;
    let out = toolchain::cargo(&channel)
        .current_dir(&repo.root)
        .args([
            "rustdoc",
            "--quiet",
            "--locked",
            "-p",
            "slopos-abi",
            "--lib",
            "--target-dir",
        ])
        .arg(repo.path(TARGET_DIR))
        .args([
            "--",
            "--cap-lints",
            "allow",
            "--document-private-items",
            "-Zunstable-options",
            "--output-format",
            "json",
        ])
        .output()
        .map_err(|e| format!("{} rustdoc: {e}", toolchain::shown(&channel)))?;
    if !out.status.success() {
        eprint!("{}", String::from_utf8_lossy(&out.stdout));
        eprint!("{}", String::from_utf8_lossy(&out.stderr));
        return Err(format!(
            "{} rustdoc on slopos-abi failed",
            toolchain::shown(&channel)
        )
        .into());
    }
    let json = repo.read(JSON)?;
    if json.is_empty() {
        return Err(format!("rustdoc wrote an empty {JSON}").into());
    }
    serde_json::from_str(&json).map_err(|e| format!("{JSON}: {e}").into())
}
