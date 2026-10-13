use super::*;
use crate::wire::deframer;
use alloc::vec;
use slopos_abi::fs::SESSION_UID;

fn ask() -> Ask {
    Ask {
        id: 7,
        action: "org.slopos.boot.install".into(),
        describe: "Install a kernel and base into a boot slot".into(),
        program: Some("/bin/shell".into()),
        uid: SESSION_UID,
        pid: 42,
        cwd: "/".into(),
        command: vec![b"/bin/bootctl".to_vec(), b"install".to_vec(), b"b".to_vec()],
    }
}

fn payload(framed: &[u8]) -> (u8, Vec<u8>) {
    let mut d = deframer();
    d.push(framed);
    d.next_frame().unwrap().unwrap()
}

#[test]
fn a_question_and_its_answer_round_trip() {
    let (kind, body) = payload(&ask().frame());
    assert_eq!(kind, kind::ASK);
    assert_eq!(Ask::decode(&body), Ok(ask()));
    let outsider = Ask {
        program: None,
        ..ask()
    };
    assert_eq!(Ask::decode(&payload(&outsider.frame()).1), Ok(outsider));
    for allow in [true, false] {
        let answer = Answer { id: 9, allow };
        let (kind, body) = payload(&answer.frame());
        assert_eq!(kind, kind::ANSWER);
        assert_eq!(Answer::decode(&body), Ok(answer));
    }
    let (kind, body) = payload(&cancel_frame(11));
    assert_eq!((kind, decode_cancel(&body)), (kind::CANCEL, Ok(11)));
    assert_eq!(payload(&ready_frame()), (kind::READY, vec![]));
}

#[test]
fn a_question_outside_the_protocol_is_malformed() {
    let base = || {
        Fields::new()
            .with("id", "1")
            .with("action", "org.x")
            .with("describe", "d")
            .with("uid", "0")
            .with("pid", "1")
            .with("cwd", "/")
    };
    assert!(Ask::decode(&base().with("arg", "/bin/x").encode()).is_ok());
    for fields in [
        base(),
        base().with("arg", "/bin/x").with("extra", "1"),
        base()
            .with("arg", "/bin/x")
            .with("program", "a")
            .with("program", "b"),
        base().with("arg", "/bin/x").with("describe", "again"),
        base().with("arg", "/bin/x").with("cwd", "/again"),
        Fields::new().with("id", "1").with("arg", "x"),
    ] {
        assert_eq!(Ask::decode(&fields.encode()), Err(Malformed), "{fields:?}");
    }
    for allow in ["", "2", "yes", "11"] {
        let fields = Fields::new().with("id", "1").with("allow", allow);
        assert_eq!(
            Answer::decode(&fields.encode()),
            Err(Malformed),
            "{allow:?}"
        );
    }
    let mutated = payload(&ask().frame()).1;
    for at in 0..mutated.len() {
        let mut bytes = mutated.clone();
        bytes[at] ^= 0x20;
        let _ = Ask::decode(&bytes);
        let _ = Ask::decode(&bytes[..at]);
    }
}

/// A dialog drawn at 0, so armed from [`ARMING_MS`].
fn drawn() -> Dialog {
    let mut dialog = Dialog::new(ask());
    dialog.drawn(0);
    dialog
}

#[test]
fn focus_starts_on_deny_so_a_stray_enter_refuses() {
    let mut dialog = drawn();
    assert_eq!(dialog.focus(), Button::Deny);
    assert_eq!(dialog.key(Key::Press, ARMING_MS), Some(false));
}

#[test]
fn allow_takes_a_move_then_a_press() {
    let mut dialog = drawn();
    assert_eq!(dialog.key(Key::Move, ARMING_MS), None);
    assert_eq!(dialog.focus(), Button::Allow);
    assert_eq!(dialog.key(Key::Press, 2 * ARMING_MS), Some(true));
    let mut back = drawn();
    back.key(Key::Move, ARMING_MS);
    back.key(Key::Move, 2 * ARMING_MS);
    assert_eq!(back.key(Key::Press, 3 * ARMING_MS), Some(false));
}

#[test]
fn input_before_the_dialog_is_drawn_or_armed_counts_for_nothing() {
    let mut unseen = Dialog::new(ask());
    assert_eq!(unseen.key(Key::Press, u64::MAX), None);
    assert_eq!(unseen.click(Button::Allow, u64::MAX), None);
    let mut dialog = drawn();
    dialog.drawn(10 * ARMING_MS);
    for key in [Key::Move, Key::Press, Key::Dismiss] {
        assert_eq!(dialog.key(key, ARMING_MS - 1), None);
    }
    assert_eq!(dialog.focus(), Button::Deny);
    assert_eq!(dialog.click(Button::Allow, ARMING_MS - 1), None);
    assert_eq!(dialog.click(Button::Allow, ARMING_MS), Some(true));
}

