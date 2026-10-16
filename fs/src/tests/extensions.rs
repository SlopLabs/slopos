//! Extension activations: an archive shown read-only and pinned at
//! `/opt/<name>` and attesting its files as its own, replaced in one step,
//! freed once nothing holds it, held to the budget and the pool, and refused
//! a name the grammar does not admit.

use slopos_abi::extension::{DIGEST_LEN, Label, MAX_ACTIVATIONS};
use slopos_abi::fs::{S_IFLNK, S_IFREG};
use slopos_ostd::process::Program;
use slopos_testing::TestResult;

use super::generations::{archive, pieces, read_all};
use crate::basefs::activate::ActivateError;
use crate::basefs::extension::{self, POOL, deactivate, is_activation, label_of};
use crate::basefs::{BaseFs, generation_of, program_of, program_path};
use crate::vfs::mount::{mount_at, mount_subtree, unmount};
use crate::vfs::path::{RESOLVE_FOLLOW, resolve_path_at};
use crate::vfs::traits::{FileSystem, InodeId, same_filesystem};
use crate::vfs::{Subject, VfsError, vfs_init_builtin_filesystems, vfs_open};
use crate::vfs_file_ops::{
    drop_vnode_for_tests, vfs_dir_handle_still_names, vnode_handle_for_tests,
};

const EXEC: u32 = S_IFREG | 0o755;
const FILE: u32 = S_IFREG | 0o644;
const ROOMY: u64 = 1 << 20;

fn digest(n: u8) -> [u8; DIGEST_LEN] {
    [n; DIGEST_LEN]
}

fn activate(name: &[u8], image: &[u8], budget: u64) -> Result<u32, ActivateError> {
    extension::activate(name, digest(name[0]), &mut pieces(image, 509), budget)
}

fn resolve(path: &[u8]) -> Result<(&'static dyn FileSystem, InodeId), VfsError> {
    resolve_path_at(path, b"/", RESOLVE_FOLLOW, &Subject::KERNEL).map(|r| (r.fs, r.inode))
}

fn instance_of(fs: &dyn FileSystem) -> Option<&'static BaseFs> {
    POOL.iter()
        .find(|mine| same_filesystem(fs, *mine as &dyn FileSystem))
}

fn named(program: Program, expected: &[u8]) -> bool {
    let mut buf = [0u8; 64];
    program_path(program, &mut buf).is_ok_and(|len| buf.get(..len) == Some(expected))
}

/// Free what nothing holds: two passes of every instance, as the reaper's
/// first and second. The reaper may run beside them, so what is freed is
/// read from what stays resident, never from a count.
fn reap_all() {
    for _ in 0..2 {
        for fs in POOL.iter() {
            fs.reap();
        }
    }
}

