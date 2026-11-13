//! slopos-abi as rustdoc's JSON sees it, and the canonical line each item
//! renders to: no docs and no rustdoc ids, so prose never moves the digest.
//! Structs carry repr and fields in order, enums their discriminants,
//! constants their evaluated value, functions and statics their source tokens.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::text::{self, Int, int_value, scalar, truthy};
use super::tokens;
use crate::Result;

pub const CRATE: &str = "slopos_abi";

fn shape(what: &str) -> crate::Error {
    format!("rustdoc JSON: {what}").into()
}

pub fn field<'v>(value: &'v Value, key: &str) -> Result<&'v Value> {
    value
        .as_object()
        .and_then(|o| o.get(key))
        .ok_or_else(|| shape(&format!("no `{key}` where one was expected")))
}

pub fn opt<'v>(value: &'v Value, key: &str) -> Result<Option<&'v Value>> {
    match value {
        Value::Object(o) => Ok(o.get(key)),
        _ => Err(shape(&format!("`{key}` asked of a non-object"))),
    }
}

pub fn string(value: &Value) -> Result<&str> {
    value
        .as_str()
        .ok_or_else(|| shape(&format!("{value} where text was expected")))
}

pub fn array(value: &Value) -> Result<&Vec<Value>> {
    value
        .as_array()
        .ok_or_else(|| shape(&format!("{value} where a list was expected")))
}

pub fn id(value: &Value) -> Result<u64> {
    value
        .as_u64()
        .ok_or_else(|| shape(&format!("{value} where an id was expected")))
}

/// The one `(kind, body)` pair of an externally tagged value.
fn single(value: &Value) -> Result<(&str, &Value)> {
    match value.as_object() {
        Some(o) if o.len() == 1 => {
            let (k, v) = o.iter().next().ok_or_else(|| shape("empty object"))?;
            Ok((k.as_str(), v))
        }
        _ => Err(shape(&format!("{value} is not one tagged variant"))),
    }
}

/// An item's kind: the first key of its `inner`, or `inner` itself where
/// rustdoc writes a bare string (`"extern_type"`).
fn inner_kind(item: &Value) -> Result<&str> {
    let inner = field(item, "inner")?;
    inner
        .as_str()
        .or_else(|| {
            inner
                .as_object()
                .and_then(|o| o.keys().next())
                .map(String::as_str)
        })
        .ok_or_else(|| shape("an item with no inner kind"))
}

struct PathEntry {
    kind: String,
    path: Vec<String>,
}

pub struct Crate<'a> {
    index: &'a Map<String, Value>,
    src_root: PathBuf,
    files: RefCell<HashMap<String, Vec<Vec<char>>>>,
    paths: HashMap<u64, PathEntry>,
    /// The crate's own items with an index entry, by id: canonical paths.
    pub local: BTreeMap<u64, String>,
    /// Inherent members (`Type::NAME`, `Type::method`), which `paths` omits.
    pub members: BTreeMap<String, u64>,
    pub by_path: BTreeMap<String, u64>,
}

impl<'a> Crate<'a> {
    pub fn new(doc: &'a Value, src_root: &Path) -> Result<Self> {
        let index = field(doc, "index")?
            .as_object()
            .ok_or_else(|| shape("`index` is not an object"))?;
        let mut paths = HashMap::new();
        let mut local = BTreeMap::new();
        let raw = field(doc, "paths")?
            .as_object()
            .ok_or_else(|| shape("`paths` is not an object"))?;
        for (key, entry) in raw {
            let ident: u64 = key
                .parse()
                .map_err(|_| shape(&format!("path key `{key}`")))?;
            let path = array(field(entry, "path")?)?
                .iter()
                .map(|s| string(s).map(str::to_string))
                .collect::<Result<Vec<_>>>()?;
            let kind = string(field(entry, "kind")?)?.to_string();
            if field(entry, "crate_id")?.as_u64() == Some(0) && index.contains_key(key) {
                local.insert(ident, path.join("::"));
            }
            paths.insert(ident, PathEntry { kind, path });
        }
        let mut crate_ = Self {
            index,
            src_root: src_root.to_path_buf(),
            files: RefCell::default(),
            paths,
            local,
            members: BTreeMap::new(),
            by_path: BTreeMap::new(),
        };
        crate_.members = crate_.inherent_members()?;
        crate_.by_path = crate_
            .local
            .iter()
            .map(|(ident, path)| (path.clone(), *ident))
            .collect();
        Ok(crate_)
    }

