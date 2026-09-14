#![feature(restricted_std)]

//! Phase 1.1's standing proof: the utilities are executables.
//!
//! Every case here spawns `/bin/<tool>` from a program that is not the shell
//! and reads what it produced — the thing that answered `ENOENT` before, and
//! the thing a build driver does on every line of its work.

use slopos_userland::apps::shell;

use std::fs;
use std::process::Command;

/// Writable on the disk root and on the initramfs alike.
const WORK: &str = "/var/coreutils";

fn setup() -> bool {
    let _ = fs::remove_dir_all(WORK);
    if let Err(e) = fs::create_dir_all(WORK) {
        eprintln!("coreutils_test: create {WORK} failed: {e:?}");
        return false;
    }
    std::env::set_current_dir(WORK).is_ok()
}

struct Output {
    status: i32,
    stdout: Vec<u8>,
}

fn run(tool: &str, args: &[&str]) -> Option<Output> {
    let path = format!("/bin/{tool}");
    match Command::new(&path).args(args).output() {
        Ok(out) => Some(Output {
            status: out.status.code().unwrap_or(-1),
            stdout: out.stdout,
        }),
        Err(e) => {
            eprintln!("coreutils_test: spawning {path} failed: {e:?}");
            None
        }
    }
}

fn run_text(tool: &str, args: &[&str]) -> Option<(i32, String)> {
    let out = run(tool, args)?;
    match String::from_utf8(out.stdout) {
        Ok(text) => Some((out.status, text)),
        Err(_) => {
            eprintln!("coreutils_test: {tool} produced non-UTF-8 output");
            None
        }
    }
}

fn expect(tool: &str, args: &[&str], want: &str) -> bool {
    let Some((status, text)) = run_text(tool, args) else {
        return false;
    };
    if status != 0 {
        eprintln!("coreutils_test: {tool} {args:?} exited {status}");
        return false;
    }
    if text != want {
        eprintln!("coreutils_test: {tool} {args:?} produced {text:?}, wanted {want:?}");
        return false;
    }
    true
}

fn expect_status(tool: &str, args: &[&str], want: i32) -> bool {
    let Some(out) = run(tool, args) else {
        return false;
    };
    if out.status != want {
        eprintln!(
            "coreutils_test: {tool} {args:?} exited {}, wanted {want}",
            out.status
        );
        return false;
    }
    true
}

/// A tool with no installed name cannot be spawned at all, so the binary's
/// table and the `/bin` symlink set must agree.
fn installed_names_match_the_binarys_table() -> bool {
    if !setup() {
        return false;
    }
    let Some((status, listing)) = run_text("coreutils", &["--list"]) else {
        return false;
    };
    if status != 0 {
        eprintln!("coreutils_test: --list exited {status}");
        return false;
    }

    let names: Vec<&str> = listing.lines().filter(|l| !l.is_empty()).collect();
    if names.len() < 50 {
        eprintln!("coreutils_test: only {} tools in the table", names.len());
        return false;
    }

    let mut missing = Vec::new();
    for name in &names {
        let path = format!("/bin/{name}");
        let is_link = fs::symlink_metadata(&path)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        let resolves = fs::metadata(&path).map(|m| m.is_file()).unwrap_or(false);
        if !is_link || !resolves {
            missing.push(*name);
        }
    }
    if !missing.is_empty() {
        eprintln!("coreutils_test: not installed in /bin: {missing:?}");
        return false;
    }
    true
}

/// The workstream in one line: a process that is not the shell spawns a
/// utility by path and reads its output.
fn a_spawned_utility_produces_output() -> bool {
    setup() && expect("echo", &["hello", "world"], "hello world\n")
}

fn text_tools_answer_as_posix_says() -> bool {
    if !setup() {
        return false;
    }
    if fs::write("data", "beta:2\nalpha:1\nbeta:2\ngamma:3\n").is_err() {
        eprintln!("coreutils_test: writing the fixture failed");
        return false;
    }

    expect("wc", &["-l", "data"], "4 data\n")
        && expect("grep", &["-c", "beta", "data"], "2\n")
        && expect("cut", &["-d:", "-f1", "data"], "beta\nalpha\nbeta\ngamma\n")
        && expect("sort", &["-u", "data"], "alpha:1\nbeta:2\ngamma:3\n")
        && expect("head", &["-n", "1", "data"], "beta:2\n")
        && expect("tail", &["-n", "1", "data"], "gamma:3\n")
        && expect("basename", &["/usr/share/x.txt", ".txt"], "x\n")
        && expect("dirname", &["/usr/share/x.txt"], "/usr/share\n")
        && tr_reads_its_standard_input()
}

