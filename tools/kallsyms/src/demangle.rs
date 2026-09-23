//! The name `llvm-nm --demangle` prints for a symbol.
//!
//! `llvm::demangle` tries the Itanium, Rust v0 and D demanglers on the name,
//! then on the name without one leading `_`, then the Microsoft demangler, and
//! prints the name unchanged when none accepts it. The kernel's symbols are v0
//! Rust names and plain C names, so only the v0 demangler is reproduced here,
//! byte for byte with LLVM's output. A name another demangler may accept is
//! refused rather than printed raw, because the table would then differ from
//! the one llvm-nm describes.
//!
//! The v0 demangler is derived from LLVM's `llvm/lib/Demangle/RustDemangle.cpp`,
//! © the llvm-project contributors, Apache-2.0 WITH LLVM-exception; see
//! NOTICE.md.

/// A name one of the demanglers this tool does not reproduce would accept.
#[derive(Debug)]
pub struct Unsupported(pub &'static str);

pub fn demangle(name: &[u8]) -> Result<Vec<u8>, Unsupported> {
    if let Some(out) = non_microsoft(name, true)? {
        return Ok(out);
    }
    if let Some(rest) = name.strip_prefix(b"_")
        && let Some(out) = non_microsoft(rest, false)?
    {
        return Ok(out);
    }
    if ms_may_accept(name) {
        return Err(Unsupported("Microsoft"));
    }
    Ok(name.to_vec())
}

/// Microsoft names start with `?`; the one exception is an RTTI type name,
/// `.` then a type that must use up the rest of the name: a class, pointer,
/// array, function or qualified type (refused whatever follows), or one
/// primitive. `.halt_loop` is an assembler label, and `h` starts no type.
fn ms_may_accept(name: &[u8]) -> bool {
    if name.starts_with(b"?") {
        return true;
    }
    let Some(ty) = name.strip_prefix(b".") else {
        return false;
    };
    matches!(
        ty,
        [
            b'?' | b'T' | b'U' | b'V' | b'W' | b'A' | b'P' | b'Q' | b'R' | b'S' | b'Y',
            ..
        ] | [b'$', b'$', ..]
            | [b'X'
                | b'D'
                | b'C'
                | b'E'
                | b'F'
                | b'G'
                | b'H'
                | b'I'
                | b'J'
                | b'K'
                | b'M'
                | b'N'
                | b'O']
            | [
                b'_',
                b'N' | b'J' | b'K' | b'W' | b'Q' | b'S' | b'U' | b'P' | b'T'
            ]
    )
}

fn non_microsoft(name: &[u8], leading_dot: bool) -> Result<Option<Vec<u8>>, Unsupported> {
    let (dot, mangled) = match name.strip_prefix(b".") {
        Some(rest) if leading_dot => (true, rest),
        _ => (false, name),
    };
    if is_itanium(mangled) {
        return Err(Unsupported("Itanium"));
    }
    if mangled.starts_with(b"_R") {
        return Ok(rust_v0(mangled).map(|body| {
            let mut out = Vec::with_capacity(body.len() + 1);
            if dot {
                out.push(b'.');
            }
            out.extend_from_slice(&body);
            out
        }));
    }
    if dlang_may_accept(mangled) {
        return Err(Unsupported("D"));
    }
    Ok(None)
}

/// One to four underscores then `Z`, after an optional `__alloc_token_<n>_`.
fn is_itanium(mut s: &[u8]) -> bool {
    if let Some(rest) = s.strip_prefix(b"__alloc_token_") {
        s = rest;
        if s.first().is_some_and(u8::is_ascii_digit) {
            while s.first().is_some_and(u8::is_ascii_digit) {
                s = &s[1..];
            }
            if let Some(rest) = s.strip_prefix(b"_") {
                s = rest;
            }
        }
    }
    let underscores = s.iter().take_while(|&&b| b == b'_').count();
    (1..=4).contains(&underscores) && s.get(underscores) == Some(&b'Z')
}

/// LLVM's D demangler fails on anything but `_Dmain` or a qualified name,
/// which starts with a length or a `Q` back reference.
fn dlang_may_accept(s: &[u8]) -> bool {
    match s.strip_prefix(b"_D") {
        Some(b"main") => true,
        Some(rest) => rest
            .first()
            .is_some_and(|&b| b.is_ascii_digit() || b == b'Q'),
        None => false,
    }
}

const MAX_RECURSION: usize = 500;

/// `<symbol-name> = "_R" <path> [<instantiating-crate>]`, with anything after
/// the first `.` printed as ` (<suffix>)`.
fn rust_v0(mangled: &[u8]) -> Option<Vec<u8>> {
    let body = mangled.strip_prefix(b"_R")?;
    let dot = body.iter().position(|&b| b == b'.');
    let mut d = V0 {
        input: &body[..dot.unwrap_or(body.len())],
        pos: 0,
        print: true,
        error: false,
        depth: 0,
        bound_lifetimes: 0,
        out: Vec::new(),
    };
    d.path(false, false);
    if d.pos != d.input.len() {
        d.print = false;
        d.path(false, false);
        d.print = true;
    }
    if d.pos != d.input.len() {
        d.error = true;
    }
    if let Some(dot) = dot {
        d.print_bytes(b" (");
        d.print_bytes(&body[dot..]);
        d.print_bytes(b")");
    }
    if d.error { None } else { Some(d.out) }
}

struct V0<'a> {
    input: &'a [u8],
    pos: usize,
    print: bool,
    error: bool,
    depth: usize,
    bound_lifetimes: u64,
    out: Vec<u8>,
}

