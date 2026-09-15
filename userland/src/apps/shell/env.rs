//! Shell and environment variables.
//!
//! Heap-backed and unbounded: the fixed 64-entry table of 256-byte values
//! this replaced silently truncated a `CFLAGS` or a long `PATH`.
//!
//! A variable is a *shell* variable until exported, as POSIX has it — a bare
//! `FOO=bar` leaking into every child is how a stray assignment changes what a
//! configure script decides.

use std::sync::Mutex;

struct Var {
    name: Vec<u8>,
    value: Vec<u8>,
    /// Passed to a child's `envp`.
    exported: bool,
}

static VARS: Mutex<Vec<Var>> = Mutex::new(Vec::new());

fn with_vars<R, F: FnOnce(&mut Vec<Var>) -> R>(f: F) -> R {
    f(&mut VARS.lock().unwrap())
}

pub fn get(name: &[u8]) -> Option<Vec<u8>> {
    with_vars(|vars| {
        vars.iter()
            .find(|v| v.name == name)
            .map(|v| v.value.clone())
    })
}

pub fn is_set(name: &[u8]) -> bool {
    with_vars(|vars| vars.iter().any(|v| v.name == name))
}

/// Copy a value into a caller-owned buffer, returning its length.
pub fn get_into(name: &[u8], dst: &mut [u8]) -> Option<usize> {
    with_vars(|vars| {
        vars.iter().find(|v| v.name == name).map(|v| {
            let len = v.value.len().min(dst.len());
            dst[..len].copy_from_slice(&v.value[..len]);
            len
        })
    })
}

/// Assign a value, leaving whether the variable is exported alone.
pub fn set(name: &[u8], value: &[u8]) {
    assign(name, value, None);
}

/// Assign and export — `export NAME=value` and a `NAME=value cmd` prefix.
pub fn set_exported(name: &[u8], value: &[u8]) {
    assign(name, value, Some(true));
}

fn assign(name: &[u8], value: &[u8], export: Option<bool>) {
    if name.is_empty() {
        return;
    }
    with_vars(|vars| match vars.iter_mut().find(|v| v.name == name) {
        Some(var) => {
            var.value.clear();
            var.value.extend_from_slice(value);
            if let Some(export) = export {
                var.exported |= export;
            }
        }
        None => vars.push(Var {
            name: name.to_vec(),
            value: value.to_vec(),
            exported: export.unwrap_or(false),
        }),
    });
}

/// `export NAME` with no value: mark it exported, creating it empty if
/// absent, as POSIX requires.
pub fn export(name: &[u8]) {
    if name.is_empty() {
        return;
    }
    with_vars(|vars| match vars.iter_mut().find(|v| v.name == name) {
        Some(var) => var.exported = true,
        None => vars.push(Var {
            name: name.to_vec(),
            value: Vec::new(),
            exported: true,
        }),
    });
}

pub fn unset(name: &[u8]) -> bool {
    with_vars(|vars| match vars.iter().position(|v| v.name == name) {
        Some(index) => {
            vars.remove(index);
            true
        }
        None => false,
    })
}

pub fn initialize_defaults() {
    for (name, value) in [
        (b"PATH".as_slice(), b"/bin:/sbin".as_slice()),
        (b"SHELL", b"/bin/shell"),
        (b"HOME", b"/"),
        (b"USER", b"root"),
        (b"TERM", b"slopos"),
    ] {
        set_exported(name, value);
    }
    // Not exported: an inherited `IFS` changes how a child shell splits every
    // word it expands.
    set(b"IFS", b" \t\n");
    set(b"PS1", b"\\u@\\h:\\w\\$ ");
    set(b"PS2", b"> ");
}

/// Every variable, exported or not — what `set` with no arguments lists.
pub fn for_each<F: FnMut(&[u8], &[u8])>(mut f: F) {
    with_vars(|vars| {
        for var in vars.iter() {
            f(&var.name, &var.value);
        }
    });
}

/// The child's environment: exported variables only.
pub fn for_each_exported<F: FnMut(&[u8], &[u8])>(mut f: F) {
    with_vars(|vars| {
        for var in vars.iter().filter(|v| v.exported) {
            f(&var.name, &var.value);
        }
    });
}

pub fn is_exported(name: &[u8]) -> bool {
    with_vars(|vars| {
        vars.iter()
            .find(|v| v.name == name)
            .is_some_and(|v| v.exported)
    })
}

pub fn count() -> usize {
    with_vars(|vars| vars.len())
}