/// `tr` is stdin-only, as POSIX specifies, so this is also the proof that a
/// spawned utility's redirected standard input reaches it.
fn tr_reads_its_standard_input() -> bool {
    let Ok(input) = fs::File::open("data") else {
        eprintln!("coreutils_test: reopening data failed");
        return false;
    };
    let out = match Command::new("/bin/tr")
        .args(["a-z", "A-Z"])
        .stdin(std::process::Stdio::from(input))
        .output()
    {
        Ok(out) => out,
        Err(e) => {
            eprintln!("coreutils_test: spawning tr failed: {e:?}");
            return false;
        }
    };
    if out.stdout != b"BETA:2\nALPHA:1\nBETA:2\nGAMMA:3\n" {
        eprintln!("coreutils_test: tr produced {:?}", out.stdout);
        return false;
    }
    true
}

/// `cat` truncated at 512 bytes as a builtin. A file past that is the
/// regression this keeps closed.
fn cat_does_not_truncate() -> bool {
    if !setup() {
        return false;
    }
    let body: Vec<u8> = (0..40_000u32).map(|i| b'a' + (i % 26) as u8).collect();
    if fs::write("big", &body).is_err() {
        eprintln!("coreutils_test: writing big failed");
        return false;
    }
    let Some(out) = run("cat", &["big"]) else {
        return false;
    };
    if out.stdout.len() != body.len() {
        eprintln!(
            "coreutils_test: cat produced {} of {} bytes",
            out.stdout.len(),
            body.len()
        );
        return false;
    }
    out.stdout == body
}

/// `ls` must emit bare names: a pipeline that counts or greps a listing is the
/// reason the builtin's `name (size)` shape was wrong.
fn ls_emits_bare_names() -> bool {
    if !setup() {
        return false;
    }
    let _ = fs::create_dir("tree");
    let _ = fs::write("tree/b", "1");
    let _ = fs::write("tree/a", "2");
    expect("ls", &["tree"], "a\nb\n")
}

/// A tool's flags are the tool's: the multicall binary reads `--list` only
/// under its own name, or `ls -l` prints the tool table instead of a listing.
fn a_tools_own_flags_reach_it() -> bool {
    if !setup() {
        return false;
    }
    let _ = fs::write("only", "12345");
    let Some((status, text)) = run_text("ls", &["-l", "only"]) else {
        return false;
    };
    if status != 0 || text.lines().count() != 1 || !text.trim_end().ends_with("only") {
        eprintln!("coreutils_test: ls -l produced {text:?}");
        return false;
    }
    let Ok(input) = fs::File::open("only") else {
        return false;
    };
    match Command::new("/bin/wc")
        .arg("-l")
        .stdin(std::process::Stdio::from(input))
        .output()
    {
        Ok(out) => out.stdout == b"0\n",
        Err(e) => {
            eprintln!("coreutils_test: spawning wc failed: {e:?}");
            false
        }
    }
}

fn trees_are_created_copied_and_removed() -> bool {
    if !setup() {
        return false;
    }
    if !expect_status("mkdir", &["-p", "src/deep/nest"], 0) {
        return false;
    }
    if fs::write("src/deep/nest/leaf", "leaf").is_err() {
        eprintln!("coreutils_test: writing leaf failed");
        return false;
    }
    if !expect_status("cp", &["-r", "src", "copy"], 0) {
        return false;
    }
    match fs::read_to_string("copy/deep/nest/leaf") {
        Ok(text) if text == "leaf" => {}
        other => {
            eprintln!("coreutils_test: cp -r did not reproduce the leaf: {other:?}");
            return false;
        }
    }
    if !expect("find", &["copy", "-name", "leaf"], "copy/deep/nest/leaf\n") {
        return false;
    }
    if !expect_status("rm", &["-r", "copy"], 0) {
        return false;
    }
    if fs::metadata("copy").is_ok() {
        eprintln!("coreutils_test: rm -r left the tree behind");
        return false;
    }
    true
}

/// The named Phase 1.1 deliverable: `diff` produces a patch and `patch`
/// applies it. The builtin could only print `<`/`>` markers.
fn diff_produces_a_patch_that_applies() -> bool {
    if !setup() {
        return false;
    }
    let old = "one\ntwo\nthree\nfour\nfive\n";
    let new = "one\ntwo\nTHREE\nfour\nfive\nsix\n";
    if fs::write("old", old).is_err() || fs::write("new", new).is_err() {
        eprintln!("coreutils_test: writing the diff fixtures failed");
        return false;
    }

    let Some(out) = run("diff", &["-u", "old", "new"]) else {
        return false;
    };
    if out.status != 1 {
        eprintln!("coreutils_test: diff -u exited {}, wanted 1", out.status);
        return false;
    }
    if fs::write("patchfile", &out.stdout).is_err() {
        eprintln!("coreutils_test: writing the patch failed");
        return false;
    }

    if !expect_status("patch", &["-i", "patchfile", "old"], 0) {
        return false;
    }
    match fs::read_to_string("old") {
        Ok(text) if text == new => {}
        other => {
            eprintln!("coreutils_test: patched file is {other:?}");
            return false;
        }
    }
    expect_status("cmp", &["-s", "old", "new"], 0)
}

