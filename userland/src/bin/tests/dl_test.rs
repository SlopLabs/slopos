//! The standing proof that a `PT_INTERP` program runs and `dlopen` works.
//!
//! The work is in `/bin/dl_probe` and the `dl_search_*` probes, which are
//! dynamically linked: they reach the C library exclusively through
//! `libc.so`, the way a cross-built C or C++ program will. This binary is
//! static, so it can report through the harness, and it grades the probes'
//! exit statuses.

use std::ffi::CString;
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::FromRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::process::Command;
use std::sync::LazyLock;

use slopos_abi::caps::SYS_INSPECT;
use slopos_abi::fs::{SESSION_GID, SESSION_HOME, SESSION_UID};
use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};
use slopos_userland::syscall::fs::pipe;
use slopos_userland::syscall::process::{self, Endowment};

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
const LOCAL_LIB: &str = "/usr/local/lib";
/// Below a person's home, their own libraries, searched ahead of
/// [`LOCAL_LIB`] for them alone.
const HOME_LIB: &str = ".local/lib";
/// A probe whose only search entry, `${ORIGIN}/rpath`, names nothing, so a
/// bare name falls through to the loader's own directories.
const DEFAULT_PROBE: &str = "/bin/dl_search_rpath";

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
    (
        "usr_local_lib_is_not_searched_for_root_or_secure",
        usr_local_lib_is_not_searched_for_root_or_secure,
    ),
    (
        "a_person_s_own_libraries_come_before_usr_local_lib",
        a_person_s_own_libraries_come_before_usr_local_lib,
    ),
];

/// Run as the first person, from `utest_dl_session`.
const SESSION_CASES: &[(&str, fn() -> bool)] = &[(
    "a_person_installs_libraries_of_their_own",
    a_person_installs_libraries_of_their_own,
)];

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--session") => slopos_slibc::test_harness::run(SESSION_CASES),
        _ => slopos_slibc::test_harness::run(CASES),
    }
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

/// Run the secure probe with `args`, `env` its whole environment, holding a
/// capability delegated to it, which is what starts it `AT_SECURE`. Answers
/// its exit code and what it wrote to stderr.
fn run_secure(args: &[&str], env: &[&str]) -> Option<(i32, String)> {
    run_endowed(
        SECURE_PROBE,
        args,
        env,
        Endowment {
            caps: SYS_INSPECT,
            ..Endowment::NONE
        },
    )
}

