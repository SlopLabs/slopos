//! The standing proof that a `PT_INTERP` program runs and `dlopen` works.
//!
//! The work is in `/bin/dl_probe` and the `dl_search_*` probes, which are
//! dynamically linked: they reach the C library exclusively through
//! `libc.so`, the way a cross-built C or C++ program will. This binary is
//! static, so it can report through the harness, and it grades the probes'
//! exit statuses.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::process::Command;
use std::sync::LazyLock;

const SIGSEGV: i32 = slopos_abi::signal::SIGSEGV as i32;

const PROBE: &str = "/bin/dl_probe";
const INTERP: &str = "/lib/ld-slopos.so.1";
const LIBC: &str = "/lib/libc.so";
const LIBDL: &str = "/lib/libdltest.so";

const FIXTURE: &str = "/lib/libdlsearch-fixture.so";
/// Where the search cases lay their copies out; `$ORIGIN` of a copy is
/// somewhere in here.
const TREE: &str = "/tmp/dl_search";
const SECURE_PROBE: &str = "/bin/dl_secure_probe";
const SECURE_ORIGIN_DIR: &str = "/tmp/dl_secure";
const SECURE_ABS_DIR: &str = "/tmp/dl_secure_abs";

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
    (
        "origin_runpath_resolves_a_needed_library",
        origin_runpath_resolves_a_needed_library,
    ),
    (
        "origin_expands_in_rpath_and_runpath",
        origin_expands_in_rpath_and_runpath,
    ),
    (
        "library_path_precedes_rpath_and_runpath",
        library_path_precedes_rpath_and_runpath,
    ),
    (
        "runpath_suppresses_the_loaders_rpath",
        runpath_suppresses_the_loaders_rpath,
    ),
    (
        "secure_exec_ignores_library_path_and_origin",
        secure_exec_ignores_library_path_and_origin,
    ),
    (
        "execfn_names_the_resolved_executable",
        execfn_names_the_resolved_executable,
    ),
    (
        "ld_debug_statistics_reports_startup_binding",
        ld_debug_statistics_reports_startup_binding,
    ),
];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}

fn stage(src: &str, dst: &str) -> bool {
    let parent = std::path::Path::new(dst).parent().expect("absolute path");
    let staged = fs::create_dir_all(parent)
        .and_then(|()| fs::copy(src, dst))
        .and_then(|_| fs::set_permissions(dst, fs::Permissions::from_mode(0o755)));
    if let Err(e) = staged {
        eprintln!("dl_test: staging {src} at {dst}: {e}");
        return false;
    }
    true
}

/// The layout every search case reads, built once: a probe copy per search
/// entry kind, each with a `libdlsearch.so` only where that entry points.
static SEARCH_TREE: LazyLock<bool> = LazyLock::new(|| {
    for dir in [TREE, SECURE_ORIGIN_DIR, SECURE_ABS_DIR] {
        let _ = fs::remove_dir_all(dir);
    }
    let lib = |dir: &str| format!("{dir}/libdlsearch.so");
    let copies = [
        (
            "/bin/dl_search_origin",
            format!("{TREE}/bin/dl_search_origin"),
        ),
        (FIXTURE, lib(&format!("{TREE}/lib"))),
        (
            "/bin/dl_search_rpath",
            format!("{TREE}/bin/dl_search_rpath"),
        ),
        (FIXTURE, lib(&format!("{TREE}/bin/rpath"))),
        (
            "/bin/dl_search_runpath",
            format!("{TREE}/bin/dl_search_runpath"),
        ),
        (FIXTURE, lib(&format!("{TREE}/bin/runpath"))),
        (FIXTURE, lib(&format!("{TREE}/llp"))),
        (
            "/lib/libdlrunpath.so",
            format!("{TREE}/dep/libdlrunpath.so"),
        ),
        ("/lib/libdlplain.so", format!("{TREE}/dep/libdlplain.so")),
        (FIXTURE, lib(SECURE_ORIGIN_DIR)),
        (FIXTURE, lib(SECURE_ABS_DIR)),
    ];
    if !copies.iter().all(|(src, dst)| stage(src, dst)) {
        return false;
    }
    if let Err(e) = std::os::unix::fs::symlink("/bin/dl_search_rpath", format!("{TREE}/link")) {
        eprintln!("dl_test: symlink {TREE}/link: {e}");
        return false;
    }
    true
});

fn search_tree() -> bool {
    *SEARCH_TREE
}

/// Run a search probe and answer its exit code; `None` if it did not exit.
fn run_search(program: &str, args: &[&str], library_path: Option<&str>) -> Option<i32> {
    let mut cmd = Command::new(program);
    cmd.args(args).env_remove("LD_LIBRARY_PATH");
    if let Some(dirs) = library_path {
        cmd.env("LD_LIBRARY_PATH", dirs);
    }
    match cmd.status() {
        Ok(status) => status.code(),
        Err(e) => {
            eprintln!("dl_test: spawning {program}: {e}");
            None
        }
    }
}

/// The probe loaded `name` from `expect`, or with `expect == "-"` found none.
fn loads(program: &str, name: &str, expect: &str, library_path: Option<&str>) -> bool {
    match run_search(program, &["open", name, expect], library_path) {
        Some(0) => true,
        other => {
            eprintln!(
                "dl_test: {program} open {name} (LD_LIBRARY_PATH={library_path:?}) wanted {expect}, exit {other:?}"
            );
            false
        }
    }
}