struct Ident<'a> {
    name: &'a [u8],
    punycode: bool,
}

fn is_valid(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn basic_type(c: u8) -> Option<&'static str> {
    Some(match c {
        b'a' => "i8",
        b'b' => "bool",
        b'c' => "char",
        b'd' => "f64",
        b'e' => "str",
        b'f' => "f32",
        b'h' => "u8",
        b'i' => "isize",
        b'j' => "usize",
        b'l' => "i32",
        b'm' => "u32",
        b'n' => "i128",
        b'o' => "u128",
        b'p' => "_",
        b's' => "i16",
        b't' => "u16",
        b'u' => "()",
        b'v' => "...",
        b'x' => "i64",
        b'y' => "u64",
        b'z' => "!",
        _ => return None,
    })
}

impl<'a> V0<'a> {
    fn enter(&mut self) -> bool {
        if self.error || self.depth >= MAX_RECURSION {
            self.error = true;
            return false;
        }
        self.depth += 1;
        true
    }

    /// Returns whether the generic arguments were left open.
    fn path(&mut self, in_type: bool, leave_open: bool) -> bool {
        if !self.enter() {
            return false;
        }
        let open = self.path_body(in_type, leave_open);
        self.depth -= 1;
        open
    }

    fn path_body(&mut self, in_type: bool, leave_open: bool) -> bool {
        match self.consume() {
            b'C' => {
                self.optional_base62(b's');
                let ident = self.identifier();
                self.print_identifier(&ident);
            }
            b'M' => {
                self.impl_path(in_type);
                self.print_bytes(b"<");
                self.ty();
                self.print_bytes(b">");
            }
            b'X' => {
                self.impl_path(in_type);
                self.print_bytes(b"<");
                self.ty();
                self.print_bytes(b" as ");
                self.path(true, false);
                self.print_bytes(b">");
            }
            b'Y' => {
                self.print_bytes(b"<");
                self.ty();
                self.print_bytes(b" as ");
                self.path(true, false);
                self.print_bytes(b">");
            }
            b'N' => {
                let ns = self.consume();
                if !ns.is_ascii_alphabetic() {
                    self.error = true;
                    return false;
                }
                self.path(in_type, false);
                let disambiguator = self.optional_base62(b's');
                let ident = self.identifier();
                if ns.is_ascii_uppercase() {
                    self.print_bytes(b"::{");
                    match ns {
                        b'C' => self.print_bytes(b"closure"),
                        b'S' => self.print_bytes(b"shim"),
                        _ => self.print_bytes(&[ns]),
                    }
                    if !ident.name.is_empty() {
                        self.print_bytes(b":");
                        self.print_identifier(&ident);
                    }
                    self.print_bytes(b"#");
                    self.print_decimal(disambiguator);
                    self.print_bytes(b"}");
                } else if !ident.name.is_empty() {
                    self.print_bytes(b"::");
                    self.print_identifier(&ident);
                }
            }
            b'I' => {
                self.path(in_type, false);
                if !in_type {
                    self.print_bytes(b"::");
                }
                self.print_bytes(b"<");
                let mut i = 0;
                while !self.error && !self.consume_if(b'E') {
                    if i > 0 {
                        self.print_bytes(b", ");
                    }
                    self.generic_arg();
                    i += 1;
                }
                if leave_open {
                    return true;
                }
                self.print_bytes(b">");
            }
            b'B' => {
                let mut open = false;
                self.backref(|d| open = d.path(in_type, leave_open));
                return open;
            }
            _ => self.error = true,
        }
        false
    }

