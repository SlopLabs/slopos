use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const FONT_DIR: &str = "/usr/share/fonts";

static ELSEWHERE: OnceLock<PathBuf> = OnceLock::new();

/// Load fonts from `dir` rather than `/usr/share/fonts`, as a host tool
/// rendering what the desktop draws does; only before any text is drawn.
pub fn fonts_in(dir: impl Into<PathBuf>) {
    let _ = ELSEWHERE.set(dir.into());
}

const FONT_MAP: &[(&str, &str)] = &[
    ("mono", "JetBrainsMono-Regular.ttf"),
    ("sans", "Inter-Regular.ttf"),
    ("sans-semibold", "Inter-SemiBold.ttf"),
    ("icons", "Lucide.ttf"),
];

/// The face `name` (`mono`, `sans`, `sans-semibold` or `icons`), read once
/// and kept for the life of the process.
pub fn load_font(name: &str) -> Option<&'static [u8]> {
    static FACES: [OnceLock<Option<&'static [u8]>>; FONT_MAP.len()] =
        [const { OnceLock::new() }; FONT_MAP.len()];
    let at = FONT_MAP.iter().position(|(k, _)| *k == name)?;
    *FACES[at].get_or_init(|| {
        let dir = ELSEWHERE
            .get()
            .map_or(Path::new(FONT_DIR), PathBuf::as_path);
        let data = std::fs::read(dir.join(FONT_MAP[at].1)).ok()?;
        (!data.is_empty()).then(|| &*Box::leak(data.into_boxed_slice()))
    })
}
