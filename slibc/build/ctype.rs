//! Rust-to-C type translation, C constant-expression translation, and the ABI
//! classification used to check a declaration against the real slibc export.
//!
//! Everything here is mechanical: an input this module does not understand is
//! an error, never a guess, because a wrong header is a miscompile rather than
//! a build failure.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt::Write as _;

/// How a value is passed in the System V x86-64 C ABI, to the resolution a
/// header declaration can disagree about.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Abi {
    Void,
    /// Diverging (`!`): `void` plus a noreturn attribute.
    Never,
    Int {
        bytes: u8,
        signed: bool,
    },
    Ptr,
    /// `...`; only ever the final parameter.
    Variadic,
    /// Passed by value; compared by name, since layout equality is the
    /// `const _: () = assert!` pins' job rather than this module's.
    Aggregate(String),
    /// A type this module cannot resolve, e.g. a slibc-internal Rust type in
    /// an export signature. Compares equal to nothing and is reported.
    Opaque(String),
}

impl Abi {
    pub fn describe(&self) -> String {
        match self {
            Abi::Void => "void".to_string(),
            Abi::Never => "noreturn".to_string(),
            Abi::Int { bytes, signed } => {
                format!("{}{}", if *signed { "i" } else { "u" }, bytes * 8)
            }
            Abi::Ptr => "ptr".to_string(),
            Abi::Variadic => "...".to_string(),
            Abi::Aggregate(name) => format!("struct {name}"),
            Abi::Opaque(name) => format!("?{name}"),
        }
    }
}

/// The primitive Rust spellings that reach a C header, and their ABI class.
/// `c_char` is `i8` and `char` in C; SlopOS is x86-64 only, so `c_long`,
/// `isize` and `i64` are all one 8-byte signed integer.
const PRIMITIVES: &[(&str, &str, u8, bool)] = &[
    ("c_void", "void", 0, false),
    ("c_char", "char", 1, true),
    ("c_schar", "signed char", 1, true),
    ("c_uchar", "unsigned char", 1, false),
    ("c_short", "short", 2, true),
    ("c_ushort", "unsigned short", 2, false),
    ("c_int", "int", 4, true),
    ("c_uint", "unsigned int", 4, false),
    ("c_long", "long", 8, true),
    ("c_ulong", "unsigned long", 8, false),
    ("c_longlong", "long long", 8, true),
    ("c_ulonglong", "unsigned long long", 8, false),
    ("i8", "signed char", 1, true),
    ("u8", "unsigned char", 1, false),
    ("i16", "short", 2, true),
    ("u16", "unsigned short", 2, false),
    ("i32", "int", 4, true),
    ("u32", "unsigned int", 4, false),
    ("i64", "long", 8, true),
    ("u64", "unsigned long", 8, false),
    ("isize", "long", 8, true),
    ("usize", "unsigned long", 8, false),
];

/// The type universe of the generated headers.
pub struct Types {
    /// `pub type A = B;`, used to resolve an ABI class through alias chains.
    aliases: BTreeMap<String, String>,
    /// Emitted as `struct X` in C (POSIX exposes these as struct tags).
    struct_tags: BTreeSet<String>,
    /// Emitted as a bare identifier (POSIX requires these to be types).
    typedefs: BTreeSet<String>,
    /// Integer constants the contract uses as array lengths, e.g. `NCCS` in
    /// `termios.c_cc`.
    lengths: BTreeMap<String, usize>,
}

impl Types {
    pub fn new(
        aliases: BTreeMap<String, String>,
        struct_tags: BTreeSet<String>,
        typedefs: BTreeSet<String>,
        lengths: BTreeMap<String, usize>,
    ) -> Self {
        Self {
            aliases,
            struct_tags,
            typedefs,
            lengths,
        }
    }

    /// An array length: a literal, or a contract constant that is one. The
    /// generated header carries the resolved number, because a C struct
    /// cannot name a macro that the same header defines further down.
    pub fn array_len(&self, text: &str) -> Result<usize, String> {
        let text = text.trim();
        if let Ok(len) = text.replace('_', "").parse::<usize>() {
            return Ok(len);
        }
        self.lengths
            .get(text)
            .copied()
            .ok_or_else(|| format!("array length `{text}` is not a known integer constant"))
    }

    /// POSIX spells a type whose name ends in `_t` (plus `fd_set`, `Dl_info`
    /// and the ELF gABI's own `Elf64_*`) as a typedef, and everything else as
    /// a struct tag: C code writes `sigset_t s;` but `struct stat st;`. A
    /// typedef named `stat` would also collide with the function of the same
    /// name, which is exactly why the C library never introduces one.
    pub fn is_typedef_shaped(name: &str) -> bool {
        name.ends_with("_t") || name == "fd_set" || name == "Dl_info" || name.starts_with("Elf64_")
    }

    fn primitive(name: &str) -> Option<(&'static str, u8, bool)> {
        PRIMITIVES
            .iter()
            .find(|(rust, _, _, _)| *rust == name)
            .map(|(_, c, bytes, signed)| (*c, *bytes, *signed))
    }

