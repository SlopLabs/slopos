//! `just icons`: renders the shipped app icons; its tests hold them to it.

mod art;
mod png;
mod raster;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use slopos_abi::draw::Color32;

use art::Art;

struct Shipped {
    dir: &'static str,
    id: &'static str,
    art: Art,
}

const ICONS: [Shipped; 6] = [
    Shipped {
        dir: "apps/share/slopos/icons",
        id: "org.slopos.files",
        art: Art::Files,
    },
    Shipped {
        dir: "apps/share/slopos/icons",
        id: "org.slopos.editor",
        art: Art::Editor,
    },
    Shipped {
        dir: "apps/share/slopos/icons",
        id: "org.slopos.image-viewer",
        art: Art::ImageViewer,
    },
    Shipped {
        dir: "assets/icons",
        id: "org.slopos.terminal",
        art: Art::Terminal,
    },
    Shipped {
        dir: "assets/icons",
        id: "org.slopos.installer",
        art: Art::Installer,
    },
    Shipped {
        dir: "assets/icons",
        id: "org.slopos.sysmon",
        art: Art::Sysmon,
    },
];

const SIZES: [(u32, &str); 2] = [(64, ""), (128, "@2x")];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn rendered() -> impl Iterator<Item = (PathBuf, u32, Vec<Color32>)> {
    ICONS.into_iter().flat_map(|icon| {
        SIZES.into_iter().map(move |(size, suffix)| {
            let path = root()
                .join(icon.dir)
                .join(format!("{}{suffix}.png", icon.id));
            (path, size, art::render(icon.art, size))
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
    fn every_shipped_icon_is_its_artwork_s() {
        let drifted: Vec<PathBuf> = rendered()
            .filter(|(path, _, pixels)| {
                std::fs::read(path).ok().and_then(|b| decoded(&b)).as_ref() != Some(pixels)
            })
            .map(|(path, ..)| path)
            .collect();
        assert!(
            drifted.is_empty(),
            "not the artwork's; run `just icons`: {drifted:?}"
        );
    }

    #[test]
    fn an_icon_one_pixel_off_is_not_its_artwork_s() {
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
