//! Shell functions and the shell options `set` toggles.
//!
//! A body is an `Arc<Command>` rather than a clone, so a recursive call copies
//! no tree and a redefinition during a call cannot pull the body out from
//! under the frame running it.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use slopos_shell_core::ast::Command;

static FUNCS: Mutex<Vec<(Vec<u8>, Arc<Command>)>> = Mutex::new(Vec::new());

pub fn define(name: &[u8], body: Arc<Command>) {
    let mut table = FUNCS.lock().unwrap();
    match table.iter_mut().find(|(n, _)| n == name) {
        Some(slot) => slot.1 = body,
        None => table.push((name.to_vec(), body)),
    }
}

pub fn lookup(name: &[u8]) -> Option<Arc<Command>> {
    FUNCS
        .lock()
        .unwrap()
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, body)| Arc::clone(body))
}

pub fn undefine(name: &[u8]) -> bool {
    let mut table = FUNCS.lock().unwrap();
    match table.iter().position(|(n, _)| n == name) {
        Some(index) => {
            table.remove(index);
            true
        }
        None => false,
    }
}

pub fn names() -> Vec<Vec<u8>> {
    FUNCS
        .lock()
        .unwrap()
        .iter()
        .map(|(n, _)| n.clone())
        .collect()
}

/// `set -e` — a command that fails ends the shell.
static ERREXIT: AtomicBool = AtomicBool::new(false);
/// `set -u` — expanding an unset variable is an error.
static NOUNSET: AtomicBool = AtomicBool::new(false);
/// `set -x` — trace each command to stderr before running it.
static XTRACE: AtomicBool = AtomicBool::new(false);
/// `set -f` — no pathname expansion.
static NOGLOB: AtomicBool = AtomicBool::new(false);

macro_rules! flag {
    ($get:ident, $set:ident, $cell:ident) => {
        pub fn $get() -> bool {
            $cell.load(Ordering::Relaxed)
        }
        pub fn $set(on: bool) {
            $cell.store(on, Ordering::Relaxed);
        }
    };
}

flag!(errexit, set_errexit, ERREXIT);
flag!(nounset, set_nounset, NOUNSET);
flag!(xtrace, set_xtrace, XTRACE);
flag!(noglob, set_noglob, NOGLOB);

/// `$-` — the option letters in force.
pub fn option_letters() -> Vec<u8> {
    let mut out = Vec::new();
    for (on, letter) in [
        (errexit(), b'e'),
        (nounset(), b'u'),
        (xtrace(), b'x'),
        (noglob(), b'f'),
    ] {
        if on {
            out.push(letter);
        }
    }
    if super::is_interactive() {
        out.push(b'i');
    }
    out
}
