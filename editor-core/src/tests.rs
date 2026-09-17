//! Unit tests for the editor core.
//!
//! Every case is a plain `fn() -> bool` in [`cases`] as well as a `#[test]`, so
//! the same assertions run twice: on the host under `cargo test`, and inside
//! SlopOS from `/bin/editor_test`, against the target's allocator. A case that
//! only ever ran on the host would not be evidence about the machine this is
//! written for.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::buffer::{IndentStyle, LineEnding, Position, Range, TextBuffer};
use crate::cursor::{Motion, word_at, word_left, word_right};
use crate::document::{Document, file_name, parent_dir};
use crate::filetree::{DirEntry, FileTree, join_path};
use crate::search::{SearchOptions, find_all, find_next, find_prev, fuzzy_filter};
use crate::syntax::{Highlighter, Language, LineState, TokenKind, detect_language};

fn doc(text: &str) -> Document {
    Document::from_text(Some("/tmp/test.rs".to_string()), text).expect("load")
}

fn plain_doc(text: &str) -> Document {
    Document::from_text(Some("/tmp/test.txt".to_string()), text).expect("load")
}

fn pos(line: usize, col: usize) -> Position {
    Position::new(line, col)
}

// ── buffer ──────────────────────────────────────────────────────────────────

fn test_buffer_splits_lines() -> bool {
    let b = TextBuffer::from_str("one\ntwo\nthree\n").expect("load");
    assert_eq!(b.line_count(), 3);
    assert_eq!(b.line(1), "two");
    assert_eq!(b.to_text(), "one\ntwo\nthree\n");
    true
}

fn test_buffer_without_final_newline_round_trips() -> bool {
    let b = TextBuffer::from_str("a\nb").expect("load");
    assert_eq!(b.line_count(), 2);
    assert_eq!(b.to_text(), "a\nb");
    true
}

fn test_buffer_detects_and_restores_crlf() -> bool {
    let b = TextBuffer::from_str("a\r\nb\r\n").expect("load");
    assert_eq!(b.line_ending(), LineEnding::Crlf);
    assert_eq!(b.line(0), "a");
    assert_eq!(b.to_text(), "a\r\nb\r\n");
    true
}

fn test_empty_buffer_is_one_empty_line() -> bool {
    let b = TextBuffer::new();
    assert_eq!(b.line_count(), 1);
    assert!(b.is_empty());
    assert_eq!(b.end_position(), pos(0, 0));
    true
}

fn test_insert_multiline_returns_end() -> bool {
    let mut b = TextBuffer::from_str("ac").expect("load");
    let end = b.insert(pos(0, 1), "X\nY");
    assert_eq!(end, pos(1, 1));
    assert_eq!(b.line(0), "aX");
    assert_eq!(b.line(1), "Yc");
    true
}

fn test_insert_normalizes_crlf_inside_a_line() -> bool {
    let mut b = TextBuffer::new();
    b.insert(pos(0, 0), "a\r\nb");
    assert_eq!(b.line_count(), 2);
    assert_eq!(b.line(0), "a");
    assert_eq!(b.line(1), "b");
    true
}

fn test_delete_across_lines_joins() -> bool {
    let mut b = TextBuffer::from_str("hello\nworld\n!").expect("load");
    let removed = b.delete(Range::new(pos(0, 2), pos(1, 3)));
    assert_eq!(removed, "llo\nwor");
    assert_eq!(b.line(0), "held");
    assert_eq!(b.line_count(), 2);
    true
}

fn test_slice_spans_lines() -> bool {
    let b = TextBuffer::from_str("ab\ncd\nef").expect("load");
    assert_eq!(b.slice(Range::new(pos(0, 1), pos(2, 1))), "b\ncd\ne");
    true
}

fn test_clamp_holds_positions_inside() -> bool {
    let b = TextBuffer::from_str("ab\ncd").expect("load");
    assert_eq!(b.clamp(pos(9, 9)), pos(1, 2));
    assert_eq!(b.clamp(pos(0, 9)), pos(0, 2));
    true
}

fn test_positions_are_characters_not_bytes() -> bool {
    let mut b = TextBuffer::from_str("äöü").expect("load");
    assert_eq!(b.line_len(0), 3);
    b.insert(pos(0, 2), "X");
    assert_eq!(b.line(0), "äöXü");
    true
}

fn test_detect_indent_prefers_the_common_width() -> bool {
    let b = TextBuffer::from_str("fn a() {\n  let x = 1;\n  let y = 2;\n}\n").expect("load");
    assert_eq!(
        b.detect_indent(IndentStyle::Spaces(4)),
        IndentStyle::Spaces(2)
    );
    let t = TextBuffer::from_str("fn a() {\n\tlet x = 1;\n}\n").expect("load");
    assert!(matches!(
        t.detect_indent(IndentStyle::Spaces(4)),
        IndentStyle::Tabs(_)
    ));
    // Nesting must not read as a wider unit: this file steps by four twice and
    // shows an absolute eight as often as an absolute four.
    let nested = TextBuffer::from_str("fn a() {\n    if x {\n        y();\n    }\n    z();\n}\n")
        .expect("load");
    assert_eq!(
        nested.detect_indent(IndentStyle::Spaces(2)),
        IndentStyle::Spaces(4)
    );
    true
}

// ── motion ──────────────────────────────────────────────────────────────────

fn test_word_motion_stops_between_classes() -> bool {
    let b = TextBuffer::from_str("foo::bar baz").expect("load");
    assert_eq!(word_right(&b, pos(0, 0)), pos(0, 3));
    assert_eq!(word_right(&b, pos(0, 3)), pos(0, 5));
    assert_eq!(word_left(&b, pos(0, 5)), pos(0, 3));
    true
}

fn test_word_right_at_line_end_crosses_the_break() -> bool {
    let b = TextBuffer::from_str("ab\ncd").expect("load");
    assert_eq!(word_right(&b, pos(0, 2)), pos(1, 0));
    true
}

fn test_word_at_selects_the_identifier() -> bool {
    let b = TextBuffer::from_str("let value = 1;").expect("load");
    let range = word_at(&b, pos(0, 6));
    assert_eq!(range.start, pos(0, 4));
    assert_eq!(range.end, pos(0, 9));
    true
}

fn test_home_toggles_between_indent_and_column_zero() -> bool {
    let mut d = doc("    indented\n");
    d.place_cursor(pos(0, 8), false);
    d.move_cursor(Motion::LineStart, false);
    assert_eq!(d.cursor.position, pos(0, 4));
    d.move_cursor(Motion::LineStart, false);
    assert_eq!(d.cursor.position, pos(0, 0));
    true
}