/// A `DT_NEEDED` found only through `$ORIGIN/../lib` in the executable's
/// `DT_RUNPATH`: the same binary one directory over cannot start.
fn origin_runpath_resolves_a_needed_library() -> bool {
    if !search_tree() {
        return false;
    }
    let copy = format!("{TREE}/bin/dl_search_origin");
    // Not normalised, as glibc's is not: the object is named by the path the
    // entry expanded to.
    let lib = format!("{TREE}/bin/../lib/libdlsearch.so");
    if !loads(&copy, "libdlsearch.so", &lib, None) {
        return false;
    }
    // 127 is the loader's own failure exit: `/bin/../lib` has no such file.
    match run_search(
        "/bin/dl_search_origin",
        &["open", "libdlsearch.so", &lib],
        None,
    ) {
        Some(127) => true,
        other => {
            eprintln!("dl_test: /bin/dl_search_origin started without its library: {other:?}");
            false
        }
    }
}

/// `${ORIGIN}` in `DT_RPATH` and `$ORIGIN` in `DT_RUNPATH` both name the
/// directory the executable was opened from.
fn origin_expands_in_rpath_and_runpath() -> bool {
    search_tree()
        && loads(
            &format!("{TREE}/bin/dl_search_rpath"),
            "libdlsearch.so",
            &format!("{TREE}/bin/rpath/libdlsearch.so"),
            None,
        )
        && loads(
            &format!("{TREE}/bin/dl_search_runpath"),
            "libdlsearch.so",
            &format!("{TREE}/bin/runpath/libdlsearch.so"),
            None,
        )
}

/// `LD_LIBRARY_PATH` is searched before either kind of entry, and an empty
/// or missing directory in it is passed over.
fn library_path_precedes_rpath_and_runpath() -> bool {
    let llp = format!("/tmp/no-such-dir::{TREE}/llp");
    let expect = format!("{TREE}/llp/libdlsearch.so");
    search_tree()
        && loads(
            &format!("{TREE}/bin/dl_search_rpath"),
            "libdlsearch.so",
            &expect,
            Some(&llp),
        )
        && loads(
            &format!("{TREE}/bin/dl_search_runpath"),
            "libdlsearch.so",
            &expect,
            Some(&llp),
        )
}

/// A dependency of an object that carries `DT_RUNPATH` is looked for there
/// and not in the executable's `DT_RPATH`; one of an object with neither
/// falls back to the `DT_RPATH` of whoever loaded it. The object itself
/// loads once its dependency is findable, so the refusal is the search.
fn runpath_suppresses_the_loaders_rpath() -> bool {
    let probe = format!("{TREE}/bin/dl_search_rpath");
    let runpath_lib = format!("{TREE}/dep/libdlrunpath.so");
    search_tree()
        && loads(&probe, &runpath_lib, "-", None)
        && loads(
            &probe,
            &runpath_lib,
            &format!("{TREE}/llp/libdlsearch.so"),
            Some(&format!("{TREE}/llp")),
        )
        && loads(
            &probe,
            &format!("{TREE}/dep/libdlplain.so"),
            &format!("{TREE}/bin/rpath/libdlsearch.so"),
            None,
        )
}

/// A program whose exec conferred a grant runs with `AT_SECURE`, and its
/// loader passes over `LD_LIBRARY_PATH` and the `$ORIGIN` entry — both name a
/// directory holding the library — for the absolute entry after them.
fn secure_exec_ignores_library_path_and_origin() -> bool {
    if !search_tree() {
        return false;
    }
    if run_search(SECURE_PROBE, &["self", SECURE_PROBE, "1"], None) != Some(0) {
        eprintln!("dl_test: {SECURE_PROBE} did not see AT_SECURE=1 and its own path");
        return false;
    }
    loads(
        SECURE_PROBE,
        "libdlsearch.so",
        &format!("{SECURE_ABS_DIR}/libdlsearch.so"),
        Some(SECURE_ORIGIN_DIR),
    )
}

/// The startup-binding report `LD_DEBUG=statistics` asks for, and its
/// absence under `AT_SECURE`, where the environment is not the program's to
/// act on.
fn ld_debug_statistics_reports_startup_binding() -> bool {
    let stderr_of = |program: &str| {
        Command::new(program)
            .args(["self", program, "-"])
            .env_remove("LD_LIBRARY_PATH")
            .env("LD_DEBUG", "statistics")
            .output()
            .map(|out| String::from_utf8_lossy(&out.stderr).into_owned())
    };
    match stderr_of("/bin/dl_search_rpath") {
        Ok(err)
            if err.lines().any(|l| {
                l.starts_with("ld.so: ")
                    && l.ends_with(" objects")
                    && l.contains(" relocations bound in ")
            }) => {}
        other => {
            eprintln!("dl_test: LD_DEBUG=statistics printed {other:?}");
            return false;
        }
    }
    match stderr_of(SECURE_PROBE) {
        Ok(err) if !err.contains("ld.so:") => true,
        other => {
            eprintln!("dl_test: {SECURE_PROBE} honoured LD_DEBUG: {other:?}");
            false
        }
    }
}

/// `getauxval(AT_EXECFN)` and `dladdr` on the executable name the file the
/// kernel opened, symlinks resolved, and an ungranted exec has
/// `AT_SECURE` 0.
fn execfn_names_the_resolved_executable() -> bool {
    if !search_tree() {
        return false;
    }
    let copy = format!("{TREE}/bin/dl_search_rpath");
    let link = format!("{TREE}/link");
    for (program, expect) in [
        (copy.as_str(), copy.as_str()),
        (link.as_str(), "/bin/dl_search_rpath"),
    ] {
        match run_search(program, &["self", expect, "0"], None) {
            Some(0) => {}
            other => {
                eprintln!("dl_test: {program} self {expect}: exit {other:?}");
                return false;
            }
        }
    }
    true
}
