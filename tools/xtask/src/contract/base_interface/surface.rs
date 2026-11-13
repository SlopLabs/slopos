//! The closure: the private calls, then every abi item their signatures name,
//! their docs link or a `root` glob matches, then everything those reach
//! through field, variant, alias and constant types.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use super::rustdoc::{self, CRATE, Crate, Refs, field, string};
use super::text::{Int, int_value};
use super::{Hit, Tag, signature};
use crate::Result;
use crate::glob;

pub const NUMBERS: [&str; 3] = [CRATE, "syscall", "numbers"];
const EXCLUDED_PREFIX: &str = "slopos_abi::base_interface";
const RANGE_BOUNDS: [&str; 3] = [
    "SYSCALL_PRIVATE_BASE",
    "SYSCALL_PRIVATE_END",
    "SYSCALL_PRIVATE_TABLE_SIZE",
];
const TYPE_KINDS: [&str; 5] = ["struct", "enum", "union", "type_alias", "constant"];
const ITEM_KINDS: [&str; 7] = [
    "struct",
    "enum",
    "union",
    "type_alias",
    "constant",
    "function",
    "static",
];

fn under(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with("::"))
}

pub struct Root {
    pub line: usize,
    pub glob: String,
}

pub struct Surface<'c, 'a> {
    crate_: &'c Crate<'a>,
    pub hits: Vec<Hit>,
    pub lines: BTreeSet<String>,
    pub calls: usize,
    done: HashSet<String>,
    /// `slopos_abi::syscall::numbers`'s constants, by name.
    pub numbers: BTreeMap<String, u64>,
    pub private_base: Option<Int>,
    pub base_interface: Option<Int>,
    pub base_interface_min: Option<Int>,
}

/// A constant item's evaluated value.
pub fn const_value(item: &serde_json::Value) -> Result<Option<Int>> {
    let konst = field(field(field(item, "inner")?, "constant")?, "const")?;
    int_value(rustdoc::opt(konst, "value")?)
}

impl<'c, 'a> Surface<'c, 'a> {
    pub fn read(crate_: &'c Crate<'a>, gate: &str, roots: &[Root]) -> Result<Self> {
        let mut s = Self {
            crate_,
            hits: Vec::new(),
            lines: BTreeSet::new(),
            calls: 0,
            done: HashSet::new(),
            numbers: BTreeMap::new(),
            private_base: None,
            base_interface: None,
            base_interface_min: None,
        };
        for (&ident, _) in &crate_.local {
            if crate_.kind(ident) == Some("constant")
                && let Some(segs) = crate_.segments(ident)
                && let Some((name, module)) = segs.split_last()
                && module == NUMBERS
            {
                s.numbers.insert(name.clone(), ident);
            }
        }
        let bound = |name: &str| -> Result<Option<Int>> {
            match s.numbers.get(name) {
                Some(&ident) => const_value(crate_.require(ident)?),
                None => Ok(None),
            }
        };
        let base = bound("SYSCALL_PRIVATE_BASE")?;
        let end = bound("SYSCALL_PRIVATE_END")?;
        if base.is_none() || end.is_none() {
            s.hit(
                Tag::Meta,
                CRATE,
                format!(
                    "SYSCALL_PRIVATE_BASE / SYSCALL_PRIVATE_END missing from {} or not evaluated",
                    NUMBERS.join("::")
                ),
            );
        }
        s.private_base = base.clone();
        s.base_interface = s.single_constant("BASE_INTERFACE")?;
        s.base_interface_min = s.single_constant("BASE_INTERFACE_MIN")?;

        let mut pending_items: Vec<u64> = Vec::new();
        let mut pending_prefixes: Vec<String> = Vec::new();
        if let (Some(base), Some(end)) = (&base, &end) {
            let numbers: Vec<(String, u64)> =
                s.numbers.iter().map(|(n, i)| (n.clone(), *i)).collect();
            for (name, ident) in numbers {
                if !name.starts_with("SYSCALL_") || RANGE_BOUNDS.contains(&name.as_str()) {
                    continue;
                }
                let item = crate_.require(ident)?;
                let Some(number) = const_value(item)? else {
                    continue;
                };
                if !(*base <= number && number < *end) {
                    continue;
                }
                s.calls += 1;
                s.call(
                    &name,
                    &number,
                    item,
                    &mut pending_items,
                    &mut pending_prefixes,
                )?;
            }
        }

        let universe: BTreeSet<&String> =
            crate_.by_path.keys().chain(crate_.members.keys()).collect();
        let renders = |p: &str| {
            !under(p, EXCLUDED_PREFIX)
                && (crate_.members.contains_key(p)
                    || crate_
                        .kind(crate_.by_path[p])
                        .is_some_and(|k| ITEM_KINDS.contains(&k)))
        };
        for root in roots {
            let matched: Vec<&String> = universe
                .iter()
                .copied()
                .filter(|p| glob::matches(&root.glob, p))
                .collect();
            let live = matched
                .iter()
                .any(|p| universe.iter().any(|q| under(q, p) && renders(q)));
            if !live {
                s.hit(
                    Tag::Dead,
                    format!("{gate}:{}", root.line),
                    format!("`root {}` roots nothing the surface renders", root.glob),
                );
            }
            for p in matched {
                if crate_.members.contains_key(p) {
                    s.add_member(p)?;
                } else if crate_.kind(crate_.by_path[p]) == Some("module") {
                    pending_prefixes.push(p.clone());
                } else {
                    pending_items.push(crate_.by_path[p]);
                }
            }
        }

        for prefix in &pending_prefixes {
            for p in &universe {
                if under(p, prefix) {
                    if crate_.members.contains_key(*p) {
                        s.add_member(p)?;
                    } else {
                        pending_items.push(crate_.by_path[*p]);
                    }
                }
            }
        }
        for ident in pending_items {
            s.add_item(ident)?;
        }
        Ok(s)
    }

