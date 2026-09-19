//! The standing proof that a `PT_INTERP` program runs and `dlopen` works.
//!
//! The work is in `/bin/dl_probe`, which is the tree's only dynamically
//! linked program: it reaches the C library exclusively through `libc.so`,
//! the way a cross-built C or C++ program will. This binary is static, so it
//! can report through the harness, and it grades the probe's exit status.

use std::os::unix::process::ExitStatusExt;
use std::process::Command;

const SIGSEGV: i32 = slopos_abi::signal::SIGSEGV as i32;

const PROBE: &str = "/bin/dl_probe";
const INTERP: &str = "/lib/ld-slopos.so.1";
const LIBC: &str = "/lib/libc.so";
const LIBDL: &str = "/lib/libdltest.so";

/// The interpreter is one artifact under two names, which is what keeps a
/// process to one allocator however many objects it loads.
fn the_interpreter_is_the_c_library() -> bool {
    let libc = match std::fs::metadata(LIBC) {
        Ok(m) => m.len(),
        Err(e) => {
            eprintln!("dl_test: {LIBC}: {e}");
            return false;
        }
    };
    let interp = match std::fs::metadata(INTERP) {
        Ok(m) => m.len(),
        Err(e) => {
            eprintln!("dl_test: {INTERP}: {e}");
            return false;
        }
    };
    if libc != interp {
        eprintln!("dl_test: {INTERP} is {interp} bytes, {LIBC} is {libc}");
        return false;
    }
    match std::fs::read_link(INTERP) {
        Ok(target) => target.as_os_str() == "libc.so",
        Err(e) => {
            eprintln!("dl_test: {INTERP} is not a symlink: {e}");
            false
        }
    }
}

/// The whole probe: `PT_INTERP`, `AT_BASE`, the interpreter's own
/// relocations, the executable's `DT_NEEDED`, the static TLS block, then
/// every `dlfcn.h` call against a freshly loaded object. A non-zero status is
/// the number of the check that failed.
fn a_dynamic_program_loads_and_uses_a_shared_object() -> bool {
    if std::fs::metadata(LIBDL).is_err() {
        eprintln!("dl_test: {LIBDL} is not installed");
        return false;
    }
    match Command::new(PROBE).status() {
        Ok(status) => match status.code() {
            Some(0) => true,
            Some(code) => {
                eprintln!("dl_test: {PROBE} failed check {code}");
                false
            }
            None => {
                eprintln!("dl_test: {PROBE} died by a signal");
                false
            }
        },
        Err(e) => {
            eprintln!("dl_test: spawning {PROBE} failed: {e}");
            false
        }
    }
}

/// `PT_GNU_RELRO` is sealed, so a write into the loaded object's relocated
/// GOT faults instead of landing. Eager binding is what makes the whole
/// segment sealable: nothing writes a slot after relocation.
fn relro_is_sealed_after_relocation() -> bool {
    match Command::new(PROBE).arg("relro").status() {
        Ok(status) => {
            if let Some(code) = status.code() {
                eprintln!("dl_test: writing into RELRO exited {code} instead of faulting");
                return false;
            }
            // Which signal matters: a probe that failed to start also dies
            // without an exit code, and would otherwise pass this case.
            if status.signal() != Some(SIGSEGV) {
                eprintln!(
                    "dl_test: RELRO write died by {:?}, wanted SIGSEGV",
                    status.signal()
                );
                return false;
            }
            true
        }
        Err(e) => {
            eprintln!("dl_test: spawning {PROBE} relro failed: {e}");
            false
        }
    }
}

/// The interpreter runs before the program does, so replacing it is
/// replacing every dynamic program at once. `spawn_privilege_test` asserts
/// `/lib` refuses a rename and a create; this asserts the file itself
/// refuses a writer.
fn the_interpreter_is_sealed() -> bool {
    if std::fs::OpenOptions::new().write(true).open(LIBC).is_ok() {
        eprintln!("dl_test: {LIBC} is writable");
        return false;
    }
    true
}

const CASES: &[(&str, fn() -> bool)] = &[
    (
        "the_interpreter_is_the_c_library",
        the_interpreter_is_the_c_library,
    ),
    ("the_interpreter_is_sealed", the_interpreter_is_sealed),
    (
        "a_dynamic_program_loads_and_uses_a_shared_object",
        a_dynamic_program_loads_and_uses_a_shared_object,
    ),
    (
        "relro_is_sealed_after_relocation",
        relro_is_sealed_after_relocation,
    ),
];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
