//! Arithmetic expansion (`$(( ))`), POSIX §2.6.4.
//!
//! Signed 64-bit integers with C's operator set and precedence, over a
//! caller-supplied [`Vars`] so this file never sees the shell's variable
//! table. Every operation wraps: a panicking `$(( ))` is a shell whose scripts
//! stop, and wrapping is what C and every other shell give.

use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithError {
    Syntax,
    DivideByZero,
}

/// The variable table arithmetic reads and writes. An unset or non-numeric
/// name reads as 0, which is what POSIX requires.
pub trait Vars {
    fn get(&mut self, name: &[u8]) -> i64;
    fn set(&mut self, name: &[u8], value: i64);
}

/// A [`Vars`] over nothing: every name reads 0 and assignment is dropped.
pub struct NoVars;

impl Vars for NoVars {
    fn get(&mut self, _name: &[u8]) -> i64 {
        0
    }
    fn set(&mut self, _name: &[u8], _value: i64) {}
}

/// Parse `text` as an integer the way arithmetic expansion does: an optional
/// sign, then decimal, `0x` hex or leading-zero octal. Trailing blanks are
/// tolerated; anything else makes it 0.
pub fn value_of(text: &[u8]) -> i64 {
    let mut lexer = Lexer::new(text);
    let mut sign = 1i64;
    lexer.skip_blanks();
    while let Some(b) = lexer.peek() {
        match b {
            b'-' => {
                sign = -sign;
                lexer.pos += 1;
            }
            b'+' => lexer.pos += 1,
            _ => break,
        }
    }
    match lexer.number() {
        Some(v) => sign.wrapping_mul(v),
        None => 0,
    }
}

pub fn eval(expr: &[u8], vars: &mut dyn Vars) -> Result<i64, ArithError> {
    let mut parser = Parser {
        lexer: Lexer::new(expr),
        vars,
        skipping: false,
    };
    let value = parser.expr(0)?;
    parser.lexer.skip_blanks();
    if parser.lexer.peek().is_some() {
        return Err(ArithError::Syntax);
    }
    Ok(value)
}

struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a [u8]) -> Self {
        Self { src, pos: 0 }
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.src.get(self.pos + offset).copied()
    }

    fn skip_blanks(&mut self) {
        while self
            .peek()
            .is_some_and(|b| b == b' ' || b == b'\t' || b == b'\n' || b == b'\r')
        {
            self.pos += 1;
        }
    }

    fn number(&mut self) -> Option<i64> {
        let start = self.pos;
        let (radix, digits_start) =
            if self.peek() == Some(b'0') && matches!(self.peek_at(1), Some(b'x') | Some(b'X')) {
                (16u32, start + 2)
            } else if self.peek() == Some(b'0') {
                (8u32, start)
            } else {
                (10u32, start)
            };
        self.pos = digits_start;
        let mut value: i64 = 0;
        let mut any = false;
        while let Some(b) = self.peek() {
            let digit = match b {
                b'0'..=b'9' => (b - b'0') as u32,
                b'a'..=b'f' => (b - b'a') as u32 + 10,
                b'A'..=b'F' => (b - b'A') as u32 + 10,
                _ => break,
            };
            if digit >= radix {
                break;
            }
            value = value.wrapping_mul(radix as i64).wrapping_add(digit as i64);
            any = true;
            self.pos += 1;
        }
        if !any {
            self.pos = start;
            return None;
        }
        Some(value)
    }

    fn name(&mut self) -> Option<Vec<u8>> {
        let start = self.pos;
        if !self
            .peek()
            .is_some_and(|b| b == b'_' || b.is_ascii_alphabetic())
        {
            return None;
        }
        while self
            .peek()
            .is_some_and(|b| b == b'_' || b.is_ascii_alphanumeric())
        {
            self.pos += 1;
        }
        Some(self.src[start..self.pos].to_vec())
    }
}

/// Binary operators, with their binding power. A higher number binds tighter.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BinOp {
    LogOr,
    LogAnd,
    BitOr,
    BitXor,
    BitAnd,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Shl,
    Shr,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

