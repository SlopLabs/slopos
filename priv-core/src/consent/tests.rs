use super::*;
use crate::wire::deframer;
use alloc::vec;
use slopos_abi::fs::SESSION_UID;

fn ask() -> Ask {
    Ask {
        id: 7,
        action: "org.slopos.boot.install".into(),
        describe: "install a new kernel".into(),
        program: Some("/bin/shell".into()),
        uid: SESSION_UID,
        pid: 42,
        cwd: "/".into(),
        command: vec![b"/bin/bootctl".to_vec(), b"install".to_vec(), b"b".to_vec()],
        kind: Kind::Confirm,
        person: Some("user".into()),
        admins: vec![],
        app: Some("Terminal".into()),
        icon: Some("/usr/share/slopos/icons/org.slopos.terminal.png".into()),
        detail: Some("The kernel that starts by default stays.".into()),
        verb: Some("Install".into()),
        destructive: false,
        extension: None,
    }
}

fn admin_ask() -> Ask {
    Ask {
        kind: Kind::Admin,
        person: Some("bob".into()),
        admins: vec!["alice".into(), "dave".into()],
        uid: 1002,
        ..ask()
    }
}

fn typed(text: &str) -> Secret {
    let mut secret = Secret::default();
    text.chars().for_each(|ch| assert!(secret.push(ch)));
    secret
}

fn allow(id: u64) -> Answer {
    Answer {
        id,
        allow: true,
        secret: None,
        admin: None,
    }
}

fn payload(framed: &[u8]) -> (u8, Vec<u8>) {
    let mut d = deframer();
    d.push(framed);
    d.next_frame().unwrap().unwrap()
}

#[test]
fn a_question_and_its_answer_round_trip() {
    let (kind, body) = payload(&ask().frame(VERSION));
    assert_eq!(kind, kind::ASK);
    assert_eq!(Ask::decode(&body), Ok(ask()));
    let outsider = Ask {
        program: None,
        person: None,
        ..ask()
    };
    assert_eq!(
        Ask::decode(&payload(&outsider.frame(VERSION)).1),
        Ok(outsider)
    );
    assert_eq!(
        Ask::decode(&payload(&admin_ask().frame(VERSION)).1),
        Ok(admin_ask())
    );
    for answer in [
        allow(9),
        Answer::deny(9),
        Answer {
            secret: Some(typed("p\u{e4}ss w0rd")),
            admin: Some("alice".into()),
            ..allow(9)
        },
        Answer {
            secret: Some(typed("x")),
            ..allow(9)
        },
    ] {
        let (kind, body) = payload(&answer.frame());
        assert_eq!(kind, kind::ANSWER);
        assert_eq!(Answer::decode(&body), Ok(answer));
    }
    let (kind, body) = payload(&cancel_frame(11));
    assert_eq!((kind, decode_cancel(&body)), (kind::CANCEL, Ok(11)));
    let (kind, body) = payload(&shown_frame(12));
    assert_eq!((kind, decode_shown(&body)), (kind::SHOWN, Ok(12)));
    let (kind, body) = payload(&ready_frame(VERSION));
    assert_eq!((kind, decode_ready(&body)), (kind::READY, Ok(VERSION)));
}

/// An agent of the first version sends an empty `READY` and reads only the
/// first version's fields, and a question from a privd of that version reads
/// as one to confirm.
#[test]
fn the_first_version_is_read_and_written_as_it_was() {
    assert_eq!(decode_ready(&[]), Ok(1));
    for version in ["0", "1", "x", ""] {
        let body = Fields::new().with("version", version).encode();
        assert_eq!(decode_ready(&body), Err(Malformed), "{version:?}");
    }
    let first = payload(&admin_ask().frame(1)).1;
    let fields = Fields::decode(&first).unwrap();
    assert!(
        fields
            .keys()
            .all(|k| !["kind", "person", "admin"].contains(&k))
    );
    let old = Ask {
        person: None,
        app: None,
        icon: None,
        detail: None,
        verb: None,
        ..ask()
    };
    assert_eq!(Ask::decode(&payload(&old.frame(1)).1), Ok(old));
    let unsigned = Fields::new().with("id", "1").with("allow", "1").encode();
    assert_eq!(Answer::decode(&unsigned), Ok(allow(1)));
}

