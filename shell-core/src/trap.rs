//! The operand grammar of the `trap` special builtin (POSIX XCU 2.14 `trap`)
//! and the signal names it shares with `kill`.
//!
//! Only the reading and writing of operands lives here; installing handlers
//! and running actions is the userland shell's.

use alloc::vec::Vec;

use slopos_abi::signal::{
    NSIG, SIGABRT, SIGALRM, SIGBUS, SIGCHLD, SIGCONT, SIGFPE, SIGHUP, SIGILL, SIGINT, SIGKILL,
    SIGPIPE, SIGQUIT, SIGSEGV, SIGSTOP, SIGTERM, SIGTRAP, SIGTSTP, SIGTTIN, SIGTTOU, SIGUSR1,
    SIGUSR2, SIGWINCH,
};

/// Signal names without the `SIG` prefix, upper case as POSIX spells them.
pub const SIGNAL_NAMES: &[(&str, u8)] = &[
    ("HUP", SIGHUP),
    ("INT", SIGINT),
    ("QUIT", SIGQUIT),
    ("ILL", SIGILL),
    ("TRAP", SIGTRAP),
    ("ABRT", SIGABRT),
    ("BUS", SIGBUS),
    ("FPE", SIGFPE),
    ("KILL", SIGKILL),
    ("USR1", SIGUSR1),
    ("SEGV", SIGSEGV),
    ("USR2", SIGUSR2),
    ("PIPE", SIGPIPE),
    ("ALRM", SIGALRM),
    ("TERM", SIGTERM),
    ("CHLD", SIGCHLD),
    ("CONT", SIGCONT),
    ("STOP", SIGSTOP),
    ("TSTP", SIGTSTP),
    ("TTIN", SIGTTIN),
    ("TTOU", SIGTTOU),
    ("WINCH", SIGWINCH),
];

/// The highest signal number the ABI defines; signals are `1..=MAX_SIGNAL`.
pub const MAX_SIGNAL: u8 = NSIG as u8;

const _: () = assert!(NSIG <= u8::MAX as usize);

/// A signal by name, with or without the `SIG` prefix. Case-sensitive.
pub fn signal_by_name(name: &[u8]) -> Option<u8> {
    let bare = name.strip_prefix(b"SIG").unwrap_or(name);
    SIGNAL_NAMES
        .iter()
        .find(|(known, _)| known.as_bytes() == bare)
        .map(|&(_, num)| num)
}

/// The `SIG`-less name of `signum`, if it has one.
pub fn signal_name(signum: u8) -> Option<&'static str> {
    SIGNAL_NAMES
        .iter()
        .find(|&&(_, num)| num == signum)
        .map(|&(name, _)| name)
}

/// What a `trap` condition operand designates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Condition {
    Exit,
    Signal(u8),
}

/// `EXIT`, `0`, a signal name with or without `SIG`, or a signal number in
/// `1..=MAX_SIGNAL`.
pub fn parse_condition(text: &[u8]) -> Option<Condition> {
    if text == b"EXIT" {
        return Some(Condition::Exit);
    }
    if is_unsigned_integer(text) {
        let mut value = 0u32;
        for &b in text {
            value = value.checked_mul(10)?.checked_add(u32::from(b - b'0'))?;
        }
        return match value {
            0 => Some(Condition::Exit),
            n if n <= u32::from(MAX_SIGNAL) => Some(Condition::Signal(n as u8)),
            _ => None,
        };
    }
    signal_by_name(text).map(Condition::Signal)
}

/// POSIX: when the first operand is an unsigned decimal integer, every
/// operand is a condition and each is reset.
pub fn is_unsigned_integer(text: &[u8]) -> bool {
    !text.is_empty() && text.iter().all(u8::is_ascii_digit)
}

/// What a `trap` action operand asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action<'a> {
    Default,
    Ignore,
    Command(&'a [u8]),
}

pub fn classify_action(text: &[u8]) -> Action<'_> {
    match text {
        b"-" => Action::Default,
        b"" => Action::Ignore,
        command => Action::Command(command),
    }
}

/// Append `value` single-quoted so the shell reads it back unchanged: each
/// `'` becomes `'\''`.
pub fn push_single_quoted(out: &mut Vec<u8>, value: &[u8]) {
    out.push(b'\'');
    for &b in value {
        if b == b'\'' {
            out.extend_from_slice(b"'\\''");
        } else {
            out.push(b);
        }
    }
    out.push(b'\'');
}

