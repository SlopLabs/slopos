//! Renders the app icons the system and the apps ship from appkit's template,
//! at 64 px and as `<id>@2x.png` at 128 px; its test holds them to it.

mod png;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use slopos_abi::draw::Color32;
use slopos_appkit::app_icon;
use slopos_theme_core::Icon;

struct Shipped {
    dir: &'static str,
    id: &'static str,
    glyph: Icon,
    tint: usize,
}

const ICONS: [Shipped; 6] = [
    Shipped {
        dir: "apps/share/slopos/icons",
        id: "org.slopos.files",
        glyph: Icon::Folder,
        tint: 0,
    },
    Shipped {
        dir: "apps/share/slopos/icons",
        id: "org.slopos.editor",
        glyph: Icon::Pencil,
        tint: 1,
    },
    Shipped {
        dir: "apps/share/slopos/icons",
        id: "org.slopos.image-viewer",
        glyph: Icon::Image,
        tint: 2,
    },
    Shipped {
        dir: "assets/icons",
        id: "org.slopos.terminal",
        glyph: Icon::Terminal,
        tint: 4,
    },
    Shipped {
        dir: "assets/icons",
        id: "org.slopos.installer",
        glyph: Icon::HardDriveDownload,
        tint: 5,
    },
    Shipped {
        dir: "assets/icons",
        id: "org.slopos.sysmon",
        glyph: Icon::Activity,
        tint: 6,
    },
];

/// What each of `app_icon::SIZES` adds to an icon's file name.
const SUFFIXES: [&str; 2] = ["", "@2x"];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every icon: where it ships and the template's pixels for it, which a
/// missing icon font would leave glyphless.
fn rendered() -> Result<impl Iterator<Item = (PathBuf, u32, Vec<Color32>)>, String> {
    let fonts = root().join("assets/fonts");
    slopos_appkit::text::fonts_in(&fonts);
    if slopos_appkit::text::load_font("icons").is_none() {
        return Err(format!("no icon font in {}", fonts.display()));
    }
    Ok(ICONS.into_iter().flat_map(|icon| {
        app_icon::SIZES
            .into_iter()
            .zip(SUFFIXES)
            .map(move |(size, suffix)| {
                let path = root()
                    .join(icon.dir)
                    .join(format!("{}{suffix}.png", icon.id));
                (path, size, app_icon::render(size, icon.tint, icon.glyph))
            })
    }))
}

fn main() -> ExitCode {
    let icons = match rendered() {
        Ok(icons) => icons,
        Err(e) => {
            eprintln!("icons: {e}");
            return ExitCode::FAILURE;
        }
    };
    for (path, size, pixels) in icons {
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
            .unwrap()
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
        let (_, size, pixels) = rendered().unwrap().next().unwrap();
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
