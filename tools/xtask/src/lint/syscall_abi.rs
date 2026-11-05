//! Every syscall number in `abi/src/syscall/numbers.rs` is Linux x86-64's own
//! allocation. SlopOS numbers its syscalls as Linux does, and that makes one
//! promise: a Linux number carries that call's Linux signature and semantics.
//! Put a different operation at 59, or let 262 drift onto `fstat`'s shape, and
//! every consumer that believes the number (a static binary, a strace-shaped
//! debugger, a libc port) is mis-taught, while both sides of the mistake are
//! this repository's code agreeing with itself. So the authority is outside
//! the tree: `scripts/gates/syscall/linux-x86_64.tbl`, the `common`/`64` rows
//! of Linux's `syscall_64.tbl` as `<number> <name>` pairs.
//!
//! Calls with no Linux analogue live at `SYSCALL_PRIVATE_BASE + k`, as ARM's
//! `__ARM_NR_BASE` does. The checks, on every top-level
//! `pub const SYSCALL_<NAME>: u64`:
//!
//! - name: a bare number below the private base is the number the table gives
//!   `lowercase(<NAME>)`.
//! - range: it is at most `SYSCALL_LINUX_MAX`, which equals the table's
//!   highest number, so the dispatch table is exactly the allocated space.
//! - private: the `k`s run contiguously from 0 (a hole is a dispatch slot
//!   answering nothing) and stay inside `SYSCALL_PRIVATE_TABLE_SIZE`.
//! - dup: no number and no constant name is defined twice.
//! - collide: a private constant whose name Linux also uses is either a call
//!   that belongs on its Linux number or one whose shape diverged and must say
//!   so in `scripts/gates/syscall/private-allowlist.txt` (`<name> <reason>`).
//! - dead: an allowlist entry exempts something and states a reason.
//! - shape: a value is a bare number or `SYSCALL_PRIVATE_BASE + k`.
//! - floor: fewer than 100 Linux-numbered constants, 20 private ones or 300
//!   table rows means a read that fell over, and fails.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use quote::ToTokens;
use syn::{BinOp, Expr, Item, Lit, Type, Visibility};

use crate::Result;
use crate::repo::Repo;
use crate::report::Report;
use crate::rust::{self, Source};

const NUMBERS_RS: &str = "abi/src/syscall/numbers.rs";
const LINUX_TBL: &str = "scripts/gates/syscall/linux-x86_64.tbl";
const PRIVATE_ALLOWLIST: &str = "scripts/gates/syscall/private-allowlist.txt";

const MIN_LINUX_CONSTS: usize = 100;
const MIN_PRIVATE_CONSTS: usize = 20;
const MIN_TABLE_ROWS: usize = 300;

pub fn run(repo: &Repo) -> Result<Report> {
    let read = |rel: &str| -> Result<String> {
        let text = repo.read(rel)?;
        if text.trim().is_empty() {
            return Err(
                format!("{rel} is empty — the join would compare nothing against nothing").into(),
            );
        }
        Ok(text)
    };
    let numbers = Source::parse(NUMBERS_RS, read(NUMBERS_RS)?)?;
    let scan = scan(
        &numbers,
        (LINUX_TBL, &read(LINUX_TBL)?),
        (PRIVATE_ALLOWLIST, &read(PRIVATE_ALLOWLIST)?),
    );
    let mut report = Report::new();
    for hit in &scan.hits {
        report.finding(
            hit.location.clone(),
            format!("{} — {}", hit.message, hit.tag.remedy()),
        );
    }
    report.summary(format!(
        "joined {} Linux-numbered and {} private constants in {NUMBERS_RS} against {} rows of \
         the Linux x86-64 table",
        scan.linux, scan.private, scan.rows
    ));
    Ok(report)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tag {
    Name,
    Range,
    Private,
    Dup,
    DupName,
    Collide,
    Dead,
    Shape,
    Meta,
    Table,
    Floor,
}

impl Tag {
    fn remedy(self) -> &'static str {
        match self {
            Tag::Name => {
                "a Linux number promises that call's Linux signature: move the call to the \
                 number Linux gave it, or give it a private number (SYSCALL_PRIVATE_BASE + k)"
            }
            Tag::Range => {
                "Linux numbers stop where Linux's allocation does, and SYSCALL_LINUX_MAX follows \
                 the vendored table's highest number"
            }
            Tag::Private => {
                "private numbers run SYSCALL_PRIVATE_BASE + 0, 1, 2, … with no hole, inside \
                 SYSCALL_PRIVATE_TABLE_SIZE"
            }
            Tag::Dup | Tag::DupName => "give each call one constant and one number",
            Tag::Collide => {
                "move the call to its Linux number, or say why it stays private in \
                 scripts/gates/syscall/private-allowlist.txt"
            }
            Tag::Dead => "delete the entry, or give it its reason",
            Tag::Shape => "write a bare Linux number or SYSCALL_PRIVATE_BASE + k",
            Tag::Meta => "define it as an integer literal",
            Tag::Table => {
                "the table comes from Linux's syscall_64.tbl; regenerate it with the recipe in \
                 its header rather than editing it to fit"
            }
            Tag::Floor => "the scan read too little to be believed",
        }
    }
}

