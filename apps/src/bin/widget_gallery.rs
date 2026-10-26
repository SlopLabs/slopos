use slopos_appkit::catalogue::{Catalogue, PAGES};

/// `--page <name>` opens on that page of the catalogue.
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let page = match args.as_slice() {
        [flag, name] if flag == "--page" => PAGES.iter().position(|p| p == name),
        [] => Some(0),
        _ => None,
    };
    let Some(page) = page else {
        eprintln!("usage: widget_gallery [--page {}]", PAGES.join("|"));
        std::process::exit(2);
    };
    slopos_appkit::run_app(Catalogue::new(page));
}