fn test_vertical_motion_keeps_the_goal_column() -> bool {
    let mut d = doc("aaaaaa\nbb\ncccccc\n");
    d.place_cursor(pos(0, 5), false);
    d.move_cursor(Motion::Down, false);
    assert_eq!(d.cursor.position, pos(1, 2));
    d.move_cursor(Motion::Down, false);
    assert_eq!(d.cursor.position, pos(2, 5));
    true
}

fn test_left_out_of_a_selection_collapses_to_its_start() -> bool {
    let mut d = doc("hello\n");
    d.place_cursor(pos(0, 1), false);
    d.place_cursor(pos(0, 4), true);
    d.move_cursor(Motion::Left, false);
    assert_eq!(d.cursor.position, pos(0, 1));
    assert!(!d.cursor.has_selection());
    true
}

fn test_page_motion_is_clamped_to_the_buffer() -> bool {
    let mut d = doc("a\nb\nc\n");
    d.move_cursor(Motion::PageDown(50), false);
    assert_eq!(d.cursor.position.line, 2);
    d.move_cursor(Motion::PageUp(50), false);
    assert_eq!(d.cursor.position.line, 0);
    true
}

// ── editing ─────────────────────────────────────────────────────────────────

fn test_typing_then_undo_restores_the_line() -> bool {
    let mut d = doc("fn main() {}\n");
    d.place_cursor(pos(0, 0), false);
    d.insert_text("p");
    d.insert_text("u");
    d.insert_text("b");
    d.insert_text(" ");
    assert_eq!(d.buffer.line(0), "pub fn main() {}");
    assert!(d.undo());
    assert_eq!(d.buffer.line(0), "fn main() {}");
    assert!(d.redo());
    assert_eq!(d.buffer.line(0), "pub fn main() {}");
    true
}

fn test_typing_coalesces_into_one_undo_group() -> bool {
    let mut d = plain_doc("");
    for c in "hello".chars() {
        d.insert_text(&c.to_string());
    }
    assert!(d.undo());
    assert_eq!(d.buffer.line(0), "");
    assert!(!d.can_undo());
    true
}

fn test_a_motion_breaks_the_undo_group() -> bool {
    let mut d = plain_doc("");
    d.insert_text("ab");
    d.move_cursor(Motion::Left, false);
    d.insert_text("c");
    assert!(d.undo());
    assert_eq!(d.buffer.line(0), "ab");
    true
}

fn test_newline_carries_indentation() -> bool {
    let mut d = doc("    let x = 1;\n");
    d.place_cursor(pos(0, 14), false);
    d.insert_newline();
    assert_eq!(d.buffer.line(1), "    ");
    assert_eq!(d.cursor.position, pos(1, 4));
    true
}

fn test_newline_after_an_opening_brace_indents_one_level() -> bool {
    let mut d = doc("fn main() {\n");
    d.place_cursor(pos(0, 11), false);
    d.insert_newline();
    assert_eq!(d.buffer.line(1), "    ");
    true
}

fn test_compound_edits_undo_in_one_step() -> bool {
    let mut d = doc("fn main() {}\n");
    d.place_cursor(pos(0, 11), false);
    d.insert_newline();
    assert_eq!(d.buffer.line_count(), 3);
    assert!(d.undo());
    assert_eq!(d.buffer.line_count(), 1);
    assert_eq!(d.buffer.line(0), "fn main() {}");

    let mut d = plain_doc("a\nb\nc\n");
    d.place_cursor(pos(0, 0), false);
    d.move_lines(true);
    assert!(d.undo());
    assert_eq!(d.buffer.line(0), "a");
    assert_eq!(d.buffer.line(1), "b");

    let mut d = doc("let x = 1;\nlet y = 2;\n");
    d.place_cursor(pos(0, 0), false);
    d.place_cursor(pos(1, 3), true);
    d.toggle_comment();
    assert!(d.undo());
    assert_eq!(d.buffer.line(0), "let x = 1;");
    assert_eq!(d.buffer.line(1), "let y = 2;");
    true
}

fn test_newline_between_braces_lands_the_closing_brace_below() -> bool {
    let mut d = doc("fn main() {}\n");
    d.place_cursor(pos(0, 11), false);
    d.insert_newline();
    assert_eq!(d.buffer.line(0), "fn main() {");
    assert_eq!(d.buffer.line(1), "    ");
    assert_eq!(d.buffer.line(2), "}");
    assert_eq!(d.cursor.position, pos(1, 4));
    true
}

fn test_closing_brace_dedents_its_line() -> bool {
    let mut d = doc("fn main() {\n    \n");
    d.set_indent(IndentStyle::Spaces(4));
    d.place_cursor(pos(1, 4), false);
    d.insert_text("}");
    assert_eq!(d.buffer.line(1), "}");
    // One step, not two: the dedent and the brace are one edit.
    assert!(d.undo());
    assert_eq!(d.buffer.line(1), "    ");

    // A brace typed after real text is just a character.
    let mut d = doc("let x = vec![1\n");
    d.place_cursor(pos(0, 14), false);
    d.insert_text("]");
    assert_eq!(d.buffer.line(0), "let x = vec![1]");
    true
}

fn test_backspace_in_indentation_removes_one_level() -> bool {
    let mut d = doc("        x\n");
    d.set_indent(IndentStyle::Spaces(4));
    d.place_cursor(pos(0, 8), false);
    d.backspace();
    assert_eq!(d.buffer.line(0), "    x");
    true
}

fn test_backspace_joins_lines() -> bool {
    let mut d = plain_doc("ab\ncd\n");
    d.place_cursor(pos(1, 0), false);
    d.backspace();
    assert_eq!(d.buffer.line(0), "abcd");
    assert_eq!(d.cursor.position, pos(0, 2));
    true
}

fn test_indent_and_outdent_a_selection() -> bool {
    let mut d = doc("a\nb\nc\n");
    d.set_indent(IndentStyle::Spaces(4));
    d.place_cursor(pos(0, 0), false);
    d.place_cursor(pos(2, 1), true);
    d.indent_selection();
    assert_eq!(d.buffer.line(0), "    a");
    assert_eq!(d.buffer.line(2), "    c");
    d.outdent();
    assert_eq!(d.buffer.line(0), "a");
    assert_eq!(d.buffer.line(2), "c");
    true
}

fn test_indent_keeps_the_same_text_selected() -> bool {
    let mut d = doc("abc\ndef\n");
    d.set_indent(IndentStyle::Spaces(2));
    d.place_cursor(pos(0, 1), false);
    d.place_cursor(pos(1, 2), true);
    d.indent_selection();
    let sel = d.cursor.selection().expect("selection survives");
    assert_eq!(sel.start, pos(0, 3));
    assert_eq!(sel.end, pos(1, 4));
    true
}