#[derive(Debug)]
struct Hit {
    tag: Tag,
    location: String,
    message: String,
}

struct Scan {
    hits: Vec<Hit>,
    rows: usize,
    linux: usize,
    private: usize,
}

#[derive(Clone, Copy)]
enum Number {
    Linux(u64),
    Private(u64),
}

fn int(expr: &Expr) -> Option<u64> {
    match expr {
        Expr::Lit(e) => match &e.lit {
            Lit::Int(i) if matches!(i.suffix(), "" | "u64" | "usize") => i.base10_parse().ok(),
            _ => None,
        },
        _ => None,
    }
}

/// What a constant's value says, or its text when it says neither.
fn value(expr: &Expr) -> Result<Number, String> {
    if let Some(n) = int(expr) {
        return Ok(Number::Linux(n));
    }
    if let Expr::Binary(b) = expr
        && matches!(b.op, BinOp::Add(_))
        && matches!(&*b.left, Expr::Path(p) if p.qself.is_none() && p.path.is_ident("SYSCALL_PRIVATE_BASE"))
        && let Some(k) = int(&b.right)
    {
        return Ok(Number::Private(k));
    }
    Err(expr.to_token_stream().to_string())
}

/// A data file's lines with `#` comments stripped, blank ones dropped, as
/// `(1-based line, whitespace-split fields)`.
fn rows(text: &str) -> impl Iterator<Item = (usize, Vec<&str>)> {
    text.lines().enumerate().filter_map(|(i, line)| {
        let line = line.split('#').next().unwrap_or_default();
        let fields: Vec<&str> = line.split_whitespace().collect();
        (!fields.is_empty()).then_some((i + 1, fields))
    })
}

