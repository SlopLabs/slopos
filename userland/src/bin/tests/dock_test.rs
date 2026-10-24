//! The dock holds to the app entries: it pins exactly the apps the root's
//! entries name, each launching the program its entry names, an entry the
//! person adds replaces the system's with its id, and the installer's shows
//! only on a boot from the install medium.

use std::ffi::CString;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use slopos_slibc::ffi::syscalls::mkfifo;

use slopos_userland::app_entry::{self, AppEntry};
use slopos_userland::apps::compositor::dock::LauncherShelf;

const HOME: &str = "/var/tmp/dock_test";

const INSTALLER: &str = "org.slopos.installer";

const INSTALLED: &[(&str, &str)] = &[
    ("org.slopos.editor", "/usr/local/bin/editor"),
    ("org.slopos.files", "/usr/local/bin/file_manager"),
    ("org.slopos.image-viewer", "/usr/local/bin/image_viewer"),
    ("org.slopos.sysmon", "/bin/sysmon"),
    ("org.slopos.terminal", "/bin/terminal"),
];

fn pinned(home: Option<&str>, medium: bool) -> (Vec<AppEntry>, LauncherShelf) {
    let entries = app_entry::read(&app_entry::shares(home), medium);
    let mut shelf = LauncherShelf::new();
    shelf.pin(entries.clone());
    (entries, shelf)
}

/// The shelf pins every entry the base and `/usr/local` carry, in the order
/// read, each launching an executable and showing an icon that decodes.
fn the_shelf_pins_every_installed_app() -> bool {
    let (entries, shelf) = pinned(None, false);
    let mut found: Vec<(&str, &str)> = entries
        .iter()
        .map(|e| (e.id.as_str(), e.exec.as_str()))
        .collect();
    found.sort();
    if found != INSTALLED {
        eprintln!("dock_test: the entries name {found:?}, not {INSTALLED:?}");
        return false;
    }
    let held: Vec<(&str, &str)> = shelf
        .entries()
        .iter()
        .map(|e| (e.app_id.as_str(), e.exec.as_str()))
        .collect();
    let read: Vec<(&str, &str)> = entries
        .iter()
        .map(|e| (e.id.as_str(), e.exec.as_str()))
        .collect();
    if held != read || !shelf.entries().iter().all(|e| e.pinned) {
        eprintln!("dock_test: the shelf pins {held:?}, the entries say {read:?}");
        return false;
    }
    entries.iter().zip(shelf.entries()).all(|(entry, held)| {
        let runs = fs::metadata(&entry.exec)
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0);
        if !runs || !held.has_icon() {
            eprintln!(
                "dock_test: {entry:?} runs {runs}, shows its icon {}",
                held.has_icon()
            );
        }
        runs && held.has_icon()
    })
}

fn write(path: &str, text: &str) -> bool {
    let staged = Path::new(path)
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| fs::write(path, text));
    if let Err(e) = &staged {
        eprintln!("dock_test: {path}: {e}");
    }
    staged.is_ok()
}

/// An entry in the person's own share replaces the system's with its id and
/// leaves the rest; one that does not parse is passed over.
fn the_persons_entries_come_last() -> bool {
    let _ = fs::remove_dir_all(HOME);
    let apps = format!("{HOME}/.local/share/slopos/apps");
    let mine =
        "id=org.slopos.editor\nname=My Editor\nexec=/bin/terminal\nicon=mine.png\nlater=key\n";
    let broken = [
        (
            "org.slopos.nameless",
            "id=org.slopos.nameless\nexec=/bin/terminal\n",
        ),
        (
            "org.slopos.relative",
            "id=org.slopos.relative\nname=R\nexec=terminal\n",
        ),
        (
            "org.slopos.other",
            "id=org.slopos.misnamed\nname=O\nexec=/bin/terminal\n",
        ),
        (
            "org.slopos.twice",
            "id=org.slopos.twice\nname=T\nname=U\nexec=/bin/terminal\n",
        ),
        (
            "org.slopos.climbs",
            "id=org.slopos.climbs\nname=C\nexec=/bin/terminal\nicon=../x.png\n",
        ),
        (
            "org.slopos.bare",
            "id=org.slopos.bare\nname=B\nexec=/bin/terminal\nnot a pair\n",
        ),
    ];
    let staged = write(&format!("{apps}/org.slopos.editor.app"), mine)
        && broken
            .iter()
            .all(|(id, text)| write(&format!("{apps}/{id}.app"), text))
        && write(&format!("{apps}/README"), "id=x\nname=x\nexec=/x\n");
    let (entries, shelf) = pinned(Some(HOME), false);
    let _ = fs::remove_dir_all(HOME);
    if !staged {
        return false;
    }
    let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
    let mut sorted = ids.clone();
    sorted.sort();
    if sorted != INSTALLED.iter().map(|(id, _)| *id).collect::<Vec<_>>() {
        eprintln!("dock_test: with the person's share the entries are {ids:?}");
        return false;
    }
    let Some(editor) = entries.iter().find(|e| e.id == "org.slopos.editor") else {
        return false;
    };
    let icon = format!("{HOME}/.local/share/slopos/icons/mine.png");
    if editor.name != "My Editor"
        || editor.exec != "/bin/terminal"
        || editor.icon.as_deref() != Some(Path::new(&icon))
    {
        eprintln!("dock_test: the person's entry read as {editor:?}");
        return false;
    }
    let names: Vec<&str> = shelf.entries().iter().map(|e| e.name.as_str()).collect();
    let mut by_name = names.clone();
    by_name.sort_by_key(|name| name.to_lowercase());
    if names != by_name {
        eprintln!("dock_test: the shelf is not in name order: {names:?}");
        return false;
    }
    true
}