    /// Strips `crate::`, `libc::` and `core::ffi::` qualification: the contract
    /// reaches shared libc items through `crate::`, and C has one namespace.
    fn unqualify(ty: &str) -> &str {
        ty.rsplit("::").next().unwrap_or(ty).trim()
    }

    /// The C spelling of a non-pointer, non-array, non-function type.
    fn base(&self, ty: &str) -> Result<String, String> {
        let ty = Self::unqualify(ty);
        if let Some((c, _, _)) = Self::primitive(ty) {
            return Ok(c.to_string());
        }
        if self.typedefs.contains(ty) {
            return Ok(ty.to_string());
        }
        if self.struct_tags.contains(ty) {
            return Ok(format!("struct {ty}"));
        }
        Err(format!("no C spelling for Rust type `{ty}`"))
    }

    /// Declares `name` as having type `ty`, C declarator rules included: the
    /// name sits inside the type for arrays and function pointers.
    pub fn declare(&self, ty: &str, name: &str) -> Result<String, String> {
        let ty = ty.trim();
        if let Some(inner) = ty.strip_prefix("*const ") {
            // Pointer-to-const. Written prefix-style (`const char *p`) when the
            // pointee is not itself a pointer, and suffix-style otherwise
            // (`const char *const *argv`), which is the only correct spelling
            // for the nested case.
            return if inner.trim_start().starts_with('*') {
                self.declare(inner, &format!("const *{name}"))
            } else if let Some(func) = self.function_pointer(inner, name)? {
                Ok(func)
            } else {
                Ok(format!("const {} *{name}", self.base(inner)?))
            };
        }
        if let Some(inner) = ty.strip_prefix("*mut ") {
            return self.declare(inner, &format!("*{name}"));
        }
        if let Some(array) = ty.strip_prefix('[') {
            let body = array
                .strip_suffix(']')
                .ok_or_else(|| format!("unterminated array type `{ty}`"))?;
            let (elem, len) = body
                .split_once(';')
                .ok_or_else(|| format!("array type without a length: `{ty}`"))?;
            let len = self.array_len(len)?;
            return Ok(format!("{} {name}[{len}]", self.base(elem)?, len = len));
        }
        if let Some(func) = self.function_pointer(ty, name)? {
            return Ok(func);
        }
        Ok(format!("{} {name}", self.base(ty)?))
    }

    /// `Option<extern "C" fn(A) -> R>` / `extern "C" fn(A) -> R` as a C
    /// function-pointer declarator. `Option` is how the contract spells a
    /// nullable callback; C has no other kind.
    fn function_pointer(&self, ty: &str, name: &str) -> Result<Option<String>, String> {
        let inner = match ty.strip_prefix("Option<") {
            Some(rest) => rest
                .strip_suffix('>')
                .ok_or_else(|| format!("unterminated `Option<` in `{ty}`"))?,
            None => ty,
        }
        .trim();
        let Some(sig) = inner
            .strip_prefix("unsafe extern \"C\" fn")
            .or_else(|| inner.strip_prefix("extern \"C\" fn"))
        else {
            return Ok(None);
        };
        let sig = sig.trim();
        let close = sig
            .find(')')
            .ok_or_else(|| format!("function-pointer type without `)`: `{ty}`"))?;
        let args = &sig[1..close];
        let ret = sig[close + 1..]
            .trim()
            .strip_prefix("->")
            .unwrap_or("()")
            .trim();
        let mut rendered = Vec::new();
        for arg in split_top_level(args) {
            if arg.is_empty() {
                continue;
            }
            rendered.push(self.declare(&arg, "")?.trim().to_string());
        }
        let args = if rendered.is_empty() {
            "void".to_string()
        } else {
            rendered.join(", ")
        };
        let ret = if ret == "()" {
            "void".to_string()
        } else {
            self.declare(ret, "")?.trim().to_string()
        };
        Ok(Some(format!("{ret} (*{name})({args})")))
    }

    /// The ABI class of `ty`, resolved through the alias chain.
    pub fn abi(&self, ty: &str) -> Abi {
        let ty = ty.trim();
        if ty == "..." {
            return Abi::Variadic;
        }
        if ty == "()" {
            return Abi::Void;
        }
        if ty == "!" {
            return Abi::Never;
        }
        if ty.starts_with('*')
            || ty.starts_with("Option<")
            || ty.starts_with("extern \"C\" fn")
            || ty.starts_with("unsafe extern \"C\" fn")
            || ty.starts_with('&')
        {
            return Abi::Ptr;
        }
        if ty.starts_with('[') {
            return Abi::Aggregate(ty.to_string());
        }
        let mut name = Self::unqualify(ty).to_string();
        for _ in 0..8 {
            if let Some((_, bytes, signed)) = Self::primitive(&name) {
                return if bytes == 0 {
                    Abi::Void
                } else {
                    Abi::Int { bytes, signed }
                };
            }
            match self.aliases.get(&name) {
                Some(next) => {
                    let next = next.trim();
                    if next.starts_with('*') {
                        return Abi::Ptr;
                    }
                    name = Self::unqualify(next).to_string();
                }
                None => break,
            }
        }
        if self.struct_tags.contains(&name) || self.typedefs.contains(&name) {
            // A by-value aggregate, or an opaque (`FILE`) that only ever
            // appears behind a pointer.
            return Abi::Aggregate(name);
        }
        Abi::Opaque(name)
    }
}

