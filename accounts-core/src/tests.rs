use alloc::string::{String, ToString};
use alloc::vec::Vec;

use slopos_abi::fs::{ADMIN_GID, SESSION_HOME, SESSION_UID};
use slopos_slibc_core::passwd::{BASE_GROUP, BASE_PASSWD, Group, Passwd, lines};

use super::*;
use crate::record::{DESKTOP_PERSON, LAST_ADMIN, NAME_HELD, NO_SUCH_PERSON, NO_UIDS};

const HASH: &str =
    "$argon2id$v=19$m=256,t=2,p=1$c29tZXNhbHQ$nf65EOgLrQMR/uIPnA4rEsF5h7TKyQwu9U1bMCHGi/4";

fn extension(name: &str, uid: u32) -> Entry {
    Entry {
        name: name.to_string(),
        uid,
        gecos: "hello extension".to_string(),
        home: "/var/lib/hello".to_string(),
    }
}

#[test]
fn the_one_person_database_is_the_c_librarys_own() {
    let one = Accounts::one_person();
    assert_eq!(passwd(&one, &[]), BASE_PASSWD);
    assert_eq!(group(&one, &[]), BASE_GROUP);
    assert_eq!(shadow(&one), "user::::::::\n");
    let user = one.autologin().expect("logged in by the desktop");
    assert_eq!((user.uid, user.home.as_str()), (SESSION_UID, SESSION_HOME));
    assert!(user.admin && user.password.is_none());
    assert_eq!(Accounts::parse(&one.render()), Some(one));
}

#[test]
fn a_record_reads_back_in_its_one_spelling() {
    let mut accounts = Accounts::first("ada", Some(HASH.to_string())).unwrap();
    assert_eq!(accounts.add("bob", false), Ok(1001));
    accounts.set_autologin(None).unwrap();
    let text = accounts.render();
    assert_eq!(
        text,
        alloc::format!(
            "next 1002\nautologin -\nperson 1000 ada /home/user admin {HASH}\nperson 1001 bob /home/bob - -\n"
        )
    );
    assert_eq!(Accounts::parse(&text).as_ref(), Some(&accounts));
    for bad in [
        text.replace("next 1002", "next 1001"),
        text.replace("next 1002", "next 01002"),
        text.replace(" admin ", " - "),
        text.replace("bob /home/bob", "ada /home/bob"),
        text.replace("1001 bob", "1000 bob"),
        text.replace("1001 bob", "999 bob"),
        text.replace("/home/bob", "home/bob"),
        text.replace("/home/bob", "/home/b:b"),
        text.replace("bob", "Bob"),
        text.replace("autologin -", "autologin carol"),
        text.replace("- -\n", "- - extra\n"),
        text.replace("- -\n", "- $argon2id$\n"),
        text.replace(" - -", "  - -"),
        text.replace('\n', "\r\n"),
        alloc::format!("{text}\n"),
        alloc::format!("# a comment\n{text}"),
        String::from("next 1000\nautologin -\n"),
    ] {
        assert_eq!(Accounts::parse(&bad), None, "{bad}");
    }
}

#[test]
fn a_uid_is_never_given_twice() {
    let mut accounts = Accounts::one_person();
    assert_eq!(accounts.add("bob", false), Ok(1001));
    accounts.remove("bob").unwrap();
    assert_eq!(accounts.add("bob", false), Ok(1002));
    assert_eq!(accounts.person("bob").unwrap().home, "/home/bob");
    let mut full =
        Accounts::parse("next 60000\nautologin -\nperson 1000 user /home/user admin -\n")
            .expect("a record whose uids are spent");
    assert_eq!(full.add("late", false), Err(NO_UIDS));
}

#[test]
fn names_are_checked_and_held_once() {
    let mut accounts = Accounts::one_person();
    for bad in [
        "",
        "Bob",
        "1bob",
        "-bob",
        "bo b",
        "bo:b",
        "root",
        "nobody",
        "admin",
        "greeter",
        "a23456789012345678901234567890123",
    ] {
        assert!(!valid_name(bad), "{bad:?}");
        assert!(accounts.add(bad, false).is_err(), "{bad:?}");
    }
    assert!(valid_name("a2345678901234567890123456789012"));
    assert!(valid_name("x_y-z9"));
    assert_eq!(accounts.add("user", false), Err(NAME_HELD));
    accounts.add("bob", false).unwrap();
    assert_eq!(accounts.rename("bob", "user"), Err(NAME_HELD));
    assert_eq!(accounts.rename("carol", "dave"), Err(NO_SUCH_PERSON));
}

#[test]
fn the_last_administrator_stays_one() {
    let mut accounts = Accounts::one_person();
    assert_eq!(accounts.set_admin("user", false), Err(LAST_ADMIN));
    assert_eq!(accounts.remove("user"), Err(DESKTOP_PERSON));
    accounts.add("bob", true).unwrap();
    accounts.set_admin("user", false).unwrap();
    assert_eq!(accounts.set_admin("bob", false), Err(LAST_ADMIN));
    assert_eq!(accounts.remove("bob"), Err(LAST_ADMIN));
    accounts.set_admin("user", true).unwrap();
    assert_eq!(accounts.remove("bob").map(|p| p.uid), Ok(1001));
    assert_eq!(accounts.admins().count(), 1);
}

