use super::*;
use alloc::vec;

const KEY: &str = "SHA256:bbXpuKG6zhzdmnxq256TlqzFBzRl2f6OOg722cYNbU8";

fn record() -> Record {
    let image = |version, byte| Installed {
        version,
        sha256: [byte; 32],
        signer: Some(KEY.into()),
        grant: Grant {
            caps: slopos_abi::caps::NET_ADMIN,
            unasked: Vec::new(),
            users: vec!["hello".into()],
            commands: vec!["hello".into()],
        },
    };
    Record {
        name: "hello".into(),
        enabled: true,
        signer: Some(KEY.into()),
        floor: Some((3, [3; 32])),
        refused: vec![4, 5],
        uids: vec![("hello".into(), 100), ("hello-worker".into(), 107)],
        config: 2,
        why: None,
        current: Some(image(3, 3)),
        previous: Some(Installed {
            signer: None,
            ..image(2, 2)
        }),
    }
}

#[test]
fn a_record_reads_back_as_written() {
    for record in [
        record(),
        Record::new("bare"),
        Record {
            current: None,
            previous: None,
            ..record()
        },
        {
            let mut stopped = record();
            stopped.stop("its key is no \"longer\" trusted\n");
            stopped
        },
        Record {
            uids: Vec::new(),
            ..record()
        },
        Record {
            config: 0,
            ..record()
        },
    ] {
        let text = record.render();
        assert_eq!(Record::parse(&text), Some(record), "{text}");
    }
}

#[test]
fn a_stopped_record_says_why_in_printable_words() {
    let mut stopped = record();
    stopped.stop("bad\tkey \"x\"");
    assert!(!stopped.enabled);
    assert_eq!(stopped.why.as_deref(), Some("bad?key ?x?"));
    stopped.stop("   ");
    assert_eq!(stopped.why, None);
}

#[test]
fn any_other_spelling_is_refused() {
    let text = record().render();
    for (from, to) in [
        ("enabled yes", "enabled maybe"),
        ("name hello", "name etc"),
        ("refused 4,5", "refused 4,,5"),
        ("refused 4,5\n", ""),
        ("image current", "image spare"),
        ("  version 3", "  version 3 "),
        ("  version 3", " version 3"),
        ("signer SHA256:bbX", "signer SHA256:bb"),
        ("floor 3 ", "floor 03 "),
        ("users hello", "users hello,hello"),
        ("config 2", "config 02"),
        ("config 2", "config 0"),
        ("config 2\n", "config 2\nconfig 3\n"),
    ] {
        let other = text.replacen(from, to, 1);
        assert_ne!(other, text, "{from}");
        assert_eq!(Record::parse(&other), None, "{from} -> {to}");
    }
    let previous_first = text
        .replace("image current", "image swap")
        .replace("image previous", "image current")
        .replace("image swap", "image previous");
    assert_eq!(Record::parse(&previous_first), None);
    assert_eq!(Record::parse(&format!("{text}image current\n")), None);
}

#[test]
fn a_fingerprint_is_sha256_and_43_digits() {
    assert!(valid_fingerprint(KEY));
    assert!(!valid_fingerprint(&KEY.replace("SHA256", "MD5")));
    assert!(!valid_fingerprint(&format!("{KEY}=")));
    assert!(!valid_fingerprint(&KEY[..40]));
}

#[test]
fn a_uid_is_in_range_and_held_once() {
    let text = record().render();
    assert!(
        text.contains("\nuids hello=100,hello-worker=107\n"),
        "{text}"
    );
    for bad in [
        "uids hello=99,hello-worker=107",
        "uids hello=1000,hello-worker=107",
        "uids hello=100,hello-worker=100",
        "uids hello=100,hello=107",
        "uids Hello=100,hello-worker=107",
        "uids hello=0100,hello-worker=107",
        "uids hello=100,",
        "uids -",
    ] {
        let text = text.replace("uids hello=100,hello-worker=107", bad);
        assert_eq!(Record::parse(&text), None, "{bad}");
    }
    let mut removed = record();
    removed.remove();
    assert_eq!(removed.number(), Some(100));
    assert_eq!(removed.uid("hello-worker"), Some(107));
}
