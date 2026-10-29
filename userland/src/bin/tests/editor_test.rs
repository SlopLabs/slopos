//! `editor-core`'s cases on the target's allocator, then the editor driven by
//! the messages its widgets emit, with no compositor in the path.

use slopos_userland as _;

use slopos_appkit::traits::Role;
use slopos_appkit::widgets::code_view::CodeInput;
use slopos_appkit::widgets::editor_tabs::TabInput;
use slopos_appkit::widgets::tree_view::TreeInput;
use slopos_appkit::{App, DragInput, Key, LineEditInput, Modifiers, NamedKey};
use slopos_apps::editor::{Answer, Command, EditorApp, EditorMsg, MIN_CODE_WIDTH, said};

const DIR: &str = "/home/editor_test";

fn setup() -> bool {
    let _ = std::fs::create_dir(DIR);
    std::fs::write(
        format!("{DIR}/sample.rs"),
        "fn main() {\n    let answer = 42;\n    println!(\"{answer}\");\n}\n",
    )
    .is_ok()
        && std::fs::write(format!("{DIR}/notes.md"), "# Notes\n\nplain text\n").is_ok()
}

fn app_with(path: &str) -> EditorApp {
    EditorApp::new(&[String::from(path)])
}

fn key(app: &mut EditorApp, key: Key, mods: Modifiers) {
    app.on_key(key, mods);
}

fn ctrl() -> Modifiers {
    Modifiers {
        ctrl: true,
        ..Modifiers::default()
    }
}

fn ctrl_shift() -> Modifiers {
    Modifiers {
        ctrl: true,
        shift: true,
        ..Modifiers::default()
    }
}

fn type_text(app: &mut EditorApp, text: &str) {
    for c in text.chars() {
        app.update(EditorMsg::Code(CodeInput::Text { character: c }));
    }
}

fn opens_a_file_argument() -> bool {
    let app = app_with(&format!("{DIR}/sample.rs"));
    let doc = &app.documents()[app.active_index()];
    doc.title() == "sample.rs" && !doc.is_modified() && doc.buffer.line_count() == 4
}

fn populates_the_sidebar() -> bool {
    let app = app_with(&format!("{DIR}/sample.rs"));
    let rows = app.tree_rows(0, 32);
    let names: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    names.contains(&"sample.rs") && names.contains(&"notes.md")
}

fn types_and_saves() -> bool {
    let path = format!("{DIR}/typed.txt");
    let _ = std::fs::remove_file(&path);
    let mut app = app_with(DIR);
    type_text(&mut app, "hello");
    if !app.documents()[app.active_index()].is_modified() {
        return false;
    }
    key(&mut app, Key::Char('s'), ctrl_shift());
    app.clear_prompt();
    app.type_in_prompt(&path);
    key(&mut app, Key::Named(NamedKey::Enter), Modifiers::default());

    let written = std::fs::read_to_string(&path).unwrap_or_default();
    written == "hello\n" && !app.documents()[app.active_index()].is_modified()
}

fn reopening_switches_rather_than_duplicating() -> bool {
    let mut app = app_with(&format!("{DIR}/sample.rs"));
    let before = app.documents().len();
    app.update(EditorMsg::Run(Command::NewFile));
    let with_scratch = app.documents().len();
    app.open(&format!("{DIR}/sample.rs"));
    before == 1 && with_scratch == 2 && app.documents().len() == 2 && app.active_index() == 0
}

fn finds_and_counts() -> bool {
    let mut app = app_with(&format!("{DIR}/sample.rs"));
    key(&mut app, Key::Char('f'), ctrl());
    app.type_in_prompt("answer");
    let (total, _) = app.find_progress().unwrap_or((0, 0));
    if total != 2 {
        return false;
    }
    key(&mut app, Key::Named(NamedKey::Enter), Modifiers::default());
    let first = app.documents()[app.active_index()].cursor.position.line;
    key(&mut app, Key::Named(NamedKey::Enter), Modifiers::default());
    let second = app.documents()[app.active_index()].cursor.position.line;
    first == 1 && second == 2
}

fn palette_filters_and_runs() -> bool {
    let mut app = app_with(&format!("{DIR}/sample.rs"));
    key(&mut app, Key::Char('p'), ctrl_shift());
    app.type_in_prompt("toggle sideb");
    let visible_before = app.sidebar_visible();
    key(&mut app, Key::Named(NamedKey::Enter), Modifiers::default());
    app.sidebar_visible() != visible_before
}