    fn impl_path(&mut self, in_type: bool) {
        let saved = self.print;
        self.print = false;
        self.optional_base62(b's');
        self.path(in_type, false);
        self.print = saved;
    }

    fn backref(&mut self, f: impl FnOnce(&mut Self)) {
        let target = self.base62();
        if self.error || target >= self.pos as u64 {
            self.error = true;
            return;
        }
        if !self.print {
            return;
        }
        let saved = self.pos;
        self.pos = target as usize;
        f(self);
        self.pos = saved;
    }

    fn generic_arg(&mut self) {
        if self.consume_if(b'L') {
            let lifetime = self.base62();
            self.print_lifetime(lifetime);
        } else if self.consume_if(b'K') {
            self.constant();
        } else {
            self.ty();
        }
    }

    fn ty(&mut self) {
        if !self.enter() {
            return;
        }
        self.ty_body();
        self.depth -= 1;
    }

    fn ty_body(&mut self) {
        let start = self.pos;
        let c = self.consume();
        if let Some(name) = basic_type(c) {
            self.print_bytes(name.as_bytes());
            return;
        }
        match c {
            b'A' => {
                self.print_bytes(b"[");
                self.ty();
                self.print_bytes(b"; ");
                self.constant();
                self.print_bytes(b"]");
            }
            b'S' => {
                self.print_bytes(b"[");
                self.ty();
                self.print_bytes(b"]");
            }
            b'T' => {
                self.print_bytes(b"(");
                let mut i = 0;
                while !self.error && !self.consume_if(b'E') {
                    if i > 0 {
                        self.print_bytes(b", ");
                    }
                    self.ty();
                    i += 1;
                }
                if i == 1 {
                    self.print_bytes(b",");
                }
                self.print_bytes(b")");
            }
            b'R' | b'Q' => {
                self.print_bytes(b"&");
                if self.consume_if(b'L') {
                    let lifetime = self.base62();
                    if lifetime != 0 {
                        self.print_lifetime(lifetime);
                        self.print_bytes(b" ");
                    }
                }
                if c == b'Q' {
                    self.print_bytes(b"mut ");
                }
                self.ty();
            }
            b'P' => {
                self.print_bytes(b"*const ");
                self.ty();
            }
            b'O' => {
                self.print_bytes(b"*mut ");
                self.ty();
            }
            b'F' => self.fn_sig(),
            b'D' => {
                self.dyn_bounds();
                if self.consume_if(b'L') {
                    let lifetime = self.base62();
                    if lifetime != 0 {
                        self.print_bytes(b" + ");
                        self.print_lifetime(lifetime);
                    }
                } else {
                    self.error = true;
                }
            }
            b'B' => self.backref(|d| d.ty()),
            _ => {
                self.pos = start;
                self.path(true, false);
            }
        }
    }

