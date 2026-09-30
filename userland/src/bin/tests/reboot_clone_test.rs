//! A git clone on the root survives a power-off: one boot of `just
//! test-toolchain` clones the workspace and commits in the clone, the next
//! boot of the same image finds both intact.

use slopos_userland as _;

use slopos_slibc::test_harness::note;
use slopos_userland::selfhost::{SOURCE, git, stdout_of, workspace};
use slopos_userland::syscall::fs::sync;
use std::fs;
use std::io::ErrorKind;

const CLONE: &str = "/home/clone";
const WITNESS: &str = "SURVIVES";

fn a_clone_survives_a_reboot() -> bool {
    if let Err(why) = workspace() {
        note(why);
        return true;
    }
    let outcome = match fs::symlink_metadata(CLONE) {
        Ok(_) => verify(),
        // Only an absent clone means the first boot; cloning over any other
        // error would report the failure this test exists to catch as a pass.
        Err(e) if e.kind() == ErrorKind::NotFound => write(),
        Err(e) => Err(format!("{CLONE}: {e}")),
    };
    match outcome {
        Ok(said) => {
            note(&said);
            true
        }
        Err(why) => {
            note(&why);
            false
        }
    }
}

fn write() -> Result<String, String> {
    stdout_of(
        git("/", &["clone", "-q", "--no-local", SOURCE, CLONE]),
        "git clone",
    )?;
    fs::write(format!("{CLONE}/{WITNESS}"), "written before a power-off\n")
        .map_err(|e| format!("writing {WITNESS}: {e}"))?;
    stdout_of(git(CLONE, &["add", WITNESS]), "git add")?;
    stdout_of(
        git(
            CLONE,
            &[
                "-c",
                "user.name=reboot_clone_test",
                "-c",
                "user.email=reboot_clone_test@slopos.invalid",
                "commit",
                "-q",
                "-m",
                "reboot_clone_test: the witness",
            ],
        ),
        "git commit",
    )?;
    let head = stdout_of(git(CLONE, &["rev-parse", "HEAD"]), "git rev-parse")?;
    sync().map_err(|e| format!("sync: {e:?}"))?;
    Ok(format!("CLONE-WRITTEN {}", head.trim()))
}

/// `git read-tree` drops the index's stat data, so `git status` rehashes
/// every file: a clean status is the whole working tree read back.
fn verify() -> Result<String, String> {
    stdout_of(
        git(
            CLONE,
            &[
                "fsck",
                "--no-progress",
                "--full",
                "--strict",
                "--no-dangling",
            ],
        ),
        "git fsck",
    )?;
    stdout_of(git(CLONE, &["read-tree", "HEAD"]), "git read-tree")?;
    let status = stdout_of(git(CLONE, &["status", "--porcelain"]), "git status")?;
    if !status.is_empty() {
        return Err(format!("the working tree changed:\n{status}"));
    }
    let subject = stdout_of(git(CLONE, &["log", "-1", "--format=%s"]), "git log")?;
    if subject.trim() != "reboot_clone_test: the witness" {
        return Err(format!("HEAD is {:?}, not the witness", subject.trim()));
    }
    let head = stdout_of(git(CLONE, &["rev-parse", "HEAD"]), "git rev-parse")?;
    Ok(format!("CLONE-SURVIVED {}", head.trim()))
}

fn main() {
    slopos_slibc::test_harness::run(&[("a_clone_survives_a_reboot", a_clone_survives_a_reboot)]);
}