#[test]
fn remoted_runs_as_the_first_administrator_unless_one_is_named() {
    let mut accounts = Accounts::one_person();
    accounts.add("bob", false).unwrap();
    accounts.add("carol", true).unwrap();
    assert_eq!(accounts.remote().map(|p| p.uid), Some(SESSION_UID));
    accounts.set_remote(Some("bob")).unwrap();
    assert_eq!(accounts.remote().map(|p| p.name.as_str()), Some("bob"));
    let text = accounts.render();
    assert!(text.lines().nth(2) == Some("remote bob"), "{text}");
    assert_eq!(Accounts::parse(&text), Some(accounts.clone()));
    accounts.rename("bob", "robert").unwrap();
    assert_eq!(accounts.remote().map(|p| p.name.as_str()), Some("robert"));
    accounts.remove("robert").unwrap();
    assert_eq!(accounts.remote().map(|p| p.uid), Some(SESSION_UID));
    assert_eq!(accounts.set_remote(Some("dave")), Err(NO_SUCH_PERSON));
    let unknown = text.replace("remote bob", "remote dave");
    assert_eq!(Accounts::parse(&unknown), None);
    assert_eq!(
        Accounts::parse(&Accounts::one_person().render()),
        Some(Accounts::one_person()),
        "a record that names nobody spells no remote line"
    );
}

#[test]
fn the_administrators_fit_the_line_the_c_library_reads() {
    let mut accounts = Accounts::one_person();
    let mut promoted = 0;
    let refused = (0..64).find_map(|i| {
        let name = alloc::format!("admin{i:02}-{}", "x".repeat(24));
        match accounts.add(&name, true) {
            Ok(_) => {
                promoted += 1;
                None
            }
            Err(e) => Some(e),
        }
    });
    assert_eq!(refused, Some(crate::record::TOO_MANY_ADMINS));
    let group = group(&accounts, &[]);
    let line = group.lines().find(|l| l.starts_with("admin:")).unwrap();
    assert!(
        line.len() <= slopos_slibc_core::passwd::LINE_MAX,
        "{}",
        line.len()
    );
    assert!(Group::parse(line.as_bytes()).is_some());
    assert_eq!(accounts.admins().count(), promoted + 1);
    let plain = alloc::format!("plain-{}", "y".repeat(26));
    accounts.add(&plain, false).unwrap();
    assert_eq!(
        accounts.set_admin(&plain, true),
        Err(crate::record::TOO_MANY_ADMINS)
    );
    assert_eq!(Accounts::parse(&accounts.render()), Some(accounts));
}

#[test]
fn a_rename_keeps_the_uid_home_and_autologin() {
    let mut accounts = Accounts::one_person();
    accounts.rename("user", "ada").unwrap();
    let ada = accounts
        .autologin()
        .expect("still logged in by the desktop");
    assert_eq!((ada.name.as_str(), ada.uid), ("ada", SESSION_UID));
    assert_eq!(ada.home, SESSION_HOME);
    accounts.add("bob", false).unwrap();
    accounts.set_autologin(Some("bob")).unwrap();
    accounts.remove("bob").unwrap();
    assert!(accounts.autologin().is_none());
    assert_eq!(accounts.set_autologin(Some("bob")), Err(NO_SUCH_PERSON));
}

#[test]
fn a_password_is_a_hash_or_none() {
    let mut accounts = Accounts::one_person();
    assert!(
        accounts
            .set_password("user", Some("hunter2".to_string()))
            .is_err()
    );
    accounts
        .set_password("user", Some(HASH.to_string()))
        .unwrap();
    assert_eq!(shadow(&accounts), alloc::format!("user:{HASH}:::::::\n"));
    assert_eq!(shadow_hash(&shadow(&accounts), "user"), Some(HASH));
    accounts.set_password("user", None).unwrap();
    assert_eq!(shadow_hash(&shadow(&accounts), "user"), Some(""));
    assert_eq!(shadow_hash(&shadow(&accounts), "use"), None);
}

#[test]
fn the_database_lists_people_then_system_users() {
    let mut accounts = Accounts::one_person();
    accounts.add("bob", true).unwrap();
    let others = [
        extension("hello", 100),
        extension("bob", 101),
        extension("hello-worker", 1001),
        extension("admin", 102),
        extension("nobody", 103),
        extension("tool", 104),
        extension("greeter", 105),
        extension("logind", GREETER_UID),
    ];
    let passwd = passwd(&accounts, &others);
    let rows: Vec<Passwd<'_>> = lines(passwd.as_bytes())
        .map(|l| Passwd::parse(l).unwrap())
        .collect();
    let named: Vec<(&[u8], u32)> = rows.iter().map(|r| (r.name, r.uid)).collect();
    assert_eq!(
        named,
        [
            (&b"root"[..], 0),
            (b"greeter", GREETER_UID),
            (b"user", 1000),
            (b"bob", 1001),
            (b"hello", 100),
            (b"tool", 104),
            (b"nobody", NOBODY_UID),
        ]
    );
    assert_eq!(rows[1].shell, NO_SHELL.as_bytes());
    assert_eq!(rows[4].dir, b"/var/lib/hello");
    assert_eq!(rows[4].shell, NO_SHELL.as_bytes());
    assert_eq!(rows[3].shell, SHELL.as_bytes());
    let group = group(&accounts, &others);
    let groups: Vec<Group<'_>> = lines(group.as_bytes())
        .map(|l| Group::parse(l).unwrap())
        .collect();
    let admin = groups.iter().find(|g| g.gid == ADMIN_GID).unwrap();
    assert_eq!(admin.name, ADMIN_GROUP.as_bytes());
    assert!(admin.lists(b"user") && admin.lists(b"bob"));
    assert_eq!(groups.iter().filter(|g| g.name == b"admin").count(), 1);
    for row in &rows {
        let own = groups.iter().find(|g| g.name == row.name).unwrap();
        assert_eq!((own.gid, own.members().count()), (row.gid, 0));
    }
    assert_eq!(groups.len(), rows.len() + 1);
}