    fn inherent_members(&self) -> Result<BTreeMap<String, u64>> {
        let mut members = BTreeMap::new();
        for (&ident, path) in &self.local {
            let item = self.require(ident)?;
            let kind = inner_kind(item)?;
            if !matches!(kind, "struct" | "enum" | "union") {
                continue;
            }
            let Some(impls) = opt(field(field(item, "inner")?, kind)?, "impls")? else {
                continue;
            };
            for impl_id in array(impls)? {
                let Some(imp) = self.item(id(impl_id)?) else {
                    continue;
                };
                let imp = field(field(imp, "inner")?, "impl")?;
                if !field(imp, "trait")?.is_null()
                    || truthy(Some(field(imp, "is_synthetic")?))
                    || truthy(Some(field(imp, "blanket_impl")?))
                {
                    continue;
                }
                for member in array(field(imp, "items")?)? {
                    let member = id(member)?;
                    let Some(m) = self.item(member) else {
                        continue;
                    };
                    let name = field(m, "name")?;
                    if truthy(Some(name)) && matches!(inner_kind(m)?, "assoc_const" | "function") {
                        members.insert(format!("{path}::{}", scalar(name)?), member);
                    }
                }
            }
        }
        Ok(members)
    }

    pub fn item(&self, ident: u64) -> Option<&'a Value> {
        self.index.get(&ident.to_string())
    }

    pub fn require(&self, ident: u64) -> Result<&'a Value> {
        self.item(ident)
            .ok_or_else(|| shape(&format!("id {ident} is not in the index")))
    }

    pub fn kind(&self, ident: u64) -> Option<&str> {
        self.paths.get(&ident).map(|e| e.kind.as_str())
    }

    pub fn segments(&self, ident: u64) -> Option<&[String]> {
        self.paths.get(&ident).map(|e| e.path.as_slice())
    }

    /// An item's span, as its normalised source tokens.
    pub fn source(&self, span: &Value) -> Result<String> {
        let name = string(field(span, "filename")?)?;
        let mut files = self.files.borrow_mut();
        if !files.contains_key(name) {
            let path = if Path::new(name).is_absolute() {
                PathBuf::from(name)
            } else {
                self.src_root.join(name)
            };
            let raw = std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {} for an item's source: {e}", path.display()))?;
            let lines = text::universal_newlines(&raw)
                .split('\n')
                .map(|l| l.chars().collect())
                .collect();
            files.insert(name.to_string(), lines);
        }
        let lines = &files[name];
        let pos = |key: &str| -> Result<(i64, i64)> {
            let v = array(field(span, key)?)?;
            match v.as_slice() {
                [l, c] => Ok((
                    l.as_i64().ok_or_else(|| shape("span line"))?,
                    c.as_i64().ok_or_else(|| shape("span column"))?,
                )),
                _ => Err(shape("a span position is not [line, column]")),
            }
        };
        let ((bl, bc), (el, ec)) = (pos("begin")?, pos("end")?);
        let line = |l: i64| -> Result<&Vec<char>> {
            text::index(lines.len(), l - 1)
                .map(|i| &lines[i])
                .ok_or_else(|| format!("{name}: line {l} is past the file's end").into())
        };
        let cut = |l: &Vec<char>, s: Option<i64>, e: Option<i64>| -> String {
            let (s, e) = text::slice(l.len(), s, e);
            l[s..e].iter().collect()
        };
        let joined = if bl == el {
            cut(line(bl)?, Some(bc - 1), Some(ec - 1))
        } else {
            let mut parts = vec![cut(line(bl)?, Some(bc - 1), None)];
            let (s, e) = text::slice(lines.len(), Some(bl), Some(el - 1));
            parts.extend(lines[s..e].iter().map(|l| l.iter().collect::<String>()));
            parts.push(cut(line(el)?, None, Some(ec - 1)));
            parts.join("\n")
        };
        Ok(tokens::normalise(&joined))
    }
}

/// The local items a rendering names, followed into the closure.
pub type Refs = BTreeSet<u64>;

