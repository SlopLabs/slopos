use std::sync::OnceLock;

const FONT_DIR: &str = "/usr/share/fonts";

static ELSEWHERE: OnceLock<&'static str> = OnceLock::new();

pub fn fonts_in(dir: &'static str) {
    let _ = ELSEWHERE.set(dir);
}

const FONT_MAP: &[(&str, &str)] = &[
    ("mono", "JetBrainsMono-Regular.ttf"),
    ("mono-bold", "JetBrainsMono-Bold.ttf"),
    ("sans", "Inter-Regular.ttf"),
    ("sans-semibold", "Inter-SemiBold.ttf"),
    ("icons", "Lucide.ttf"),
];

pub fn load_font(name: &str) -> Option<&'static [u8]> {
    let filename = FONT_MAP
        .iter()
        .find(|(k, _)| *k == name)
        .map(|(_, v)| *v)
        .unwrap_or(name);

    let dir = ELSEWHERE.get().copied().unwrap_or(FONT_DIR);
    let path = std::format!("{dir}/{filename}");
    let data = std::fs::read(&path).ok()?;
    if data.is_empty() {
        return None;
    }
    // Intentional leak: font data must be 'static and is loaded once per session.
    Some(std::boxed::Box::leak(data.into_boxed_slice()))
}