/// The third version names the app, its icon, what follows, the verb, the
/// extension and whether it destroys; the second reads none of them, and
/// tells the extension by the `describe` it was told before.
#[test]
fn the_third_version_names_the_app_and_the_second_reads_as_it_did() {
    let full = Ask {
        destructive: true,
        extension: Some("act".into()),
        icon: Some("/opt/act/share/slopos/icons/act.png".into()),
        ..ask()
    };
    assert_eq!(Ask::decode(&payload(&full.frame(3)).1), Ok(full.clone()));
    let unnamed = Ask {
        verb: None,
        ..full.clone()
    };
    assert_eq!(
        Ask::decode(&payload(&unnamed.frame(3)).1).map(|a| a.verb),
        Ok(Some("Allow".into())),
        "a question of the third version always names its verb"
    );
    let second = Ask::decode(&payload(&full.frame(2)).1).unwrap();
    assert_eq!(
        second,
        Ask {
            describe: "install a new kernel (extension act)".into(),
            app: None,
            icon: None,
            detail: None,
            verb: None,
            destructive: false,
            extension: None,
            ..full.clone()
        }
    );
    let base = Ask {
        extension: None,
        ..full
    };
    let second = Ask::decode(&payload(&base.frame(2)).1).unwrap();
    assert_eq!(second.describe, base.describe);
}