fn archives_round_trip() -> bool {
    if !setup() {
        return false;
    }
    let _ = fs::create_dir_all("ar/sub");
    if fs::write("ar/sub/file", "archive payload\n").is_err() {
        eprintln!("coreutils_test: writing the archive fixture failed");
        return false;
    }

    if !expect_status("tar", &["-c", "-f", "bundle.tar", "ar"], 0) {
        return false;
    }
    if !expect_status("gzip", &["bundle.tar"], 0) {
        return false;
    }
    if fs::metadata("bundle.tar.gz").is_err() {
        eprintln!("coreutils_test: gzip produced no bundle.tar.gz");
        return false;
    }
    if !expect_status("gunzip", &["bundle.tar.gz"], 0) {
        return false;
    }
    let _ = fs::remove_dir_all("ar");
    if !expect_status("tar", &["-x", "-f", "bundle.tar"], 0) {
        return false;
    }
    match fs::read_to_string("ar/sub/file") {
        Ok(text) if text == "archive payload\n" => {}
        other => {
            eprintln!("coreutils_test: extracted member is {other:?}");
            return false;
        }
    }

    // The digest is the file's, so it is the same before and after the round
    // trip only if every byte survived.
    let Some((status, sum)) = run_text("sha256sum", &["ar/sub/file"]) else {
        return false;
    };
    if status != 0
        || !sum.starts_with("92f86a430bca3d71cc27fcedb1a9b67a487e7f1ac1c97aad57df2d40ec9f8b71")
    {
        // sha256("archive payload\n"); a wrong digest here is a wrong digest,
        // not a wrong archive.
        eprintln!("coreutils_test: sha256sum said {sum:?}");
        return false;
    }
    true
}

/// A build driver reads a status, not a message. `grep`'s 0/1/2 and `test`'s
/// 0/1 are the two every script depends on.
fn statuses_are_posix() -> bool {
    if !setup() {
        return false;
    }
    if fs::write("hay", "needle\n").is_err() {
        return false;
    }
    expect_status("grep", &["needle", "hay"], 0)
        && expect_status("grep", &["absent", "hay"], 1)
        && expect_status("grep", &["needle", "nosuchfile"], 2)
        && expect_status("test", &["-f", "hay"], 0)
        && expect_status("test", &["-f", "nosuchfile"], 1)
        && expect_status("true", &[], 0)
        && expect_status("false", &[], 1)
        && expect_status("which", &["ls"], 0)
        && expect_status("which", &["definitely-not-a-command"], 1)
}

fn which_reports_the_installed_path() -> bool {
    setup() && expect("which", &["ls"], "/bin/ls\n")
}

/// The shell reaches a utility through `PATH` now that none of them is a
/// builtin, so a pipeline of two of them plus a redirect is the integration
/// the builtin table used to hide.
fn the_shell_pipes_one_utility_into_another() -> bool {
    if !setup() {
        return false;
    }
    if fs::write("nums", "3\n1\n2\n1\n").is_err() {
        eprintln!("coreutils_test: writing nums failed");
        return false;
    }

    shell::cwd_set(WORK.as_bytes());
    shell::env::initialize_defaults();
    shell::exec::initialize_job_control();

    let mut tokens = shell::buffers::ParsedTokens::new();
    for token in [
        b"sort".as_slice(),
        b"-n",
        b"nums",
        b"|",
        b"uniq",
        b"-c",
        b">",
        b"counted",
    ] {
        tokens.push_token(token);
    }
    let rc = shell::exec::execute_tokens(&tokens);
    if rc != 0 {
        eprintln!("coreutils_test: the pipeline exited {rc}");
        return false;
    }

    match fs::read_to_string("counted") {
        Ok(text) if text == "      2 1\n      1 2\n      1 3\n" => true,
        other => {
            eprintln!("coreutils_test: the pipeline wrote {other:?}");
            false
        }
    }
}

fn main() {
    slopos_slibc::test_harness::run(&[
        (
            "installed_names_match_the_binarys_table",
            installed_names_match_the_binarys_table,
        ),
        (
            "a_spawned_utility_produces_output",
            a_spawned_utility_produces_output,
        ),
        (
            "text_tools_answer_as_posix_says",
            text_tools_answer_as_posix_says,
        ),
        ("cat_does_not_truncate", cat_does_not_truncate),
        ("ls_emits_bare_names", ls_emits_bare_names),
        (
            "trees_are_created_copied_and_removed",
            trees_are_created_copied_and_removed,
        ),
        (
            "diff_produces_a_patch_that_applies",
            diff_produces_a_patch_that_applies,
        ),
        ("archives_round_trip", archives_round_trip),
        ("statuses_are_posix", statuses_are_posix),
        (
            "which_reports_the_installed_path",
            which_reports_the_installed_path,
        ),
        ("a_tools_own_flags_reach_it", a_tools_own_flags_reach_it),
        (
            "the_shell_pipes_one_utility_into_another",
            the_shell_pipes_one_utility_into_another,
        ),
    ]);
}