fn render_type(c: &Crate, ty: &Value, refs: &mut Refs) -> Result<String> {
    if ty.is_null() {
        return Ok("()".to_string());
    }
    let (kind, body) = single(ty)?;
    Ok(match kind {
        "primitive" | "generic" => string(body)?.to_string(),
        "resolved_path" => {
            let ident = id(field(body, "id")?)?;
            let path = match c.segments(ident).map(|s| s.join("::")) {
                Some(p) if !p.is_empty() => p,
                _ => string(field(body, "path")?)?.to_string(),
            };
            if c.local.contains_key(&ident) {
                refs.insert(ident);
            }
            path + &render_args(c, opt(body, "args")?, refs)?
        }
        "array" => format!(
            "[{}; {}]",
            render_type(c, field(body, "type")?, refs)?,
            scalar(field(body, "len")?)?
        ),
        "slice" => format!("[{}]", render_type(c, body, refs)?),
        "raw_pointer" => format!(
            "*{} {}",
            if truthy(Some(field(body, "is_mutable")?)) {
                "mut"
            } else {
                "const"
            },
            render_type(c, field(body, "type")?, refs)?
        ),
        "borrowed_ref" => {
            let lifetime = field(body, "lifetime")?;
            let lt = if truthy(Some(lifetime)) {
                format!("{} ", scalar(lifetime)?)
            } else {
                String::new()
            };
            let mutable = if truthy(Some(field(body, "is_mutable")?)) {
                "mut "
            } else {
                ""
            };
            format!(
                "&{lt}{mutable}{}",
                render_type(c, field(body, "type")?, refs)?
            )
        }
        "tuple" => format!(
            "({})",
            array(body)?
                .iter()
                .map(|t| render_type(c, t, refs))
                .collect::<Result<Vec<_>>>()?
                .join(", ")
        ),
        _ => text::dumps_without_ids(ty),
    })
}

fn render_args(c: &Crate, args: Option<&Value>, refs: &mut Refs) -> Result<String> {
    let Some(args) = args.filter(|a| truthy(Some(a))) else {
        return Ok(String::new());
    };
    let angled = match args {
        Value::Object(o) => o.get("angle_bracketed"),
        Value::String(s) if !s.contains("angle_bracketed") => None,
        Value::Array(a) if !a.iter().any(|v| v == "angle_bracketed") => None,
        _ => return Err(shape(&format!("generic args {args}"))),
    };
    let Some(angled) = angled else {
        return Ok(text::dumps_without_ids(args));
    };
    let mut parts = Vec::new();
    for arg in array(field(angled, "args")?)? {
        let (kind, body) = single(arg)?;
        parts.push(match kind {
            "type" => render_type(c, body, refs)?,
            "lifetime" => string(body)?.to_string(),
            _ => text::dumps_without_ids(body),
        });
    }
    for constraint in array(field(angled, "constraints")?)? {
        parts.push(text::dumps_without_ids(constraint));
    }
    Ok(if parts.is_empty() {
        String::new()
    } else {
        format!("<{}>", parts.join(", "))
    })
}

fn render_repr(item: &Value) -> Result<String> {
    for attr in array(field(item, "attrs")?)? {
        let Some(r) = attr.as_object().and_then(|a| a.get("repr")) else {
            continue;
        };
        let mut parts = vec![string(field(r, "kind")?)?.to_string()];
        for key in ["int", "align", "packed"] {
            if let Some(v) = opt(r, key)?.filter(|v| !v.is_null()) {
                parts.push(format!("{key}={}", scalar(v)?));
            }
        }
        return Ok(format!(" repr({})", parts.join(",")));
    }
    Ok(" repr()".to_string())
}

fn render_generics(generics: &Value) -> Result<String> {
    let names = array(field(generics, "params")?)?
        .iter()
        .map(|p| string(field(p, "name")?).map(str::to_string))
        .collect::<Result<Vec<_>>>()?;
    Ok(if names.is_empty() {
        String::new()
    } else {
        format!("<{}>", names.join(", "))
    })
}

fn field_type<'v>(c: &Crate<'v>, f: &Value) -> Result<(&'v Value, &'v Value)> {
    let item = c.require(id(f)?)?;
    Ok((item, field(field(item, "inner")?, "struct_field")?))
}

fn render_fields(c: &Crate, fields: &Value, refs: &mut Refs) -> Result<Vec<String>> {
    array(fields)?
        .iter()
        .map(|f| {
            let (item, ty) = field_type(c, f)?;
            Ok(format!(
                "{}: {}",
                scalar(field(item, "name")?)?,
                render_type(c, ty, refs)?
            ))
        })
        .collect()
}