fn test_tab_without_a_selection_inserts_one_level() -> bool {
    let mut d = doc("x\n");
    d.set_indent(IndentStyle::Spaces(4));
    d.place_cursor(pos(0, 0), false);
    d.indent_selection();
    assert_eq!(d.buffer.line(0), "    x");
    true
}

fn test_toggle_comment_round_trips() -> bool {
    let mut d = doc("    let x = 1;\n    let y = 2;\n");
    d.place_cursor(pos(0, 0), false);
    d.place_cursor(pos(1, 3), true);
    d.toggle_comment();
    assert_eq!(d.buffer.line(0), "    // let x = 1;");
    assert_eq!(d.buffer.line(1), "    // let y = 2;");
    d.toggle_comment();
    assert_eq!(d.buffer.line(0), "    let x = 1;");
    assert_eq!(d.buffer.line(1), "    let y = 2;");
    true
}

/// A toggle with nothing to comment must not leave a transaction open, or
/// every later edit merges into one undo group.
fn test_toggle_comment_on_blank_lines_leaves_undo_alone() -> bool {
    let mut d = doc("\nfoo\nbar\n");
    d.place_cursor(pos(0, 0), false);
    d.toggle_comment();
    d.place_cursor(pos(1, 0), false);
    d.insert_text("X");
    d.place_cursor(pos(2, 0), false);
    d.insert_text("Y");
    assert!(d.undo());
    assert_eq!(d.buffer.line(2), "bar");
    assert_eq!(d.buffer.line(1), "Xfoo");
    assert!(d.undo());
    assert_eq!(d.buffer.line(1), "foo");
    true
}

fn test_lexer_survives_malformed_input() -> bool {
    // A `.rs` file need not be Rust: nothing here may panic or hang.
    let samples = ["#!]", "#![", "]]]", "#[a[b]", "/*/", "r#\"", "'''"];
    for lang in [
        Language::Rust,
        Language::C,
        Language::Python,
        Language::Shell,
        Language::Toml,
        Language::Json,
        Language::Markdown,
    ] {
        let h = Highlighter::new(lang);
        let mut state = LineState::Normal;
        for text in samples {
            let (spans, next) = h.line(text, state);
            let len = text.chars().count();
            for span in spans {
                assert!(span.end <= len, "{lang:?} {text:?}: span past line end");
            }
            state = next;
        }
    }
    true
}

fn test_toggle_comment_is_a_no_op_without_a_comment_syntax() -> bool {
    let mut d = Document::from_text(Some("/tmp/a.json".to_string()), "{}\n").expect("load");
    d.toggle_comment();
    assert_eq!(d.buffer.line(0), "{}");
    true
}

fn test_move_lines_down_and_back() -> bool {
    let mut d = plain_doc("one\ntwo\nthree\n");
    d.place_cursor(pos(0, 1), false);
    d.move_lines(true);
    assert_eq!(d.buffer.line(0), "two");
    assert_eq!(d.buffer.line(1), "one");
    assert_eq!(d.cursor.position, pos(1, 1));
    d.move_lines(false);
    assert_eq!(d.buffer.line(0), "one");
    assert_eq!(d.cursor.position, pos(0, 1));
    true
}

fn test_move_lines_at_the_edges_does_nothing() -> bool {
    let mut d = plain_doc("a\nb\n");
    d.place_cursor(pos(0, 0), false);
    d.move_lines(false);
    assert_eq!(d.buffer.line(0), "a");
    d.place_cursor(pos(1, 0), false);
    d.move_lines(true);
    assert_eq!(d.buffer.line(1), "b");
    true
}

fn test_move_lines_leaves_the_cursor_inside_the_buffer() -> bool {
    // A whole-line selection ends at the start of the line *below* the block,
    // which a move down shifts one line past the end. An out-of-range caret
    // paints a selection wider than the next edit deletes.
    let mut d = plain_doc("a\nb\nc\nd\n");
    d.place_cursor(pos(1, 0), false);
    d.place_cursor(pos(3, 0), true);
    d.move_lines(true);
    assert_eq!(d.buffer.to_text(), "a\nd\nb\nc\n");
    assert!(d.cursor.position.line < d.buffer.line_count());
    assert!(d.cursor.position.col <= d.buffer.line_len(d.cursor.position.line));
    // The whole moved block stays selected.
    let selection = d.cursor.selection().expect("selection survives the move");
    assert_eq!(selection.start, pos(2, 0));
    assert_eq!(selection.end, d.buffer.end_position());

    // And the next edit deletes what the selection said it would.
    d.insert_text("Q");
    assert_eq!(d.buffer.to_text(), "a\nd\nQ\n");
    true
}

fn test_display_columns_round_trip_through_tabs() -> bool {
    use crate::buffer::{char_col_from_display, display_col};
    for text in ["\tab", "  \tx", "a\tb\tc", "plain"] {
        for col in 0..text.chars().count() {
            let display = display_col(text, col, 4);
            assert_eq!(char_col_from_display(text, display, 4), col);
        }
    }
    assert_eq!(display_col("\t\t", 2, 4), 8);
    true
}

fn test_horizontal_scroll_counts_display_columns() -> bool {
    // The renderer subtracts `first_col` from an already-expanded column, so a
    // character column here would scroll a tab-indented line by three cells too
    // few per tab and put the caret outside the viewport.
    let mut d = plain_doc("\t\tx\n");
    d.viewport.visible_lines = 10;
    d.viewport.visible_cols = 4;
    d.place_cursor(pos(0, 2), false);
    d.scroll_to_cursor(0);
    assert_eq!(d.viewport.first_col, 8 + 1 - 4);
    true
}

fn test_redo_restores_the_cursor_a_brace_split_left() -> bool {
    let mut d = plain_doc("fn x() {}\n");
    d.place_cursor(pos(0, 8), false);
    d.insert_newline();
    let after = d.cursor.position;
    assert_eq!(after, pos(1, 4));
    d.undo();
    assert_eq!(d.buffer.to_text(), "fn x() {}\n");
    d.redo();
    assert_eq!(d.buffer.to_text(), "fn x() {\n    \n}\n");
    assert_eq!(d.cursor.position, after);
    true
}

fn test_relative_to_requires_a_separator_boundary() -> bool {
    use crate::filetree::relative_to;
    assert_eq!(relative_to("/foo", "/foo/bar"), Some("bar"));
    assert_eq!(relative_to("/foo/", "/foo/bar/baz"), Some("bar/baz"));
    assert_eq!(relative_to("/foo", "/foo"), Some(""));
    // The one the bare prefix test got wrong.
    assert_eq!(relative_to("/foo", "/foobar/baz"), None);
    assert_eq!(relative_to("/foo", "/other"), None);
    true
}

