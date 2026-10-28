use super::*;
use alloc::vec;

fn request() -> Request {
    Request {
        id: 77,
        action: "org.slopos.boot.install".into(),
        args: vec![b"install".to_vec(), b"b".to_vec(), vec![0xff, b' ', b'\n']],
        cwd: true,
        terminal: true,
    }
}

fn payload(framed: &[u8]) -> (u8, Vec<u8>) {
    let mut d = deframer();
    d.push(framed);
    d.next_frame().unwrap().unwrap()
}

#[test]
fn a_request_round_trips_and_counts_its_descriptors() {
    let (kind, body) = payload(&request().frame());
    assert_eq!(kind, kind::REQUEST);
    let back = Request::decode(&body).unwrap();
    assert_eq!(back, request());
    assert_eq!(back.descriptors(), 4);
    assert_eq!(back.args()[0], b"install");
    let plain = Request {
        cwd: false,
        terminal: false,
        args: vec![],
        ..request()
    };
    assert_eq!(Request::decode(&payload(&plain.frame()).1).unwrap(), plain);
    assert_eq!(plain.descriptors(), STDIO);
}

#[test]
fn a_request_outside_the_protocol_is_malformed() {
    let refuse = |fields: Fields| {
        assert_eq!(
            Request::decode(&fields.encode()),
            Err(Malformed),
            "{fields:?}"
        );
    };
    let base = || Fields::new().with("id", "1").with("action", "org.x");
    assert!(Request::decode(&base().encode()).is_ok());
    refuse(Fields::new());
    refuse(Fields::new().with("action", "org.x"));
    refuse(Fields::new().with("id", "1").with("action", "Bad"));
    refuse(base().with("action", "org.y"));
    refuse(base().with("id", "2"));
    refuse(Fields::new().with("id", "0").with("action", "org.x"));
    refuse(base().with("arg", b"a\0b"));
    refuse(base().with("cwd", "0"));
    refuse(base().with("cwd", "1").with("cwd", "1"));
    refuse(base().with("terminal", "0"));
    refuse(base().with("terminal", "1").with("terminal", "1"));
    refuse(base().with("env", "LD_PRELOAD=x"));
    refuse(base().with("arg", vec![b'a'; EXEC_MAX_ARG_STRLEN + 1]));
    let mut many = base();
    for _ in 0..=MAX_ARGS {
        many.push("arg", "a");
    }
    refuse(many);
    assert_eq!(Request::decode(&[1, b'a']), Err(Malformed));
}

#[test]
fn replies_round_trip_with_the_id_they_answer() {
    for reply in [
        Reply::Asking { app: None },
        Reply::Asking {
            app: Some("Install SlopOS".into()),
        },
        Reply::Started { pid: 42 },
        Reply::Exited(Exit::Code(0)),
        Reply::Exited(Exit::Code(-1)),
        Reply::Exited(Exit::Code(i32::MAX)),
        Reply::Exited(Exit::Signal(9)),
        Reply::Denied {
            reason: "the person said no".into(),
            by_person: true,
        },
        Reply::denied("no such action"),
    ] {
        let (kind, body) = payload(&reply.frame(u64::MAX));
        assert_eq!(Reply::decode(kind, &body), Ok((u64::MAX, reply.clone())));
    }
}

#[test]
fn a_long_reason_is_cut_on_a_character() {
    let Reply::Denied { reason, .. } = Reply::denied("\u{e9}".repeat(MAX_REASON)) else {
        panic!("not a denial");
    };
    assert!(reason.len() <= MAX_REASON);
    assert!(reason.chars().all(|c| c == '\u{e9}'));
}

#[test]
fn numbers_are_canonical_decimal() {
    for (text, ok) in [
        ("0", true),
        ("42", true),
        ("-1", false),
        ("", false),
        ("+1", false),
        ("01", false),
        ("-0", false),
        ("-", false),
        ("1 ", false),
        ("4294967296", false),
    ] {
        let fields = Fields::new().with("id", "1").with("pid", text);
        assert_eq!(
            Reply::decode(kind::STARTED, &fields.encode()).is_ok(),
            ok,
            "{text:?}"
        );
    }
    let exited = |fields: Fields| Reply::decode(kind::EXITED, &fields.encode());
    let id = || Fields::new().with("id", "1");
    assert!(exited(id().with("code", "-1")).is_ok());
    assert_eq!(
        exited(id().with("code", "1").with("code", "1")),
        Err(Malformed)
    );
    assert_eq!(
        exited(id().with("code", "1").with("signal", "9")),
        Err(Malformed)
    );
    assert_eq!(exited(Fields::new().with("code", "1")), Err(Malformed));
    assert_eq!(Reply::decode(kind::EXITED, &[]), Err(Malformed));
    let person = id().with("reason", "x").with("person", "yes");
    assert_eq!(
        Reply::decode(kind::DENIED, &person.encode()),
        Err(Malformed)
    );
    assert_eq!(Reply::decode(kind::REQUEST, &[]), Err(Malformed));
}