fn render_tuple(c: &Crate, fields: &Value, refs: &mut Refs) -> Result<String> {
    let parts = array(fields)?
        .iter()
        .map(|f| {
            if f.is_null() {
                Ok("_".to_string())
            } else {
                render_type(c, field_type(c, f)?.1, refs)
            }
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(format!("({})", parts.join(", ")))
}

fn render_struct_kind(c: &Crate, kind: &Value, refs: &mut Refs) -> Result<String> {
    if kind == "unit" {
        return Ok(";".to_string());
    }
    if let Some(tuple) = opt(kind, "tuple")? {
        return render_tuple(c, tuple, refs);
    }
    Ok(format!(
        "{{{}}}",
        render_fields(c, field(field(kind, "plain")?, "fields")?, refs)?.join(", ")
    ))
}

fn render_const_value(c: &Crate, item: &Value, konst: &Value) -> Result<String> {
    if let Some(v) = opt(konst, "value")?.filter(|v| !v.is_null()) {
        return scalar(v);
    }
    if let Some(e) = opt(konst, "expr")?.filter(|e| !e.is_null() && *e != "_") {
        return scalar(e);
    }
    Ok(format!("{{{}}}", c.source(field(item, "span")?)?))
}

enum Prev {
    Int(Int),
    Text(String),
}

fn render_enum(
    c: &Crate,
    path: &str,
    item: &Value,
    body: &Value,
    refs: &mut Refs,
) -> Result<String> {
    let mut variants = Vec::new();
    let mut prev = Prev::Int(Int::from_u64(0));
    let mut first = true;
    for v in array(field(body, "variants")?)? {
        let var = c.require(id(v)?)?;
        let vb = field(field(var, "inner")?, "variant")?;
        let disc = field(vb, "discriminant")?;
        let value = if !disc.is_null() {
            string(field(disc, "value")?)?.to_string()
        } else {
            match &prev {
                Prev::Int(_) if first => "0".to_string(),
                Prev::Int(n) => n.succ().to_string(),
                Prev::Text(t) => format!("{t}+1"),
            }
        };
        first = false;
        prev = match int_value(Some(&Value::String(value.clone())))? {
            Some(n) => Prev::Int(n),
            None => Prev::Text(value.clone()),
        };
        let vk = field(vb, "kind")?;
        let shape_text = if vk == "plain" {
            String::new()
        } else if let Some(tuple) = opt(vk, "tuple")? {
            render_tuple(c, tuple, refs)?
        } else {
            format!(
                "{{{}}}",
                render_fields(c, field(field(vk, "struct")?, "fields")?, refs)?.join(", ")
            )
        };
        variants.push(format!(
            "{}{shape_text} = {value}",
            scalar(field(var, "name")?)?
        ));
    }
    Ok(format!(
        "enum {path}{}{} {{{}}}",
        render_generics(field(body, "generics")?)?,
        render_repr(item)?,
        variants.join(", ")
    ))
}

/// One canonical line for `item`, or `None` for kinds the surface skips.
pub fn render(c: &Crate, path: &str, item: &Value, refs: &mut Refs) -> Result<Option<String>> {
    let (kind, body) = single(field(item, "inner")?)?;
    Ok(Some(match kind {
        "struct" => format!(
            "struct {path}{}{} {}",
            render_generics(field(body, "generics")?)?,
            render_repr(item)?,
            render_struct_kind(c, field(body, "kind")?, refs)?
        ),
        "union" => format!(
            "union {path}{}{} {{{}}}",
            render_generics(field(body, "generics")?)?,
            render_repr(item)?,
            render_fields(c, field(body, "fields")?, refs)?.join(", ")
        ),
        "enum" => render_enum(c, path, item, body, refs)?,
        "type_alias" => format!(
            "type {path}{} = {}",
            render_generics(field(body, "generics")?)?,
            render_type(c, field(body, "type")?, refs)?
        ),
        "constant" => format!(
            "const {path}: {} = {}",
            render_type(c, field(body, "type")?, refs)?,
            render_const_value(c, item, field(body, "const")?)?
        ),
        "assoc_const" => {
            let value = match opt(body, "value")? {
                Some(v) if !v.is_null() && *v != "_" => scalar(v)?,
                _ => format!("{{{}}}", c.source(field(item, "span")?)?),
            };
            format!(
                "const {path}: {} = {value}",
                render_type(c, field(body, "type")?, refs)?
            )
        }
        "static" => format!(
            "static {path}: {} = {{{}}}",
            render_type(c, field(body, "type")?, refs)?,
            c.source(field(item, "span")?)?
        ),
        "function" => format!("fn {path} {{{}}}", c.source(field(item, "span")?)?),
        _ => return Ok(None),
    }))
}
