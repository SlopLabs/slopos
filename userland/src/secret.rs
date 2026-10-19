//! What a person types that nobody else should see: a line read from
//! standard input with the terminal's echo off, wiped once dropped.

use std::io::{BufRead, Write};

use slopos_abi::syscall::termios::LocalFlags;

use crate::syscall::{fs, process};

pub struct Secret(String);

impl Secret {
    pub fn bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl PartialEq for Secret {
    fn eq(&self, other: &Secret) -> bool {
        slopos_tls_core::ct::eq(self.bytes(), other.bytes())
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // SAFETY: zero bytes are UTF-8.
        slopos_tls_core::ct::wipe(unsafe { self.0.as_bytes_mut() });
    }
}

/// Standard input's terminal with its echo off, as it was again once dropped.
pub struct Quiet(slopos_abi::syscall::UserTermios);

/// Turn standard input's echo off while the result lives: `None` when it is
/// no terminal, refused when it is one that would show what is typed.
pub fn quiet() -> Result<Option<Quiet>, String> {
    if !fs::isatty(0) {
        return Ok(None);
    }
    const SHOWN: &str = "the terminal would show what is typed";
    let saved = fs::tcgetattr(0).map_err(|_| SHOWN)?;
    let mut quiet = saved;
    quiet.c_lflag.remove(LocalFlags::ECHO);
    fs::tcsetattr(0, &quiet).map_err(|_| SHOWN)?;
    Ok(Some(Quiet(saved)))
}

impl Drop for Quiet {
    fn drop(&mut self) {
        let _ = fs::tcsetattr(0, &self.0);
    }
}

/// A line typed after `prompt`, which goes to standard error; `None` at the
/// end of input.
pub fn read(prompt: &str) -> Result<Option<Secret>, String> {
    let quiet = quiet()?;
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let mut line = Secret(String::new());
    let read = std::io::stdin().lock().read_line(&mut line.0);
    if quiet.is_some() {
        eprintln!();
    }
    match read {
        Ok(0) => Ok(None),
        Ok(_) => {
            if line.0.ends_with('\n') {
                line.0.pop();
            }
            Ok(Some(line))
        }
        Err(e) => Err(format!("reading what was typed: {e}")),
    }
}

/// A new password, typed twice to the same; `None` at the end of input.
pub fn read_new(whose: &str) -> Result<Option<Secret>, String> {
    let _quiet = quiet()?;
    let Some(first) = read(&format!("New password for {whose}: "))? else {
        return Ok(None);
    };
    if first.is_empty() {
        return Err("a password is not empty".into());
    }
    let Some(again) = read("Again: ")? else {
        return Ok(None);
    };
    if again != first {
        return Err("the two passwords differ".into());
    }
    Ok(Some(first))
}

/// Make standard input this process's controlling terminal when it is a
/// terminal of nobody's, as the one privd gives a mechanism of its own is,
/// so an interrupt typed there reaches it.
pub fn claim_terminal() {
    if fs::isatty(0) && fs::tcgetsid(0).is_err() {
        let _ = process::setsid();
        let _ = fs::tiocsctty(0);
    }
}
