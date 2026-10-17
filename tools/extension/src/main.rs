//! `extension <dir> <out-dir> <libc.so>` — packs the extension laid out in
//! `<dir>` into `<out-dir>/<name>.ext` and its sidecar `<name>.meta`, once it
//! checks out as `extctl` checks an install: the image, and its programs
//! against `<libc.so>`, the one they will bind against. Prints what it wrote
//! and the grant a first install asks the person for.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use slopos_extension_core::image::Image;
use slopos_extension_core::{source, symbols};

fn run(dir: &Path, out: &Path, libc: &Path) -> Result<String, String> {
    let (image, sidecar) = source::pack(dir)?;
    let checked = Image::check(&image).map_err(|e| e.to_string())?;
    let libc_bytes = fs::read(libc).map_err(|e| format!("{}: {e}", libc.display()))?;
    symbols::check_image(&checked, &libc_bytes).map_err(|e| e.to_string())?;
    fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let at = |ext: &str| out.join(format!("{}.{ext}", sidecar.name));
    let (image_at, sidecar_at) = (at("ext"), at("meta"));
    fs::write(&image_at, &image).map_err(|e| format!("{}: {e}", image_at.display()))?;
    fs::write(&sidecar_at, sidecar.render())
        .map_err(|e| format!("{}: {e}", sidecar_at.display()))?;
    Ok(format!(
        "name {}\nversion {}\nimage {}\nsidecar {}\n{}\n",
        sidecar.name,
        sidecar.version,
        image_at.display(),
        sidecar_at.display(),
        checked.declaration.grant().words().join(" ")
    ))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [dir, out, libc] = args.as_slice() else {
        eprintln!("usage: extension <dir> <out-dir> <libc.so>");
        return ExitCode::from(2);
    };
    match run(Path::new(dir), Path::new(out), Path::new(libc)) {
        Ok(said) => {
            print!("{said}");
            ExitCode::SUCCESS
        }
        Err(why) => {
            eprintln!("extension: {dir}: {why}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slopos_extension_core::elf::{Kind, build};
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_laid_out_extension_packs_checked_against_the_libc_named() {
        let dir = std::env::temp_dir().join(format!("slopos-extension-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src/.extension")).unwrap();
        fs::create_dir_all(dir.join("src/bin")).unwrap();
        fs::write(
            dir.join("src/.extension/release"),
            "name hello\nversion 4\n",
        )
        .unwrap();
        fs::write(dir.join("src/.extension/declare"), "command hello\n").unwrap();
        let program = |imports: &[&str]| {
            build(
                Kind::Executable,
                Some("/lib/ld-slopos.so.1"),
                &["libc.so"],
                imports,
                &[],
            )
        };
        let hello = dir.join("src/bin/hello");
        fs::write(&hello, program(&["puts"])).unwrap();
        fs::set_permissions(&hello, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            dir.join("libc.so"),
            build(Kind::Shared, None, &[], &[], &["puts"]),
        )
        .unwrap();
        let said = run(&dir.join("src"), &dir.join("out"), &dir.join("libc.so")).unwrap();
        assert!(said.starts_with("name hello\nversion 4\n"), "{said}");
        assert!(
            said.ends_with("grant - unasked - users - commands hello\n"),
            "{said}"
        );
        let image = fs::read(dir.join("out/hello.ext")).unwrap();
        assert_eq!(Image::check(&image).unwrap().release.version, 4);
        assert!(
            fs::read_to_string(dir.join("out/hello.meta"))
                .unwrap()
                .starts_with("name hello\n")
        );
        fs::write(&hello, program(&["puts", "absent"])).unwrap();
        let why = run(&dir.join("src"), &dir.join("out"), &dir.join("libc.so")).unwrap_err();
        assert!(why.contains("binds absent"), "{why}");
        let _ = fs::remove_dir_all(&dir);
    }
}