fn a_picked_palette_row_runs() -> bool {
    let mut app = app_with(&format!("{DIR}/sample.rs"));
    key(&mut app, Key::Char('p'), ctrl_shift());
    app.type_in_prompt("toggle sideb");
    let visible_before = app.sidebar_visible();
    app.update(EditorMsg::Pick(0));
    !app.prompt_is_open() && app.sidebar_visible() != visible_before
}

/// More files than the finder shows at once, in a folder of their own.
fn many_files() -> Option<String> {
    let many = format!("{DIR}/many");
    let _ = std::fs::create_dir(&many);
    (0..20)
        .all(|i| std::fs::write(format!("{many}/file{i:02}.txt"), "x\n").is_ok())
        .then_some(many)
}

/// The finder scrolled past its first row: what row 1 shows, and the app.
fn finder_scrolled() -> Option<(EditorApp, String)> {
    let mut app = app_with(&many_files()?);
    key(&mut app, Key::Char('p'), ctrl());
    for _ in 0..app.overlay_rows() {
        key(&mut app, Key::Named(NamedKey::Down), Modifiers::default());
    }
    let first = app.overlay_first_row();
    let second = app.finder_results().get(first + 1).cloned()?;
    (first > 0).then_some((app, second))
}

fn active_path(app: &EditorApp) -> Option<&str> {
    app.documents()[app.active_index()].path()
}

fn a_picked_row_counts_from_the_first_shown() -> bool {
    let Some((mut app, second)) = finder_scrolled() else {
        return false;
    };
    app.update(EditorMsg::Pick(1));
    !app.prompt_is_open() && active_path(&app) == Some(second.as_str())
}

fn a_click_opens_the_row_under_it() -> bool {
    let Some((mut app, second)) = finder_scrolled() else {
        return false;
    };
    let shown = app
        .finder_results()
        .len()
        .saturating_sub(app.overlay_first_row())
        .min(app.overlay_rows());
    let Some(list) = dispatch::overlay_list(&app, dispatch::WINDOW) else {
        return false;
    };
    let row = list.height / shown.max(1) as i32;
    dispatch::click(&mut app, list.x + list.width / 2, list.y + row + row / 2);
    !app.prompt_is_open() && active_path(&app) == Some(second.as_str())
}

fn a_row_past_the_results_opens_nothing() -> bool {
    let Some(many) = many_files() else {
        return false;
    };
    let mut app = app_with(&many);
    key(&mut app, Key::Char('p'), ctrl());
    app.type_in_prompt("file00");
    let shown = app.finder_results().len();
    app.update(EditorMsg::Pick(shown));
    app.prompt_is_overlay() && active_path(&app).is_none()
}

fn the_overlays_fit_the_least_window() -> bool {
    let Some(many) = many_files() else {
        return false;
    };
    let mut app = app_with(&many);
    let least = app.window().min_size;
    app.on_resize(least.width as u32, least.height as u32);
    for chord in [ctrl(), ctrl_shift()] {
        key(&mut app, Key::Char('p'), chord);
        // A card too tall for the window would be moved up to fit it.
        let (Some(fitted), Some(roomy)) = (
            dispatch::overlay_list(&app, least),
            dispatch::overlay_list(&app, dispatch::WINDOW),
        ) else {
            return false;
        };
        if fitted.y != roomy.y || fitted.y + fitted.height > least.height {
            eprintln!("editor_test: the list is at {fitted:?}, not below {roomy:?}");
            return false;
        }
        key(&mut app, Key::Named(NamedKey::Escape), Modifiers::default());
    }
    true
}

fn tab_moves_the_finder_s_choice() -> bool {
    let Some(many) = many_files() else {
        return false;
    };
    let mut app = app_with(&many);
    key(&mut app, Key::Char('p'), ctrl());
    let rows = app.overlay_rows();
    for _ in 0..rows {
        dispatch::press(&mut app, Key::Named(NamedKey::Tab), Modifiers::default());
    }
    if app.overlay_first_row() != 1 {
        return false;
    }
    let shift = Modifiers {
        shift: true,
        ..Modifiers::default()
    };
    for _ in 0..rows {
        dispatch::press(&mut app, Key::Named(NamedKey::Tab), shift);
    }
    dispatch::text(&mut app, 'f');
    app.overlay_first_row() == 0 && app.prompt_text() == "f"
}