/// An activation shows its archive at `/opt/<name>`, read-only and pinned;
/// each file attests as the activation's program, named by its path under
/// the name and labelled with the name and digest it was activated as.
/// Deactivated, the name shows nothing, and once reaped its programs name
/// nothing either.
pub fn test_an_activation_is_shown_at_its_name_and_attests_its_files() -> TestResult {
    let Some(image) = archive(&[
        ("bin/tool", EXEC, b"tool one"),
        ("lib/libx.so", FILE, b"library"),
        ("share/link", S_IFLNK | 0o777, b"../bin/tool"),
    ]) else {
        return slopos_testing::fail!("no room for the archive");
    };
    if vfs_init_builtin_filesystems().is_err() {
        return slopos_testing::fail!("no VFS");
    }
    let number = match activate(b"ext-shown", &image, ROOMY) {
        Ok(number) => number,
        Err(e) => return slopos_testing::fail!("the archive did not activate: {:?}", e),
    };
    let outcome = (|| {
        let (fs, tool) =
            resolve(b"/opt/ext-shown/bin/tool").map_err(|_| "the tool is not shown")?;
        if !is_activation(fs) || generation_of(tool) != number {
            return Err("the tool is not the activation's");
        }
        if resolve(b"/opt/ext-shown/share/link").map(|(_, at)| at) != Ok(tool) {
            return Err("a link inside the activation does not reach its file");
        }
        let mut buf = [0u8; 16];
        if read_all(instance_of(fs).ok_or("no instance")?, tool, &mut buf) != Some(8)
            || &buf[..8] != b"tool one"
        {
            return Err("the tool reads other than the archive's bytes");
        }
        let mounted = mount_at(b"/opt/ext-shown").ok_or("nothing is mounted at the name")?;
        if !mounted.read_only() || !mounted.pinned() {
            return Err("the mount is not read-only and pinned");
        }
        if vfs_open(b"/opt/ext-shown/bin/new", true).is_ok() {
            return Err("a file was made inside the activation");
        }
        let program = program_of(fs, tool);
        if !matches!(program, Program::Extension(inode) if inode.get() == tool) {
            return Err("the tool does not attest as the activation's program");
        }
        if !named(program, b"/opt/ext-shown/bin/tool") {
            return Err("the program is not named by its path under the name");
        }
        if label_of(program) != Label::new(b"ext-shown", digest(b'e')) {
            return Err("the program's label is not its activation's");
        }
        Ok(program)
    })();
    let taken = deactivate(b"ext-shown");
    let program = match outcome {
        Ok(program) => program,
        Err(why) => return slopos_testing::fail!("{}", why),
    };
    if taken.is_err() || resolve(b"/opt/ext-shown/bin/tool").is_ok() {
        return slopos_testing::fail!("the deactivated name still shows the activation");
    }
    reap_all();
    if program_path(program, &mut []).is_ok() || label_of(program).is_some() {
        return slopos_testing::fail!("the unheld activation was not freed with its names");
    }
    TestResult::Pass
}

/// A second activation of a name moves its mount, keeping its id, in one
/// step, and a directory handle with it; a file of the first held open still
/// reads the first's bytes, and the first is freed only once nothing holds
/// it. A name another filesystem
/// is mounted at is neither activated over nor deactivated.
pub fn test_a_replacement_switches_the_mount_in_one_step() -> TestResult {
    const BIN: &[u8] = b"/opt/ext-swap/bin";
    let (Some(one), Some(two)) = (
        archive(&[("bin/tool", EXEC, b"one")]),
        archive(&[("bin/tool", EXEC, b"two"), ("bin/more", EXEC, b"more")]),
    ) else {
        return slopos_testing::fail!("no room for the archives");
    };
    if vfs_init_builtin_filesystems().is_err() {
        return slopos_testing::fail!("no VFS");
    }
    let Ok(first) = activate(b"ext-swap", &one, ROOMY) else {
        return slopos_testing::fail!("the first did not activate");
    };
    let held = resolve(b"/opt/ext-swap/bin/tool");
    let id = mount_at(b"/opt/ext-swap").map(|m| m.id);
    let opened = held.is_ok_and(|(fs, inode)| crate::vfs::orphan::open_ref(fs, inode).is_ok());
    let dir = resolve(BIN)
        .ok()
        .and_then(|(fs, inode)| vnode_handle_for_tests(fs, inode));
    let second = activate(b"ext-swap", &two, ROOMY);
    let outcome = (|| {
        let (fs, old) = held.map_err(|_| "the first's tool is not shown")?;
        let instance = instance_of(fs).ok_or("no instance")?;
        let second = second.map_err(|_| "the second did not activate")?;
        if !opened {
            return Err("could not hold the first's tool open");
        }
        if !dir.is_some_and(|dir| vfs_dir_handle_still_names(dir, BIN)) {
            return Err("a directory handle did not follow the replacement");
        }
        let shown = mount_at(b"/opt/ext-swap").ok_or("the name shows nothing")?;
        if Some(shown.id) != id || generation_of(shown.root) != second || second <= first {
            return Err("the mount did not move to the second in place");
        }
        let (_, new) = resolve(b"/opt/ext-swap/bin/tool").map_err(|_| "no tool now")?;
        let mut buf = [0u8; 8];
        if generation_of(new) != second
            || read_all(instance, old, &mut buf) != Some(3)
            || &buf[..3] != b"one"
        {
            return Err("the held file does not read the first's bytes");
        }
        reap_all();
        if instance.stat(old).is_err() {
            return Err("the first was freed while held");
        }
        crate::vfs::orphan::close_ref(fs, old);
        reap_all();
        if instance.stat(old).is_ok() {
            return Err("the first was not freed once nothing held it");
        }
        Ok(())
    })();
    let _ = deactivate(b"ext-swap");
    let outlived = dir.is_some_and(|dir| vfs_dir_handle_still_names(dir, BIN));
    if let Some(dir) = dir {
        drop_vnode_for_tests(dir);
    }
    reap_all();
    if let Err(why) = outcome {
        return slopos_testing::fail!("{}", why);
    }
    if outlived {
        return slopos_testing::fail!("a directory handle outlived the deactivation");
    }
    a_mount_that_is_no_activation_stays()
}

