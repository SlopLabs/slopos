//! Splitting a `${...}` body into a parameter name, an operator and its word.
//!
//! Pure text work: the parameter's value and the operator word's expansion
//! both belong to the expander, which owns the variable table.

/// The operators POSIX §2.6.2 defines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamOp {
    /// `${name}`
    None,
    /// `${#name}` — the length of the value.
    Length,
    /// `${name:-word}` / `${name-word}`
    UseDefault { colon: bool },
    /// `${name:=word}` / `${name=word}`
    AssignDefault { colon: bool },
    /// `${name:?word}` / `${name?word}`
    Error { colon: bool },
    /// `${name:+word}` / `${name+word}`
    UseAlternate { colon: bool },
    /// `${name#word}` / `${name##word}`
    TrimPrefix { longest: bool },
    /// `${name%word}` / `${name%%word}`
    TrimSuffix { longest: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Param<'a> {
    pub name: &'a [u8],
    pub op: ParamOp,
    /// Raw word text for the operator, still to be expanded.
    pub arg: &'a [u8],
}

fn is_name_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

/// The one-character parameters that are not names.
fn is_special(b: u8) -> bool {
    matches!(b, b'@' | b'*' | b'#' | b'?' | b'-' | b'$' | b'!')
}

/// Split a `${...}` body. `None` means the body is malformed.
pub fn parse(content: &[u8]) -> Option<Param<'_>> {
    if content.is_empty() {
        return None;
    }

    // `${#}` is the count of positional parameters; `${#name}` is a length.
    if content[0] == b'#' && content.len() > 1 {
        let name = &content[1..];
        if name == b"@" || name == b"*" || name.iter().all(|&b| is_name_byte(b)) {
            return Some(Param {
                name,
                op: ParamOp::Length,
                arg: &[],
            });
        }
        return None;
    }

    let name_len = if is_special(content[0]) {
        1
    } else if content[0].is_ascii_digit() {
        content.iter().take_while(|b| b.is_ascii_digit()).count()
    } else if is_name_byte(content[0]) {
        content.iter().take_while(|&&b| is_name_byte(b)).count()
    } else {
        return None;
    };

    let name = &content[..name_len];
    let rest = &content[name_len..];
    if rest.is_empty() {
        return Some(Param {
            name,
            op: ParamOp::None,
            arg: &[],
        });
    }

    let (op, skip) = match (rest[0], rest.get(1).copied()) {
        (b':', Some(b'-')) => (ParamOp::UseDefault { colon: true }, 2),
        (b':', Some(b'=')) => (ParamOp::AssignDefault { colon: true }, 2),
        (b':', Some(b'?')) => (ParamOp::Error { colon: true }, 2),
        (b':', Some(b'+')) => (ParamOp::UseAlternate { colon: true }, 2),
        (b'-', _) => (ParamOp::UseDefault { colon: false }, 1),
        (b'=', _) => (ParamOp::AssignDefault { colon: false }, 1),
        (b'?', _) => (ParamOp::Error { colon: false }, 1),
        (b'+', _) => (ParamOp::UseAlternate { colon: false }, 1),
        (b'#', Some(b'#')) => (ParamOp::TrimPrefix { longest: true }, 2),
        (b'#', _) => (ParamOp::TrimPrefix { longest: false }, 1),
        (b'%', Some(b'%')) => (ParamOp::TrimSuffix { longest: true }, 2),
        (b'%', _) => (ParamOp::TrimSuffix { longest: false }, 1),
        _ => return None,
    };

    Some(Param {
        name,
        op,
        arg: &rest[skip..],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_name() {
        assert_eq!(
            parse(b"HOME"),
            Some(Param {
                name: b"HOME",
                op: ParamOp::None,
                arg: b""
            })
        );
    }

    #[test]
    fn multi_digit_positionals_are_one_name() {
        assert_eq!(parse(b"12").unwrap().name, b"12");
    }

    #[test]
    fn length_and_count_are_told_apart() {
        assert_eq!(parse(b"#").unwrap().op, ParamOp::None);
        assert_eq!(parse(b"#").unwrap().name, b"#");
        assert_eq!(parse(b"#x").unwrap().op, ParamOp::Length);
        assert_eq!(parse(b"#x").unwrap().name, b"x");
    }

    #[test]
    fn the_colon_forms_are_distinct_from_the_bare_ones() {
        assert_eq!(
            parse(b"x:-a").unwrap().op,
            ParamOp::UseDefault { colon: true }
        );
        assert_eq!(
            parse(b"x-a").unwrap().op,
            ParamOp::UseDefault { colon: false }
        );
        assert_eq!(
            parse(b"x:=a").unwrap().op,
            ParamOp::AssignDefault { colon: true }
        );
        assert_eq!(parse(b"x:?msg").unwrap().arg, b"msg");
        assert_eq!(
            parse(b"x:+a").unwrap().op,
            ParamOp::UseAlternate { colon: true }
        );
    }

    #[test]
    fn trim_forms_carry_their_greed() {
        assert_eq!(
            parse(b"x#*/").unwrap().op,
            ParamOp::TrimPrefix { longest: false }
        );
        assert_eq!(
            parse(b"x##*/").unwrap().op,
            ParamOp::TrimPrefix { longest: true }
        );
        assert_eq!(
            parse(b"x%.c").unwrap().op,
            ParamOp::TrimSuffix { longest: false }
        );
        assert_eq!(parse(b"x%%.c").unwrap().arg, b".c");
    }

    #[test]
    fn specials_are_single_character_names() {
        for (body, name) in [
            (b"?".as_slice(), b"?".as_slice()),
            (b"@", b"@"),
            (b"*", b"*"),
            (b"$", b"$"),
            (b"!", b"!"),
        ] {
            assert_eq!(parse(body).unwrap().name, name);
        }
    }

    #[test]
    fn a_malformed_body_is_refused() {
        assert_eq!(parse(b""), None);
        assert_eq!(parse(b"x^y"), None);
        assert_eq!(parse(b"#a-b"), None);
    }
}