    fn hit(&mut self, tag: Tag, location: impl Into<String>, message: String) {
        self.hits.push(Hit {
            tag,
            location: location.into(),
            message,
        });
    }

    fn call(
        &mut self,
        name: &str,
        number: &Int,
        item: &serde_json::Value,
        pending_items: &mut Vec<u64>,
        pending_prefixes: &mut Vec<String>,
    ) -> Result<()> {
        let c = self.crate_;
        let call = name["SYSCALL_".len()..].to_lowercase();
        let docs = match field(item, "docs")? {
            serde_json::Value::Null => "",
            d => string(d)?,
        };
        let Some(sig) = signature::find(docs, &call) else {
            self.hit(
                Tag::Signature,
                name,
                format!(
                    "its docs carry no `{call}(...)` code span — the call's arguments are part \
                     of the interface and must be written down"
                ),
            );
            return Ok(());
        };
        self.lines.insert(format!("call {number} {name} {sig}"));
        let Some(idents) = signature::type_idents(&sig, &call) else {
            self.hit(
                Tag::Signature,
                name,
                format!("unbalanced parentheses in `{sig}`"),
            );
            return Ok(());
        };
        for ident in idents {
            let hits = resolve(c, &ident);
            if hits.len() != 1 {
                let why = if hits.is_empty() {
                    format!("names nothing in {CRATE}")
                } else {
                    let mut paths: Vec<&str> = hits.iter().map(|h| c.local[h].as_str()).collect();
                    paths.sort();
                    format!("is ambiguous: {}", paths.join(", "))
                };
                self.hit(Tag::Unresolved, name, format!("`{ident}` in `{sig}` {why}"));
                continue;
            }
            pending_items.push(hits[0]);
        }
        let links = field(item, "links")?
            .as_object()
            .ok_or_else(|| format!("rustdoc JSON: {name}'s links are not an object"))?;
        let call_prefix = format!("{}::SYSCALL_", NUMBERS.join("::"));
        for target in links.values() {
            let target = rustdoc::id(target)?;
            let Some(path) = c.local.get(&target) else {
                continue;
            };
            if path.starts_with(&call_prefix) {
                continue;
            }
            if c.kind(target) == Some("module") {
                pending_prefixes.push(path.clone());
            } else {
                pending_items.push(target);
            }
        }
        Ok(())
    }

    fn single_constant(&mut self, name: &str) -> Result<Option<Int>> {
        let c = self.crate_;
        let hits: Vec<u64> = c
            .local
            .iter()
            .filter(|(i, p)| c.kind(**i) == Some("constant") && p.rsplit("::").next() == Some(name))
            .map(|(i, _)| *i)
            .collect();
        let value = match hits.as_slice() {
            [one] => const_value(c.require(*one)?)?,
            _ => None,
        };
        if value.is_none() {
            self.hit(
                Tag::Meta,
                CRATE,
                format!("{CRATE} defines no single evaluated constant {name}"),
            );
        }
        Ok(value)
    }

    fn add_member(&mut self, path: &str) -> Result<()> {
        if self.done.contains(path) || under(path, EXCLUDED_PREFIX) {
            return Ok(());
        }
        self.done.insert(path.to_string());
        let c = self.crate_;
        let mut refs = Refs::new();
        if let Some(line) = rustdoc::render(c, path, c.require(c.members[path])?, &mut refs)? {
            self.lines.insert(line);
        }
        for r in refs {
            self.add_item(r)?;
        }
        Ok(())
    }

    fn add_item(&mut self, ident: u64) -> Result<()> {
        let c = self.crate_;
        let mut work = vec![ident];
        while let Some(ident) = work.pop() {
            let Some(path) = c.local.get(&ident) else {
                continue;
            };
            if self.done.contains(path) || under(path, EXCLUDED_PREFIX) {
                continue;
            }
            if !c.kind(ident).is_some_and(|k| ITEM_KINDS.contains(&k)) {
                continue;
            }
            self.done.insert(path.clone());
            let mut refs = Refs::new();
            if let Some(line) = rustdoc::render(c, path, c.require(ident)?, &mut refs)? {
                self.lines.insert(line);
            }
            // A type's inherent constants are part of its shape (`SERIALIZED_LEN`).
            for (member, &mid) in &c.members {
                let item = c.require(mid)?;
                if member.rsplit_once("::").map(|(owner, _)| owner) == Some(path.as_str())
                    && field(item, "inner")?.get("assoc_const").is_some()
                    && !self.done.contains(member)
                {
                    self.done.insert(member.clone());
                    if let Some(line) = rustdoc::render(c, member, item, &mut refs)? {
                        self.lines.insert(line);
                    }
                }
            }
            work.extend(refs);
        }
        Ok(())
    }
}

/// The type-like items whose path ends in `ident`'s segments.
fn resolve(c: &Crate, ident: &str) -> Vec<u64> {
    let segs = signature::segments(ident);
    c.local
        .iter()
        .filter(|(i, path)| {
            c.kind(**i).is_some_and(|k| TYPE_KINDS.contains(&k)) && {
                let parts: Vec<&str> = path.split("::").collect();
                let tail = if segs.is_empty() || segs.len() > parts.len() {
                    &parts[..]
                } else {
                    &parts[parts.len() - segs.len()..]
                };
                tail == segs.as_slice()
            }
        })
        .map(|(i, _)| *i)
        .collect()
}