static FOREIGN: BaseFs = BaseFs::new();

fn a_mount_that_is_no_activation_stays() -> TestResult {
    let Some(image) = archive(&[("bin/tool", EXEC, b"one")]) else {
        return slopos_testing::fail!("no room for the archive");
    };
    if FOREIGN.install(&super::BASE_SAMPLE, &[]).is_err()
        || mount_subtree(b"/opt/ext-foreign", &FOREIGN, FOREIGN.root_inode(), 0).is_err()
    {
        return slopos_testing::fail!("could not mount another filesystem at the name");
    }
    let over = activate(b"ext-foreign", &image, ROOMY);
    let taken = deactivate(b"ext-foreign");
    let stays = mount_at(b"/opt/ext-foreign").is_some_and(|m| same_filesystem(m.fs, &FOREIGN));
    let _ = unmount(b"/opt/ext-foreign");
    reap_all();
    if over != Err(ActivateError::Mounts(VfsError::Busy)) {
        return slopos_testing::fail!("an activation went over another mount: {:?}", over);
    }
    if taken != Err(ActivateError::Mounts(VfsError::NotFound)) || !stays {
        return slopos_testing::fail!("deactivating took another filesystem's mount");
    }
    TestResult::Pass
}

/// The copies resident together stay within the budget, a deactivated one
/// held open among them, and the pool holds at most its number of
/// activations at once. What an earlier test left resident counts too.
pub fn test_activations_stay_within_the_budget_and_the_pool() -> TestResult {
    let Some(image) = archive(&[("bin/tool", EXEC, &[b'x'; 3000])]) else {
        return slopos_testing::fail!("no room for the archive");
    };
    if vfs_init_builtin_filesystems().is_err() {
        return slopos_testing::fail!("no VFS");
    }
    reap_all();
    let resident: u64 = POOL.iter().map(BaseFs::resident).sum();
    let tight = resident + image.len() as u64 + 1000;
    if activate(b"ext-held", &image, tight).is_err() {
        return slopos_testing::fail!("one copy within the budget was refused");
    }
    let held = resolve(b"/opt/ext-held/bin/tool")
        .ok()
        .filter(|&(fs, inode)| crate::vfs::orphan::open_ref(fs, inode).is_ok());
    let _ = deactivate(b"ext-held");
    let crowded = activate(b"ext-next", &image, tight);
    if let Some((fs, inode)) = held {
        crate::vfs::orphan::close_ref(fs, inode);
    }
    reap_all();
    let room = activate(b"ext-next", &image, tight);
    let _ = deactivate(b"ext-next");
    reap_all();
    if held.is_none() || crowded != Err(ActivateError::Busy) {
        return slopos_testing::fail!("a held copy did not count: {:?}", crowded);
    }
    if room.is_err() {
        return slopos_testing::fail!("the budget was not given back once freed");
    }
    the_pool_holds_its_number()
}