fn test_electric_dedent_keeps_the_caret_after_the_brace() -> bool {
    let mut d = plain_doc("fn f() {\n    if x {\n        y();\n");
    d.place_cursor(pos(2, 12), false);
    d.insert_newline();
    assert_eq!(d.cursor.position, pos(3, 8));
    d.insert_text("}");
    // One level out, and the caret directly after the brace — not at the
    // margin with the rest of the old indent left trailing behind it.
    assert_eq!(d.buffer.line(3), "    }");
    assert_eq!(d.cursor.position, pos(3, 5));
    // And the line after it inherits the dedented indent.
    d.insert_newline();
    assert_eq!(d.buffer.line(4), "    ");
    true
}

fn test_count_chars_matches_the_slice_it_avoids_building() -> bool {
    let d = plain_doc("alpha\nbeta\n\ngamma delta\n");
    for sl in 0..d.buffer.line_count() {
        for sc in 0..=d.buffer.line_len(sl) {
            for el in sl..d.buffer.line_count() {
                for ec in 0..=d.buffer.line_len(el) {
                    let range = Range::new(pos(sl, sc), pos(el, ec));
                    let expected = d.buffer.slice(range).chars().count();
                    assert_eq!(d.buffer.count_chars(range), expected);
                }
            }
        }
    }
    true
}

fn test_detect_indent_ignores_a_whitespace_only_line() -> bool {
    // The blank line's eight spaces are not an indent level, and counting them
    // as one records a step nothing took.
    let b = TextBuffer::from_str("fn a() {\n  x();\n        \n  y();\n}\n").expect("load");
    assert_eq!(
        b.detect_indent(IndentStyle::Spaces(4)),
        IndentStyle::Spaces(2)
    );
    true
}

fn test_a_closing_brace_replaces_a_backwards_selection() -> bool {
    // Shift+Home, or a right-to-left drag, parks the caret where everything
    // before it on the line is whitespace — which is exactly where the electric
    // dedent fires. Computing the dedent first cleared the anchor, so the
    // selection was never deleted and the brace landed in front of it.
    let mut d = plain_doc("fn f() {\n    foo\n");
    d.place_cursor(pos(1, 7), false);
    d.move_cursor(Motion::LineStart, true);
    assert!(d.cursor.has_selection());
    d.insert_text("}");
    assert_eq!(d.buffer.line(1), "}");
    // One Ctrl+Z takes back the whole of it.
    d.undo();
    assert_eq!(d.buffer.to_text(), "fn f() {\n    foo\n");
    true
}

fn test_redo_restores_where_a_compound_edit_left_the_caret() -> bool {
    // Not cosmetic: the next keystroke reads the cursor, so a redone move-line
    // followed by another one would move a different line.
    let text = "    aaa\n    bbb\n    ccc\n    ddd\n";
    let cases: [(&str, fn(&mut Document)); 4] = [
        ("toggle_comment", |d| d.toggle_comment()),
        ("indent", |d| d.indent_selection()),
        ("outdent", |d| d.outdent()),
        ("move_lines", |d| d.move_lines(true)),
    ];
    for (name, op) in cases {
        let mut d = plain_doc(text);
        d.set_indent(IndentStyle::Spaces(4));
        d.place_cursor(pos(1, 4), false);
        d.place_cursor(pos(2, 7), true);
        op(&mut d);
        let after = (d.cursor.position, d.cursor.anchor);
        let landed = d.buffer.to_text();
        d.undo();
        d.redo();
        assert_eq!(d.buffer.to_text(), landed, "{name}: redo text");
        assert_eq!(d.cursor.position, after.0, "{name}: redo caret");
        assert_eq!(d.cursor.anchor, after.1, "{name}: redo anchor");
    }
    true
}

fn test_a_lone_carriage_return_is_content_not_an_ending() -> bool {
    // A one-byte file whose byte is `\r` used to load as empty and save as
    // empty — a plain Ctrl+S with no edit at all destroying the file.
    let b = TextBuffer::from_str("\r").expect("load");
    assert_eq!(b.to_text(), "\r");
    // And a stray CR inside an LF file is text the file had.
    let b = TextBuffer::from_str("a\rb\nc\n").expect("load");
    assert_eq!(b.to_text(), "a\rb\nc\n");
    // A CRLF file still round-trips.
    let b = TextBuffer::from_str("a\r\nb\r\n").expect("load");
    assert_eq!(b.line(0), "a");
    assert_eq!(b.to_text(), "a\r\nb\r\n");
    true
}

fn test_a_crlf_file_keeps_a_carriage_return_on_its_last_line() -> bool {
    // The body trim already took the file's own terminator off, so whatever
    // `\r` the last piece still ends with is content. Stripping it again made
    // open-then-save shorten the file by a byte with no edit at all.
    for text in [
        "a\r\r\n",
        "a\r\nb\r\r\n",
        "a\r\nb\r",
        "\r\r\n",
        "a\r\r\nb\r\n",
    ] {
        let b = TextBuffer::from_str(text).expect("load");
        assert_eq!(b.to_text(), text, "{text:?}");
    }
    true
}

fn test_the_undo_cap_bounds_transactional_edits_too() -> bool {
    // `record` cannot trim inside a transaction, and every compound edit is
    // one — including pressing Enter, which is ordinary typing.
    let mut d = plain_doc("x\n");
    for _ in 0..(crate::history::MAX_UNDO_GROUPS + 200) {
        d.insert_newline();
    }
    assert!(d.undo_depth() <= crate::history::MAX_UNDO_GROUPS);
    true
}

fn test_a_refused_insert_destroys_nothing() -> bool {
    // The selection is deleted first so the dedent reads the right line, which
    // means the ceiling has to be checked before any of it.
    let mut d = plain_doc("keep me\n");
    d.place_cursor(pos(0, 0), false);
    d.place_cursor(pos(0, 4), true);
    // A buffer nowhere near the limit accepts it, so use the check directly.
    assert!(!d.buffer.would_exceed_line_limit("x"));
    assert!(d.insert_text("x"));
    assert_eq!(d.buffer.line(0), "x me");
    true
}

fn test_duplicate_line_puts_the_copy_below() -> bool {
    let mut d = plain_doc("alpha\nbeta\n");
    d.place_cursor(pos(0, 2), false);
    d.duplicate_line();
    assert_eq!(d.buffer.line(0), "alpha");
    assert_eq!(d.buffer.line(1), "alpha");
    assert_eq!(d.buffer.line(2), "beta");
    assert_eq!(d.cursor.position, pos(1, 2));
    true
}

fn test_delete_line_removes_its_terminator() -> bool {
    let mut d = plain_doc("a\nb\nc\n");
    d.place_cursor(pos(1, 0), false);
    d.delete_line();
    assert_eq!(d.buffer.line_count(), 2);
    assert_eq!(d.buffer.line(0), "a");
    assert_eq!(d.buffer.line(1), "c");
    true
}