/// One line of `trap`'s listing, `trap -- 'action' NAME\n`, in a form the
/// shell can read back as a command. An ignored condition's action is `''`.
pub fn listing_line(out: &mut Vec<u8>, action: &[u8], condition: Condition) {
    out.extend_from_slice(b"trap -- ");
    push_single_quoted(out, action);
    out.push(b' ');
    match condition {
        Condition::Exit => out.extend_from_slice(b"EXIT"),
        Condition::Signal(signum) => match signal_name(signum) {
            Some(name) => out.extend_from_slice(name.as_bytes()),
            None => push_decimal(out, signum),
        },
    }
    out.push(b'\n');
}

fn push_decimal(out: &mut Vec<u8>, value: u8) {
    if value >= 100 {
        out.push(b'0' + value / 100);
    }
    if value >= 10 {
        out.push(b'0' + value / 10 % 10);
    }
    out.push(b'0' + value % 10);
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn exit_by_name_and_by_zero() {
        assert_eq!(parse_condition(b"EXIT"), Some(Condition::Exit));
        assert_eq!(parse_condition(b"0"), Some(Condition::Exit));
        assert_eq!(parse_condition(b"00"), Some(Condition::Exit));
    }

    #[test]
    fn signal_names_with_and_without_the_prefix() {
        assert_eq!(parse_condition(b"INT"), Some(Condition::Signal(SIGINT)));
        assert_eq!(parse_condition(b"SIGINT"), Some(Condition::Signal(SIGINT)));
        assert_eq!(parse_condition(b"USR1"), Some(Condition::Signal(SIGUSR1)));
        assert_eq!(parse_condition(b"SIGEXIT"), None);
        assert_eq!(parse_condition(b"SIG"), None);
    }

    #[test]
    fn names_are_upper_case() {
        assert_eq!(parse_condition(b"int"), None);
        assert_eq!(parse_condition(b"exit"), None);
    }

    #[test]
    fn signal_numbers() {
        assert_eq!(parse_condition(b"2"), Some(Condition::Signal(SIGINT)));
        assert_eq!(parse_condition(b"15"), Some(Condition::Signal(SIGTERM)));
    }

    #[test]
    fn the_numeric_bound_is_the_abi_signal_count() {
        let last = alloc::format!("{NSIG}");
        let past = alloc::format!("{}", NSIG + 1);
        assert_eq!(
            parse_condition(last.as_bytes()),
            Some(Condition::Signal(NSIG as u8))
        );
        assert_eq!(parse_condition(past.as_bytes()), None);
        assert_eq!(parse_condition(b"99999999999999999999"), None);
    }

    #[test]
    fn unknown_or_malformed_conditions() {
        assert_eq!(parse_condition(b"NOPE"), None);
        assert_eq!(parse_condition(b""), None);
        assert_eq!(parse_condition(b"-2"), None);
        assert_eq!(parse_condition(b"2x"), None);
    }

    #[test]
    fn a_leading_integer_selects_the_reset_form() {
        assert!(is_unsigned_integer(b"0"));
        assert!(is_unsigned_integer(b"15"));
        assert!(!is_unsigned_integer(b""));
        assert!(!is_unsigned_integer(b"-"));
        assert!(!is_unsigned_integer(b"echo 1"));
    }

    #[test]
    fn actions() {
        assert_eq!(classify_action(b"-"), Action::Default);
        assert_eq!(classify_action(b""), Action::Ignore);
        assert_eq!(classify_action(b"--"), Action::Command(b"--"));
        assert_eq!(classify_action(b"echo hi"), Action::Command(b"echo hi"));
    }

    #[test]
    fn listing_quotes_so_the_line_reads_back() {
        let mut out = vec![];
        listing_line(&mut out, b"echo it's done", Condition::Exit);
        assert_eq!(out, b"trap -- 'echo it'\\''s done' EXIT\n");

        out.clear();
        listing_line(&mut out, b"", Condition::Signal(SIGINT));
        assert_eq!(out, b"trap -- '' INT\n");

        out.clear();
        listing_line(&mut out, b"a'b'", Condition::Signal(SIGUSR1));
        assert_eq!(out, b"trap -- 'a'\\''b'\\''' USR1\n");
    }

    #[test]
    fn an_unnamed_signal_lists_by_number() {
        let Some(unnamed) = (1..=MAX_SIGNAL).find(|&n| signal_name(n).is_none()) else {
            return;
        };
        let mut out = vec![];
        listing_line(&mut out, b"x", Condition::Signal(unnamed));
        let want = alloc::format!("trap -- 'x' {unnamed}\n");
        assert_eq!(out, want.as_bytes());
    }
}