#[test]
fn what_names_the_app_is_held_to_its_shape() {
    for good in [
        "/usr/share/slopos/icons/org.slopos.terminal.png",
        "/opt/act/share/slopos/icons/a.png",
    ] {
        assert!(icon_path(good), "{good}");
    }
    for bad in [
        "usr/share/slopos/icons/a.png",
        "/usr/local/share/slopos/icons/a.png",
        "/home/user/.local/share/slopos/icons/a.png",
        "/usr/share/slopos/icons/../../../etc/shadow.png",
        "/usr/share/slopos/icons//a.png",
        "/usr/share/slopos/icons/a.svg",
        "/usr/share/slopos/icons/.png",
        "/opt/Act/share/slopos/icons/a.png",
        "/opt/act/share/slopos/icons/sub/a.png",
    ] {
        assert!(!icon_path(bad), "{bad}");
    }
    assert!(valid_app("Install SlopOS"));
    assert!(valid_app("Caf\u{e9}"));
    for bad in [
        "",
        "a\u{1b}[31m",
        "line\nbreak",
        &"x".repeat(APP_MAX + 1),
        "Files\u{202e}txt.exe",
        "Sys\u{200b}tem Monitor",
        "\u{2067}Terminal",
    ] {
        assert!(!valid_app(bad), "{bad:?}");
    }
    for bad in [
        Ask {
            icon: Some("/tmp/a.png".into()),
            ..ask()
        },
        Ask {
            app: Some("\u{7}".into()),
            ..ask()
        },
        Ask {
            extension: Some("Act".into()),
            ..ask()
        },
        Ask {
            verb: Some(String::new()),
            ..ask()
        },
        Ask {
            detail: Some("caf\u{e9}.".into()),
            ..ask()
        },
    ] {
        assert_eq!(
            Ask::decode(&payload(&bad.frame(3)).1),
            Err(Malformed),
            "{bad:?}"
        );
    }
    let mut body = payload(&ask().frame(3)).1;
    body.extend(Fields::new().with("destructive", "0").encode());
    assert_eq!(Ask::decode(&body), Err(Malformed));
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
    let arg = || base().with("arg", "/bin/x");
    for fields in [
        base(),
        arg().with("extra", "1"),
        arg().with("kind", "consent"),
        arg().with("kind", "confirm").with("kind", "confirm"),
        arg().with("kind", "admin"),
        arg().with("kind", "admin").with("admin", "a b"),
        arg().with("kind", "confirm").with("admin", "alice"),
        arg().with("kind", "password"),
        arg().with("person", ""),
        arg().with("person", "x".repeat(33)),
        arg().with("person", "a").with("person", "b"),
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
    let allowing = || Fields::new().with("id", "1").with("allow", "1");
    for fields in [
        Fields::new()
            .with("id", "1")
            .with("allow", "0")
            .with("secret", "x"),
        allowing().with("secret", ""),
        allowing().with("secret", "a\nb"),
        allowing().with("secret", "a\0b"),
        allowing().with("secret", [0xffu8]),
        allowing().with("secret", vec![b'a'; SECRET_MAX + 1]),
        allowing().with("secret", "x").with("secret", "y"),
        allowing().with("as", "alice"),
        allowing().with("secret", "x").with("as", ""),
    ] {
        assert_eq!(
            Answer::decode(&fields.encode()),
            Err(Malformed),
            "{fields:?}"
        );
    }
    let longest = allowing().with("secret", vec![b'a'; SECRET_MAX]).encode();
    assert!(Answer::decode(&longest).is_ok());
    let mutated = payload(&admin_ask().frame(VERSION)).1;
    for at in 0..mutated.len() {
        let mut bytes = mutated.clone();
        bytes[at] ^= 0x20;
        let _ = Ask::decode(&bytes);
        let _ = Ask::decode(&bytes[..at]);
    }
}

/// A dialog drawn at 0, so armed from [`ARMING_MS`].
fn drawn() -> Dialog {
    shown(ask())
}

fn shown(ask: Ask) -> Dialog {
    let mut dialog = Dialog::new(ask);
    assert!(dialog.drawn(0));
    assert!(!dialog.drawn(5));
    dialog
}

fn allows(answer: Option<Answer>) -> Option<bool> {
    answer.map(|a| a.allow)
}

#[test]
fn focus_starts_on_deny_so_a_stray_enter_refuses() {
    let mut dialog = drawn();
    assert_eq!(dialog.focus(), Focus::Deny);
    assert_eq!(dialog.key(Key::Press, ARMING_MS), Some(Answer::deny(7)));
}

#[test]
fn allow_takes_a_move_then_a_press() {
    let mut dialog = drawn();
    assert_eq!(dialog.key(Key::Move, ARMING_MS), None);
    assert_eq!(dialog.focus(), Focus::Allow);
    assert_eq!(dialog.key(Key::Press, 2 * ARMING_MS), Some(allow(7)));
    let mut back = drawn();
    back.key(Key::Move, ARMING_MS);
    back.key(Key::Move, 2 * ARMING_MS);
    assert_eq!(back.focus(), Focus::Details);
    back.key(Key::Move, 3 * ARMING_MS);
    assert_eq!(allows(back.key(Key::Press, 4 * ARMING_MS)), Some(false));
}

#[test]
fn input_before_the_dialog_is_drawn_or_armed_counts_for_nothing() {
    let mut unseen = Dialog::new(ask());
    assert_eq!(unseen.key(Key::Press, u64::MAX), None);
    assert_eq!(clicked(&mut unseen, Focus::Allow, u64::MAX), None);
    let mut dialog = drawn();
    dialog.drawn(10 * ARMING_MS);
    for key in [Key::Move, Key::Press, Key::Dismiss] {
        assert_eq!(dialog.key(key, ARMING_MS - 1), None);
    }
    assert_eq!(dialog.focus(), Focus::Deny);
    assert_eq!(clicked(&mut dialog, Focus::Allow, ARMING_MS - 1), None);
    assert_eq!(
        clicked(&mut dialog, Focus::Allow, ARMING_MS),
        Some(allow(7))
    );
    let mut field = shown(admin_ask());
    field.key(Key::Text('x'), ARMING_MS - 1);
    assert_eq!(
        field.typed_chars(),
        0,
        "what was typed for what was there before"
    );
}

#[test]
fn a_press_just_after_a_move_counts_for_nothing() {
    let mut dialog = drawn();
    dialog.key(Key::Move, ARMING_MS);
    assert_eq!(dialog.key(Key::Press, 2 * ARMING_MS - 1), None);
    assert_eq!(clicked(&mut dialog, Focus::Allow, 2 * ARMING_MS - 1), None);
    assert_eq!(dialog.focus(), Focus::Allow);
    assert_eq!(allows(dialog.key(Key::Press, 2 * ARMING_MS)), Some(true));
}

#[test]
fn dismissing_denies_whatever_has_focus() {
    let mut dialog = drawn();
    dialog.key(Key::Move, ARMING_MS);
    assert_eq!(allows(dialog.key(Key::Dismiss, 2 * ARMING_MS)), Some(false));
    let mut typing = shown(admin_ask());
    typing.key(Key::Text('a'), ARMING_MS);
    assert_eq!(
        typing.key(Key::Dismiss, ARMING_MS),
        Some(Answer::deny(7)),
        "and what was typed goes nowhere"
    );
}

fn clicked(dialog: &mut Dialog, target: Focus, at: u64) -> Option<Answer> {
    dialog.pointer_down(at);
    dialog.click(target, at)
}

/// The pointer is pressed while input waits and let go once it no longer
/// does, or pressed and the dialog moves before it is let go: neither
/// press answers.
#[test]
fn a_click_counts_only_from_a_press_the_armed_dialog_took() {
    let mut early = drawn();
    assert!(!early.pointer_down(ARMING_MS - 1));
    assert_eq!(early.click(Focus::Allow, ARMING_MS), None);
    let mut moved = drawn();
    assert!(moved.pointer_down(ARMING_MS));
    moved.moved(ARMING_MS + 1);
    assert_eq!(moved.click(Focus::Allow, 3 * ARMING_MS), None);
    let mut tabbed = drawn();
    tabbed.pointer_down(ARMING_MS);
    tabbed.key(Key::Move, ARMING_MS);
    assert_eq!(tabbed.click(Focus::Allow, 3 * ARMING_MS), None);
    assert!(tabbed.pointer_down(3 * ARMING_MS));
    assert_eq!(
        allows(tabbed.click(Focus::Allow, 3 * ARMING_MS)),
        Some(true)
    );
}

#[test]
fn a_click_answers_with_the_button_clicked() {
    let mut dialog = drawn();
    assert_eq!(
        allows(clicked(&mut dialog, Focus::Allow, ARMING_MS)),
        Some(true)
    );
    assert_eq!(
        allows(clicked(&mut dialog, Focus::Deny, ARMING_MS)),
        Some(false)
    );
    assert_eq!(clicked(&mut dialog, Focus::Field, ARMING_MS), None);
    assert_eq!(dialog.focus(), Focus::Deny, "a question without a field");
    assert_eq!(dialog.key(Key::Press, ARMING_MS), Some(Answer::deny(7)));
}

#[test]
fn a_password_is_typed_into_its_field_and_allows_once_there_is_one() {
    let ask = Ask {
        kind: Kind::Password,
        person: Some("bob".into()),
        ..ask()
    };
    let mut dialog = shown(ask);
    assert_eq!(dialog.focus(), Focus::Field);
    assert_eq!(
        dialog.key(Key::Press, ARMING_MS),
        None,
        "a stray Enter in an empty field does nothing"
    );
    assert_eq!(clicked(&mut dialog, Focus::Allow, ARMING_MS), None);
    for ch in "s\u{e9}cret!".chars() {
        assert_eq!(dialog.key(Key::Text(ch), ARMING_MS), None);
    }
    dialog.key(Key::Text('\n'), ARMING_MS);
    dialog.key(Key::Text('\u{7}'), ARMING_MS);
    dialog.key(Key::Erase, ARMING_MS);
    assert_eq!(dialog.typed_chars(), 6);
    dialog.key(Key::Arrow, ARMING_MS);
    assert_eq!(
        dialog.focus(),
        Focus::Field,
        "an arrow in the field moves nothing"
    );
    dialog.key(Key::Move, ARMING_MS);
    assert_eq!(dialog.focus(), Focus::Details);
    dialog.key(Key::Arrow, 2 * ARMING_MS);
    assert_eq!(
        dialog.focus(),
        Focus::Details,
        "an arrow moves only between buttons"
    );
    dialog.key(Key::Move, 2 * ARMING_MS);
    assert_eq!(dialog.focus(), Focus::Deny);
    dialog.key(Key::Arrow, 3 * ARMING_MS);
    assert_eq!(dialog.focus(), Focus::Allow);
    dialog.key(Key::Arrow, 4 * ARMING_MS);
    assert_eq!(dialog.focus(), Focus::Deny);
    dialog.key(Key::Text('?'), 5 * ARMING_MS);
    assert_eq!(dialog.focus(), Focus::Field, "typing takes focus back");
    let answer = dialog.key(Key::Press, 5 * ARMING_MS).unwrap();
    assert_eq!(answer.secret, Some(typed("s\u{e9}cret?")));
    assert_eq!((answer.allow, answer.admin), (true, None));
    assert_eq!(dialog.typed_chars(), 0, "the answer took the password");
}

#[test]
fn an_administrator_is_chosen_and_types_their_own_password() {
    let mut dialog = shown(admin_ask());
    assert_eq!(dialog.admin(), Some(0));
    assert_eq!(dialog.field_label().as_deref(), Some("Password for alice"));
    dialog.key(Key::Text('a'), ARMING_MS);
    dialog.key(Key::Choose { next: true }, ARMING_MS);
    assert_eq!(dialog.admin(), Some(1));
    assert_eq!(dialog.field_label().as_deref(), Some("Password for dave"));
    assert_eq!(dialog.typed_chars(), 0, "a password typed for another goes");
    assert_eq!(dialog.key(Key::Text('d'), 2 * ARMING_MS - 1), None);
    assert_eq!(dialog.typed_chars(), 0);
    dialog.key(Key::Choose { next: true }, 2 * ARMING_MS);
    dialog.key(Key::Choose { next: false }, 3 * ARMING_MS);
    assert_eq!(dialog.admin(), Some(1));
    dialog.key(Key::Text('d'), 4 * ARMING_MS);
    for at in 4..7 {
        dialog.key(Key::Move, at * ARMING_MS);
    }
    assert_eq!(dialog.focus(), Focus::Allow);
    let answer = clicked(&mut dialog, Focus::Allow, 7 * ARMING_MS).unwrap();
    assert_eq!(answer.admin.as_deref(), Some("dave"));
    assert_eq!(answer.secret, Some(typed("d")));
    let mut alone = shown(Ask {
        admins: vec!["alice".into()],
        ..admin_ask()
    });
    alone.key(Key::Text('a'), ARMING_MS);
    alone.key(Key::Choose { next: true }, ARMING_MS);
    assert_eq!((alone.admin(), alone.typed_chars()), (Some(0), 1));
    let mut picked = shown(admin_ask());
    picked.key(Key::Text('a'), ARMING_MS);
    picked.pointer_down(ARMING_MS);
    picked.pick(1, ARMING_MS);
    assert_eq!((picked.admin(), picked.typed_chars()), (Some(1), 0));
    picked.pointer_down(2 * ARMING_MS - 1);
    picked.pick(0, 2 * ARMING_MS - 1);
    picked.pointer_down(2 * ARMING_MS);
    picked.pick(7, 2 * ARMING_MS);
    assert_eq!(picked.admin(), Some(1), "a click too soon or on no one");
    assert_eq!(drawn().admin(), None);
}

#[test]
fn a_password_field_holds_only_so_much() {
    let mut secret = Secret::default();
    while secret.push('\u{20ac}') {}
    assert!(secret.bytes().len() <= SECRET_MAX);
    assert!(secret.bytes().len() + 3 > SECRET_MAX);
    assert_eq!(secret.chars(), secret.bytes().len() / 3);
    secret.pop();
    assert!(core::str::from_utf8(secret.bytes()).is_ok());
    secret.clear();
    assert!(secret.is_empty());
    assert_eq!(alloc::format!("{:?}", typed("hidden")), "Secret(6 bytes)");
}

#[test]
fn the_dialog_names_who_asks() {
    let dialog = Dialog::new(ask());
    assert_eq!(dialog.requester(), "/bin/shell as user (pid 42)");
    assert_eq!(dialog.headline(), "Terminal wants to install a new kernel.");
    assert_eq!(dialog.verb(), "Install");
    let root = Ask {
        uid: 0,
        program: None,
        person: None,
        app: None,
        verb: Some("Cancel".into()),
        describe: "restart the computer".into(),
        ..ask()
    };
    let root = Dialog::new(root);
    assert_eq!(
        root.requester(),
        "A program outside the system as root (pid 42)"
    );
    assert_eq!(
        root.headline(),
        "An unidentified program wants to restart the computer."
    );
    assert_eq!(root.verb(), "Allow", "no verb says what Cancel says");
    let older = Dialog::new(Ask {
        verb: None,
        describe: "Restart the machine".into(),
        ..ask()
    });
    assert!(!older.names_app());
    assert_eq!(older.headline(), "Allow: Restart the machine?");
    assert_eq!(older.verb(), "Allow");
    let other = Ask {
        uid: 5,
        person: None,
        ..ask()
    };
    assert_eq!(
        Dialog::new(other).requester(),
        "/bin/shell as uid 5 (pid 42)"
    );
    assert_eq!(Dialog::new(ask()).field_label(), None);
    let password = Ask {
        kind: Kind::Password,
        person: Some("bob".into()),
        ..ask()
    };
    assert_eq!(
        Dialog::new(password).field_label().as_deref(),
        Some("Password for bob")
    );
}

/// Enter in the field of a destructive question only moves to the button,
/// which a second Enter, once armed again, presses; a question that moved
/// on the screen waits again.
#[test]
fn enter_in_the_field_never_destroys() {
    let mut dialog = shown(Ask {
        destructive: true,
        ..admin_ask()
    });
    assert_eq!(dialog.key(Key::Press, ARMING_MS), None);
    assert_eq!(dialog.focus(), Focus::Field, "nothing typed, nothing moves");
    dialog.key(Key::Text('a'), ARMING_MS);
    assert_eq!(dialog.key(Key::Press, ARMING_MS), None);
    assert_eq!(dialog.focus(), Focus::Allow);
    assert_eq!(dialog.key(Key::Press, 2 * ARMING_MS - 1), None);
    let answer = dialog.key(Key::Press, 2 * ARMING_MS).unwrap();
    assert_eq!(
        (answer.allow, answer.admin.as_deref()),
        (true, Some("alice"))
    );
    let mut moving = drawn();
    moving.moved(ARMING_MS);
    assert_eq!(clicked(&mut moving, Focus::Allow, 2 * ARMING_MS - 1), None);
    assert_eq!(
        allows(clicked(&mut moving, Focus::Allow, 2 * ARMING_MS)),
        Some(true)
    );
    let mut unseen = Dialog::new(ask());
    unseen.moved(10);
    assert!(unseen.drawn(20), "a move before it is drawn arms nothing");
}

/// The details start shown; Tab reaches their toggle, and Enter on it or a
/// click shows or hides them, after which input waits again as it does
/// after a move, since the buttons moved.
#[test]
fn the_details_show_until_they_are_hidden() {
    let mut dialog = drawn();
    assert!(dialog.expanded());
    for at in 1..3 {
        dialog.key(Key::Move, at * ARMING_MS);
    }
    assert_eq!(dialog.focus(), Focus::Details);
    assert_eq!(dialog.key(Key::Press, 3 * ARMING_MS), None);
    assert!(!dialog.expanded());
    assert_eq!(dialog.key(Key::Press, 4 * ARMING_MS - 1), None);
    assert!(!dialog.expanded(), "a press too soon after the toggle");
    assert_eq!(clicked(&mut dialog, Focus::Allow, 4 * ARMING_MS - 1), None);
    assert_eq!(clicked(&mut dialog, Focus::Details, 4 * ARMING_MS), None);
    assert!(dialog.expanded());
    assert_eq!(dialog.focus(), Focus::Details);
    assert_eq!(allows(dialog.key(Key::Dismiss, 5 * ARMING_MS)), Some(false));
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
    assert_eq!(
        Ask::decode(&payload(&fitting.frame(VERSION)).1),
        Ok(fitting)
    );
    let over = Ask {
        command: vec![vec![b'a'; MAX_SHOWN + 1]],
        ..ask()
    };
    assert_eq!(
        Ask::decode(&payload(&over.frame(VERSION)).1),
        Err(Malformed)
    );
    let escaped = Ask {
        command: vec![vec![0x1b; MAX_SHOWN / 4]],
        ..ask()
    };
    assert_eq!(
        Ask::decode(&payload(&escaped.frame(VERSION)).1),
        Err(Malformed)
    );
    let deep = Ask {
        cwd: alloc::format!("/{}", "d".repeat(MAX_SHOWN)),
        ..ask()
    };
    assert!(!deep.fits());
    assert_eq!(
        Ask::decode(&payload(&deep.frame(VERSION)).1),
        Err(Malformed)
    );
}

fn take(link: &mut Link, framed: &[u8]) -> Inbound {
    let (kind, body) = payload(framed);
    link.take(kind, body).unwrap()
}

fn reset(inbound: Inbound) -> Vec<u8> {
    match inbound {
        Inbound::Reset(seen) => seen,
        other => panic!("not a reset: {other:?}"),
    }
}

/// Every frame `link` takes from `framed`, which may hold several.
fn take_all(link: &mut Link, framed: &[u8]) -> Vec<Inbound> {
    let mut frames = deframer();
    frames.push(framed);
    let mut taken = Vec::new();
    while let Some((kind, body)) = frames.next_frame().unwrap() {
        taken.push(link.take(kind, body).unwrap());
    }
    taken
}

#[test]
fn a_new_end_acts_only_on_what_follows_the_acknowledgement_of_its_reset() {
    let mut privd = Link::new(1);
    let mut agent = Link::new(2);
    let question = ask().frame(VERSION);
    assert_eq!(take(&mut agent, &question), Inbound::Stale);
    let to_privd = reset(take(&mut agent, &privd.hello()));
    assert_eq!(take(&mut agent, &question), Inbound::Stale);
    let to_agent = reset(take(&mut privd, &agent.hello()));
    assert_eq!(take(&mut privd, &ready_frame(VERSION)), Inbound::Stale);
    assert_eq!(take(&mut agent, &to_agent), Inbound::Stale);
    assert_eq!(
        take(&mut agent, &question),
        Inbound::Frame(kind::ASK, payload(&question).1)
    );
    assert_eq!(take(&mut privd, &to_privd), Inbound::Stale);
    assert_eq!(
        take(&mut privd, &ready_frame(VERSION)),
        Inbound::Frame(kind::READY, payload(&ready_frame(VERSION)).1)
    );
}

#[test]
fn a_reset_reaches_an_end_already_talking_and_leaves_it_talking() {
    let mut privd = Link::new(1);
    let seen = reset(take(&mut Link::new(2), &privd.hello()));
    take(&mut privd, &seen);
    let mut successor = Link::new(3);
    let to_successor = reset(take(&mut privd, &successor.hello()));
    assert_eq!(
        take(&mut privd, &ready_frame(VERSION)),
        Inbound::Frame(kind::READY, payload(&ready_frame(VERSION)).1)
    );
    assert_eq!(take(&mut successor, &seen), Inbound::Stale);
    assert_eq!(take(&mut successor, &ready_frame(VERSION)), Inbound::Stale);
    take(&mut successor, &to_successor);
    assert_eq!(
        take(&mut successor, &ready_frame(VERSION)),
        Inbound::Frame(kind::READY, payload(&ready_frame(VERSION)).1)
    );
}

#[test]
fn an_end_whose_hello_went_unread_says_it_again() {
    let mut privd = Link::new(1);
    let mut agent = Link::new(2);
    let _unread = agent.hello();
    let to_privd = reset(take(&mut agent, &privd.hello()));
    let taken = take_all(&mut privd, &to_privd);
    let [Inbound::Stale, Inbound::Reset(to_agent)] = &taken[..] else {
        panic!("not an acknowledgement and a hello: {taken:?}");
    };
    assert_eq!(take_all(&mut agent, to_agent), [Inbound::Stale]);
    assert_eq!(
        take(&mut agent, &ask().frame(VERSION)),
        Inbound::Frame(kind::ASK, payload(&ask().frame(VERSION)).1)
    );
    assert_eq!(
        take(&mut privd, &ready_frame(VERSION)),
        Inbound::Frame(kind::READY, payload(&ready_frame(VERSION)).1)
    );
}

#[test]
fn a_hello_said_again_is_only_acknowledged() {
    let mut agent = Link::new(2);
    let hello = Link::new(1).hello();
    let first = reset(take(&mut agent, &hello));
    assert_eq!(take(&mut agent, &hello), Inbound::Again(first));
    assert!(matches!(
        take(&mut agent, &Link::new(3).hello()),
        Inbound::Reset(_)
    ));
}

#[test]
fn a_reset_init_sends_for_a_side_that_exited_is_one_like_any_other() {
    let mut agent = Link::new(2);
    assert!(matches!(
        take(&mut agent, &reset_frame(u64::MAX)),
        Inbound::Reset(_)
    ));
    assert_eq!(take(&mut agent, &ready_frame(VERSION)), Inbound::Stale);
}

#[test]
fn a_reset_outside_the_protocol_is_malformed() {
    let mut link = Link::new(1);
    for fields in [
        Fields::new(),
        Fields::new().with("epoch", "x"),
        Fields::new().with("epoch", "1").with("epoch", "2"),
        Fields::new().with("epoch", "1").with("extra", "1"),
    ] {
        let body = fields.encode();
        assert_eq!(link.take(kind::RESET, body.clone()), Err(Malformed));
        assert_eq!(link.take(kind::RESET_SEEN, body), Err(Malformed));
    }
}