fn close_asks_before_discarding() -> bool {
    let mut app = app_with(&format!("{DIR}/sample.rs"));
    type_text(&mut app, "x");
    app.update(EditorMsg::Tab(TabInput::Close(0)));
    if app.dialog().is_none() {
        return false;
    }
    app.update(EditorMsg::Dialog(Answer::Cancel));
    app.dialog().is_none() && app.documents().len() == 1
}

fn tree_expands_a_directory() -> bool {
    // Its own fixture, not whichever entry another folder happens to list first.
    let nested = format!("{DIR}/nested");
    let _ = std::fs::create_dir(&nested);
    if std::fs::write(format!("{nested}/inner.txt"), "x\n").is_err() {
        return false;
    }
    let mut app = app_with(DIR);
    let rows = app.tree_rows(0, 64);
    let Some(row) = rows.iter().position(|r| r.0 == "nested") else {
        return false;
    };
    let before = app.tree_rows(0, 64).len();
    app.update(EditorMsg::Tree(TreeInput::Activate { row }));
    let after = app.tree_rows(0, 64);
    after.len() > before && after.iter().any(|r| r.0 == "inner.txt") && app.documents().len() == 1
}

fn an_unopenable_argument_still_starts() -> bool {
    let missing = format!("{DIR}/does-not-exist.txt");
    let app = EditorApp::new(&[missing.clone()]);
    if app.documents().len() != 1 || !app.status().contains("does-not-exist") {
        return false;
    }
    let _ = app.view();

    let binary = format!("{DIR}/blob.bin");
    if std::fs::write(&binary, [0x7f, b'E', b'L', b'F', 0, 1, 2, 0]).is_err() {
        return false;
    }
    let app = EditorApp::new(&[binary.clone()]);
    app.documents().len() == 1 && app.status() == said::not_text(&binary)
}

fn find_next_works_with_the_bar_closed() -> bool {
    let mut app = app_with(&format!("{DIR}/sample.rs"));
    key(&mut app, Key::Char('f'), ctrl());
    app.type_in_prompt("answer");
    key(&mut app, Key::Named(NamedKey::Escape), Modifiers::default());
    app.clear_status();
    key(&mut app, Key::Named(NamedKey::F3), Modifiers::default());
    if app.status() == said::nothing_to_find(false) {
        return false;
    }
    if app.documents()[0].selected_text().as_deref() != Some("answer") {
        return false;
    }
    // Re-opening the bar brings the query back rather than an empty field.
    key(&mut app, Key::Char('f'), ctrl());
    app.prompt_text() == "answer"
}

fn opening_over_the_scratch_buffer_cancels_save_as() -> bool {
    let mut app = EditorApp::new(&[String::from(DIR)]);
    if app.documents().len() != 1 || app.documents()[0].path().is_some() {
        return false;
    }
    key(&mut app, Key::Char('s'), ctrl_shift());
    if !app.prompt_is_open() {
        return false;
    }
    app.open(&format!("{DIR}/sample.rs"));
    // The prompt named index 0, and index 0 is a different document now.
    !app.prompt_is_open() && app.documents()[0].path().is_some()
}

fn finder_reaches_unexpanded_directories() -> bool {
    let deep = format!("{DIR}/deep/deeper");
    let _ = std::fs::create_dir(format!("{DIR}/deep"));
    let _ = std::fs::create_dir(&deep);
    if std::fs::write(format!("{deep}/buried.rs"), "fn buried() {}\n").is_err() {
        return false;
    }
    let mut app = app_with(DIR);
    if app.tree_rows(0, 64).iter().any(|r| r.0 == "buried.rs") {
        return false;
    }
    app.update(EditorMsg::Run(Command::FileFinder));
    app.type_in_prompt("buried");
    app.finder_results()
        .iter()
        .any(|p| p.ends_with("buried.rs"))
}

fn refuses_a_binary_file() -> bool {
    // A fixture: a program could be refused for its size instead.
    let path = format!("{DIR}/blob.bin");
    if std::fs::write(&path, [0x7f, b'E', b'L', b'F', 0x00, 0x01, 0x02, 0x00]).is_err() {
        return false;
    }
    let mut app = app_with(DIR);
    let before = app.documents().len();
    app.open(&path);
    app.documents().len() == before && app.status() == said::not_text(&path)
}