impl BinOp {
    fn power(self) -> u8 {
        match self {
            BinOp::LogOr => 1,
            BinOp::LogAnd => 2,
            BinOp::BitOr => 3,
            BinOp::BitXor => 4,
            BinOp::BitAnd => 5,
            BinOp::Eq | BinOp::Ne => 6,
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => 7,
            BinOp::Shl | BinOp::Shr => 8,
            BinOp::Add | BinOp::Sub => 9,
            BinOp::Mul | BinOp::Div | BinOp::Rem => 10,
        }
    }
}

/// Precedence of the ternary conditional, which sits just above `||`.
const TERNARY_POWER: u8 = 1;

struct Parser<'a, 'v> {
    lexer: Lexer<'a>,
    vars: &'v mut dyn Vars,
    /// Set on the probe that consumes a short-circuited operand, where a
    /// division by zero yields 0 rather than an error: stopping at the error
    /// left the cursor inside the operand and its tail was then read as the
    /// left side's continuation.
    skipping: bool,
}

impl Parser<'_, '_> {
    /// Precedence-climbing parse of everything binding at least as tightly as
    /// `min_power`.
    fn expr(&mut self, min_power: u8) -> Result<i64, ArithError> {
        let mut left = self.assignment_or_unary()?;

        loop {
            self.lexer.skip_blanks();
            if min_power <= TERNARY_POWER && self.lexer.peek() == Some(b'?') {
                self.lexer.pos += 1;
                let then = self.expr(0)?;
                self.lexer.skip_blanks();
                if self.lexer.peek() != Some(b':') {
                    return Err(ArithError::Syntax);
                }
                self.lexer.pos += 1;
                let otherwise = self.expr(TERNARY_POWER)?;
                left = if left != 0 { then } else { otherwise };
                continue;
            }

            let Some((op, len)) = self.peek_binop() else {
                return Ok(left);
            };
            if op.power() < min_power {
                return Ok(left);
            }
            self.lexer.pos += len;

            // A decided `&&`/`||` must not evaluate its right side, which may
            // divide by zero or assign.
            if op == BinOp::LogAnd && left == 0 {
                self.skip_operand(op.power() + 1)?;
                left = 0;
                continue;
            }
            if op == BinOp::LogOr && left != 0 {
                self.skip_operand(op.power() + 1)?;
                left = 1;
                continue;
            }

            let right = self.expr(op.power() + 1)?;
            left = self.apply(op, left, right)?;
        }
    }

    /// Consume a short-circuited operand's text without effect, by parsing it
    /// against [`NoVars`].
    fn skip_operand(&mut self, min_power: u8) -> Result<(), ArithError> {
        let mut sink = NoVars;
        let mut probe = Parser {
            lexer: Lexer {
                src: self.lexer.src,
                pos: self.lexer.pos,
            },
            vars: &mut sink,
            skipping: true,
        };
        // The probe's extent is value-independent, so a successful parse ends
        // where the real one would have.
        probe.expr(min_power)?;
        self.lexer.pos = probe.lexer.pos;
        Ok(())
    }

    fn apply(&self, op: BinOp, left: i64, right: i64) -> Result<i64, ArithError> {
        match apply(op, left, right) {
            Err(ArithError::DivideByZero) if self.skipping => Ok(0),
            other => other,
        }
    }

    fn peek_binop(&self) -> Option<(BinOp, usize)> {
        let a = self.lexer.peek()?;
        let b = self.lexer.peek_at(1);
        Some(match (a, b) {
            (b'|', Some(b'|')) => (BinOp::LogOr, 2),
            (b'&', Some(b'&')) => (BinOp::LogAnd, 2),
            (b'<', Some(b'<')) => (BinOp::Shl, 2),
            (b'>', Some(b'>')) => (BinOp::Shr, 2),
            (b'<', Some(b'=')) => (BinOp::Le, 2),
            (b'>', Some(b'=')) => (BinOp::Ge, 2),
            (b'=', Some(b'=')) => (BinOp::Eq, 2),
            (b'!', Some(b'=')) => (BinOp::Ne, 2),
            (b'|', _) => (BinOp::BitOr, 1),
            (b'^', _) => (BinOp::BitXor, 1),
            (b'&', _) => (BinOp::BitAnd, 1),
            (b'<', _) => (BinOp::Lt, 1),
            (b'>', _) => (BinOp::Gt, 1),
            (b'+', _) => (BinOp::Add, 1),
            (b'-', _) => (BinOp::Sub, 1),
            (b'*', _) => (BinOp::Mul, 1),
            (b'/', _) => (BinOp::Div, 1),
            (b'%', _) => (BinOp::Rem, 1),
            _ => return None,
        })
    }

