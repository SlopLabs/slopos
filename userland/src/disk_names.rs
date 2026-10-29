use slopos_boot_core::disk_id::{self, Bus};

const BY_ID: &str = "/dev/disk/by-id";

/// The bus the whole disk `node` is on and what its maker calls it.
pub fn by_id(node: &str) -> (Option<Bus>, Option<String>) {
    let target = format!("../../{}", slopos_installer_core::short(node));
    let names: Vec<String> = std::fs::read_dir(BY_ID)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| {
            std::fs::read_link(entry.path()).is_ok_and(|link| link.as_os_str() == target.as_str())
        })
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    let Some(described) = disk_id::describe(names.iter().map(String::as_str)) else {
        return (None, None);
    };
    let words: Vec<&str> = described.model().collect();
    (
        Some(described.bus),
        (!words.is_empty()).then(|| words.join(" ")),
    )
}