fn run_endowed(
    program: &str,
    args: &[&str],
    env: &[&str],
    endowment: Endowment<'_>,
) -> Option<(i32, String)> {
    let cstrings = |list: &mut dyn Iterator<Item = &str>| -> Vec<CString> {
        list.map(|s| CString::new(s).expect("no argument holds a NUL"))
            .collect()
    };
    let argv = cstrings(&mut std::iter::once(program).chain(args.iter().copied()));
    let envp = cstrings(&mut env.iter().copied());
    let pointers =
        |list: &[CString]| -> Vec<*const u8> { list.iter().map(|s| s.as_ptr().cast()).collect() };
    let (read, write) = pipe().ok()?;
    let actions = [
        process::clone_fd(0, 0),
        process::clone_fd(1, 1),
        process::clone_fd(write.raw(), 2),
    ];
    let tid = process::spawn_endowed(
        program.as_bytes(),
        &pointers(&argv),
        &pointers(&envp),
        &[],
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &actions,
        0,
        endowment,
    );
    drop(write);
    if tid <= 0 {
        eprintln!("dl_test: spawning {program}: {tid}");
        return None;
    }
    let mut stderr = String::new();
    // SAFETY: the read end is this process's alone, and `File` takes it over.
    let _ = unsafe { File::from_raw_fd(read.into_raw()) }.read_to_string(&mut stderr);
    Some((process::wait_exit_code(tid as u32), stderr))
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

/// A program started holding a gated capability runs with `AT_SECURE`, and
/// its loader passes over `LD_LIBRARY_PATH` and the `$ORIGIN` entry — both
/// name a directory holding the library — for the absolute entry after them.
fn secure_exec_ignores_library_path_and_origin() -> bool {
    if !search_tree() {
        return false;
    }
    if run_secure(&["self", SECURE_PROBE, "1"], &[]).map(|(code, _)| code) != Some(0) {
        eprintln!("dl_test: {SECURE_PROBE} did not see AT_SECURE=1 and its own path");
        return false;
    }
    let found = format!("{SECURE_ABS_DIR}/libdlsearch.so");
    let library_path = format!("LD_LIBRARY_PATH={SECURE_ORIGIN_DIR}");
    match run_secure(&["open", "libdlsearch.so", &found], &[&library_path]) {
        Some((0, _)) => true,
        other => {
            eprintln!("dl_test: {SECURE_PROBE} open libdlsearch.so wanted {found}: {other:?}");
            false
        }
    }
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
    match run_secure(&["self", SECURE_PROBE, "1"], &["LD_DEBUG=statistics"]) {
        Some((0, err)) if !err.contains("ld.so:") => true,
        other => {
            eprintln!("dl_test: {SECURE_PROBE} honoured LD_DEBUG: {other:?}");
            false
        }
    }
}

/// `getauxval(AT_EXECFN)` and `dladdr` on the executable name the file the
/// kernel opened, symlinks resolved, and an exec that changes no ids has
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

/// The fixture under its own name in [`LOCAL_LIB`] for as long as this
/// lives, the directory too when it was not there.
struct LocalLibrary {
    made_dir: bool,
}

impl LocalLibrary {
    fn path() -> String {
        format!("{LOCAL_LIB}/libdlsearch.so")
    }

    fn stage() -> Option<LocalLibrary> {
        let made_dir = !std::path::Path::new(LOCAL_LIB).is_dir();
        let staged = LocalLibrary { made_dir };
        stage(FIXTURE, &Self::path()).then_some(staged)
    }
}

impl Drop for LocalLibrary {
    fn drop(&mut self) {
        let _ = fs::remove_file(Self::path());
        if self.made_dir {
            let _ = fs::remove_dir(LOCAL_LIB);
        }
    }
}

/// A library the session put in [`LOCAL_LIB`] reaches no process of uid 0's
/// and no `AT_SECURE` one, even of the session's uid: either may hold more
/// than the session does.
fn usr_local_lib_is_not_searched_for_root_or_secure() -> bool {
    let Some(_staged) = LocalLibrary::stage() else {
        return false;
    };
    if run_search(DEFAULT_PROBE, &["open", "libdlsearch.so", "-"], None) != Some(0) {
        eprintln!("dl_test: a uid 0 program found libdlsearch.so in {LOCAL_LIB}");
        return false;
    }
    let as_session = Endowment {
        ids: Some((SESSION_UID, SESSION_GID)),
        ..Endowment::NONE
    };
    match run_endowed(
        DEFAULT_PROBE,
        &["self", DEFAULT_PROBE, "1"],
        &[],
        as_session,
    ) {
        Some((0, _)) => {}
        other => {
            eprintln!(
                "dl_test: {DEFAULT_PROBE} as the session from uid 0 is not AT_SECURE: {other:?}"
            );
            return false;
        }
    }
    match run_endowed(
        DEFAULT_PROBE,
        &["open", "libdlsearch.so", "-"],
        &[],
        as_session,
    ) {
        Some((0, _)) => true,
        other => {
            eprintln!("dl_test: an AT_SECURE program searched {LOCAL_LIB}: {other:?}");
            false
        }
    }
}

/// The fixture in the first person's [`HOME_LIB`] for as long as this
/// lives, theirs with each directory it made on the way, which go with it.
struct HomeLibrary {
    made: Vec<String>,
}

impl HomeLibrary {
    fn path() -> String {
        format!("{SESSION_HOME}/{HOME_LIB}/libdlsearch.so")
    }

    fn stage() -> Option<HomeLibrary> {
        let theirs = |path: &str| {
            process::geteuid() == SESSION_UID
                || std::os::unix::fs::lchown(path, Some(SESSION_UID), Some(SESSION_GID)).is_ok()
        };
        let mut staged = HomeLibrary { made: Vec::new() };
        let mut dir = SESSION_HOME.to_owned();
        for part in HOME_LIB.split('/') {
            dir = format!("{dir}/{part}");
            if fs::create_dir(&dir).is_ok() {
                staged.made.push(dir.clone());
                if !theirs(&dir) {
                    return None;
                }
            }
        }
        let path = Self::path();
        (stage(FIXTURE, &path) && theirs(&path)).then_some(staged)
    }
}

impl Drop for HomeLibrary {
    fn drop(&mut self) {
        let _ = fs::remove_file(Self::path());
        for dir in self.made.iter().rev() {
            let _ = fs::remove_dir(dir);
        }
    }
}

/// Whether the probe as the first person, with `HOME` as given or none,
/// loaded `libdlsearch.so` from `expect`: started by a shell of theirs, which
/// is `AT_SECURE` for coming from uid 0, so that the probe it runs is not.
fn loads_as_person(expect: &str, home: Option<&str>) -> bool {
    let as_person = Endowment {
        ids: Some((SESSION_UID, SESSION_GID)),
        ..Endowment::NONE
    };
    let probe = format!("{DEFAULT_PROBE} open libdlsearch.so {expect}");
    let (env, command) = match home {
        Some(home) => (vec![format!("HOME={home}")], probe),
        None => (Vec::new(), format!("unset HOME; {probe}")),
    };
    let env: Vec<&str> = env.iter().map(String::as_str).collect();
    match run_endowed("/bin/shell", &["-c", &command], &env, as_person) {
        Some((0, _)) => true,
        other => {
            eprintln!("dl_test: as the person with HOME {home:?}, wanted {expect}: {other:?}");
            false
        }
    }
}

/// A person's own libraries, below the `HOME` their process started with,
/// come before everyone's in [`LOCAL_LIB`], and neither reaches uid 0 or an
/// `AT_SECURE` process of theirs.
fn a_person_s_own_libraries_come_before_usr_local_lib() -> bool {
    let (Some(_everyone), Some(_own)) = (LocalLibrary::stage(), HomeLibrary::stage()) else {
        return false;
    };
    let home = format!("HOME={SESSION_HOME}");
    let as_person = Endowment {
        ids: Some((SESSION_UID, SESSION_GID)),
        ..Endowment::NONE
    };
    let mut root = Command::new(DEFAULT_PROBE);
    root.args(["open", "libdlsearch.so", "-"])
        .env_remove("LD_LIBRARY_PATH")
        .env("HOME", SESSION_HOME);
    let secure = run_endowed(
        DEFAULT_PROBE,
        &["open", "libdlsearch.so", "-"],
        &[&home],
        as_person,
    );
    loads_as_person(&HomeLibrary::path(), Some(SESSION_HOME))
        && loads_as_person(&HomeLibrary::path(), Some(&format!("{SESSION_HOME}/")))
        && loads_as_person(&LocalLibrary::path(), None)
        && loads_as_person(&LocalLibrary::path(), Some("home/user"))
        && match root.status().map(|status| status.code()) {
            Ok(Some(0)) => true,
            other => {
                eprintln!("dl_test: uid 0 with HOME={SESSION_HOME} searched it: {other:?}");
                false
            }
        }
        && match secure {
            Some((0, _)) => true,
            other => {
                eprintln!("dl_test: an AT_SECURE program searched its HOME: {other:?}");
                false
            }
        }
}

/// The first person puts a library in their own [`HOME_LIB`], holding
/// nothing, and what they run then loads it.
fn a_person_installs_libraries_of_their_own() -> bool {
    let Some(_staged) = HomeLibrary::stage() else {
        return false;
    };
    let found = HomeLibrary::path();
    let mut probe = Command::new(DEFAULT_PROBE);
    probe
        .args(["open", "libdlsearch.so", &found])
        .env_remove("LD_LIBRARY_PATH")
        .env("HOME", SESSION_HOME);
    match probe.status().map(|status| status.code()) {
        Ok(Some(0)) => true,
        other => {
            eprintln!("dl_test: the person's {DEFAULT_PROBE} did not load {found}: {other:?}");
            false
        }
    }
}
