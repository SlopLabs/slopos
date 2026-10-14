//! `/bin/tree-install` as the session runs it: into a directory of its own,
//! its manifest rewritten in place in uid 0's directory, replacing what the
//! tree's last install put down and nothing else.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, lchown, symlink};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Output};

use slopos_abi::fs::{SESSION_GID, SESSION_UID};
use slopos_tree_core::Manifest;
use slopos_userland::tree::{self, Install};

const NAME: &str = "tree_install_test";
const WORK: &str = "/var/tmp/tree_install_test";

fn manifest() -> String {
    slopos_tree_core::manifest_path(NAME)
}

fn src() -> String {
    format!("{WORK}/src")
}

fn dest() -> String {
    format!("{WORK}/dest")
}

fn install(dest: &str) -> Output {
    Command::new("/bin/tree-install")
        .args([NAME, &src(), dest])
        .uid(SESSION_UID)
        .gid(SESSION_GID)
        .output()
        .expect("spawning /bin/tree-install")
}

/// What uid 0 makes of the session's record: it installs the tree again.
fn doubted(dest: &str) -> bool {
    let source = Path::new(&src()).to_path_buf();
    let install = Install {
        root: "",
        name: NAME,
        guest: dest,
        source: &source,
        owner: Some((SESSION_UID, SESSION_GID)),
        keeper: Some((SESSION_UID, SESSION_GID)),
        remedy: "move it aside",
    };
    match tree::scan(&source).and_then(|new| install.run(&new, &mut |_| {})) {
        Ok(Some(_)) => true,
        other => {
            eprintln!("tree_install_test: uid 0 over the session's record: {other:?}");
            false
        }
    }
}

/// Whether `out` exited `code` saying `said`, and if not, why not.
fn said(out: &Output, code: i32, said: &str) -> bool {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let held = out.status.code() == Some(code) && text.contains(said);
    if !held {
        eprintln!(
            "tree_install_test: wanted exit {code} saying {said:?}, got {:?}: {text}",
            out.status.code()
        );
    }
    held
}

fn give(path: &str) -> bool {
    lchown(path, Some(SESSION_UID), Some(SESSION_GID)).is_ok()
}

fn stage() -> bool {
    let _ = fs::remove_dir_all(WORK);
    let _ = fs::remove_file(manifest());
    fs::create_dir_all(format!("{}/bin", src())).is_ok()
        && fs::create_dir_all(format!("{}/share", src())).is_ok()
        && fs::write(format!("{}/bin/tool", src()), "tool\n").is_ok()
        && fs::write(format!("{}/share/gone", src()), "gone\n").is_ok()
        && symlink("bin/tool", format!("{}/tool", src())).is_ok()
        && tree::hand_over(NAME, (SESSION_UID, SESSION_GID)).is_ok()
        && [
            WORK.to_string(),
            src(),
            format!("{}/bin", src()),
            format!("{}/share", src()),
            format!("{}/bin/tool", src()),
            format!("{}/share/gone", src()),
            format!("{}/tool", src()),
        ]
        .iter()
        .all(|path| give(path))
}

fn recorded() -> Option<Manifest> {
    Manifest::parse(&fs::read_to_string(manifest()).ok()?).ok()
}

fn owner(path: &str) -> Option<u32> {
    fs::symlink_metadata(path).ok().map(|meta| meta.uid())
}

fn holds(path: &str, text: &str) -> bool {
    fs::read_to_string(path).is_ok_and(|t| t == text)
}

const INSTALLED: &str = "installed tree_install_test";

/// Each step of [`the_session_installs_a_tree_of_its_own`], in order.
const STEPS: [(&str, fn(&str) -> bool); 9] = [
    (
        "a first install over the manifest uid 0 handed over",
        |dest| {
            said(&install(dest), 0, INSTALLED)
                && holds(&format!("{dest}/bin/tool"), "tool\n")
                && fs::read_link(format!("{dest}/tool")).is_ok_and(|t| t.as_os_str() == "bin/tool")
                && owner(&format!("{dest}/bin/tool")) == Some(SESSION_UID)
                && owner(&manifest()) == Some(SESSION_UID)
                && recorded().is_some_and(|m| m.is_finished() && m.at.as_deref() == Some(dest))
        },
    ),
    ("the same tree again", |dest| {
        said(&install(dest), 0, "already holds")
    }),
    ("again with a file of the tree gone", |dest| {
        fs::remove_file(format!("{dest}/bin/tool")).is_ok()
            && said(&install(dest), 0, INSTALLED)
            && holds(&format!("{dest}/bin/tool"), "tool\n")
    }),
    ("a file of the session's where the tree adds one", |dest| {
        let before = recorded();
        fs::write(format!("{}/bin/new", src()), "new\n").is_ok()
            && give(&format!("{}/bin/new", src()))
            && fs::remove_file(format!("{}/share/gone", src())).is_ok()
            && fs::write(format!("{dest}/bin/new"), "mine\n").is_ok()
            && fs::write(format!("{dest}/bin/kept"), "kept\n").is_ok()
            && said(&install(dest), 1, "move it aside")
            && recorded() == before
    }),
    ("another destination", |dest| {
        said(
            &install(&format!("{WORK}/elsewhere")),
            1,
            &format!("went to {dest}"),
        )
    }),
    ("a reinstall replacing only its own", |dest| {
        fs::remove_file(format!("{dest}/bin/new")).is_ok()
            && said(&install(dest), 0, INSTALLED)
            && holds(&format!("{dest}/bin/new"), "new\n")
            && holds(&format!("{dest}/bin/kept"), "kept\n")
            && fs::symlink_metadata(format!("{dest}/share/gone")).is_err()
    }),
    ("a manifest an in-place rewrite left empty", |dest| {
        fs::write(manifest(), "").is_ok()
            && said(&install(dest), 0, INSTALLED)
            && recorded().is_some_and(|m| m.is_finished())
            && holds(&format!("{dest}/bin/kept"), "kept\n")
    }),
    (
        "uid 0 over a session manifest naming the session's file",
        |dest| {
            fs::OpenOptions::new()
                .append(true)
                .open(manifest())
                .and_then(|mut f| f.write_all(b"f 5 bin/kept\n"))
                .is_ok()
                && doubted(dest)
                && holds(&format!("{dest}/bin/kept"), "kept\n")
        },
    ),
    ("a manifest uid 0 keeps", |dest| {
        lchown(manifest(), Some(0), Some(0)).is_ok() && said(&install(dest), 1, "is uid 0's")
    }),
];

/// The session installs a tree over the manifest uid 0 handed it, and again
/// only when the tree or what it put down changed; is refused for a file of
/// its own in the way, another destination and a manifest it does not keep;
/// recovers from a manifest rewrite cut short; and uid 0 takes back only the
/// tree's own paths whatever the session's manifest names.
fn the_session_installs_a_tree_of_its_own() -> bool {
    if !stage() {
        eprintln!("tree_install_test: staging {WORK} failed");
        return false;
    }
    let held = STEPS.iter().all(|(step, passes)| {
        passes(&dest()) || {
            eprintln!("tree_install_test: {step} failed");
            false
        }
    });
    let _ = fs::remove_dir_all(WORK);
    let _ = fs::remove_file(manifest());
    held
}

const CASES: &[(&str, fn() -> bool)] = &[(
    "the_session_installs_a_tree_of_its_own",
    the_session_installs_a_tree_of_its_own,
)];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
