use super::*;

#[test]
fn a_release_names_its_extension_and_version() {
    let release =
        Release::parse("# c\nname hello\nversion 3\ndisplay 1.2-beta\nsummary Says hi\n").unwrap();
    assert_eq!(release.name, "hello");
    assert_eq!(release.version, 3);
    assert_eq!(release.display, "1.2-beta");
    assert_eq!(release.summary.as_deref(), Some("Says hi"));
    let bare = Release::parse("name x\nversion 0\n").unwrap();
    assert_eq!((bare.display.as_str(), bare.summary), ("0", None));
}

#[test]
fn what_the_grammar_does_not_take_is_refused_with_its_line() {
    for (text, line) in [
        ("name bin\nversion 1\n", 1),
        ("name Hello\nversion 1\n", 1),
        ("name a b\nversion 1\n", 1),
        ("name a\nversion 01\n", 2),
        ("name a\nversion -1\n", 2),
        ("name a\nversion 1\nversion 2\n", 3),
        ("name a\nversion 1\nflavour x\n", 3),
        ("name a\n  version 1\n", 2),
        ("name a\nversion 1\nsummary say \"hi\"\n", 3),
        ("name a\nversion 1\ndisplay two words\n", 3),
        ("name a\n", 0),
        ("version 1\n", 0),
    ] {
        assert_eq!(
            Release::parse(text).map_err(|e| e.line),
            Err(line),
            "{text:?}"
        );
    }
}
