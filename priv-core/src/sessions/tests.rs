use super::*;
use alloc::vec;

fn s(tag: u32, uid: u32, class: Class, front: bool, locked: bool, link: Option<u8>) -> Session {
    Session {
        tag,
        uid,
        class,
        front,
        locked,
        link,
    }
}

fn payload(frame: &[u8]) -> Vec<u8> {
    let mut deframer = crate::wire::deframer();
    deframer.push(frame);
    match deframer.next_frame() {
        Ok(Some((kind::SESSIONS, payload))) => payload,
        other => panic!("{other:?}"),
    }
}

fn two_desktops() -> Table {
    Table {
        sessions: vec![
            s(0x10_0000, 1000, Class::User, true, false, Some(0)),
            s(0x10_0001, 1001, Class::User, false, true, Some(1)),
            s(0x10_0002, 1001, Class::Console, false, false, None),
            s(2, 1000, Class::Background, false, false, None),
        ],
    }
}

#[test]
fn a_table_round_trips_through_its_frame() {
    let table = two_desktops();
    assert_eq!(Table::decode(&payload(&table.frame())), Ok(table));
    assert_eq!(
        Table::decode(&payload(&Table::default().frame())),
        Ok(Table::default())
    );
}

#[test]
fn only_a_table_init_could_mean_is_read() {
    let bad = [
        "1 1000 user 1 0 0 extra",
        "1 1000 user 1 0",
        "01 1000 user 1 0 0",
        "1 1000 nobody 1 0 0",
        "1 1000 user 2 0 0",
        "1 1000 user 1 0 4",
        "1 1000 console 1 0 -",
        "1 1000 user 1 0 +1",
    ];
    for text in bad {
        let frame = frame(
            kind::SESSIONS,
            &Fields::new().with("session", text).encode(),
        );
        assert_eq!(Table::decode(&payload(&frame)), Err(Malformed), "{text}");
    }
    for twice in [
        ["1 1000 user 1 0 0", "2 1001 user 1 0 1"],
        ["1 1000 user 0 0 0", "1 1001 user 0 0 1"],
        ["1 1000 user 0 0 0", "2 1001 user 0 0 0"],
    ] {
        let fields = Fields::new()
            .with("session", twice[0])
            .with("session", twice[1]);
        let frame = frame(kind::SESSIONS, &fields.encode());
        assert_eq!(Table::decode(&payload(&frame)), Err(Malformed), "{twice:?}");
    }
    let other = frame(kind::SESSIONS, &Fields::new().with("front", "1").encode());
    assert_eq!(Table::decode(&payload(&other)), Err(Malformed));
}

#[test]
fn only_a_session_in_front_and_unlocked_or_at_its_own_terminal_is_reached() {
    let table = two_desktops();
    assert_eq!(
        table.reach(0x10_0000, 1000, false),
        (Reach::Screen { theirs: true }, Some(0))
    );
    assert_eq!(
        table.reach(0x10_0000, 1001, false),
        (Reach::Screen { theirs: false }, Some(0)),
        "a process of another uid in the session in front is not its person"
    );
    assert_eq!(
        table.reach(0x10_0001, 1001, true),
        (Reach::Nowhere, None),
        "a desktop behind, and locked, is asked nothing"
    );
    assert_eq!(table.reach(0x10_0002, 1001, true), (Reach::Terminal, None));
    assert_eq!(
        table.reach(0x10_0002, 1001, false),
        (Reach::Nowhere, None),
        "a requester that cannot ask at its terminal is not reached there"
    );
    assert_eq!(table.reach(0x10_0002, 1000, true), (Reach::Nowhere, None));
    assert_eq!(table.reach(2, 1000, true), (Reach::Nowhere, None));
    assert_eq!(table.reach(0, 1000, true), (Reach::Nowhere, None));
    let mut unlocked = two_desktops();
    unlocked.sessions[0].locked = true;
    assert_eq!(
        unlocked.reach(0x10_0000, 1000, false),
        (Reach::Nowhere, None)
    );
}

#[test]
fn a_person_is_logged_in_at_a_desktop_or_a_terminal() {
    assert_eq!(two_desktops().logged_in(), vec![1000, 1001, 1001]);
}
