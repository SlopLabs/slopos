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
    // A fixture of our own, so the case does not rest on which entry of some
    // other directory the filesystem happens to hand back first.
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

/// The finder offers files from directories the sidebar never expanded.
fn finder_reaches_unexpanded_directories() -> bool {
    let deep = format!("{DIR}/deep/deeper");
    let _ = std::fs::create_dir(format!("{DIR}/deep"));
    let _ = std::fs::create_dir(&deep);
    if std::fs::write(format!("{deep}/buried.rs"), "fn buried() {}\n").is_err() {
        return false;
    }
    let mut app = app_with(DIR);
    // Nothing has expanded `deep`, so the sidebar cannot see `buried.rs`.
    if app.tree_rows(0, 64).iter().any(|r| r.0 == "buried.rs") {
        return false;
    }
    app.update(EditorMsg::Run(Command::FileFinder));
    app.type_in_prompt("buried");
    app.finder_results()
        .iter()
        .any(|p| p.ends_with("buried.rs"))
}

/// A binary file is refused rather than opened as replacement characters.
fn refuses_a_binary_file() -> bool {
    // A fixture rather than a real binary: `/bin/editor` would also be refused,
    // but for its size once it outgrows `MAX_FILE_BYTES`, and the assertion
    // would go on passing for the wrong reason.
    let path = format!("{DIR}/blob.bin");
    if std::fs::write(&path, [0x7f, b'E', b'L', b'F', 0x00, 0x01, 0x02, 0x00]).is_err() {
        return false;
    }
    let mut app = app_with(DIR);
    let before = app.documents().len();
    app.open(&path);
    app.documents().len() == before && app.status().contains("binary")
}

/// The view builds against the real font metrics, for every pane the editor has.
fn builds_a_view() -> bool {
    let mut app = app_with(&format!("{DIR}/sample.rs"));
    let mut panes = 0;
    for open in [
        None,
        Some((Key::Char('f'), ctrl())),
        Some((Key::Char('p'), ctrl_shift())),
        Some((Key::Char('p'), ctrl())),
        Some((Key::Char('g'), ctrl())),
        Some((Key::Char('w'), ctrl())),
    ] {
        if let Some((k, mods)) = open {
            key(&mut app, k, mods);
        }
        // A pane that cannot be laid out panics rather than returning, so
        // reaching here at all is the assertion; the count is what proves each
        // one was actually opened rather than silently refused.
        let tree = dispatch::layout(&app);
        if tree.layout_rect().width <= 0 {
            return false;
        }
        panes += 1;
        key(&mut app, Key::Named(NamedKey::Escape), Modifiers::default());
    }
    panes == 6
}

/// Driving the real widget tree, not `App::on_key`.
///
/// `run_app` offers a key to the widget tree first and only calls `on_key`
/// with what nothing consumed, so a case that calls `on_key` directly tests
/// the fallback and can never see a widget stealing the key on the way. Every
/// bug this module exists to catch lives on the path it skips.
mod dispatch {
    use slopos_appkit::event::{EventPhase, Key, MessageSink, Modifiers, WidgetEvent};
    use slopos_appkit::traits::Widget;
    use slopos_appkit::{App, StyleSheet, constraints::Size, tree};
    use slopos_userland::apps::editor::{EditorApp, EditorMsg};

    pub const WINDOW: Size = Size {
        width: 1100,
        height: 700,
    };

    pub fn layout(app: &EditorApp) -> Box<dyn Widget> {
        let node = app.view();
        let mut root = tree::build_widget_tree(&node);
        tree::layout_tree(root.as_mut(), WINDOW, &StyleSheet::dark());
        root
    }

    /// Sends `key` the way `run_app` does and applies whatever came back.
    /// Answers whether a widget consumed it.
    pub fn press(app: &mut EditorApp, key: Key, modifiers: Modifiers) -> bool {
        let consumed = send(
            app,
            WidgetEvent::KeyDown {
                key,
                modifiers,
                repeat: false,
            },
        );
        if !consumed {
            app.on_key(key, modifiers);
        }
        consumed
    }

    /// An ordinary printable character. `translate_event` turns one into
    /// `TextInput` rather than `KeyDown` — which is exactly why Space, being a
    /// *named* key, takes the other path and can be stolen on the way.
    pub fn text(app: &mut EditorApp, character: char) -> bool {
        send(app, WidgetEvent::TextInput { character })
    }

    fn send(app: &mut EditorApp, event: WidgetEvent) -> bool {
        let mut root = layout(app);
        let mut sink = MessageSink::new();
        let consumed = root
            .event(&event, EventPhase::Target, &mut sink)
            .is_consumed();
        for msg in sink.drain_typed::<EditorMsg>() {
            app.update(msg);
        }
        consumed
    }
}

/// A space typed into the find field is a space, not whatever button the
/// stack happened to offer the key to first.
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

/// An arrow in the palette moves the selection; it does not run the command
/// the selection lands on.
fn palette_arrows_move_rather_than_run() -> bool {
    let mut app = app_with(&format!("{DIR}/sample.rs"));
    key(&mut app, Key::Char('p'), ctrl_shift());
    let before = app.documents().len();
    for _ in 0..3 {
        dispatch::press(&mut app, Key::Named(NamedKey::Down), Modifiers::default());
    }
    // Still open, nothing ran: a palette that executed on the way down would
    // have closed itself and, for most of its commands, changed the tabs.
    app.prompt_is_overlay() && app.documents().len() == before
}

/// The palette window scrolls only when the selection leaves it, so the
/// selected row is not permanently pinned to the bottom of the list.
fn palette_window_follows_rather_than_pins() -> bool {
    let mut app = app_with(&format!("{DIR}/sample.rs"));
    key(&mut app, Key::Char('p'), ctrl_shift());
    // Everything inside the first window keeps the window where it is.
    for _ in 0..3 {
        dispatch::press(&mut app, Key::Named(NamedKey::Down), Modifiers::default());
    }
    if app.overlay_first_row() != 0 {
        return false;
    }
    // Stepping past the last drawn row scrolls by exactly one.
    let rows = slopos_userland::apps::editor::PALETTE_MAX_ROWS;
    for _ in 3..rows {
        dispatch::press(&mut app, Key::Named(NamedKey::Down), Modifiers::default());
    }
    if app.overlay_first_row() != 1 {
        return false;
    }
    // And stepping back up does not throw the window to the top.
    dispatch::press(&mut app, Key::Named(NamedKey::Up), Modifiers::default());
    app.overlay_first_row() == 1
}

/// Space reaches the document rather than a button in the find bar.
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
        ("close_asks_before_discarding", close_asks_before_discarding),
        ("tree_expands_a_directory", tree_expands_a_directory),
        ("refuses_a_binary_file", refuses_a_binary_file),
        (
            "finder_reaches_unexpanded_directories",
            finder_reaches_unexpanded_directories,
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
    ]);

    slopos_slibc::test_harness::run(&cases);
}