fn make_fifo(path: &str) -> bool {
    let c = CString::new(path).expect("no NUL in a test path");
    // SAFETY: `c` is a NUL-terminated string that outlives the call.
    let made = unsafe { mkfifo(c.as_ptr(), 0o644) } == 0;
    if !made {
        eprintln!(
            "dock_test: mkfifo {path}: errno {}",
            slopos_slibc::errno_get()
        );
    }
    made
}

/// What the session leaves in its share neither redirects, blocks nor
/// swells the compositor: an entry or icon that is a link, a FIFO or too large
/// is passed over, and a share is read only so far.
fn a_share_cannot_stall_or_swell_the_dock() -> bool {
    let _ = fs::remove_dir_all(HOME);
    let apps = format!("{HOME}/.local/share/slopos/apps");
    let icons = format!("{HOME}/.local/share/slopos/icons");
    let Some(icon) = pinned(None, false)
        .0
        .into_iter()
        .find_map(|entry| entry.icon)
    else {
        eprintln!("dock_test: no installed entry has an icon");
        return false;
    };
    let entry = |id: &str, icon: &str| format!("id={id}\nname={id}\nexec=/bin/terminal\n{icon}");
    let mut big_icon = fs::read(&icon).unwrap_or_default();
    big_icon.resize((1 << 20) + 1, 0);
    let made = |what: &str, done: std::io::Result<()>| {
        done.map_err(|e| eprintln!("dock_test: {what}: {e}"))
            .is_ok()
    };
    let staged = made(&apps, fs::create_dir_all(&apps))
        && made(&icons, fs::create_dir_all(&icons))
        && write(
            &format!("{HOME}/elsewhere.app"),
            &entry("org.slopos.linked", ""),
        )
        && made(
            "the linked entry",
            symlink(
                format!("{HOME}/elsewhere.app"),
                format!("{apps}/org.slopos.linked.app"),
            ),
        )
        && make_fifo(&format!("{apps}/org.slopos.fifo.app"))
        && write(
            &format!("{apps}/org.slopos.big.app"),
            &entry("org.slopos.big", &format!("note={}\n", "x".repeat(4096))),
        )
        && write(
            &format!("{apps}/org.slopos.fifoicon.app"),
            &entry("org.slopos.fifoicon", "icon=fifo.png\n"),
        )
        && make_fifo(&format!("{icons}/fifo.png"))
        && write(
            &format!("{apps}/org.slopos.linkicon.app"),
            &entry("org.slopos.linkicon", "icon=link.png\n"),
        )
        && made(
            "the linked icon",
            symlink(&icon, format!("{icons}/link.png")),
        )
        && write(
            &format!("{apps}/org.slopos.bigicon.app"),
            &entry("org.slopos.bigicon", "icon=big.png\n"),
        )
        && made("big.png", fs::write(format!("{icons}/big.png"), &big_icon));
    if !staged {
        eprintln!("dock_test: staging the share failed");
        let _ = fs::remove_dir_all(HOME);
        return false;
    }
    let (entries, shelf) = pinned(Some(HOME), false);
    let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
    let refused = ["org.slopos.linked", "org.slopos.fifo", "org.slopos.big"];
    let iconless = [
        "org.slopos.fifoicon",
        "org.slopos.linkicon",
        "org.slopos.bigicon",
    ];
    let mut held = true;
    if refused.iter().any(|id| ids.contains(id)) || !iconless.iter().all(|id| ids.contains(id)) {
        eprintln!(
            "dock_test: from a share of links, FIFOs and large files the entries are {ids:?}"
        );
        held = false;
    }
    if let Some(shown) = shelf
        .entries()
        .iter()
        .find(|e| iconless.contains(&e.app_id.as_str()) && e.has_icon())
    {
        eprintln!("dock_test: {} shows an icon it should not", shown.app_id);
        held = false;
    }

    let many = (0..100).all(|n| {
        let id = format!("org.slopos.many{n:02}");
        write(&format!("{apps}/{id}.app"), &entry(&id, ""))
    });
    let read = app_entry::read(&app_entry::shares(Some(HOME)), false)
        .iter()
        .filter(|e| e.id.starts_with("org.slopos.many"))
        .count();
    let _ = fs::remove_dir_all(HOME);
    if !(1..=64).contains(&read) {
        eprintln!("dock_test: a share of a hundred entries gave {read}");
        held = false;
    }
    many && held
}