    /// An assignment, or a unary expression. Assignment binds loosest and is
    /// right-associative, hence here rather than in the operator loop.
    fn assignment_or_unary(&mut self) -> Result<i64, ArithError> {
        self.lexer.skip_blanks();
        let save = self.lexer.pos;
        if let Some(name) = self.lexer.name() {
            self.lexer.skip_blanks();
            let compound = self.peek_binop().and_then(|(op, len)| {
                if self.lexer.peek_at(len) == Some(b'=') && !matches!(op, BinOp::Eq | BinOp::Ne) {
                    Some((op, len + 1))
                } else {
                    None
                }
            });
            if let Some((op, len)) = compound {
                self.lexer.pos += len;
                let right = self.expr(0)?;
                let current = self.vars.get(&name);
                let value = self.apply(op, current, right)?;
                self.vars.set(&name, value);
                return Ok(value);
            }
            if self.lexer.peek() == Some(b'=') && self.lexer.peek_at(1) != Some(b'=') {
                self.lexer.pos += 1;
                let value = self.expr(0)?;
                self.vars.set(&name, value);
                return Ok(value);
            }
            self.lexer.pos = save;
        }
        self.unary()
    }

    fn unary(&mut self) -> Result<i64, ArithError> {
        self.lexer.skip_blanks();
        match self.lexer.peek() {
            Some(b'-') => {
                self.lexer.pos += 1;
                Ok(self.unary()?.wrapping_neg())
            }
            Some(b'+') => {
                self.lexer.pos += 1;
                self.unary()
            }
            Some(b'!') if self.lexer.peek_at(1) != Some(b'=') => {
                self.lexer.pos += 1;
                Ok(i64::from(self.unary()? == 0))
            }
            Some(b'~') => {
                self.lexer.pos += 1;
                Ok(!self.unary()?)
            }
            _ => self.primary(),
        }
    }

    fn primary(&mut self) -> Result<i64, ArithError> {
        self.lexer.skip_blanks();
        match self.lexer.peek() {
            Some(b'(') => {
                self.lexer.pos += 1;
                let value = self.expr(0)?;
                self.lexer.skip_blanks();
                if self.lexer.peek() != Some(b')') {
                    return Err(ArithError::Syntax);
                }
                self.lexer.pos += 1;
                Ok(value)
            }
            // `$name` inside `$(( ))` is redundant but legal and common.
            Some(b'$') => {
                self.lexer.pos += 1;
                self.primary()
            }
            Some(b) if b.is_ascii_digit() => self.lexer.number().ok_or(ArithError::Syntax),
            _ => match self.lexer.name() {
                Some(name) => Ok(self.vars.get(&name)),
                None => Err(ArithError::Syntax),
            },
        }
    }
}