    /// `<fn-sig> := [<binder>] ["U"] ["K" <abi>] {<type>} "E" <type>`
    fn fn_sig(&mut self) {
        let saved = self.bound_lifetimes;
        self.optional_binder();
        if self.consume_if(b'U') {
            self.print_bytes(b"unsafe ");
        }
        if self.consume_if(b'K') {
            self.print_bytes(b"extern \"");
            if self.consume_if(b'C') {
                self.print_bytes(b"C");
            } else {
                let ident = self.identifier();
                if ident.punycode {
                    self.error = true;
                }
                for &b in ident.name {
                    self.print_bytes(&[if b == b'_' { b'-' } else { b }]);
                }
            }
            self.print_bytes(b"\" ");
        }
        self.print_bytes(b"fn(");
        let mut i = 0;
        while !self.error && !self.consume_if(b'E') {
            if i > 0 {
                self.print_bytes(b", ");
            }
            self.ty();
            i += 1;
        }
        self.print_bytes(b")");
        if !self.consume_if(b'u') {
            self.print_bytes(b" -> ");
            self.ty();
        }
        self.bound_lifetimes = saved;
    }

    fn dyn_bounds(&mut self) {
        let saved = self.bound_lifetimes;
        self.print_bytes(b"dyn ");
        self.optional_binder();
        let mut i = 0;
        while !self.error && !self.consume_if(b'E') {
            if i > 0 {
                self.print_bytes(b" + ");
            }
            self.dyn_trait();
            i += 1;
        }
        self.bound_lifetimes = saved;
    }

    fn dyn_trait(&mut self) {
        let mut open = self.path(true, true);
        while !self.error && self.consume_if(b'p') {
            if open {
                self.print_bytes(b", ");
            } else {
                open = true;
                self.print_bytes(b"<");
            }
            let ident = self.identifier();
            self.print_bytes(ident.name);
            self.print_bytes(b" = ");
            self.ty();
        }
        if open {
            self.print_bytes(b">");
        }
    }

    fn optional_binder(&mut self) {
        let binder = self.optional_base62(b'G');
        if self.error || binder == 0 {
            return;
        }
        // Each bound lifetime costs at least one byte of input to reference.
        if binder >= (self.input.len() as u64).wrapping_sub(self.bound_lifetimes) {
            self.error = true;
            return;
        }
        self.print_bytes(b"for<");
        for i in 0..binder {
            self.bound_lifetimes += 1;
            if i > 0 {
                self.print_bytes(b", ");
            }
            self.print_lifetime(1);
        }
        self.print_bytes(b"> ");
    }

    fn constant(&mut self) {
        if !self.enter() {
            return;
        }
        self.constant_body();
        self.depth -= 1;
    }

    fn constant_body(&mut self) {
        match self.consume() {
            b'a' | b's' | b'l' | b'x' | b'n' | b'i' | b'h' | b't' | b'm' | b'y' | b'o' | b'j' => {
                self.const_int()
            }
            b'b' => {
                let (digits, _) = self.hex_number();
                match digits {
                    b"0" => self.print_bytes(b"false"),
                    b"1" => self.print_bytes(b"true"),
                    _ => self.error = true,
                }
            }
            b'c' => self.const_char(),
            b'p' => self.print_bytes(b"_"),
            b'B' => self.backref(|d| d.constant()),
            _ => self.error = true,
        }
    }