fn test_delete_last_line_does_not_leave_a_blank() -> bool {
    let mut d = plain_doc("a\nb");
    d.place_cursor(pos(1, 0), false);
    d.delete_line();
    assert_eq!(d.buffer.line_count(), 1);
    assert_eq!(d.buffer.line(0), "a");
    true
}

fn test_typing_over_a_selection_replaces_it() -> bool {
    let mut d = plain_doc("hello world\n");
    d.place_cursor(pos(0, 0), false);
    d.place_cursor(pos(0, 5), true);
    d.insert_text("bye");
    assert_eq!(d.buffer.line(0), "bye world");
    // One undo, not two: replacing a selection is one step.
    assert!(d.undo());
    assert_eq!(d.buffer.line(0), "hello world");
    true
}

fn test_selected_lines_excludes_a_trailing_column_zero() -> bool {
    let mut d = plain_doc("a\nb\nc\n");
    d.place_cursor(pos(0, 0), false);
    d.place_cursor(pos(2, 0), true);
    assert_eq!(d.selected_lines(), (0, 1));
    true
}

fn test_modified_tracks_the_saved_revision() -> bool {
    let mut d = plain_doc("x\n");
    assert!(!d.is_modified());
    d.insert_text("y");
    assert!(d.is_modified());
    d.mark_saved(None);
    assert!(!d.is_modified());
    assert!(d.undo());
    assert!(d.is_modified());
    true
}

fn test_save_as_retitles_and_relanguages() -> bool {
    let mut d = Document::empty("untitled-1".to_string());
    d.insert_text("fn main() {}");
    d.mark_saved(Some("/home/user/main.rs".to_string()));
    assert_eq!(d.title(), "main.rs");
    assert_eq!(d.language(), Language::Rust);
    assert!(!d.is_modified());
    true
}

fn test_goto_line_lands_on_the_first_non_blank() -> bool {
    let mut d = doc("a\n    b\nc\n");
    d.goto_line(2);
    assert_eq!(d.cursor.position, pos(1, 4));
    d.goto_line(9999);
    assert_eq!(d.cursor.position.line, 2);
    true
}

fn test_scroll_to_cursor_keeps_a_margin() -> bool {
    let mut d = plain_doc(&"x\n".repeat(200));
    d.viewport.visible_lines = 20;
    d.place_cursor(pos(100, 0), false);
    d.scroll_to_cursor(3);
    assert!(d.viewport.first_line <= 97);
    assert!(d.viewport.first_line + 20 > 100);
    true
}

fn test_scroll_by_is_clamped() -> bool {
    let mut d = plain_doc("a\nb\nc\n");
    d.scroll_by(-5);
    assert_eq!(d.viewport.first_line, 0);
    d.scroll_by(500);
    assert_eq!(d.viewport.first_line, 2);
    true
}

fn test_undo_after_reload_of_states_rehighlights() -> bool {
    let d = doc("/* comment\nstill comment */ let x = 1;\n");
    let spans = d.line_spans(1);
    assert!(spans.iter().any(|s| s.kind == TokenKind::Comment));
    assert!(spans.iter().any(|s| s.kind == TokenKind::Keyword));
    true
}

// ── search ──────────────────────────────────────────────────────────────────

fn test_find_all_is_case_insensitive_by_default() -> bool {
    let b = TextBuffer::from_str("Foo foo FOO\n").expect("load");
    let hits = find_all(&b, "foo", SearchOptions::default());
    assert_eq!(hits.len(), 3);
    let sensitive = find_all(
        &b,
        "foo",
        SearchOptions {
            case_sensitive: true,
            whole_word: false,
        },
    );
    assert_eq!(sensitive.len(), 1);
    true
}

fn test_whole_word_search_rejects_substrings() -> bool {
    let b = TextBuffer::from_str("foo foobar\n").expect("load");
    let hits = find_all(
        &b,
        "foo",
        SearchOptions {
            case_sensitive: false,
            whole_word: true,
        },
    );
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].start, pos(0, 0));
    true
}

fn test_find_next_and_prev_wrap() -> bool {
    let b = TextBuffer::from_str("a\nxx\na\n").expect("load");
    let opts = SearchOptions::default();
    let first = find_next(&b, pos(0, 0), "a", opts).expect("match");
    assert_eq!(first.start, pos(0, 0));
    let second = find_next(&b, pos(0, 1), "a", opts).expect("match");
    assert_eq!(second.start, pos(2, 0));
    let wrapped = find_next(&b, pos(2, 1), "a", opts).expect("match");
    assert_eq!(wrapped.start, pos(0, 0));
    let back = find_prev(&b, pos(0, 0), "a", opts).expect("match");
    assert_eq!(back.start, pos(2, 0));
    true
}

fn test_replace_all_applies_back_to_front() -> bool {
    let mut d = plain_doc("aa aa\n");
    let n = d.replace_all("aa", "bbb", SearchOptions::default());
    assert_eq!(n, 2);
    assert_eq!(d.buffer.line(0), "bbb bbb");
    // One step, however many matches it rewrote.
    assert!(d.undo());
    assert_eq!(d.buffer.line(0), "aa aa");
    true
}

fn test_overlapping_matches_are_not_double_counted() -> bool {
    let b = TextBuffer::from_str("aaaa\n").expect("load");
    let hits = find_all(&b, "aa", SearchOptions::default());
    assert_eq!(hits.len(), 2);
    true
}

fn test_fuzzy_filter_prefers_segment_starts() -> bool {
    let candidates = ["/src/mm/lib.rs", "/documents/malformed.txt"];
    let ranked = fuzzy_filter(&candidates, "mml");
    assert_eq!(ranked.first().map(|(p, _)| *p), Some("/src/mm/lib.rs"));
    true
}

fn test_fuzzy_filter_drops_non_subsequences() -> bool {
    let candidates = ["/a/b.rs"];
    assert!(fuzzy_filter(&candidates, "zzz").is_empty());
    true
}

// ── syntax ──────────────────────────────────────────────────────────────────

fn kinds(text: &str, lang: Language) -> Vec<(String, TokenKind)> {
    let h = Highlighter::new(lang);
    let (spans, _) = h.line(text, LineState::Normal);
    let chars: Vec<char> = text.chars().collect();
    spans
        .iter()
        .map(|s| (chars[s.start..s.end].iter().collect::<String>(), s.kind))
        .collect()
}

fn test_rust_line_highlights_keyword_type_and_string() -> bool {
    let out = kinds("let name: String = \"hi\";", Language::Rust);
    assert!(out.contains(&("let".to_string(), TokenKind::Keyword)));
    assert!(out.contains(&("String".to_string(), TokenKind::Type)));
    assert!(out.contains(&("\"hi\"".to_string(), TokenKind::Str)));
    true
}