fn scan(
    numbers: &Source,
    (tbl_path, tbl): (&str, &str),
    (allow_path, allow): (&str, &str),
) -> Scan {
    let mut hits = Vec::new();
    let mut hit = |tag, location: String, message: String| {
        hits.push(Hit {
            tag,
            location,
            message,
        })
    };

    let mut table_name: HashMap<u64, &str> = HashMap::new();
    let mut table_num: HashMap<&str, u64> = HashMap::new();
    let mut table_max = None;
    let mut row_count = 0;
    for (line, f) in rows(tbl) {
        let at = format!("{tbl_path}:{line}");
        let num = match (f[0].bytes().all(|b| b.is_ascii_digit()), f.get(1)) {
            (true, Some(_)) => f[0].parse::<u64>().ok(),
            _ => None,
        };
        let Some(num) = num else {
            hit(Tag::Table, at, format!("unparseable row: {}", f.join(" ")));
            continue;
        };
        let name = f[1];
        if let Some(prev) = table_name.get(&num) {
            hit(
                Tag::Table,
                at,
                format!("number {num} appears twice ({prev}, {name})"),
            );
            continue;
        }
        table_name.insert(num, name);
        table_num.insert(name, num);
        table_max = table_max.max(Some(num));
        row_count += 1;
    }

    let mut allowed: Vec<(usize, &str)> = Vec::new();
    for (line, f) in rows(allow) {
        let at = format!("{allow_path}:{line}");
        if f.len() < 2 {
            hit(
                Tag::Dead,
                at,
                format!("entry \"{}\" states no reason", f[0]),
            );
        } else if allowed.iter().any(|(_, n)| *n == f[0]) {
            hit(Tag::Dead, at, format!("entry \"{}\" appears twice", f[0]));
        } else {
            allowed.push((line, f[0]));
        }
    }

    let file = numbers.path.as_str();
    let mut linux_max = None;
    let mut private_base = None;
    let mut private_size = None;
    let mut consts = Vec::new();
    for item in &numbers.file.items {
        let Item::Const(c) = item else { continue };
        let name = c.ident.to_string();
        if !matches!(c.vis, Visibility::Public(_)) || !name.starts_with("SYSCALL_") {
            continue;
        }
        let line = rust::line(c.ident.span());
        let meta = match name.as_str() {
            "SYSCALL_LINUX_MAX" => &mut linux_max,
            "SYSCALL_PRIVATE_BASE" => &mut private_base,
            "SYSCALL_PRIVATE_TABLE_SIZE" => &mut private_size,
            "SYSCALL_TABLE_SIZE" | "SYSCALL_PRIVATE_END" => continue,
            _ => {
                if matches!(&*c.ty, Type::Path(t) if t.qself.is_none() && t.path.is_ident("u64")) {
                    consts.push((name, line, value(&c.expr)));
                }
                continue;
            }
        };
        match int(&c.expr) {
            Some(n) => *meta = Some(n),
            None => hit(
                Tag::Meta,
                format!("{file}:{line}"),
                format!(
                    "{name} = {} is not an integer literal",
                    c.expr.to_token_stream()
                ),
            ),
        }
    }
    for (name, v) in [
        ("SYSCALL_LINUX_MAX", linux_max),
        ("SYSCALL_PRIVATE_BASE", private_base),
        ("SYSCALL_PRIVATE_TABLE_SIZE", private_size),
    ] {
        if v.is_none() {
            hit(
                Tag::Meta,
                file.to_string(),
                format!("{name} is not defined"),
            );
        }
    }
    if let Some(max) = linux_max
        && Some(max) != table_max
    {
        hit(
            Tag::Range,
            file.to_string(),
            format!(
                "SYSCALL_LINUX_MAX is {max}, but Linux has allocated up to {}",
                table_max.map_or("nothing".to_string(), |m| m.to_string())
            ),
        );
    }

    let mut linux = 0;
    let mut private = 0;
    let mut private_k: BTreeMap<u64, &str> = BTreeMap::new();
    let mut allow_used: BTreeSet<&str> = BTreeSet::new();
    if let Some(base) = private_base {
        let mut seen_name: HashMap<&str, usize> = HashMap::new();
        let mut owner: HashMap<u64, &str> = HashMap::new();
        for (name, line, v) in &consts {
            let at = format!("{file}:{line}");
            let short = name["SYSCALL_".len()..].to_lowercase();
            if let Some(first) = seen_name.get(name.as_str()) {
                hit(
                    Tag::DupName,
                    at,
                    format!("{name} is already defined at line {first}"),
                );
                continue;
            }
            seen_name.insert(name, *line);
            let v = match v {
                Ok(n) => *n,
                Err(expr) => {
                    hit(
                        Tag::Shape,
                        at,
                        format!(
                            "{name} = {expr} is neither a bare Linux number nor \
                             SYSCALL_PRIVATE_BASE + k"
                        ),
                    );
                    continue;
                }
            };
            let number = match v {
                Number::Linux(num) if num >= base => {
                    hit(
                        Tag::Shape,
                        at,
                        format!(
                            "{name} = {num} is in the private range; write it as \
                             SYSCALL_PRIVATE_BASE + k"
                        ),
                    );
                    continue;
                }
                Number::Linux(num) => {
                    linux += 1;
                    match table_name.get(&num) {
                        None => hit(
                            Tag::Name,
                            at.clone(),
                            format!(
                                "{name} claims number {num}, which Linux x86-64 has not allocated"
                            ),
                        ),
                        Some(linux_name) if *linux_name != short => hit(
                            Tag::Name,
                            at.clone(),
                            format!(
                                "number {num} belongs to Linux syscall {linux_name}, but {name} \
                                 puts {short} there"
                            ),
                        ),
                        Some(_) => {}
                    }
                    if let Some(max) = linux_max
                        && num > max
                    {
                        hit(
                            Tag::Range,
                            at.clone(),
                            format!("{name} = {num} exceeds SYSCALL_LINUX_MAX ({max})"),
                        );
                    }
                    num
                }
                Number::Private(k) => {
                    private += 1;
                    private_k.entry(k).or_insert(name);
                    if let Some(size) = private_size
                        && k >= size
                    {
                        hit(
                            Tag::Private,
                            at.clone(),
                            format!(
                                "{name} is SYSCALL_PRIVATE_BASE + {k}, past \
                                 SYSCALL_PRIVATE_TABLE_SIZE ({size})"
                            ),
                        );
                    }
                    if let Some(linux_num) = table_num.get(short.as_str()) {
                        match allowed.iter().find(|(_, n)| *n == short) {
                            Some((_, n)) => {
                                allow_used.insert(n);
                            }
                            None => hit(
                                Tag::Collide,
                                at.clone(),
                                format!(
                                    "{name} is private, but {short} is Linux x86-64 number \
                                     {linux_num}"
                                ),
                            ),
                        }
                    }
                    base + k
                }
            };
            match owner.get(&number) {
                Some(prev) => hit(
                    Tag::Dup,
                    at,
                    format!("{name} and {prev} are both number {number}"),
                ),
                None => {
                    owner.insert(number, name);
                }
            }
        }
    }

    if let Some((&max_k, top)) = private_k.last_key_value() {
        for k in (0..=max_k).filter(|k| !private_k.contains_key(k)) {
            hit(
                Tag::Private,
                file.to_string(),
                format!("SYSCALL_PRIVATE_BASE + {k} is unused while + {max_k} is taken by {top}"),
            );
        }
    }
    for (line, name) in &allowed {
        if !allow_used.contains(name) {
            hit(
                Tag::Dead,
                format!("{allow_path}:{line}"),
                format!("entry \"{name}\" exempts nothing"),
            );
        }
    }
    if row_count < MIN_TABLE_ROWS {
        hit(
            Tag::Floor,
            tbl_path.to_string(),
            format!(
                "parsed {row_count} rows, expected at least {MIN_TABLE_ROWS} — the vendored \
                 table looks truncated"
            ),
        );
    }
    if linux < MIN_LINUX_CONSTS {
        hit(
            Tag::Floor,
            file.to_string(),
            format!(
                "found {linux} Linux-numbered constants, expected at least {MIN_LINUX_CONSTS} — \
                 the scan did not parse"
            ),
        );
    }
    if private < MIN_PRIVATE_CONSTS {
        hit(
            Tag::Floor,
            file.to_string(),
            format!(
                "found {private} private constants, expected at least {MIN_PRIVATE_CONSTS} — \
                 the scan did not parse"
            ),
        );
    }
    Scan {
        hits,
        rows: row_count,
        linux,
        private,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `<i> sc<i>` for `rows` rows, so every number's name is mechanical,
    /// behind a `#` line that looks exactly like a row.
    fn table(rows: usize) -> String {
        let mut t = String::from(
            "# Fixture table. A '#' line that looks exactly like a row must not\n\
             # be parsed as one — if it were, the wrong name would land on 5.\n\
             # 5 not_sc5\n\n",
        );
        for i in 0..rows {
            t += &format!("{i} sc{i}\n");
        }
        t
    }

    fn numbers(linux_max: usize, linux: usize, private: usize) -> String {
        let mut n = format!(
            "//! Fixture header. A doc comment may name a constant and its value:\n\
             //! `pub const SYSCALL_SC1: u64 = 77;` is prose, not a definition.\n\
             pub const SYSCALL_LINUX_MAX: u64 = {linux_max};\n\
             pub const SYSCALL_TABLE_SIZE: usize = (SYSCALL_LINUX_MAX as usize) + 1;\n\
             pub const SYSCALL_PRIVATE_BASE: u64 = 1024;\n\
             pub const SYSCALL_PRIVATE_TABLE_SIZE: usize = 48;\n\
             pub const SYSCALL_PRIVATE_END: u64 =\n    \
             SYSCALL_PRIVATE_BASE + (SYSCALL_PRIVATE_TABLE_SIZE as u64);\n"
        );
        for i in 0..linux {
            n += &format!("/// `sc{i}(arg0)`.\npub const SYSCALL_SC{i}: u64 = {i};\n");
        }
        for i in 0..private {
            n += &format!("pub const SYSCALL_PRIV{i}: u64 = SYSCALL_PRIVATE_BASE + {i};\n");
        }
        n += &format!(
            "pub const SYSCALL_SC200: u64 = SYSCALL_PRIVATE_BASE + {private};\n\
             const _: () = assert!(SYSCALL_PRIVATE_BASE > SYSCALL_LINUX_MAX);\n\
             const _: () = assert!(\n    SYSCALL_SC200 < SYSCALL_PRIVATE_END,\n    \
             \"private syscall range is full\",\n);\n"
        );
        n
    }

    fn good() -> String {
        numbers(399, 120, 25)
    }

    const ALLOW: &str = "sc200 private until the fixture's struct layout matches Linux's\n";

    fn run_on(rs: &str, tbl: &str, allow: &str) -> Vec<Hit> {
        let src = Source::parse("numbers.rs", rs.to_string()).unwrap();
        scan(&src, ("linux.tbl", tbl), ("allow.txt", allow)).hits
    }

    fn count(hits: &[Hit], tag: Tag) -> usize {
        hits.iter().filter(|h| h.tag == tag).count()
    }

    fn only(hits: &[Hit], tag: Tag, n: usize) {
        assert_eq!(count(hits, tag), n, "{hits:#?}");
        assert_eq!(hits.len(), n, "{hits:#?}");
    }

    #[test]
    fn a_correct_abi_is_silent() {
        let src = Source::parse("numbers.rs", good()).unwrap();
        let s = scan(&src, ("linux.tbl", &table(400)), ("allow.txt", ALLOW));
        assert!(s.hits.is_empty(), "{:#?}", s.hits);
        assert_eq!((s.rows, s.linux, s.private), (400, 120, 26));
    }

    #[test]
    fn a_constant_on_another_calls_number() {
        let hits = run_on(
            &(good() + "pub const SYSCALL_SC300: u64 = 301;\n"),
            &table(400),
            ALLOW,
        );
        only(&hits, Tag::Name, 1);
    }

    #[test]
    fn a_number_above_linux_max() {
        let hits = run_on(
            &(good() + "pub const SYSCALL_SC999: u64 = 999;\n"),
            &table(400),
            ALLOW,
        );
        assert_eq!(count(&hits, Tag::Range), 1, "{hits:#?}");
    }

    #[test]
    fn a_duplicate_number() {
        let hits = run_on(
            &(good() + "pub const SYSCALL_PRIV_AGAIN: u64 = SYSCALL_PRIVATE_BASE + 3;\n"),
            &table(400),
            ALLOW,
        );
        only(&hits, Tag::Dup, 1);
    }

    #[test]
    fn a_duplicate_name() {
        let hits = run_on(
            &(good() + "pub const SYSCALL_PRIV3: u64 = SYSCALL_PRIVATE_BASE + 40;\n"),
            &table(400),
            ALLOW,
        );
        only(&hits, Tag::DupName, 1);
    }

    #[test]
    fn an_unrecorded_private_collision() {
        let hits = run_on(
            &(good() + "pub const SYSCALL_SC201: u64 = SYSCALL_PRIVATE_BASE + 26;\n"),
            &table(400),
            ALLOW,
        );
        only(&hits, Tag::Collide, 1);
    }

    #[test]
    fn a_hole_in_the_private_range() {
        let rs: String = good()
            .lines()
            .filter(|l| !l.contains("SYSCALL_PRIV5:"))
            .map(|l| format!("{l}\n"))
            .collect();
        only(&run_on(&rs, &table(400), ALLOW), Tag::Private, 1);
    }

    #[test]
    fn private_numbers_past_the_table() {
        let rs = good().replace(
            "SYSCALL_PRIVATE_TABLE_SIZE: usize = 48;",
            "SYSCALL_PRIVATE_TABLE_SIZE: usize = 20;",
        );
        only(&run_on(&rs, &table(400), ALLOW), Tag::Private, 6);
    }

    #[test]
    fn stale_and_reasonless_allowlist_entries() {
        let allow = format!("{ALLOW}sc300 nothing private is called this\nsc250\n");
        only(&run_on(&good(), &table(400), &allow), Tag::Dead, 2);
    }

    #[test]
    fn a_truncated_table() {
        let hits = run_on(&good(), &table(50), ALLOW);
        assert_eq!(count(&hits, Tag::Floor), 1, "{hits:#?}");
    }

    #[test]
    fn a_scan_that_barely_parsed() {
        only(
            &run_on(&numbers(399, 3, 2), &table(400), ALLOW),
            Tag::Floor,
            2,
        );
    }

    #[test]
    fn values_are_read_from_the_expression_not_the_line() {
        let rs = good()
            + "pub const SYSCALL_PRIV25: u64 = SYSCALL_PRIVATE_BASE\n    + 26; // + 99\n"
            + "pub const SYSCALL_SC121: u64 = 0x79;\n"
            + "pub const SYSCALL_SC122: u64 = SYSCALL_SC121 + 1;\n"
            + "pub const SYSCALL_SC123: u64 = 1024 + 3;\n"
            + "pub(crate) const SYSCALL_SC124: u64 = 5;\n"
            + "mod inner { pub const SYSCALL_SC125: u64 = 5; }\n";
        let hits = run_on(&rs, &table(400), ALLOW);
        assert_eq!(count(&hits, Tag::Shape), 2, "{hits:#?}");
        assert_eq!(hits.len(), 2, "{hits:#?}");
    }

    #[test]
    fn missing_metadata_is_reported() {
        let rs = good().replace("pub const SYSCALL_PRIVATE_BASE: u64 = 1024;\n", "");
        let hits = run_on(&rs, &table(400), ALLOW);
        assert_eq!(count(&hits, Tag::Meta), 1, "{hits:#?}");
    }
}