#[test]
fn a_press_just_after_a_move_counts_for_nothing() {
    let mut dialog = drawn();
    dialog.key(Key::Move, ARMING_MS);
    assert_eq!(dialog.key(Key::Press, 2 * ARMING_MS - 1), None);
    assert_eq!(dialog.click(Button::Allow, 2 * ARMING_MS - 1), None);
    assert_eq!(dialog.focus(), Button::Allow);
    assert_eq!(dialog.key(Key::Press, 2 * ARMING_MS), Some(true));
}

#[test]
fn dismissing_denies_whatever_has_focus() {
    let mut dialog = drawn();
    dialog.key(Key::Move, ARMING_MS);
    assert_eq!(dialog.key(Key::Dismiss, 2 * ARMING_MS), Some(false));
}

#[test]
fn a_click_answers_with_the_button_clicked() {
    let dialog = drawn();
    assert_eq!(dialog.click(Button::Allow, ARMING_MS), Some(true));
    assert_eq!(dialog.click(Button::Deny, ARMING_MS), Some(false));
}

#[test]
fn the_dialog_names_who_asks() {
    assert_eq!(
        Dialog::new(ask()).requester(),
        "/bin/shell, as user, pid 42"
    );
    let root = Ask {
        uid: 0,
        program: None,
        ..ask()
    };
    assert_eq!(
        Dialog::new(root).requester(),
        "a program from outside the system, as root, pid 42"
    );
    let other = Ask { uid: 5, ..ask() };
    assert_eq!(
        Dialog::new(other).requester(),
        "/bin/shell, as uid 5, pid 42"
    );
}

#[test]
fn the_command_escapes_what_could_disguise_it() {
    let shown = |command: Vec<Vec<u8>>| Ask { command, ..ask() }.command_line();
    assert_eq!(
        shown(vec![
            b"/bin/bootctl".to_vec(),
            b"install".to_vec(),
            b"b".to_vec()
        ]),
        "/bin/bootctl install b"
    );
    assert_eq!(
        shown(vec![b"x".to_vec(), b"two words".to_vec(), b"".to_vec()]),
        "x 'two words' ''"
    );
    assert_eq!(shown(vec![b"it's".to_vec()]), "'it'\\''s'");
    assert_eq!(shown(vec![b"a\\b".to_vec()]), "'a\\\\b'");
    assert_eq!(shown(vec![b"\x1b[2J".to_vec()]), "'\\x1b[2J'");
    // U+202E RIGHT-TO-LEFT OVERRIDE, which would reverse what follows.
    assert_eq!(
        shown(vec!["a\u{202e}b".as_bytes().to_vec()]),
        "'a\\xe2\\x80\\xaeb'"
    );
    assert!(shown(vec![b"\n".to_vec()]).is_ascii());
}

#[test]
fn the_directory_is_shown_apart_from_the_command() {
    let elsewhere = Ask {
        cwd: "/home/user/my src".into(),
        command: vec![b"/bin/bootctl".to_vec(), b"in".to_vec(), b"/x".to_vec()],
        ..ask()
    };
    assert_eq!(elsewhere.command_line(), "/bin/bootctl in /x");
    assert_eq!(elsewhere.directory(), "'/home/user/my src'");
    assert_eq!(ask().directory(), "/");
}

#[test]
fn a_question_too_long_to_show_whole_is_malformed() {
    let fitting = Ask {
        command: vec![vec![b'a'; MAX_SHOWN]],
        ..ask()
    };
    assert_eq!(fitting.command_line().len(), MAX_SHOWN);
    assert_eq!(Ask::decode(&payload(&fitting.frame()).1), Ok(fitting));
    let over = Ask {
        command: vec![vec![b'a'; MAX_SHOWN + 1]],
        ..ask()
    };
    assert_eq!(Ask::decode(&payload(&over.frame()).1), Err(Malformed));
    let escaped = Ask {
        command: vec![vec![0x1b; MAX_SHOWN / 4]],
        ..ask()
    };
    assert_eq!(Ask::decode(&payload(&escaped.frame()).1), Err(Malformed));
    let deep = Ask {
        cwd: alloc::format!("/{}", "d".repeat(MAX_SHOWN)),
        ..ask()
    };
    assert!(!deep.fits());
    assert_eq!(Ask::decode(&payload(&deep.frame()).1), Err(Malformed));
}