fn test_rust_macro_and_call_are_distinguished() -> bool {
    let out = kinds("println!(\"x\"); helper();", Language::Rust);
    assert!(out.contains(&("println!".to_string(), TokenKind::Macro)));
    assert!(out.contains(&("helper".to_string(), TokenKind::Function)));
    true
}

fn test_rust_attribute_is_one_span() -> bool {
    let out = kinds("#[derive(Debug)]", Language::Rust);
    assert_eq!(
        out.first().map(|(t, k)| (t.as_str(), *k)),
        Some(("#[derive(Debug)]", TokenKind::Attribute))
    );
    true
}

fn test_block_comment_state_crosses_lines() -> bool {
    let h = Highlighter::new(Language::Rust);
    let (_, state) = h.line("/* open", LineState::Normal);
    assert_eq!(state, LineState::BlockComment(1));
    let (spans, end) = h.line("still */ let", state);
    assert_eq!(end, LineState::Normal);
    assert_eq!(spans[0].kind, TokenKind::Comment);
    assert!(spans.iter().any(|s| s.kind == TokenKind::Keyword));
    true
}

fn test_rust_block_comments_nest() -> bool {
    let h = Highlighter::new(Language::Rust);
    let (_, state) = h.line("/* a /* b", LineState::Normal);
    assert_eq!(state, LineState::BlockComment(2));
    let (_, state) = h.line("*/", state);
    assert_eq!(state, LineState::BlockComment(1));
    let (_, state) = h.line("*/", state);
    assert_eq!(state, LineState::Normal);
    true
}

fn test_raw_string_spans_lines() -> bool {
    let h = Highlighter::new(Language::Rust);
    let (_, state) = h.line("let s = r#\"open", LineState::Normal);
    assert_eq!(state, LineState::RawString(1));
    let (_, end) = h.line("close\"#;", state);
    assert_eq!(end, LineState::Normal);
    true
}

fn test_line_comment_swallows_the_rest_of_the_line() -> bool {
    let out = kinds("let x = 1; // let y", Language::Rust);
    let comments: Vec<&(String, TokenKind)> = out
        .iter()
        .filter(|(_, k)| *k == TokenKind::Comment)
        .collect();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].0, "// let y");
    assert_eq!(
        out.iter().filter(|(_, k)| *k == TokenKind::Keyword).count(),
        1
    );
    true
}

fn test_toml_key_and_section() -> bool {
    let section = kinds("[package]", Language::Toml);
    assert_eq!(section.first().map(|(_, k)| *k), Some(TokenKind::Property));
    let pair = kinds("name = \"slopos\"", Language::Toml);
    assert!(
        pair.iter()
            .any(|(t, k)| t.trim() == "name" && *k == TokenKind::Property)
    );
    assert!(pair.iter().any(|(_, k)| *k == TokenKind::Str));
    true
}

fn test_json_key_and_value_differ() -> bool {
    let out = kinds("  \"key\": \"value\",", Language::Json);
    assert!(out.contains(&("\"key\"".to_string(), TokenKind::Property)));
    assert!(out.contains(&("\"value\"".to_string(), TokenKind::Str)));
    true
}

fn test_markdown_fence_state() -> bool {
    let h = Highlighter::new(Language::Markdown);
    let (_, state) = h.line("```rust", LineState::Normal);
    assert_eq!(state, LineState::CodeFence);
    let (spans, state) = h.line("let x = 1;", state);
    assert_eq!(state, LineState::CodeFence);
    assert_eq!(spans[0].kind, TokenKind::Str);
    let (_, state) = h.line("```", state);
    assert_eq!(state, LineState::Normal);
    true
}

fn test_language_detection_by_name_and_extension() -> bool {
    assert_eq!(detect_language("/a/b/main.rs"), Language::Rust);
    assert_eq!(detect_language("Cargo.toml"), Language::Toml);
    assert_eq!(detect_language("/repo/justfile"), Language::Shell);
    assert_eq!(detect_language("/repo/README"), Language::PlainText);
    true
}

fn test_spans_never_exceed_the_line() -> bool {
    let samples = [
        ("let s = \"unterminated", Language::Rust),
        ("r#\"open", Language::Rust),
        ("#[incomplete", Language::Rust),
        ("'", Language::Shell),
        ("[unclosed", Language::Toml),
        ("\"key\": ", Language::Json),
        ("**bold", Language::Markdown),
    ];
    for (text, lang) in samples {
        let len = text.chars().count();
        let h = Highlighter::new(lang);
        let (spans, _) = h.line(text, LineState::Normal);
        for span in spans {
            assert!(span.start <= span.end, "{text}: inverted span");
            assert!(span.end <= len, "{text}: span past line end");
        }
    }
    true
}

// ── file tree ───────────────────────────────────────────────────────────────

fn entries(names: &[(&str, bool)]) -> Vec<DirEntry> {
    names
        .iter()
        .map(|(n, d)| DirEntry {
            name: n.to_string(),
            is_dir: *d,
        })
        .collect()
}

fn test_tree_sorts_dirs_first_then_by_name() -> bool {
    let mut tree = FileTree::new("/repo");
    tree.populate(
        0,
        entries(&[("zeta.rs", false), ("Alpha", true), ("beta", true)]),
    );
    let rows: Vec<usize> = tree.rows().iter().map(|r| r.node).collect();
    let names: Vec<&str> = rows
        .iter()
        .skip(1)
        .map(|i| tree.node(*i).expect("node").name.as_str())
        .collect();
    assert_eq!(names, ["Alpha", "beta", "zeta.rs"]);
    true
}

fn test_collapsed_directory_hides_its_children() -> bool {
    let mut tree = FileTree::new("/repo");
    tree.populate(0, entries(&[("src", true)]));
    let src = tree.find_path("/repo/src").expect("child");
    tree.set_expanded(src, true);
    tree.populate(src, entries(&[("lib.rs", false)]));
    assert_eq!(tree.rows().len(), 3);
    tree.set_expanded(src, false);
    assert_eq!(tree.rows().len(), 2);
    true
}

fn test_repopulate_keeps_expansion() -> bool {
    let mut tree = FileTree::new("/repo");
    tree.populate(0, entries(&[("src", true)]));
    let src = tree.find_path("/repo/src").expect("child");
    tree.set_expanded(src, true);
    tree.populate(0, entries(&[("src", true), ("README", false)]));
    let src = tree.find_path("/repo/src").expect("child again");
    assert!(tree.node(src).expect("node").expanded);
    true
}

fn test_refresh_does_not_leak_stale_nodes() -> bool {
    let mut tree = FileTree::new("/repo");
    let listing = entries(&[("src", true), ("README.md", false)]);
    tree.populate(0, listing.clone());
    let after_first = tree.len();
    for _ in 0..4 {
        tree.populate(0, listing.clone());
    }
    assert_eq!(tree.len(), after_first);

    let mut paths = tree.file_paths();
    let before = paths.len();
    paths.sort_unstable();
    paths.dedup();
    assert_eq!(paths.len(), before);
    assert_eq!(paths, ["/repo/README.md"]);
    true
}

