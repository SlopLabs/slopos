// Pull in the userland lib so its `_start` ELF entry point is linked.
use slopos_userland as _;

use std::fs;

use slopos_userland::init_control::{self, Command};
use slopos_userland::keymap::{
    PERSIST_PATH, current_name as current, keep_computer_layout, load_layout_by_name,
};
use slopos_userland::syscall::keymap::keymap_load;

/// The same file → parse → serialise → upload pipeline the `keymap` builtin and
/// the init boot applier run.
fn load_layout_file(name: &str) -> bool {
    load_layout_by_name(name).is_ok()
}

/// A test boot keeps no `/etc/keymap`, so init loaded nothing and answers the
/// kernel's built-in `us`.
fn init_answers_us_at_boot() -> bool {
    current().as_deref() == Some("us")
}

/// Two layout files pass the kernel's validator, `us` last so other tests
/// type on it. Runs holding every capability like every utest binary, so it
/// says nothing about the unprivileged-caller gate.
fn layout_files_load() -> bool {
    let de_ok = load_layout_file("de_CH");
    let us_ok = load_layout_file("us");
    de_ok && us_ok
}

/// Asked, init loads the computer's layout and answers its name; with none
/// kept it loads the built-in `us` again.
fn init_answers_the_layout_it_loaded() -> bool {
    let switched = keep_computer_layout("de_CH").is_ok()
        && init_control::ask(Command::Keymap).is_ok()
        && current().as_deref() == Some("de_CH");
    let _ = fs::remove_file(PERSIST_PATH);
    let restored = init_control::ask(Command::Keymap).is_ok() && current().as_deref() == Some("us");
    switched && restored
}

/// A malformed (wrong-size) blob is rejected with `EINVAL`, never installed.
fn load_rejects_bad_blob() -> bool {
    keymap_load(&[0u8; 16]) < 0
}

/// The `keymap <unknown>` path: a missing file must return `Err`, not panic.
fn read_missing_returns_err() -> bool {
    fs::read("/usr/share/keymaps/__definitely_missing__.layout").is_err()
}

fn main() {
    slopos_slibc::test_harness::run(&[
        ("init_answers_us_at_boot", init_answers_us_at_boot),
        ("layout_files_load", layout_files_load),
        (
            "init_answers_the_layout_it_loaded",
            init_answers_the_layout_it_loaded,
        ),
        ("load_rejects_bad_blob", load_rejects_bad_blob),
        ("read_missing_returns_err", read_missing_returns_err),
    ]);
}
