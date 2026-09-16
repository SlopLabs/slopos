#![feature(restricted_std)]

//! The editor on the machine it was written for.
//!
//! Two halves: every `editor-core` case, run here against the target's
//! allocator rather than only on the host, and the application's own state
//! machine — open, edit, save, search, close — driven through the same messages
//! the widgets emit, with no compositor in the path.

use slopos_userland as _;

use slopos_appkit::widgets::code_view::CodeInput;
use slopos_appkit::widgets::editor_tabs::TabInput;
use slopos_appkit::widgets::tree_view::TreeInput;
use slopos_appkit::{App, Key, Modifiers, NamedKey};
use slopos_userland::apps::editor::{Command, EditorApp, EditorMsg};

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

fn type_text(app: &mut EditorApp, text: &str) {
    for c in text.chars() {
        app.update(EditorMsg::Code(CodeInput::Text { character: c }));
    }
}

/// Opening a file names the tab after it and leaves the buffer clean.
fn opens_a_file_argument() -> bool {
    let app = app_with(&format!("{DIR}/sample.rs"));
    let doc = &app.documents()[app.active_index()];
    doc.title() == "sample.rs" && !doc.is_modified() && doc.buffer.line_count() == 4
}

/// The sidebar is rooted at the file's folder and lists what is in it.
fn populates_the_sidebar() -> bool {
    let app = app_with(&format!("{DIR}/sample.rs"));
    let rows = app.tree_rows(0, 32);
    let names: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    names.contains(&"sample.rs") && names.contains(&"notes.md")
}

/// Typing marks the buffer dirty; saving writes it and marks it clean.
fn types_and_saves() -> bool {
    let path = format!("{DIR}/typed.txt");
    let _ = std::fs::remove_file(&path);
    let mut app = app_with(DIR);
    type_text(&mut app, "hello");
    if !app.documents()[app.active_index()].is_modified() {
        return false;
    }
    // Save As, through the prompt the Ctrl+Shift+S chord opens.
    key(
        &mut app,
        Key::Char('s'),
        Modifiers {
            ctrl: true,
            shift: true,
            ..Modifiers::default()
        },
    );
    app.clear_prompt();
    app.type_in_prompt(&path);
    key(&mut app, Key::Named(NamedKey::Enter), Modifiers::default());

    let written = std::fs::read_to_string(&path).unwrap_or_default();
    written == "hello\n" && !app.documents()[app.active_index()].is_modified()
}

/// A second open of the same path switches to the tab that already holds it.
fn reopening_switches_rather_than_duplicating() -> bool {
    let mut app = app_with(&format!("{DIR}/sample.rs"));
    let before = app.documents().len();
    app.update(EditorMsg::Run(Command::NewFile));
    let with_scratch = app.documents().len();
    app.open(&format!("{DIR}/sample.rs"));
    before == 1 && with_scratch == 2 && app.documents().len() == 2 && app.active_index() == 0
}

/// Find counts every match and walks them.
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

/// The command palette filters by name and runs what it lands on.
fn palette_filters_and_runs() -> bool {
    let mut app = app_with(&format!("{DIR}/sample.rs"));
    key(
        &mut app,
        Key::Char('p'),
        Modifiers {
            ctrl: true,
            shift: true,
            ..Modifiers::default()
        },
    );
    app.type_in_prompt("toggle sideb");
    let visible_before = app.sidebar_visible();
    key(&mut app, Key::Named(NamedKey::Enter), Modifiers::default());
    app.sidebar_visible() != visible_before
}

/// Closing a modified tab asks first, and cancelling keeps it open.
fn close_asks_before_discarding() -> bool {
    let mut app = app_with(&format!("{DIR}/sample.rs"));
    type_text(&mut app, "x");
    app.update(EditorMsg::Tab(TabInput::Close(0)));
    if !app.dialog().is_some() {
        return false;
    }
    // Action 2 is Cancel.
    app.update(EditorMsg::Dialog(2));
    !app.dialog().is_some() && app.documents().len() == 1
}

/// A directory row in the tree expands rather than opening a buffer.
fn tree_expands_a_directory() -> bool {
    let mut app = app_with("/usr");
    let before = app.tree_rows(0, 64).len();
    app.update(EditorMsg::Tree(TreeInput::Activate { row: 1 }));
    let after = app.tree_rows(0, 64).len();
    after > before && app.documents().len() == 1
}

/// A binary file is refused rather than opened as replacement characters.
fn refuses_a_binary_file() -> bool {
    let mut app = app_with(DIR);
    let before = app.documents().len();
    app.open("/bin/editor");
    app.documents().len() == before && app.status().contains("binary")
}

/// The view builds against the real font metrics, for every pane the editor has.
fn builds_a_view() -> bool {
    let mut app = app_with(&format!("{DIR}/sample.rs"));
    let _ = app.view();
    key(&mut app, Key::Char('f'), ctrl());
    let _ = app.view();
    key(&mut app, Key::Named(NamedKey::Escape), Modifiers::default());
    key(
        &mut app,
        Key::Char('p'),
        Modifiers {
            ctrl: true,
            shift: true,
            ..Modifiers::default()
        },
    );
    let _ = app.view();
    true
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
        ("close_asks_before_discarding", close_asks_before_discarding),
        ("tree_expands_a_directory", tree_expands_a_directory),
        ("refuses_a_binary_file", refuses_a_binary_file),
        ("builds_a_view", builds_a_view),
    ]);

    slopos_slibc::test_harness::run(&cases);
}