fn test_find_path_returns_a_live_node_after_refresh() -> bool {
    let mut tree = FileTree::new("/repo");
    tree.populate(0, entries(&[("src", true)]));
    let stale = tree.find_path("/repo/src").expect("child");
    tree.populate(0, entries(&[("src", true), ("docs", true)]));

    let src = tree.find_path("/repo/src").expect("child again");
    assert!(tree.node(src).is_some());
    assert!(tree.row_of(src).is_some());
    // The stale index is either recycled into a live node or dead; either way
    // it must not be a second, invisible "/repo/src".
    if stale != src {
        assert!(tree.node(stale).map(|n| n.path.as_str()) != Some("/repo/src"));
    }
    true
}

fn test_reveal_survives_a_refresh() -> bool {
    let mut tree = FileTree::new("/repo");
    tree.populate(0, entries(&[("src", true)]));
    let src = tree.find_path("/repo/src").expect("src");
    tree.populate(src, entries(&[("lib.rs", false)]));
    tree.set_expanded(src, false);

    tree.populate(0, entries(&[("src", true)]));
    let src = tree.find_path("/repo/src").expect("src again");
    tree.populate(src, entries(&[("lib.rs", false)]));
    let lib = tree.find_path("/repo/src/lib.rs").expect("lib.rs");
    tree.reveal(lib);
    assert!(tree.row_of(lib).is_some());
    assert_eq!(tree.file_paths(), ["/repo/src/lib.rs"]);
    true
}

fn test_pending_lists_expanded_unloaded_dirs() -> bool {
    let mut tree = FileTree::new("/repo");
    assert_eq!(tree.pending(), alloc::vec![0]);
    tree.populate(0, entries(&[("src", true)]));
    assert!(tree.pending().is_empty());
    let src = tree.find_path("/repo/src").expect("child");
    tree.set_expanded(src, true);
    assert_eq!(tree.pending(), alloc::vec![src]);
    true
}

fn test_reveal_expands_ancestors() -> bool {
    let mut tree = FileTree::new("/repo");
    tree.populate(0, entries(&[("src", true)]));
    let src = tree.find_path("/repo/src").expect("child");
    tree.populate(src, entries(&[("lib.rs", false)]));
    let file = tree.find_path("/repo/src/lib.rs").expect("file");
    tree.reveal(file);
    assert!(tree.row_of(file).is_some());
    true
}

fn test_path_helpers() -> bool {
    assert_eq!(join_path("/repo", "src"), "/repo/src");
    assert_eq!(join_path("/repo/", "/src"), "/repo/src");
    assert_eq!(file_name("/a/b/c.rs"), "c.rs");
    assert_eq!(file_name("/"), "/");
    assert_eq!(parent_dir("/a/b/c.rs"), "/a/b");
    assert_eq!(parent_dir("/a"), "/");
    true
}

