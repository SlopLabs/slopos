//! The Linux-numbered calls the kernel answers: the rows of the one top-level,
//! ungated `SYSCALL_TABLE` static in `core/src/syscall/handlers.rs`, a
//! `syscall_table!` whose body is `size: ..; base: 0;` then
//! `[SYSCALL_X] => handler, "name";` rows.

use syn::parse::{ParseStream, Parser};
use syn::{Expr, Ident, Item, Lit, LitStr, Path, Token, bracketed};

use crate::Result;
use crate::rust::{self, Source};

pub const TABLE: &str = "SYSCALL_TABLE";

/// A row: the number's constant and the line it is on.
pub struct Row {
    pub constant: String,
    pub line: usize,
}

/// The table's rows, or why the table could not be read as one.
pub fn rows(path: &str, text: &str) -> Result<std::result::Result<Vec<Row>, String>> {
    let source = Source::parse(path, text.to_string())?;
    let tables: Vec<_> = source
        .file
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Static(s) if s.ident == TABLE => Some(s),
            _ => None,
        })
        .collect();
    let table = match tables.as_slice() {
        [] => return Ok(Err(format!("no `static {TABLE}`"))),
        [one] if rust::is_cfg_gated(&one.attrs) => {
            return Ok(Err(format!(
                "`{TABLE}` is `#[cfg]`-gated, so which calls are answered depends on the build"
            )));
        }
        [one] => one,
        _ => {
            return Ok(Err(format!(
                "{} `static {TABLE}`s: the table read would be a guess",
                tables.len()
            )));
        }
    };
    let Expr::Macro(mac) = &*table.expr else {
        return Ok(Err(format!("`{TABLE}` is not a `syscall_table!`")));
    };
    if rust::last_ident(&mac.mac.path).as_deref() != Some("syscall_table") {
        return Ok(Err(format!("`{TABLE}` is not a `syscall_table!`")));
    }
    Ok(body
        .parse2(mac.mac.tokens.clone())
        .map_err(|e| format!("`{TABLE}`'s body, line {}: {e}", rust::line(e.span()))))
}

fn body(input: ParseStream) -> syn::Result<Vec<Row>> {
    for header in ["size", "base"] {
        let key: Ident = input.parse()?;
        if key != header {
            return Err(syn::Error::new(key.span(), format!("expected `{header}:`")));
        }
        input.parse::<Token![:]>()?;
        let value: Expr = input.parse()?;
        if header == "base"
            && !matches!(&value, Expr::Lit(l) if matches!(&l.lit, Lit::Int(i) if i.base10_digits() == "0"))
        {
            return Err(syn::Error::new_spanned(
                value,
                "a Linux-numbered table starts at `base: 0`",
            ));
        }
        input.parse::<Token![;]>()?;
    }
    let mut rows = Vec::new();
    while !input.is_empty() {
        let slot;
        bracketed!(slot in input);
        let constant: Ident = slot.parse()?;
        if !slot.is_empty() {
            return Err(slot.error("a slot names one constant"));
        }
        input.parse::<Token![=>]>()?;
        input.parse::<Path>()?;
        input.parse::<Token![,]>()?;
        input.parse::<LitStr>()?;
        input.parse::<Token![;]>()?;
        rows.push(Row {
            constant: constant.to_string(),
            line: rust::line(constant.span()),
        });
    }
    Ok(rows)
}
