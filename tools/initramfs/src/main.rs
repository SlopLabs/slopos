//! Pack a SlopOS base image: the tree a boot module carries, as an
//! uncompressed `newc` (SVR4) cpio archive.
//!
//! Usage: `initramfs <repo root> <out.cpio> <build dir> <program>...`
//!
//! `COREUTILS_LINKS` names the multicall binary's installed names,
//! `EXTRA_SHARED_OBJECTS` the shared objects a tests image adds to `/lib`, and
//! `SLOPOS_BUILD_TAG`, when set, is written to `/usr/share/slopos/build-tag`.
//! `RECIPE_PROGRAMS` names programs a recipe built, by their path below
//! `RECIPE_PREFIX`, which they keep below `/`, and `RECIPE_LICENSES` the
//! recipes whose licence texts go with them.
//!
//! `initramfs tree <out.cpio> <dir>=<path>...` packs whole directory trees
//! instead, each below its path in the archive, streamed rather than held:
//! the install medium, whose payload is a toolchain and a source tree.
//!
//! The output is a function of the inputs alone: entries in a fixed order,
//! every timestamp, owner and inode number 0. Dependency-free, so it builds
//! for the Linux host and the SlopOS guest alike.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const MODE_EXEC: u32 = 0o100_755;
const MODE_DATA: u32 = 0o100_644;
const MODE_DIR: u32 = 0o040_755;
const MODE_LINK: u32 = 0o120_777;

/// The kernel's cap on one path component.
const MAX_NAME_LEN: usize = 255;

/// The root's writable directories, recorded even with nothing packed beneath
/// them; `/media` is where a boot's `mount=` puts a volume.
const ROOT_DIRS: [&str; 4] = ["/etc", "/var", "/home", "/media"];

const SLIBC_LICENSES: [&str; 3] = ["LICENSE-MIT", "LICENSE-APACHE", "NOTICE"];

struct Entry {
    path: String,
    mode: u32,
    data: Vec<u8>,
}

#[derive(Default)]
struct Extras {
    coreutils_links: String,
    shared_objects: String,
    build_tag: String,
    recipe_prefix: String,
    recipe_programs: String,
    recipe_licenses: String,
}