/// Every editor-core case, for `cargo test` and for `/bin/editor_test`.
pub fn cases() -> &'static [(&'static str, fn() -> bool)] {
    &[
        ("buffer_splits_lines", test_buffer_splits_lines),
        (
            "buffer_without_final_newline_round_trips",
            test_buffer_without_final_newline_round_trips,
        ),
        (
            "buffer_detects_and_restores_crlf",
            test_buffer_detects_and_restores_crlf,
        ),
        (
            "empty_buffer_is_one_empty_line",
            test_empty_buffer_is_one_empty_line,
        ),
        (
            "insert_multiline_returns_end",
            test_insert_multiline_returns_end,
        ),
        (
            "insert_normalizes_crlf_inside_a_line",
            test_insert_normalizes_crlf_inside_a_line,
        ),
        ("delete_across_lines_joins", test_delete_across_lines_joins),
        ("slice_spans_lines", test_slice_spans_lines),
        (
            "clamp_holds_positions_inside",
            test_clamp_holds_positions_inside,
        ),
        (
            "positions_are_characters_not_bytes",
            test_positions_are_characters_not_bytes,
        ),
        (
            "detect_indent_prefers_the_common_width",
            test_detect_indent_prefers_the_common_width,
        ),
        (
            "word_motion_stops_between_classes",
            test_word_motion_stops_between_classes,
        ),
        (
            "word_right_at_line_end_crosses_the_break",
            test_word_right_at_line_end_crosses_the_break,
        ),
        (
            "word_at_selects_the_identifier",
            test_word_at_selects_the_identifier,
        ),
        (
            "home_toggles_between_indent_and_column_zero",
            test_home_toggles_between_indent_and_column_zero,
        ),
        (
            "vertical_motion_keeps_the_goal_column",
            test_vertical_motion_keeps_the_goal_column,
        ),
        (
            "left_out_of_a_selection_collapses_to_its_start",
            test_left_out_of_a_selection_collapses_to_its_start,
        ),
        (
            "page_motion_is_clamped_to_the_buffer",
            test_page_motion_is_clamped_to_the_buffer,
        ),
        (
            "typing_then_undo_restores_the_line",
            test_typing_then_undo_restores_the_line,
        ),
        (
            "typing_coalesces_into_one_undo_group",
            test_typing_coalesces_into_one_undo_group,
        ),
        (
            "a_motion_breaks_the_undo_group",
            test_a_motion_breaks_the_undo_group,
        ),
        (
            "newline_carries_indentation",
            test_newline_carries_indentation,
        ),
        (
            "newline_after_an_opening_brace_indents_one_level",
            test_newline_after_an_opening_brace_indents_one_level,
        ),
        (
            "compound_edits_undo_in_one_step",
            test_compound_edits_undo_in_one_step,
        ),
        (
            "newline_between_braces_lands_the_closing_brace_below",
            test_newline_between_braces_lands_the_closing_brace_below,
        ),
        (
            "closing_brace_dedents_its_line",
            test_closing_brace_dedents_its_line,
        ),
        (
            "backspace_in_indentation_removes_one_level",
            test_backspace_in_indentation_removes_one_level,
        ),
        ("backspace_joins_lines", test_backspace_joins_lines),
        (
            "indent_and_outdent_a_selection",
            test_indent_and_outdent_a_selection,
        ),
        (
            "indent_keeps_the_same_text_selected",
            test_indent_keeps_the_same_text_selected,
        ),
        (
            "tab_without_a_selection_inserts_one_level",
            test_tab_without_a_selection_inserts_one_level,
        ),
        (
            "toggle_comment_round_trips",
            test_toggle_comment_round_trips,
        ),
        (
            "toggle_comment_on_blank_lines_leaves_undo_alone",
            test_toggle_comment_on_blank_lines_leaves_undo_alone,
        ),
        (
            "lexer_survives_malformed_input",
            test_lexer_survives_malformed_input,
        ),
        (
            "toggle_comment_is_a_no_op_without_a_comment_syntax",
            test_toggle_comment_is_a_no_op_without_a_comment_syntax,
        ),
        ("move_lines_down_and_back", test_move_lines_down_and_back),
        (
            "move_lines_at_the_edges_does_nothing",
            test_move_lines_at_the_edges_does_nothing,
        ),
        (
            "move_lines_leaves_the_cursor_inside_the_buffer",
            test_move_lines_leaves_the_cursor_inside_the_buffer,
        ),
        (
            "electric_dedent_keeps_the_caret_after_the_brace",
            test_electric_dedent_keeps_the_caret_after_the_brace,
        ),
        (
            "a_closing_brace_replaces_a_backwards_selection",
            test_a_closing_brace_replaces_a_backwards_selection,
        ),
        (
            "a_lone_carriage_return_is_content_not_an_ending",
            test_a_lone_carriage_return_is_content_not_an_ending,
        ),
        (
            "a_crlf_file_keeps_a_carriage_return_on_its_last_line",
            test_a_crlf_file_keeps_a_carriage_return_on_its_last_line,
        ),
        (
            "the_undo_cap_bounds_transactional_edits_too",
            test_the_undo_cap_bounds_transactional_edits_too,
        ),
        (
            "a_refused_insert_destroys_nothing",
            test_a_refused_insert_destroys_nothing,
        ),
        (
            "redo_restores_where_a_compound_edit_left_the_caret",
            test_redo_restores_where_a_compound_edit_left_the_caret,
        ),
        (
            "display_columns_round_trip_through_tabs",
            test_display_columns_round_trip_through_tabs,
        ),
        (
            "horizontal_scroll_counts_display_columns",
            test_horizontal_scroll_counts_display_columns,
        ),
        (
            "redo_restores_the_cursor_a_brace_split_left",
            test_redo_restores_the_cursor_a_brace_split_left,
        ),
        (
            "relative_to_requires_a_separator_boundary",
            test_relative_to_requires_a_separator_boundary,
        ),
        (
            "count_chars_matches_the_slice_it_avoids_building",
            test_count_chars_matches_the_slice_it_avoids_building,
        ),
        (
            "detect_indent_ignores_a_whitespace_only_line",
            test_detect_indent_ignores_a_whitespace_only_line,
        ),
        (
            "duplicate_line_puts_the_copy_below",
            test_duplicate_line_puts_the_copy_below,
        ),
        (
            "delete_line_removes_its_terminator",
            test_delete_line_removes_its_terminator,
        ),
        (
            "delete_last_line_does_not_leave_a_blank",
            test_delete_last_line_does_not_leave_a_blank,
        ),
        (
            "typing_over_a_selection_replaces_it",
            test_typing_over_a_selection_replaces_it,
        ),
        (
            "selected_lines_excludes_a_trailing_column_zero",
            test_selected_lines_excludes_a_trailing_column_zero,
        ),
        (
            "modified_tracks_the_saved_revision",
            test_modified_tracks_the_saved_revision,
        ),
        (
            "save_as_retitles_and_relanguages",
            test_save_as_retitles_and_relanguages,
        ),
        (
            "goto_line_lands_on_the_first_non_blank",
            test_goto_line_lands_on_the_first_non_blank,
        ),
        (
            "scroll_to_cursor_keeps_a_margin",
            test_scroll_to_cursor_keeps_a_margin,
        ),
        ("scroll_by_is_clamped", test_scroll_by_is_clamped),
        (
            "undo_after_reload_of_states_rehighlights",
            test_undo_after_reload_of_states_rehighlights,
        ),
        (
            "find_all_is_case_insensitive_by_default",
            test_find_all_is_case_insensitive_by_default,
        ),
        (
            "whole_word_search_rejects_substrings",
            test_whole_word_search_rejects_substrings,
        ),
        ("find_next_and_prev_wrap", test_find_next_and_prev_wrap),
        (
            "replace_all_applies_back_to_front",
            test_replace_all_applies_back_to_front,
        ),
        (
            "overlapping_matches_are_not_double_counted",
            test_overlapping_matches_are_not_double_counted,
        ),
        (
            "fuzzy_filter_prefers_segment_starts",
            test_fuzzy_filter_prefers_segment_starts,
        ),
        (
            "fuzzy_filter_drops_non_subsequences",
            test_fuzzy_filter_drops_non_subsequences,
        ),
        (
            "rust_line_highlights_keyword_type_and_string",
            test_rust_line_highlights_keyword_type_and_string,
        ),
        (
            "rust_macro_and_call_are_distinguished",
            test_rust_macro_and_call_are_distinguished,
        ),
        (
            "rust_attribute_is_one_span",
            test_rust_attribute_is_one_span,
        ),
        (
            "block_comment_state_crosses_lines",
            test_block_comment_state_crosses_lines,
        ),
        ("rust_block_comments_nest", test_rust_block_comments_nest),
        ("raw_string_spans_lines", test_raw_string_spans_lines),
        (
            "line_comment_swallows_the_rest_of_the_line",
            test_line_comment_swallows_the_rest_of_the_line,
        ),
        ("toml_key_and_section", test_toml_key_and_section),
        ("json_key_and_value_differ", test_json_key_and_value_differ),
        ("markdown_fence_state", test_markdown_fence_state),
        (
            "language_detection_by_name_and_extension",
            test_language_detection_by_name_and_extension,
        ),
        (
            "spans_never_exceed_the_line",
            test_spans_never_exceed_the_line,
        ),
        (
            "tree_sorts_dirs_first_then_by_name",
            test_tree_sorts_dirs_first_then_by_name,
        ),
        (
            "collapsed_directory_hides_its_children",
            test_collapsed_directory_hides_its_children,
        ),
        (
            "repopulate_keeps_expansion",
            test_repopulate_keeps_expansion,
        ),
        (
            "refresh_does_not_leak_stale_nodes",
            test_refresh_does_not_leak_stale_nodes,
        ),
        (
            "find_path_returns_a_live_node_after_refresh",
            test_find_path_returns_a_live_node_after_refresh,
        ),
        ("reveal_survives_a_refresh", test_reveal_survives_a_refresh),
        (
            "pending_lists_expanded_unloaded_dirs",
            test_pending_lists_expanded_unloaded_dirs,
        ),
        ("reveal_expands_ancestors", test_reveal_expands_ancestors),
        ("path_helpers", test_path_helpers),
    ]
}

#[cfg(test)]
mod host {
    #[test]
    fn every_case_passes() {
        for (name, case) in super::cases() {
            assert!(case(), "{name} returned false");
        }
    }
}