/// Splits a comma-separated list, ignoring commas inside `<>`, `()` and `[]`.
pub fn split_top_level(input: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    let mut previous = ' ';
    for ch in input.chars() {
        match ch {
            '<' | '(' | '[' => {
                depth += 1;
                current.push(ch);
            }
            // `->` is a return arrow, not a closing angle bracket.
            '>' if previous == '-' => current.push(ch),
            '>' | ')' | ']' => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => {
                parts.push(current.trim().to_string());
                current = String::new();
            }
            _ => current.push(ch),
        }
        previous = ch;
    }
    let tail = current.trim();
    if !tail.is_empty() {
        parts.push(tail.to_string());
    }
    parts
}

/// Translates a Rust constant initialiser into a C one.
///
/// Handles exactly what the contract uses — integer literals in any base,
/// `_` digit separators, literal suffixes, `!x`, `x as T`, references to other
/// constants, and the all-zero struct literals behind the `pthread_*`
/// initialisers — and errors on anything else.
pub fn const_expr(expr: &str, ty: &str, types: &Types) -> Result<String, String> {
    let expr = expr.trim();

    // `TYPE { field: [0; N] }` — the `PTHREAD_*_INITIALIZER` shape: one field,
    // an all-zero array. C's aggregate initialiser zero-fills the rest, so
    // `{ { 0 } }` is the whole of it.
    if let Some((_, body)) = expr.split_once('{') {
        if expr.ends_with('}') {
            let body = body.trim_end_matches('}');
            let fields = split_top_level(body);
            let [field] = fields.as_slice() else {
                return Err(format!(
                    "struct initialiser `{expr}` has {} fields; only the one-array-field \
                     shape the contract uses is translatable",
                    fields.len()
                ));
            };
            let (_, value) = field
                .split_once(':')
                .ok_or_else(|| format!("struct initialiser field without a value: `{field}`"))?;
            let value = value.trim();
            let element = match value.strip_prefix('[') {
                Some(array) => array.split(';').next().unwrap_or("").trim(),
                None => value,
            };
            if element.is_empty() || !element.trim_start_matches('0').is_empty() {
                return Err(format!("non-zero struct initialiser `{expr}`"));
            }
            return Ok("{ { 0 } }".to_string());
        }
    }

    if let Some((value, cast)) = split_cast(expr) {
        let inner = const_expr(&value, ty, types)?;
        let cast = types.declare(&cast, "")?.trim().to_string();
        return Ok(format!("(({cast}){inner})"));
    }

    if let Some(rest) = expr.strip_prefix('!') {
        let inner = const_expr(rest, ty, types)?;
        let cast = types.declare(ty, "")?.trim().to_string();
        return Ok(format!("(({cast})~{inner})"));
    }

    if let Some(literal) = c_literal(expr) {
        return Ok(literal);
    }

    // A reference to another constant: emitted as the C name, which is defined
    // in one of the generated headers.
    if expr
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && expr
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
    {
        return Ok(expr.to_string());
    }

    Err(format!("cannot translate constant expression `{expr}`"))
}

/// Splits `value as Type` at the last top-level `as`.
fn split_cast(expr: &str) -> Option<(String, String)> {
    let idx = expr.rfind(" as ")?;
    let (value, cast) = expr.split_at(idx);
    Some((value.trim().to_string(), cast[4..].trim().to_string()))
}

/// A Rust integer literal as a C one: `0o100` -> `0100`, `0x0000_0001` ->
/// `0x00000001`, and any `u32`/`usize`-style suffix dropped (the surrounding
/// cast or `#define` type carries it).
fn c_literal(expr: &str) -> Option<String> {
    let (sign, digits) = match expr.strip_prefix('-') {
        Some(rest) => ("-", rest.trim()),
        None => ("", expr),
    };
    let digits: String = digits.chars().filter(|c| *c != '_').collect();
    let digits = match digits.find(|c: char| c == 'u' || c == 'i') {
        // Only a trailing type suffix may contain `u`/`i`; a hex literal's
        // digits never do, so an earlier hit means this is not a literal.
        Some(at)
            if at > 0
                && digits[..at]
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() || c == 'x') =>
        {
            digits[..at].to_string()
        }
        Some(_) => return None,
        None => digits,
    };
    let body = if let Some(octal) = digits.strip_prefix("0o") {
        if !octal.chars().all(|c| c.is_digit(8)) {
            return None;
        }
        format!("0{octal}")
    } else if let Some(hex) = digits.strip_prefix("0x") {
        if hex.is_empty() || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        format!("0x{hex}")
    } else if let Some(binary) = digits.strip_prefix("0b") {
        let value = u64::from_str_radix(binary, 2).ok()?;
        format!("0x{value:x}")
    } else {
        if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        digits
    };
    let mut out = String::new();
    let _ = write!(out, "{sign}{body}");
    Some(out)
}