fn the_pool_holds_its_number() -> TestResult {
    let Some(image) = archive(&[("bin/tool", EXEC, b"t")]) else {
        return slopos_testing::fail!("no room for the archive");
    };
    reap_all();
    let vacant = || POOL.iter().filter(|fs| fs.vacant()).count();
    let available = vacant();
    let mut name = *b"ext-pool-00";
    let mut shown = 0;
    let mut refused = false;
    for i in 0..=MAX_ACTIVATIONS {
        name[9] = b'0' + (i / 10) as u8;
        name[10] = b'0' + (i % 10) as u8;
        match activate(&name, &image, ROOMY) {
            Ok(_) => shown += 1,
            Err(ActivateError::Busy) => {
                refused = true;
                break;
            }
            Err(e) => return slopos_testing::fail!("activation {} failed: {:?}", i, e),
        }
    }
    for i in 0..shown {
        name[9] = b'0' + (i / 10) as u8;
        name[10] = b'0' + (i % 10) as u8;
        let _ = deactivate(&name);
    }
    reap_all();
    if POOL.len() != MAX_ACTIVATIONS || !refused || shown < available {
        return slopos_testing::fail!(
            "{} shown of {} vacant before the pool refused one: {}",
            shown,
            available,
            refused
        );
    }
    if vacant() < available {
        return slopos_testing::fail!("the pool did not empty again: {} vacant", vacant());
    }
    TestResult::Pass
}

/// A name the grammar refuses is neither activated nor deactivated, and a
/// name nothing is shown at has nothing to take away.
pub fn test_a_name_outside_the_grammar_is_refused() -> TestResult {
    let Some(image) = archive(&[("bin/tool", EXEC, b"t")]) else {
        return slopos_testing::fail!("no room for the archive");
    };
    for name in [
        &b"bin"[..],
        b"etc",
        b"Upper",
        b"dot.ted",
        b"sl/ash",
        b"..",
        b"",
        b"abcdefghijklmnopqrstuvwxyz0123456",
    ] {
        if activate_named(name, &image) != Err(ActivateError::Name) {
            return slopos_testing::fail!("an activation named {:?} was not refused", name);
        }
        if deactivate(name) != Err(ActivateError::Name) {
            return slopos_testing::fail!("deactivating {:?} was not refused", name);
        }
    }
    if deactivate(b"ext-never") != Err(ActivateError::Mounts(VfsError::NotFound)) {
        return slopos_testing::fail!("a name nothing is shown at had something taken away");
    }
    TestResult::Pass
}

fn activate_named(name: &[u8], image: &[u8]) -> Result<u32, ActivateError> {
    extension::activate(name, digest(0), &mut pieces(image, 509), ROOMY)
}

slopos_testing::stest!(name = test_an_activation_is_shown_at_its_name_and_attests_its_files);
slopos_testing::stest!(name = test_a_replacement_switches_the_mount_in_one_step);
slopos_testing::stest!(name = test_activations_stay_within_the_budget_and_the_pool);
slopos_testing::stest!(name = test_a_name_outside_the_grammar_is_refused);

/// A live process running an activation's program holds that activation, as
/// one running a base program holds its generation, and an exited one does
/// not; neither holds the base's generation of the same number.
pub fn test_a_running_extension_program_holds_its_activation() -> TestResult {
    use crate::basefs::BASE_FS;
    use crate::basefs::reap::runs_from;
    use slopos_ostd::process::{Cred, process_retire, process_spawn, root_account};
    const NUMBER: u32 = 0x7fff_fff1;
    let Ok(process) = process_spawn(None, root_account(), Cred::ROOT) else {
        return slopos_testing::fail!("could not spawn a process");
    };
    let inode = core::num::NonZeroU64::new((u64::from(NUMBER) << 32) | 1).expect("nonzero");
    let instance = &POOL[0];
    let before = runs_from(instance, NUMBER);
    process.set_program(Program::Extension(inode));
    let running = runs_from(instance, NUMBER);
    let as_base = runs_from(&BASE_FS, NUMBER);
    process.mark_exited();
    let exited = runs_from(instance, NUMBER);
    if let Some(handle) = process.handle() {
        process_retire(handle);
    }
    if before || !running || as_base || exited {
        return slopos_testing::fail!(
            "held before {}, running {}, as the base's {}, after exiting {}",
            before,
            running,
            as_base,
            exited
        );
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_a_running_extension_program_holds_its_activation);