/// The base's installer entry is passed over off the medium and pinned on
/// it, launching the window with its icon; a person's entry for the medium
/// alone replaces the system's only there, and one naming another boot is
/// shown on none.
fn the_installer_is_pinned_from_the_medium_alone() -> bool {
    let ids = |entries: &[AppEntry]| entries.iter().map(|e| e.id.clone()).collect::<Vec<_>>();
    let (off, _) = pinned(None, false);
    let (on, shelf) = pinned(None, true);
    let installer = on.iter().find(|e| e.id == INSTALLER);
    let mut expected = ids(&off);
    expected.push(INSTALLER.to_owned());
    expected.sort();
    let mut shown = ids(&on);
    shown.sort();
    if ids(&off).iter().any(|id| id == INSTALLER) || shown != expected {
        eprintln!(
            "dock_test: off the medium {:?}, on it {:?}",
            ids(&off),
            ids(&on)
        );
        return false;
    }
    let Some(installer) = installer else {
        return false;
    };
    let held = shelf.entries().iter().find(|e| e.app_id == INSTALLER);
    if installer.name != "Install SlopOS"
        || installer.exec != "/bin/installer-ui"
        || !installer.medium
        || !held.is_some_and(|held| held.has_icon())
    {
        eprintln!("dock_test: the installer's entry read as {installer:?}");
        return false;
    }

    let _ = fs::remove_dir_all(HOME);
    let apps = format!("{HOME}/.local/share/slopos/apps");
    let mine = "id=org.slopos.terminal\nname=Live Terminal\nexec=/bin/sysmon\nboot=medium\n";
    let elsewhere = ["boot=disk", "boot=Medium", "boot="];
    let staged = write(&format!("{apps}/org.slopos.terminal.app"), mine)
        && elsewhere.iter().enumerate().all(|(n, boot)| {
            let id = format!("org.slopos.elsewhere{n}");
            write(
                &format!("{apps}/{id}.app"),
                &format!("id={id}\nname=W\nexec=/bin/terminal\n{boot}\n"),
            )
        });
    let read = |medium| pinned(Some(HOME), medium).0;
    let (off, on) = (read(false), read(true));
    let _ = fs::remove_dir_all(HOME);
    if !staged {
        return false;
    }
    let terminal = |entries: &[AppEntry]| {
        entries
            .iter()
            .find(|e| e.id == "org.slopos.terminal")
            .map(|e| e.exec.clone())
    };
    let (off_runs, on_runs) = (terminal(&off), terminal(&on));
    if off_runs.as_deref() != Some("/bin/terminal") || on_runs.as_deref() != Some("/bin/sysmon") {
        eprintln!(
            "dock_test: the person's live terminal runs {off_runs:?} off the medium, {on_runs:?} on it"
        );
        return false;
    }
    let shown: Vec<&str> = off
        .iter()
        .chain(&on)
        .map(|e| e.id.as_str())
        .filter(|id| id.starts_with("org.slopos.elsewhere"))
        .collect();
    if !shown.is_empty() {
        eprintln!("dock_test: entries naming another boot were shown: {shown:?}");
        return false;
    }
    true
}

const CASES: &[(&str, fn() -> bool)] = &[
    (
        "the_shelf_pins_every_installed_app",
        the_shelf_pins_every_installed_app,
    ),
    (
        "the_persons_entries_come_last",
        the_persons_entries_come_last,
    ),
    (
        "a_share_cannot_stall_or_swell_the_dock",
        a_share_cannot_stall_or_swell_the_dock,
    ),
    (
        "the_installer_is_pinned_from_the_medium_alone",
        the_installer_is_pinned_from_the_medium_alone,
    ),
];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