    fn const_int(&mut self) {
        if self.consume_if(b'n') {
            self.print_bytes(b"-");
        }
        let (digits, value) = self.hex_number();
        if digits.len() <= 16 {
            self.print_decimal(value);
        } else {
            self.print_bytes(b"0x");
            self.print_bytes(digits);
        }
    }

    fn const_char(&mut self) {
        let (digits, code) = self.hex_number();
        if self.error || digits.len() > 6 {
            self.error = true;
            return;
        }
        self.print_bytes(b"'");
        match code {
            0x09 => self.print_bytes(b"\\t"),
            0x0d => self.print_bytes(b"\\r"),
            0x0a => self.print_bytes(b"\\n"),
            0x5c => self.print_bytes(b"\\\\"),
            0x22 => self.print_bytes(b"\""),
            0x27 => self.print_bytes(b"\\'"),
            0x20..=0x7e => self.print_bytes(&[code as u8]),
            _ => {
                self.print_bytes(b"\\u{");
                self.print_bytes(digits);
                self.print_bytes(b"}");
            }
        }
        self.print_bytes(b"'");
    }

    /// `<undisambiguated-identifier> = ["u"] <decimal-number> ["_"] <bytes>`
    fn identifier(&mut self) -> Ident<'a> {
        let punycode = self.consume_if(b'u');
        let len = self.decimal();
        self.consume_if(b'_');
        let empty = Ident {
            name: &[],
            punycode: false,
        };
        if self.error || len > (self.input.len() - self.pos) as u64 {
            self.error = true;
            return empty;
        }
        let name = &self.input[self.pos..self.pos + len as usize];
        self.pos += len as usize;
        if !name.iter().all(|&b| is_valid(b)) {
            self.error = true;
            return empty;
        }
        Ident { name, punycode }
    }

    /// 0 when `tag` is absent, the number plus one otherwise.
    fn optional_base62(&mut self, tag: u8) -> u64 {
        if !self.consume_if(tag) {
            return 0;
        }
        let n = self.base62();
        if self.error {
            return 0;
        }
        self.checked(n.checked_add(1))
    }

    /// `<base-62-number> = {<0-9a-zA-Z>} "_"`, offset by one so `_` is 0.
    fn base62(&mut self) -> u64 {
        if self.consume_if(b'_') {
            return 0;
        }
        let mut value: u64 = 0;
        loop {
            let c = self.consume();
            let digit = match c {
                b'_' => break,
                b'0'..=b'9' => c - b'0',
                b'a'..=b'z' => 10 + c - b'a',
                b'A'..=b'Z' => 36 + c - b'A',
                _ => {
                    self.error = true;
                    return 0;
                }
            };
            value = self.checked(value.checked_mul(62));
            if self.error {
                return 0;
            }
            value = self.checked(value.checked_add(u64::from(digit)));
            if self.error {
                return 0;
            }
        }
        self.checked(value.checked_add(1))
    }

    /// `<decimal-number> = "0" | <1-9> {<0-9>}`
    fn decimal(&mut self) -> u64 {
        let c = self.look();
        if !c.is_ascii_digit() {
            self.error = true;
            return 0;
        }
        if c == b'0' {
            self.consume();
            return 0;
        }
        let mut value: u64 = 0;
        while self.look().is_ascii_digit() {
            value = self.checked(value.checked_mul(10));
            if self.error {
                return 0;
            }
            let digit = u64::from(self.consume() - b'0');
            value = self.checked(value.checked_add(digit));
            if self.error {
                return 0;
            }
        }
        value
    }

    /// `<hex-number> = "0_" | <1-9a-f> {<0-9a-f>} "_"`: the digits and their
    /// value, which is meaningless past 16 digits.
    fn hex_number(&mut self) -> (&'a [u8], u64) {
        let start = self.pos;
        let mut value: u64 = 0;
        if !matches!(self.look(), b'0'..=b'9' | b'a'..=b'f') {
            self.error = true;
        }
        if self.consume_if(b'0') {
            if !self.consume_if(b'_') {
                self.error = true;
            }
        } else {
            while !self.error && !self.consume_if(b'_') {
                let c = self.consume();
                value = value.wrapping_mul(16);
                match c {
                    b'0'..=b'9' => value = value.wrapping_add(u64::from(c - b'0')),
                    b'a'..=b'f' => value = value.wrapping_add(u64::from(10 + c - b'a')),
                    _ => self.error = true,
                }
            }
        }
        if self.error {
            return (&[], 0);
        }
        (&self.input[start..self.pos - 1], value)
    }

    fn checked(&mut self, value: Option<u64>) -> u64 {
        value.unwrap_or_else(|| {
            self.error = true;
            0
        })
    }

    fn look(&self) -> u8 {
        if self.error {
            return 0;
        }
        self.input.get(self.pos).copied().unwrap_or(0)
    }

    fn consume(&mut self) -> u8 {
        match self.input.get(self.pos) {
            Some(&b) if !self.error => {
                self.pos += 1;
                b
            }
            _ => {
                self.error = true;
                0
            }
        }
    }

    fn consume_if(&mut self, b: u8) -> bool {
        if !self.error && self.input.get(self.pos) == Some(&b) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn print_bytes(&mut self, bytes: &[u8]) {
        if !self.error && self.print {
            self.out.extend_from_slice(bytes);
        }
    }

    fn print_decimal(&mut self, n: u64) {
        if !self.error && self.print {
            self.out.extend_from_slice(n.to_string().as_bytes());
        }
    }

    /// 0 is the erased lifetime; the rest are De Bruijn indices into the
    /// enclosing binders.
    fn print_lifetime(&mut self, index: u64) {
        if index == 0 {
            self.print_bytes(b"'_");
            return;
        }
        if index > self.bound_lifetimes {
            self.error = true;
            return;
        }
        let depth = self.bound_lifetimes - index;
        self.print_bytes(b"'");
        if depth < 26 {
            self.print_bytes(&[b'a' + depth as u8]);
        } else {
            self.print_bytes(b"z");
            self.print_decimal(depth - 26 + 1);
        }
    }

    fn print_identifier(&mut self, ident: &Ident<'_>) {
        if self.error || !self.print {
            return;
        }
        if ident.punycode {
            if !decode_punycode(ident.name, &mut self.out) {
                self.error = true;
            }
        } else {
            self.out.extend_from_slice(ident.name);
        }
    }
}