#[test]
fn only_the_relayed_signals_pass() {
    for signal in 0..=64u8 {
        let (kind, body) = payload(&signal_frame(5, signal));
        assert_eq!(kind, kind::SIGNAL);
        assert_eq!(
            decode_signal(&body).ok(),
            RELAYED_SIGNALS.contains(&signal).then_some((5, signal)),
            "{signal}"
        );
    }
}

#[test]
fn a_frame_past_the_bound_is_refused_before_its_payload() {
    let mut d = deframer();
    d.push(&[kind::REQUEST, 0, 1, 0, 1]);
    assert!(d.next_frame().is_err());
}

#[test]
fn mutated_requests_decode_or_are_malformed() {
    let (_, good) = payload(&request().frame());
    let check = |bytes: &[u8]| {
        if let Ok(req) = Request::decode(bytes) {
            let (_, again) = payload(&req.frame());
            assert_eq!(Request::decode(&again), Ok(req));
        }
    };
    for cut in 0..good.len() {
        check(&good[..cut]);
    }
    for at in 0..good.len() {
        for b in [0u8, 1, b'1', b'a', b'.', 0x7f, 0xff] {
            let mut bytes = good.clone();
            bytes[at] = b;
            check(&bytes);
        }
    }
    for kind in [kind::STARTED, kind::EXITED, kind::DENIED, kind::ASKING] {
        for reply in [
            Reply::Started { pid: 7 },
            Reply::Exited(Exit::Code(3)),
            Reply::denied("x"),
            Reply::Asking {
                app: Some("Files".into()),
            },
        ] {
            let (_, good) = payload(&reply.frame(9));
            for at in 0..good.len() {
                let mut bytes = good.clone();
                bytes[at] ^= 0x41;
                let _ = Reply::decode(kind, &bytes);
                let _ = Reply::decode(kind, &bytes[..at]);
            }
        }
    }
}

#[test]
fn a_prompt_and_its_secret_round_trip_and_nothing_else_does() {
    let prompt = Reply::Prompt {
        about: "bootctl wants to install a new kernel.".into(),
        prompt: "Password for user: ".into(),
    };
    let (kind, body) = payload(&prompt.frame(9));
    assert_eq!(kind, kind::PROMPT);
    assert_eq!(Reply::decode(kind, &body), Ok((9, prompt)));
    for bad in ["\x1b[2J", "a\nb"] {
        let fields = Fields::new().with("id", "9").with("prompt", bad);
        assert_eq!(
            Reply::decode(kind::PROMPT, &fields.encode()),
            Err(Malformed)
        );
        let about = Fields::new()
            .with("id", "9")
            .with("prompt", "Password: ")
            .with("about", bad);
        assert_eq!(Reply::decode(kind::PROMPT, &about.encode()), Err(Malformed));
    }
    let unexplained = Fields::new().with("id", "9").with("prompt", "Password: ");
    assert_eq!(
        Reply::decode(kind::PROMPT, &unexplained.encode()),
        Err(Malformed)
    );
    let extra = Fields::new()
        .with("id", "9")
        .with("prompt", "Password: ")
        .with("about", "x")
        .with("hint", "y");
    assert_eq!(Reply::decode(kind::PROMPT, &extra.encode()), Err(Malformed));
    let unnamed = Fields::new().with("id", "9").with("app", "");
    assert_eq!(
        Reply::decode(kind::ASKING, &unnamed.encode()),
        Err(Malformed)
    );
    let long = "x".repeat(MAX_PROMPT + 1);
    let fields = Fields::new().with("id", "9").with("prompt", long);
    assert_eq!(
        Reply::decode(kind::PROMPT, &fields.encode()),
        Err(Malformed)
    );

    let (kind, body) = payload(&secret_frame(9, b"hunter2"));
    assert_eq!(kind, kind::SECRET);
    let (id, secret) = decode_secret(&body).unwrap();
    assert_eq!((id, secret.bytes()), (9, &b"hunter2"[..]));
    let (_, none) = decode_secret(&payload(&secret_frame(9, b"")).1).unwrap();
    assert!(none.is_empty(), "a person without a password types nothing");
    for bad in [&b"a\nb"[..], b"a\0b", &[0xff]] {
        assert_eq!(
            decode_secret(&payload(&secret_frame(9, bad)).1).map(|(id, _)| id),
            Err(Malformed)
        );
    }
    let twice = Fields::new()
        .with("id", "9")
        .with("secret", "a")
        .with("secret", "b");
    assert_eq!(
        decode_secret(&twice.encode()).map(|(id, _)| id),
        Err(Malformed)
    );
}