fn the_title_names_the_document() -> bool {
    let mut app = app_with(&format!("{DIR}/sample.rs"));
    let first = app.title() == format!("sample.rs — {}", said::NAME);
    app.open(&format!("{DIR}/notes.md"));
    first && app.title() == format!("notes.md — {}", said::NAME)
}

fn every_error_says_what_to_do_next() -> bool {
    use std::io::ErrorKind;
    let path = "/home/user/notes.txt";
    let errors = [
        said::unreadable(path, ErrorKind::NotFound),
        said::unreadable(path, ErrorKind::Other),
        said::unlisted(path, ErrorKind::Other),
        said::folder(path),
        said::too_large(path),
        said::not_text(path),
        said::too_many_lines(path),
        said::unsaved(path, ErrorKind::StorageFull),
        said::unsaved(path, ErrorKind::Other),
        said::open_elsewhere(path),
        said::not_a_folder(path),
        said::line_limit(),
        said::not_a_line("x", 4),
        said::not_a_line("", 4),
        said::index_full(64),
        said::no_manual(),
        said::unnamed_left(),
        said::tree_full(64),
        said::TAB_GONE.into(),
        said::NOT_SAVED_YET.into(),
        said::nothing_to_find(false),
        said::nothing_to_find(true),
        said::nothing_to_replace(false),
        said::nothing_to_replace(true),
        said::no_replacement(),
    ];
    errors.iter().all(|said| {
        let sentences = said.split(". ").count();
        let kept = slopos_theme_core::words::sentence(said, &[]).is_ok() && sentences >= 2;
        if !kept {
            eprintln!("editor_test: {said:?}");
        }
        kept
    })
}