/// RFC 3492 with `_` as the delimiter. Code points are decoded into 4-byte
/// NUL-padded slots so an insertion index is a slot index, then the padding is
/// dropped.
fn decode_punycode(input: &[u8], out: &mut Vec<u8>) -> bool {
    const BASE: usize = 36;
    const TMIN: usize = 1;
    const TMAX: usize = 26;
    const SKEW: usize = 38;

    let start = out.len();
    let mut idx = 0;
    if let Some(delim) = input.iter().rposition(|&b| b == b'_') {
        for &c in &input[..delim] {
            if !is_valid(c) {
                return false;
            }
            out.extend_from_slice(&[c, 0, 0, 0]);
        }
        idx = delim + 1;
    }

    let mut bias = 72;
    let mut n: usize = 0x80;
    let mut damp = 700;
    let mut i: usize = 0;
    while idx != input.len() {
        let old_i = i;
        let mut w: usize = 1;
        let mut k = BASE;
        loop {
            let Some(&c) = input.get(idx) else {
                return false;
            };
            idx += 1;
            let digit = match c {
                b'a'..=b'z' => usize::from(c - b'a'),
                b'0'..=b'9' => 26 + usize::from(c - b'0'),
                _ => return false,
            };
            if digit > (usize::MAX - i) / w {
                return false;
            }
            i += digit * w;
            let t = if k <= bias {
                TMIN
            } else if k >= bias + TMAX {
                TMAX
            } else {
                k - bias
            };
            if digit < t {
                break;
            }
            if w > usize::MAX / (BASE - t) {
                return false;
            }
            w *= BASE - t;
            k += BASE;
        }
        let points = (out.len() - start) / 4 + 1;

        let mut delta = (i - old_i) / damp;
        delta += delta / points;
        damp = 2;
        let mut kk = 0;
        while delta > (BASE - TMIN) * TMAX / 2 {
            delta /= BASE - TMIN;
            kk += BASE;
        }
        bias = kk + ((BASE - TMIN + 1) * delta) / (delta + SKEW);

        if i / points > usize::MAX - n {
            return false;
        }
        n += i / points;
        i %= points;

        let Some(utf8) = encode_utf8(n) else {
            return false;
        };
        let at = start + i * 4;
        out.splice(at..at, utf8);
        i += 1;
    }

    let decoded: Vec<u8> = out.drain(start..).filter(|&b| b != 0).collect();
    out.extend_from_slice(&decoded);
    true
}