impl Extras {
    fn from_env() -> Self {
        let var = |name| env::var(name).unwrap_or_default();
        Self {
            coreutils_links: var("COREUTILS_LINKS"),
            shared_objects: var("EXTRA_SHARED_OBJECTS"),
            build_tag: var("SLOPOS_BUILD_TAG"),
            recipe_prefix: var("RECIPE_PREFIX"),
            recipe_programs: var("RECIPE_PROGRAMS"),
            recipe_licenses: var("RECIPE_LICENSES"),
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) == Some("tree") {
        let packed = match args.get(2) {
            Some(out) => pack_trees(Path::new(out), &args[3..]),
            None => Err("usage: initramfs tree <out.cpio> <dir>=<path>...".into()),
        };
        return match packed {
            Ok(()) => ExitCode::SUCCESS,
            Err(msg) => {
                eprintln!("initramfs: {msg}");
                ExitCode::FAILURE
            }
        };
    }
    if args.len() < 4 {
        eprintln!("usage: initramfs <repo root> <out.cpio> <build dir> <program>...");
        return ExitCode::from(2);
    }
    match run(
        Path::new(&args[1]),
        Path::new(&args[2]),
        Path::new(&args[3]),
        &args[4..],
        &Extras::from_env(),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("initramfs: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run(
    root: &Path,
    out: &Path,
    build: &Path,
    programs: &[String],
    extras: &Extras,
) -> Result<(), String> {
    let entries = layout(root, build, programs, extras)?;
    check_tree(&entries)?;
    let mut archive = Vec::new();
    for entry in &entries {
        if entry.path.split('/').any(|name| name.len() > MAX_NAME_LEN) {
            return Err(format!(
                "{}: a component exceeds {MAX_NAME_LEN} bytes",
                entry.path
            ));
        }
        emit(&mut archive, &entry.path, entry.mode, &entry.data);
    }
    emit(&mut archive, "TRAILER!!!", 0, &[]);
    if let Some(dir) = out.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    fs::write(out, &archive).map_err(|e| format!("{}: {e}", out.display()))?;
    println!(
        "initramfs: wrote {} ({} bytes, {} files)",
        out.display(),
        archive.len(),
        entries.len()
    );
    Ok(())
}

/// The kernel refuses a base in which a path is two things, so the packer
/// refuses to write one.
fn check_tree(entries: &[Entry]) -> Result<(), String> {
    let mut modes = BTreeMap::new();
    for entry in entries {
        if modes.insert(entry.path.as_str(), entry.mode).is_some() {
            return Err(format!("{} is packed twice", entry.path));
        }
    }
    for entry in entries {
        let mut parent = entry.path.as_str();
        while let Some(at) = parent.rfind('/') {
            parent = &parent[..at];
            if modes.get(parent).is_some_and(|&mode| mode != MODE_DIR) {
                return Err(format!("{} lies beneath {parent}, a file", entry.path));
            }
        }
    }
    Ok(())
}

fn layout(
    root: &Path,
    build: &Path,
    programs: &[String],
    extras: &Extras,
) -> Result<Vec<Entry>, String> {
    let mut entries: Vec<Entry> = ROOT_DIRS
        .iter()
        .map(|dir| Entry {
            path: (*dir).into(),
            mode: MODE_DIR,
            data: Vec::new(),
        })
        .collect();
    let mut add = |path: String, mode: u32, data: Vec<u8>| entries.push(Entry { path, mode, data });

    for program in programs {
        let data = read(&build.join(format!("{program}.elf")))?;
        let path = if program == "init" {
            "/sbin/init".into()
        } else {
            format!("/bin/{program}")
        };
        add(path, MODE_EXEC, data);
    }
    // A symlink, so the exec grant keyed on `/bin/shell` follows `/bin/sh`.
    if programs.iter().any(|p| p == "shell") {
        add("/bin/sh".into(), MODE_LINK, b"shell".to_vec());
    }
    let tools = &extras.coreutils_links;
    for tool in tools.split_whitespace() {
        add(format!("/bin/{tool}"), MODE_LINK, b"coreutils".to_vec());
    }
    if tools.split_whitespace().any(|tool| tool == "env") {
        add("/usr/bin/env".into(), MODE_LINK, b"/bin/env".to_vec());
    }

    if build.join("libc.so").is_file() {
        add(
            "/lib/libc.so".into(),
            MODE_EXEC,
            read(&build.join("libc.so"))?,
        );
        add("/lib/ld-slopos.so.1".into(), MODE_LINK, b"libc.so".to_vec());
        for text in SLIBC_LICENSES {
            add(
                format!("/usr/share/licenses/slibc/{text}"),
                MODE_DATA,
                read(&root.join("slibc").join(text))?,
            );
        }
    }
    for object in extras.shared_objects.split_whitespace() {
        add(
            format!("/lib/{object}"),
            MODE_EXEC,
            read(&build.join(object))?,
        );
    }
    let ships_cxx = extras
        .shared_objects
        .split_whitespace()
        .any(|o| o == "libc++.so");
    let cxx_licenses = if ships_cxx {
        listing(&build.join("libc++-licenses"), |_| true)?
    } else {
        Vec::new()
    };
    for (name, path) in cxx_licenses {
        add(
            format!("/usr/share/licenses/libc++/{name}"),
            MODE_DATA,
            read(&path)?,
        );
    }

    let recipes = Path::new(&extras.recipe_prefix);
    for program in extras.recipe_programs.split_whitespace() {
        if !program.starts_with("sbin/") && !program.starts_with("bin/") {
            return Err(format!("{program}: a recipe program goes in bin/ or sbin/"));
        }
        add(
            format!("/{program}"),
            MODE_EXEC,
            read(&recipes.join(program))?,
        );
    }
    for recipe in extras.recipe_licenses.split_whitespace() {
        let texts = recipes.join("share/licenses").join(recipe);
        for (rel, path) in files_below(&texts)? {
            add(
                format!("/usr/share/licenses/{recipe}/{rel}"),
                MODE_DATA,
                read(&path)?,
            );
        }
    }

    let assets = root.join("assets");
    for (name, path) in listing(&assets.join("fonts"), |n| {
        n.ends_with(".ttf") || n.ends_with("-OFL.txt")
    })? {
        add(format!("/usr/share/fonts/{name}"), MODE_DATA, read(&path)?);
    }
    for (name, path) in listing(&assets.join("docs"), |n| n.ends_with(".md"))? {
        add(
            format!("/usr/share/slopos/doc/{name}"),
            MODE_DATA,
            read(&path)?,
        );
    }
    let logo = assets.join("logo.png");
    if logo.is_file() {
        add(
            "/usr/share/slopos/wallpapers/default.png".into(),
            MODE_DATA,
            read(&logo)?,
        );
    }
    let certs = assets.join("certs");
    let bundle = certs.join("ca-certificates.crt");
    if bundle.is_file() {
        add(
            "/etc/ssl/certs/ca-certificates.crt".into(),
            MODE_DATA,
            read(&bundle)?,
        );
        add(
            "/etc/ssl/cert.pem".into(),
            MODE_LINK,
            b"certs/ca-certificates.crt".to_vec(),
        );
        add(
            "/usr/share/licenses/ca-certificates/MPL-2.0.txt".into(),
            MODE_DATA,
            read(&certs.join("MPL-2.0.txt"))?,
        );
    }
    for (name, path) in listing(&assets.join("keymaps"), |n| n.ends_with(".layout"))? {
        add(
            format!("/usr/share/keymaps/{name}"),
            MODE_DATA,
            read(&path)?,
        );
    }
    for (name, path) in listing(&assets.join("completions"), |_| true)? {
        add(
            format!("/usr/share/shell/completions/{name}"),
            MODE_DATA,
            read(&path)?,
        );
    }
    if !extras.build_tag.is_empty() {
        add(
            "/usr/share/slopos/build-tag".into(),
            MODE_DATA,
            format!("{}\n", extras.build_tag).into_bytes(),
        );
    }
    Ok(entries)
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// The regular files of `dir` that `keep` accepts, by name; none when `dir`
/// is absent.
fn listing(dir: &Path, keep: impl Fn(&str) -> bool) -> Result<Vec<(String, PathBuf)>, String> {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut files = Vec::new();
    for entry in read_dir {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_file() && keep(&name) {
            files.push((name, path));
        }
    }
    files.sort();
    Ok(files)
}

/// Every regular file below `dir`, by its path there, in name order; an
/// absent `dir` is an error, since a recipe's licences must ship.
fn files_below(dir: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let mut out = Vec::new();
    let mut entries: Vec<_> = fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if path.is_dir() {
            for (rel, inner) in files_below(&path)? {
                out.push((format!("{name}/{rel}"), inner));
            }
        } else if path.is_file() {
            out.push((name, path));
        }
    }
    Ok(out)
}

/// Pack each `<dir>=<path>` tree below `path`, the directories on the way to
/// it included, every name sorted.
fn pack_trees(out: &Path, specs: &[String]) -> Result<(), String> {
    use std::io::{BufWriter, Write};
    let file = fs::File::create(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let mut writer = Sink {
        out: BufWriter::with_capacity(1 << 20, file),
        written: 0,
    };
    let mut seen = BTreeMap::new();
    for spec in specs {
        let (dir, at) = spec
            .split_once('=')
            .ok_or_else(|| format!("{spec}: not <dir>=<path>"))?;
        let at = format!("/{}", at.trim_matches('/'));
        let mut parent = String::new();
        for part in at.split('/').filter(|p| !p.is_empty()) {
            parent = format!("{parent}/{part}");
            if seen.insert(parent.clone(), MODE_DIR).is_none() {
                writer.record(&parent, MODE_DIR, &[])?;
            }
        }
        pack_dir(Path::new(dir), &at, &mut writer, &mut seen)?;
    }
    writer.record("TRAILER!!!", 0, &[])?;
    writer
        .out
        .flush()
        .map_err(|e| format!("{}: {e}", out.display()))?;
    println!(
        "initramfs: wrote {} ({} bytes, {} entries)",
        out.display(),
        writer.written,
        seen.len()
    );
    Ok(())
}

struct Sink<W> {
    out: W,
    written: u64,
}

impl<W: std::io::Write> Sink<W> {
    fn record(&mut self, name: &str, mode: u32, data: &[u8]) -> Result<(), String> {
        let name = name.trim_start_matches('/');
        if name.split('/').any(|part| part.len() > MAX_NAME_LEN) {
            return Err(format!("{name}: a component exceeds {MAX_NAME_LEN} bytes"));
        }
        let size = u32::try_from(data.len()).map_err(|_| format!("{name}: too large for newc"))?;
        let mut head = Vec::with_capacity(128 + name.len());
        emit_header(&mut head, name, mode, size);
        self.write(&head)?;
        self.write(data)?;
        self.write(&[0; 3][..(4 - data.len() % 4) % 4])
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.out.write_all(bytes).map_err(|e| e.to_string())?;
        self.written += bytes.len() as u64;
        Ok(())
    }
}

fn pack_dir<W: std::io::Write>(
    dir: &Path,
    at: &str,
    writer: &mut Sink<W>,
    seen: &mut BTreeMap<String, u32>,
) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut entries: Vec<_> = fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| format!("{}: a name that is not UTF-8", path.display()))?;
        let inside = format!("{at}/{name}");
        let meta = fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let perms = meta.permissions().mode() & 0o7777;
        let (mode, data) = if meta.file_type().is_symlink() {
            let target = fs::read_link(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let target = target
                .into_os_string()
                .into_string()
                .map_err(|_| format!("{}: a link target that is not UTF-8", path.display()))?;
            (MODE_LINK, target.into_bytes())
        } else if meta.is_dir() {
            (MODE_DIR & !0o7777 | perms, Vec::new())
        } else if meta.is_file() {
            (MODE_DATA & !0o7777 | perms, read(&path)?)
        } else {
            return Err(format!(
                "{}: neither a file, a directory nor a symlink",
                path.display()
            ));
        };
        if seen.insert(inside.clone(), mode).is_some() {
            return Err(format!("{inside} is packed twice"));
        }
        writer.record(&inside, mode, &data)?;
        if meta.is_dir() {
            pack_dir(&path, &inside, writer, seen)?;
        }
    }
    Ok(())
}

/// One `newc` record: the ASCII header, the NUL-terminated name and the body,
/// each padded to four bytes.
fn emit(archive: &mut Vec<u8>, name: &str, mode: u32, data: &[u8]) {
    emit_header(archive, name, mode, data.len() as u32);
    archive.extend_from_slice(data);
    pad(archive);
}

/// The record's header and name, padded to four bytes.
fn emit_header(archive: &mut Vec<u8>, name: &str, mode: u32, size: u32) {
    let start = archive.len();
    let fields = [
        0,
        mode,
        0,
        0,
        1,
        0,
        size,
        0,
        0,
        0,
        0,
        name.len() as u32 + 1,
        0,
    ];
    archive.extend_from_slice(b"070701");
    for field in fields {
        archive.extend_from_slice(format!("{field:08X}").as_bytes());
    }
    archive.extend_from_slice(name.as_bytes());
    archive.push(0);
    while (archive.len() - start) % 4 != 0 {
        archive.push(0);
    }
}

fn pad(archive: &mut Vec<u8>) {
    while archive.len() % 4 != 0 {
        archive.push(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("slopos-initramfs-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("root")).unwrap();
        fs::create_dir_all(dir.join("build")).unwrap();
        dir
    }

    /// Every record, as (name, mode, body), up to the trailer.
    fn records(archive: &[u8]) -> Vec<(String, u32, Vec<u8>)> {
        let field = |at: usize, i: usize| {
            let text = std::str::from_utf8(&archive[at + 6 + 8 * i..at + 14 + 8 * i]).unwrap();
            u32::from_str_radix(text, 16).unwrap() as usize
        };
        let align = |n: usize| n.div_ceil(4) * 4;
        let mut out = Vec::new();
        let mut at = 0;
        loop {
            assert_eq!(&archive[at..at + 6], b"070701");
            let (mode, size, name_len) = (field(at, 1), field(at, 6), field(at, 11));
            let name = std::str::from_utf8(&archive[at + 110..at + 110 + name_len - 1]).unwrap();
            let body = align(at + 110 + name_len);
            if name == "TRAILER!!!" {
                assert_eq!(align(body), archive.len());
                return out;
            }
            out.push((
                name.to_owned(),
                mode as u32,
                archive[body..body + size].to_vec(),
            ));
            at = align(body + size);
        }
    }

    #[test]
    fn packs_programs_after_the_empty_directories() {
        let dir = scratch("programs");
        fs::write(dir.join("build/init.elf"), b"\x7fELF init").unwrap();
        fs::write(dir.join("build/shell.elf"), b"\x7fELF shell").unwrap();
        let out = dir.join("out.cpio");
        let programs = ["init".to_owned(), "shell".to_owned()];
        let extras = Extras::default();
        run(
            &dir.join("root"),
            &out,
            &dir.join("build"),
            &programs,
            &extras,
        )
        .unwrap();
        let got = records(&fs::read(&out).unwrap());
        let names: Vec<&str> = got.iter().map(|(name, _, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "/etc",
                "/var",
                "/home",
                "/media",
                "/sbin/init",
                "/bin/shell",
                "/bin/sh"
            ]
        );
        assert_eq!(
            got[4],
            ("/sbin/init".to_owned(), MODE_EXEC, b"\x7fELF init".to_vec())
        );
        assert_eq!(got[6], ("/bin/sh".to_owned(), MODE_LINK, b"shell".to_vec()));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_same_inputs_pack_the_same_bytes() {
        let dir = scratch("determinism");
        fs::write(dir.join("build/init.elf"), b"init").unwrap();
        let programs = ["init".to_owned()];
        let (first, second) = (dir.join("a.cpio"), dir.join("b.cpio"));
        let extras = Extras::default();
        run(
            &dir.join("root"),
            &first,
            &dir.join("build"),
            &programs,
            &extras,
        )
        .unwrap();
        run(
            &dir.join("root"),
            &second,
            &dir.join("build"),
            &programs,
            &extras,
        )
        .unwrap();
        assert_eq!(fs::read(first).unwrap(), fs::read(second).unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// `scripts/build_fs_image.sh` mirrors `ROOT_DIRS`, `SLIBC_LICENSES` and
    /// `slopos_abi::fs::BASE_DIRS`.
    #[test]
    fn the_mirrored_lists_agree() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let script = fs::read_to_string(repo.join("scripts/build_fs_image.sh")).unwrap();
        let array = |name: &str| -> Vec<String> {
            let prefix = format!("{name}=(");
            let line = script
                .lines()
                .find_map(|l| l.strip_prefix(&prefix))
                .unwrap();
            line.trim_end_matches(')')
                .split_whitespace()
                .map(str::to_owned)
                .collect()
        };
        assert_eq!(array("ROOT_DIRS"), ROOT_DIRS);
        assert_eq!(array("SLIBC_LICENSES"), SLIBC_LICENSES);
        let abi = fs::read_to_string(repo.join("abi/src/fs.rs")).unwrap();
        let rest = &abi[abi.find("pub const BASE_DIRS").unwrap()..];
        let list = &rest[rest.find("= [").unwrap()..rest.find("];").unwrap()];
        let base_dirs: Vec<&str> = list.split('"').skip(1).step_by(2).collect();
        assert_eq!(array("BASE_DIRS"), base_dirs);
    }

    #[test]
    fn a_name_packed_twice_is_refused() {
        let dir = scratch("twice");
        fs::write(dir.join("build/shell.elf"), b"shell").unwrap();
        let extras = Extras {
            coreutils_links: "ls shell".into(),
            ..Extras::default()
        };
        let packed = run(
            &dir.join("root"),
            &dir.join("out.cpio"),
            &dir.join("build"),
            &["shell".to_owned()],
            &extras,
        );
        assert_eq!(packed, Err("/bin/shell is packed twice".into()));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn recipe_programs_keep_their_path_and_bring_their_licences() {
        let dir = scratch("recipes");
        let prefix = dir.join("prefix");
        fs::create_dir_all(prefix.join("sbin")).unwrap();
        fs::create_dir_all(prefix.join("share/licenses/tools/lib")).unwrap();
        fs::write(prefix.join("sbin/mkfs"), b"\x7fELF mkfs").unwrap();
        fs::write(prefix.join("share/licenses/tools/NOTICE"), b"notice").unwrap();
        fs::write(prefix.join("share/licenses/tools/lib/COPYING"), b"copying").unwrap();
        let extras = Extras {
            recipe_prefix: prefix.to_string_lossy().into_owned(),
            recipe_programs: "sbin/mkfs".into(),
            recipe_licenses: "tools".into(),
            ..Extras::default()
        };
        let out = dir.join("out.cpio");
        run(&dir.join("root"), &out, &dir.join("build"), &[], &extras).unwrap();
        let got = records(&fs::read(&out).unwrap());
        let find = |name: &str| got.iter().find(|(n, _, _)| n == name).map(|r| r.2.clone());
        assert_eq!(find("/sbin/mkfs").as_deref(), Some(&b"\x7fELF mkfs"[..]));
        assert_eq!(
            find("/usr/share/licenses/tools/lib/COPYING").as_deref(),
            Some(&b"copying"[..])
        );
        assert!(find("/usr/share/licenses/tools/NOTICE").is_some());
        let missing = Extras {
            recipe_programs: "sbin/gone".into(),
            ..extras
        };
        assert!(run(&dir.join("root"), &out, &dir.join("build"), &[], &missing).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_tree_packs_below_its_path_with_its_modes_and_links() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = scratch("tree");
        let tree = dir.join("root/local");
        fs::create_dir_all(tree.join("bin")).unwrap();
        fs::write(tree.join("bin/tool"), b"tool").unwrap();
        fs::set_permissions(tree.join("bin/tool"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink("tool", tree.join("bin/alias")).unwrap();
        let out = dir.join("medium.cpio");
        pack_trees(&out, &[format!("{}=usr/local", tree.display())]).unwrap();
        let got = records(&fs::read(&out).unwrap());
        let names: Vec<&str> = got.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "usr",
                "usr/local",
                "usr/local/bin",
                "usr/local/bin/alias",
                "usr/local/bin/tool"
            ]
        );
        assert_eq!(
            got[3],
            ("usr/local/bin/alias".into(), MODE_LINK, b"tool".to_vec())
        );
        assert_eq!(got[4].1, 0o100_755);
        assert_eq!(got[4].2, b"tool");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_named_shared_object_must_exist() {
        let dir = scratch("objects");
        let extras = Extras {
            shared_objects: "libgone.so".into(),
            ..Extras::default()
        };
        let packed = run(
            &dir.join("root"),
            &dir.join("out.cpio"),
            &dir.join("build"),
            &[],
            &extras,
        );
        assert!(packed.is_err_and(|e| e.contains("libgone.so")));
        fs::remove_dir_all(&dir).unwrap();
    }
}