/// A refusal says what was refused and why, a sentence for each cause; a
/// save says where else to save as well.
fn every_refusal_says_what_was_refused_and_why() -> bool {
    use slopos_appkit::Refusal;
    use std::collections::BTreeSet;
    let path = "/home/user/notes.txt";
    let mut kept = true;
    let mut said_once = |said: String, sentences: usize, seen: &mut BTreeSet<String>| {
        let holds = slopos_theme_core::words::sentence(&said, &[]).is_ok()
            && said.split(". ").count() == sentences;
        if !holds {
            eprintln!("editor_test: {said:?}");
        }
        kept &= holds;
        seen.insert(said);
    };
    let (mut opened, mut listed, mut saved) = (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
    for refusal in Refusal::ALL {
        said_once(said::refused_open(path, refusal), 1, &mut opened);
        said_once(said::refused_listing(path, refusal), 1, &mut listed);
        said_once(said::refused_save(path, refusal), 2, &mut saved);
    }
    // Only permissions refuse a read, so a read names three causes.
    kept && opened.len() == 3 && listed.len() == 3 && saved.len() == Refusal::ALL.len()
}

fn every_passing_status_is_a_label() -> bool {
    let path = "/home/user/notes.txt";
    let notes = [
        said::opened(path),
        said::saved(path),
        said::CUT.into(),
        said::COPIED.into(),
        said::EMPTY_CLIPBOARD.into(),
        said::NOTHING_TO_UNDO.into(),
        said::NOTHING_TO_REDO.into(),
        said::no_comments("Markdown"),
        said::no_match("answer"),
        said::replaced(1),
        said::replaced(2),
    ];
    notes.iter().all(|note| {
        let kept = slopos_theme_core::words::label(note, &[]).is_ok();
        if !kept {
            eprintln!("editor_test: {note:?}");
        }
        kept
    })
}

fn replace_all_says_what_it_needs() -> bool {
    let mut app = app_with(&format!("{DIR}/sample.rs"));
    app.update(EditorMsg::Run(Command::ReplaceAll));
    if app.status() != said::nothing_to_replace(false) {
        return false;
    }
    key(&mut app, Key::Char('h'), ctrl());
    app.update(EditorMsg::Run(Command::ReplaceAll));
    if app.status() != said::nothing_to_replace(true) {
        return false;
    }
    key(&mut app, Key::Named(NamedKey::Escape), Modifiers::default());
    key(&mut app, Key::Char('f'), ctrl());
    app.type_in_prompt("answer");
    app.update(EditorMsg::Run(Command::ReplaceAll));
    if app.status() != said::no_replacement() {
        return false;
    }
    key(&mut app, Key::Char('h'), ctrl());
    for character in "reply".chars() {
        app.update(EditorMsg::PromptReplace(LineEditInput::Text { character }));
    }
    app.update(EditorMsg::Run(Command::ReplaceAll));
    if app.status() != said::replaced(2) {
        return false;
    }
    app.update(EditorMsg::Run(Command::ReplaceAll));
    app.status() == said::no_match("answer")
}

fn an_empty_find_bar_asks_only_for_typing() -> bool {
    let mut app = app_with(&format!("{DIR}/sample.rs"));
    key(&mut app, Key::Char('f'), ctrl());
    app.update(EditorMsg::Run(Command::FindNext));
    if app.status() != said::nothing_to_find(true) {
        return false;
    }
    app.clear_status();
    dispatch::press(&mut app, Key::Named(NamedKey::Enter), Modifiers::default());
    if app.status() != said::nothing_to_find(true) {
        return false;
    }
    key(&mut app, Key::Named(NamedKey::Escape), Modifiers::default());
    app.update(EditorMsg::Run(Command::FindNext));
    app.status() == said::nothing_to_find(false)
}

fn switching_to_an_open_file_fits_the_window() -> bool {
    let sample = format!("{DIR}/sample.rs");
    let mut app = app_with(&sample);
    app.open(&format!("{DIR}/notes.md"));
    let least = app.window().min_size;
    app.on_resize(least.width as u32, least.height as u32);
    let fitted = app.documents()[app.active_index()].viewport.visible_lines;
    app.open(&sample);
    let doc = &app.documents()[app.active_index()];
    doc.path() == Some(sample.as_str()) && doc.viewport.visible_lines == fitted
}

fn the_divider_stays_under_the_pointer() -> bool {
    let mut app = app_with(&format!("{DIR}/sample.rs"));
    let least = app.window().min_size;
    app.on_resize(least.width as u32, least.height as u32);
    for x in [200, least.width] {
        app.update(EditorMsg::SidebarDrag(DragInput::Begin));
        app.update(EditorMsg::SidebarDrag(DragInput::Move(x)));
        app.update(EditorMsg::SidebarDrag(DragInput::End));
        let (Some(divider), Some(code)) = (
            dispatch::first(&app, least, Role::Separator),
            dispatch::first(&app, least, Role::TextField),
        ) else {
            return false;
        };
        let under = divider.x + divider.width / 2 == x;
        if (x == 200 && !under) || code.width < MIN_CODE_WIDTH {
            eprintln!(
                "editor_test: dragged to {x}, the divider is at {divider:?}, the code at {code:?}"
            );
            return false;
        }
    }
    true
}

fn the_choice_stays_shown_through_a_resize() -> bool {
    let Some(many) = many_files() else {
        return false;
    };
    let mut app = app_with(&many);
    key(&mut app, Key::Char('p'), ctrl());
    let chosen = app.overlay_rows() - 1;
    for _ in 0..chosen {
        key(&mut app, Key::Named(NamedKey::Down), Modifiers::default());
    }
    let least = app.window().min_size;
    app.on_resize(least.width as u32, least.height as u32);
    let (first, rows) = (app.overlay_first_row(), app.overlay_rows());
    rows <= chosen && first <= chosen && chosen < first + rows
}

fn builds_a_view() -> bool {
    let mut app = app_with(&format!("{DIR}/sample.rs"));
    let mut panes = 0;
    // A chord that opened no prompt would lay out the same window again.
    for (open, prompt_expected) in [
        (None, false),
        (Some((Key::Char('f'), ctrl())), true),
        (Some((Key::Char('h'), ctrl())), true),
        (Some((Key::Char('p'), ctrl_shift())), true),
        (Some((Key::Char('p'), ctrl())), true),
        (Some((Key::Char('g'), ctrl())), true),
        // Not a prompt: the window as it lays out after a tab closes.
        (Some((Key::Char('w'), ctrl())), false),
    ] {
        if let Some((k, mods)) = open {
            key(&mut app, k, mods);
        }
        if app.prompt_is_open() != prompt_expected {
            return false;
        }
        // A pane that cannot be laid out panics.
        let tree = dispatch::layout(&app);
        if tree.layout_rect().width <= 0 {
            return false;
        }
        panes += 1;
        key(&mut app, Key::Named(NamedKey::Escape), Modifiers::default());
        if app.prompt_is_open() {
            return false;
        }
    }
    panes == 7
}

/// Save as then Ctrl+N once left the field over a new, unrelated buffer.
fn a_command_closes_the_prompt_it_leaves() -> bool {
    let mut app = app_with(&format!("{DIR}/sample.rs"));
    key(&mut app, Key::Char('s'), ctrl_shift());
    if !app.prompt_is_open() {
        return false;
    }
    let tabs_before = app.documents().len();
    key(&mut app, Key::Char('n'), ctrl());
    !app.prompt_is_open() && app.documents().len() == tabs_before + 1
}

fn the_find_readout_follows_the_tab() -> bool {
    let mut app = app_with(&format!("{DIR}/sample.rs"));
    app.open(&format!("{DIR}/notes.md"));
    key(&mut app, Key::Char('f'), ctrl());
    app.type_in_prompt("answer");
    // notes.md has none of it; sample.rs has two.
    if app.find_progress() != Some((0, 0)) {
        return false;
    }
    let sample = app
        .documents()
        .iter()
        .position(|d| d.title() == "sample.rs")
        .unwrap_or(0);
    app.update(EditorMsg::Tab(TabInput::Select(sample)));
    app.find_progress().map(|(total, _)| total) == Some(2)
}

fn the_find_readout_follows_an_undo() -> bool {
    let mut app = app_with(&format!("{DIR}/sample.rs"));
    key(&mut app, Key::Char('f'), ctrl());
    app.type_in_prompt("answer");
    if app.find_progress().map(|(total, _)| total) != Some(2) {
        return false;
    }
    app.update(EditorMsg::Run(Command::SelectAll));
    app.update(EditorMsg::Run(Command::Cut));
    if app.find_progress().map(|(total, _)| total) != Some(0) {
        return false;
    }
    app.update(EditorMsg::Run(Command::Undo));
    app.find_progress().map(|(total, _)| total) == Some(2)
}

fn opens_every_file_argument() -> bool {
    let app = EditorApp::new(&[format!("{DIR}/sample.rs"), format!("{DIR}/notes.md")]);
    let titles: Vec<&str> = app.documents().iter().map(|d| d.title()).collect();
    titles == ["sample.rs", "notes.md"] && app.active_index() == 0
}

fn a_changed_file_is_not_silently_overwritten() -> bool {
    let path = format!("{DIR}/contended.txt");
    if std::fs::write(&path, "original\n").is_err() {
        return false;
    }
    let mut app = app_with(&path);
    type_text(&mut app, "X");
    // Another length, so the stamp differs whatever the clock says.
    if std::fs::write(&path, "somebody else was here\n").is_err() {
        return false;
    }
    key(&mut app, Key::Char('s'), ctrl());
    if app.dialog().is_none() {
        return false;
    }
    let untouched =
        || std::fs::read_to_string(&path).unwrap_or_default() == "somebody else was here\n";
    dispatch::press(&mut app, Key::Named(NamedKey::Enter), Modifiers::default());
    if app.dialog().is_none() || !untouched() {
        return false;
    }
    dispatch::press(&mut app, Key::Named(NamedKey::Escape), Modifiers::default());
    if !untouched() || !app.documents()[app.active_index()].is_modified() || app.dialog().is_some()
    {
        return false;
    }
    key(&mut app, Key::Char('s'), ctrl());
    app.update(EditorMsg::Dialog(Answer::Save));
    std::fs::read_to_string(&path).unwrap_or_default() == "Xoriginal\n"
        && !app.documents()[app.active_index()].is_modified()
}

/// Keys through the widget tree, as `run_app` sends them: `on_key` alone
/// never sees a widget take a key first.
mod dispatch {
    use slopos_appkit::constraints::{Rect, Size};
    use slopos_appkit::event::{
        EventResponse, Key, MessageSink, Modifiers, PointerButton, WidgetEvent,
    };
    use slopos_appkit::traits::{Role, Widget};
    use slopos_appkit::{App, StyleSheet, tree};
    use slopos_apps::editor::{EditorApp, EditorMsg};

    pub const WINDOW: Size = Size {
        width: 1100,
        height: 700,
    };

    pub fn layout(app: &EditorApp) -> Box<dyn Widget> {
        layout_at(app, WINDOW)
    }

    pub fn layout_at(app: &EditorApp, window: Size) -> Box<dyn Widget> {
        let node = app.view();
        let mut root = tree::build_widget_tree(&node);
        tree::layout_tree(root.as_mut(), window, &StyleSheet::current());
        root
    }

    /// The first widget in `role`, in tree order, in a `window` of that size.
    pub fn first(app: &EditorApp, window: Size, role: Role) -> Option<Rect> {
        fn find(widget: &dyn Widget, role: Role) -> Option<Rect> {
            if widget.role() == role {
                return Some(widget.layout_rect());
            }
            widget
                .children()
                .iter()
                .find_map(|c| find(c.as_ref(), role))
        }
        find(layout_at(app, window).as_ref(), role)
    }

    /// Where the finder's or the palette's list lies in a `window` of that size.
    pub fn overlay_list(app: &EditorApp, window: Size) -> Option<Rect> {
        fn topmost(widget: &dyn Widget) -> Option<Rect> {
            let above = widget
                .children()
                .iter()
                .rev()
                .find_map(|c| topmost(c.as_ref()));
            above.or_else(|| (widget.role() == Role::List).then(|| widget.layout_rect()))
        }
        topmost(layout_at(app, window).as_ref())
    }

    pub fn click(app: &mut EditorApp, x: i32, y: i32) -> bool {
        send(
            app,
            WidgetEvent::PointerDown {
                x,
                y,
                button: PointerButton::Left,
                modifiers: Modifiers::default(),
            },
        )
    }

    /// Answers whether a widget took it.
    pub fn press(app: &mut EditorApp, key: Key, modifiers: Modifiers) -> bool {
        let mut root = layout(app);
        let mut sink = MessageSink::new();
        let event = WidgetEvent::KeyDown { key, modifiers };
        let response = root.event(&event, &mut sink);
        let blocked = response == EventResponse::Blocked;
        let taken = (response.is_consumed() && !blocked)
            || tree::offer_unhandled(root.as_mut(), key, modifiers, &mut sink);
        for msg in sink.drain_typed::<EditorMsg>() {
            app.update(msg);
        }
        if !taken && !blocked {
            app.on_key(key, modifiers);
        }
        taken
    }

    /// A printable character arrives as `TextInput`; Space, a named key, as
    /// `KeyDown`, which a widget may take.
    pub fn text(app: &mut EditorApp, character: char) -> bool {
        send(app, WidgetEvent::TextInput { character })
    }

    fn send(app: &mut EditorApp, event: WidgetEvent) -> bool {
        let mut root = layout(app);
        let mut sink = MessageSink::new();
        let consumed = root.event(&event, &mut sink).is_consumed();
        for msg in sink.drain_typed::<EditorMsg>() {
            app.update(msg);
        }
        consumed
    }
}

fn find_field_accepts_a_space() -> bool {
    let mut app = app_with(&format!("{DIR}/sample.rs"));
    key(&mut app, Key::Char('f'), ctrl());
    // The space goes in the middle, where a stolen one is unmistakable.
    for c in "let".chars() {
        dispatch::text(&mut app, c);
    }
    dispatch::press(&mut app, Key::Named(NamedKey::Space), Modifiers::default());
    for c in "answer".chars() {
        dispatch::text(&mut app, c);
    }
    app.prompt_text() == "let answer" && app.find_progress().map(|(_, t)| t) == Some(1)
}

fn palette_arrows_move_rather_than_run() -> bool {
    let mut app = app_with(&format!("{DIR}/sample.rs"));
    key(&mut app, Key::Char('p'), ctrl_shift());
    let before = app.documents().len();
    for _ in 0..3 {
        dispatch::press(&mut app, Key::Named(NamedKey::Down), Modifiers::default());
    }
    // A command run on the way down would have closed it, and most change tabs.
    app.prompt_is_overlay() && app.documents().len() == before
}

fn palette_window_follows_rather_than_pins() -> bool {
    let mut app = app_with(&format!("{DIR}/sample.rs"));
    key(&mut app, Key::Char('p'), ctrl_shift());
    for _ in 0..3 {
        dispatch::press(&mut app, Key::Named(NamedKey::Down), Modifiers::default());
    }
    if app.overlay_first_row() != 0 {
        return false;
    }
    let rows = app.overlay_rows();
    for _ in 3..rows {
        dispatch::press(&mut app, Key::Named(NamedKey::Down), Modifiers::default());
    }
    if app.overlay_first_row() != 1 {
        return false;
    }
    dispatch::press(&mut app, Key::Named(NamedKey::Up), Modifiers::default());
    app.overlay_first_row() == 1
}

fn space_reaches_the_document() -> bool {
    let mut app = app_with(&format!("{DIR}/sample.rs"));
    key(&mut app, Key::Char('f'), ctrl());
    key(&mut app, Key::Named(NamedKey::Escape), Modifiers::default());
    let before = app.documents()[0].buffer.line(0).to_string();
    dispatch::press(&mut app, Key::Named(NamedKey::Space), Modifiers::default());
    app.documents()[0].buffer.line(0) == format!(" {before}")
}

fn main() {
    if !setup() {
        slopos_slibc::test_harness::report(
            slopos_slibc::test_harness::TestStatus::Fail,
            "editor_test_setup",
            "could not create the fixture directory",
        );
        std::process::exit(1);
    }

    let mut cases: Vec<(&'static str, fn() -> bool)> = Vec::new();
    cases.extend_from_slice(slopos_editor_core::tests::cases());
    cases.extend_from_slice(&[
        (
            "opens_a_file_argument",
            opens_a_file_argument as fn() -> bool,
        ),
        ("populates_the_sidebar", populates_the_sidebar),
        ("types_and_saves", types_and_saves),
        (
            "reopening_switches_rather_than_duplicating",
            reopening_switches_rather_than_duplicating,
        ),
        ("finds_and_counts", finds_and_counts),
        ("palette_filters_and_runs", palette_filters_and_runs),
        ("a_picked_palette_row_runs", a_picked_palette_row_runs),
        (
            "a_picked_row_counts_from_the_first_shown",
            a_picked_row_counts_from_the_first_shown,
        ),
        (
            "a_click_opens_the_row_under_it",
            a_click_opens_the_row_under_it,
        ),
        (
            "a_row_past_the_results_opens_nothing",
            a_row_past_the_results_opens_nothing,
        ),
        (
            "the_overlays_fit_the_least_window",
            the_overlays_fit_the_least_window,
        ),
        (
            "tab_moves_the_finder_s_choice",
            tab_moves_the_finder_s_choice,
        ),
        ("close_asks_before_discarding", close_asks_before_discarding),
        ("tree_expands_a_directory", tree_expands_a_directory),
        ("refuses_a_binary_file", refuses_a_binary_file),
        (
            "finder_reaches_unexpanded_directories",
            finder_reaches_unexpanded_directories,
        ),
        (
            "an_unopenable_argument_still_starts",
            an_unopenable_argument_still_starts,
        ),
        (
            "find_next_works_with_the_bar_closed",
            find_next_works_with_the_bar_closed,
        ),
        (
            "opening_over_the_scratch_buffer_cancels_save_as",
            opening_over_the_scratch_buffer_cancels_save_as,
        ),
        ("builds_a_view", builds_a_view),
        ("find_field_accepts_a_space", find_field_accepts_a_space),
        (
            "palette_arrows_move_rather_than_run",
            palette_arrows_move_rather_than_run,
        ),
        ("space_reaches_the_document", space_reaches_the_document),
        (
            "palette_window_follows_rather_than_pins",
            palette_window_follows_rather_than_pins,
        ),
        (
            "a_command_closes_the_prompt_it_leaves",
            a_command_closes_the_prompt_it_leaves,
        ),
        (
            "the_find_readout_follows_the_tab",
            the_find_readout_follows_the_tab,
        ),
        (
            "the_find_readout_follows_an_undo",
            the_find_readout_follows_an_undo,
        ),
        ("opens_every_file_argument", opens_every_file_argument),
        (
            "a_changed_file_is_not_silently_overwritten",
            a_changed_file_is_not_silently_overwritten,
        ),
        ("the_title_names_the_document", the_title_names_the_document),
        (
            "every_error_says_what_to_do_next",
            every_error_says_what_to_do_next,
        ),
        (
            "every_refusal_says_what_was_refused_and_why",
            every_refusal_says_what_was_refused_and_why,
        ),
        (
            "every_passing_status_is_a_label",
            every_passing_status_is_a_label,
        ),
        (
            "replace_all_says_what_it_needs",
            replace_all_says_what_it_needs,
        ),
        (
            "the_choice_stays_shown_through_a_resize",
            the_choice_stays_shown_through_a_resize,
        ),
        (
            "an_empty_find_bar_asks_only_for_typing",
            an_empty_find_bar_asks_only_for_typing,
        ),
        (
            "switching_to_an_open_file_fits_the_window",
            switching_to_an_open_file_fits_the_window,
        ),
        (
            "the_divider_stays_under_the_pointer",
            the_divider_stays_under_the_pointer,
        ),
    ]);

    slopos_slibc::test_harness::run(&cases);
}