fn encode_utf8(cp: usize) -> Option<[u8; 4]> {
    if (0xd800..=0xdfff).contains(&cp) {
        return None;
    }
    Some(match cp {
        0..=0x7f => [cp as u8, 0, 0, 0],
        0x80..=0x7ff => [
            0xc0 | ((cp >> 6) & 0x3f) as u8,
            0x80 | (cp & 0x3f) as u8,
            0,
            0,
        ],
        0x800..=0xffff => [
            0xe0 | (cp >> 12) as u8,
            0x80 | ((cp >> 6) & 0x3f) as u8,
            0x80 | (cp & 0x3f) as u8,
            0,
        ],
        0x1_0000..=0x10_ffff => [
            0xf0 | (cp >> 18) as u8,
            0x80 | ((cp >> 12) & 0x3f) as u8,
            0x80 | ((cp >> 6) & 0x3f) as u8,
            0x80 | (cp & 0x3f) as u8,
        ],
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::demangle;

    /// LLVM's own v0 cases: each `CHECK:` line is the output for the next
    /// line, compared as FileCheck does, with runs of blanks equal to one.
    #[test]
    fn matches_llvm_rust_demangle_suite() {
        let suite = include_str!("../testdata/llvm-rust-demangle.test");
        let mut expected: Option<&str> = None;
        let mut cases = 0;
        for line in suite.lines() {
            if let Some(want) = line.strip_prefix("CHECK: ") {
                expected = Some(want);
            } else if let Some(want) = expected.take() {
                let mangled = line.trim();
                let got = demangle(mangled.as_bytes()).expect("a v0 name");
                let got = String::from_utf8_lossy(&got);
                let squeeze = |s: &str| {
                    s.split(' ')
                        .filter(|w| !w.is_empty())
                        .collect::<Vec<_>>()
                        .join(" ")
                };
                assert_eq!(squeeze(&got), squeeze(want), "demangling {mangled}");
                cases += 1;
            }
        }
        assert_eq!(cases, 152);
    }

    #[test]
    fn refuses_names_other_demanglers_accept() {
        for name in [
            "_ZN4core3fmt5write17h0123456789abcdefE",
            "__Z3foov",
            "_D3foo3barFZv",
            "?x@@YAXXZ",
        ] {
            assert!(demangle(name.as_bytes()).is_err(), "{name}");
        }
        for name in [
            "_DYNAMIC",
            "__safestack_pointer_address",
            "memcpy",
            ".halt_loop",
        ] {
            assert_eq!(demangle(name.as_bytes()).unwrap(), name.as_bytes());
        }
    }

    #[test]
    fn strips_one_underscore_and_keeps_a_leading_dot() {
        assert_eq!(demangle(b"__RNvC1a4main").unwrap(), b"a::main");
        assert_eq!(demangle(b"._RNvC1a4main").unwrap(), b".a::main");
    }
}
