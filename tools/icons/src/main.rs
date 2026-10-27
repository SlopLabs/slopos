//! The app icons the system and the apps ship, rendered from appkit's
//! template at 64 px and, as `<id>@2x.png`, at 128 px. Its host test holds
//! every shipped icon to the template's pixels.

mod png;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use slopos_abi::draw::Color32;
use slopos_appkit::app_icon;
use slopos_theme_core::Icon;

/// Where each icon ships, its app's id, its glyph and its series colour.
const ICONS: [(&str, &str, Icon, usize); 6] = [
    (
        "apps/share/slopos/icons",
        "org.slopos.files",
        Icon::Folder,
        0,
    ),
    (
        "apps/share/slopos/icons",
        "org.slopos.editor",
        Icon::Pencil,
        1,
    ),
    (
        "apps/share/slopos/icons",
        "org.slopos.image-viewer",
        Icon::Image,
        2,
    ),
    ("assets/icons", "org.slopos.terminal", Icon::Terminal, 4),
    (
        "assets/icons",
        "org.slopos.installer",
        Icon::HardDriveDownload,
        5,
    ),
    ("assets/icons", "org.slopos.sysmon", Icon::Activity, 6),
];

/// What each of `app_icon::SIZES` adds to an icon's file name.
const SUFFIXES: [&str; 2] = ["", "@2x"];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every icon: where it ships and the template's pixels for it.
fn rendered() -> impl Iterator<Item = (PathBuf, u32, Vec<Color32>)> {
    let fonts = root().join("assets/fonts").to_string_lossy().into_owned();
    slopos_appkit::text::fonts_in(Box::leak(fonts.into_boxed_str()));
    ICONS.into_iter().flat_map(|(dir, id, icon, tint)| {
        app_icon::SIZES
            .into_iter()
            .zip(SUFFIXES)
            .map(move |(size, suffix)| {
                let path = root().join(dir).join(format!("{id}{suffix}.png"));
                (path, size, app_icon::render(size, tint, icon))
            })
    })
}

fn main() -> ExitCode {
    for (path, size, pixels) in rendered() {
        if let Err(e) = std::fs::write(&path, png::encode(size, size, &pixels)) {
            eprintln!("icons: {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use slopos_image::DecodeOptions;

    fn decoded(bytes: &[u8]) -> Option<Vec<Color32>> {
        Some(
            slopos_image::decode(bytes, DecodeOptions::default())
                .ok()?
                .pixels,
        )
    }

    #[test]
    fn every_shipped_icon_is_the_template_s() {
        let drifted: Vec<PathBuf> = rendered()
            .filter(|(path, _, pixels)| {
                std::fs::read(path).ok().and_then(|b| decoded(&b)).as_ref() != Some(pixels)
            })
            .map(|(path, ..)| path)
            .collect();
        assert!(
            drifted.is_empty(),
            "not the template's; run `just icons`: {drifted:?}"
        );
    }

    #[test]
    fn an_icon_one_pixel_off_is_not_the_template_s() {
        let (_, size, pixels) = rendered().next().unwrap();
        assert_eq!(
            decoded(&png::encode(size, size, &pixels)),
            Some(pixels.clone())
        );
        let mut off = pixels.clone();
        let middle = off.len() / 2;
        off[middle].0 ^= 1;
        assert_ne!(decoded(&png::encode(size, size, &off)), Some(pixels));
    }
}