fn apply(op: BinOp, left: i64, right: i64) -> Result<i64, ArithError> {
    Ok(match op {
        BinOp::LogOr => i64::from(left != 0 || right != 0),
        BinOp::LogAnd => i64::from(left != 0 && right != 0),
        BinOp::BitOr => left | right,
        BinOp::BitXor => left ^ right,
        BinOp::BitAnd => left & right,
        BinOp::Eq => i64::from(left == right),
        BinOp::Ne => i64::from(left != right),
        BinOp::Lt => i64::from(left < right),
        BinOp::Le => i64::from(left <= right),
        BinOp::Gt => i64::from(left > right),
        BinOp::Ge => i64::from(left >= right),
        // A shift count outside 0..64 is undefined in C; masking is what the
        // hardware does and therefore what other shells report.
        BinOp::Shl => left.wrapping_shl(right as u32),
        BinOp::Shr => left.wrapping_shr(right as u32),
        BinOp::Add => left.wrapping_add(right),
        BinOp::Sub => left.wrapping_sub(right),
        BinOp::Mul => left.wrapping_mul(right),
        BinOp::Div => {
            if right == 0 {
                return Err(ArithError::DivideByZero);
            }
            left.wrapping_div(right)
        }
        BinOp::Rem => {
            if right == 0 {
                return Err(ArithError::DivideByZero);
            }
            left.wrapping_rem(right)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    struct Table(Vec<(Vec<u8>, i64)>);

    impl Vars for Table {
        fn get(&mut self, name: &[u8]) -> i64 {
            self.0
                .iter()
                .find(|(k, _)| k == name)
                .map_or(0, |(_, v)| *v)
        }
        fn set(&mut self, name: &[u8], value: i64) {
            match self.0.iter_mut().find(|(k, _)| k == name) {
                Some(slot) => slot.1 = value,
                None => self.0.push((name.to_vec(), value)),
            }
        }
    }

    fn ev(expr: &[u8]) -> i64 {
        eval(expr, &mut NoVars).unwrap()
    }

    #[test]
    fn precedence_and_grouping() {
        assert_eq!(ev(b"1+2*3"), 7);
        assert_eq!(ev(b"(1+2)*3"), 9);
        assert_eq!(ev(b"7/2"), 3);
        assert_eq!(ev(b"-7%3"), -1);
        assert_eq!(ev(b"1<<4"), 16);
        assert_eq!(ev(b"1|6&2"), 3);
        assert_eq!(ev(b"2<3 == 1"), 1);
    }

    #[test]
    fn unary_and_ternary() {
        assert_eq!(ev(b"-5"), -5);
        assert_eq!(ev(b"!0"), 1);
        assert_eq!(ev(b"~0"), -1);
        assert_eq!(ev(b"1 ? 10 : 20"), 10);
        assert_eq!(ev(b"0 ? 10 : 20"), 20);
    }

    #[test]
    fn radix_prefixes() {
        assert_eq!(ev(b"0x1f"), 31);
        assert_eq!(ev(b"010"), 8);
        assert_eq!(ev(b"0"), 0);
    }

    #[test]
    fn variables_read_and_assign() {
        let mut vars = Table(vec![(b"i".to_vec(), 4)]);
        assert_eq!(eval(b"i+1", &mut vars), Ok(5));
        assert_eq!(eval(b"i=i+1", &mut vars), Ok(5));
        assert_eq!(vars.get(b"i"), 5);
        assert_eq!(eval(b"i+=3", &mut vars), Ok(8));
        assert_eq!(vars.get(b"i"), 8);
        assert_eq!(eval(b"unset_name", &mut vars), Ok(0));
    }

    #[test]
    fn short_circuit_skips_its_operand() {
        let mut vars = Table(vec![]);
        assert_eq!(eval(b"0 && (x=9)", &mut vars), Ok(0));
        assert_eq!(vars.get(b"x"), 0);
        assert_eq!(eval(b"1 || (y=9)", &mut vars), Ok(1));
        assert_eq!(vars.get(b"y"), 0);
        // A skipped divide-by-zero is not an error, and the skip must land on
        // the operand's real end: stopping at the error left the cursor
        // inside it and the tail was read as the left side's continuation.
        assert_eq!(eval(b"0 && 1/0", &mut vars), Ok(0));
        assert_eq!(eval(b"0 && 1/0 + 2", &mut vars), Ok(0));
        assert_eq!(eval(b"1 || 1/0 + 2", &mut vars), Ok(1));
        assert_eq!(eval(b"0 && 2/0 * 5 + 7", &mut vars), Ok(0));
        assert_eq!(eval(b"0 && (1/0) + 2", &mut vars), Ok(0));
    }

    #[test]
    fn divide_by_zero_and_junk_are_errors() {
        assert_eq!(eval(b"1/0", &mut NoVars), Err(ArithError::DivideByZero));
        assert_eq!(eval(b"1 2", &mut NoVars), Err(ArithError::Syntax));
        assert_eq!(eval(b"", &mut NoVars), Err(ArithError::Syntax));
        assert_eq!(eval(b"(1", &mut NoVars), Err(ArithError::Syntax));
    }

    #[test]
    fn overflow_wraps_rather_than_panicking() {
        assert_eq!(eval(b"9223372036854775807 + 1", &mut NoVars), Ok(i64::MIN));
    }

    #[test]
    fn value_of_reads_what_a_variable_holds() {
        assert_eq!(value_of(b"42"), 42);
        assert_eq!(value_of(b" -7 "), -7);
        assert_eq!(value_of(b"0x10"), 16);
        assert_eq!(value_of(b"abc"), 0);
        assert_eq!(value_of(b""), 0);
    }
}
