//! Extensions' users as the user database lists them, with the uids their
//! records keep. A record keeps a uid for its extension's own name whether
//! or not it declares that user: the extension's number, which init's login
//! tags for its services are made from.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use slopos_slibc_core::passwd::{BASE_GROUP, BASE_PASSWD};

use crate::record::Record;

/// The uids extensions' users take, Debian's range for dynamically allocated
/// system users. One is never given again, even once its extension is gone.
pub const FIRST_UID: u32 = 100;
pub const LAST_UID: u32 = 999;
pub const SHELL: &str = "/bin/false";

/// Give `record`'s own name and each user its images are granted the lowest
/// uid that neither it nor `others` holds; whether any was given.
pub fn allocate<'a>(
    record: &mut Record,
    others: impl IntoIterator<Item = &'a Record>,
) -> Result<bool, &'static str> {
    let mut held: Vec<u32> = others
        .into_iter()
        .flat_map(|r| r.uids.iter().map(|&(_, uid)| uid))
        .chain(record.uids.iter().map(|&(_, uid)| uid))
        .collect();
    let wanted: Vec<String> = core::iter::once(record.name.clone())
        .chain(
            record
                .current
                .iter()
                .chain(&record.previous)
                .flat_map(|image| image.grant.users.iter().cloned()),
        )
        .collect();
    let mut given = false;
    for user in wanted {
        if record.uid(&user).is_some() {
            continue;
        }
        let uid = (FIRST_UID..=LAST_UID)
            .find(|uid| !held.contains(uid))
            .ok_or("every uid for extensions' users is taken")?;
        held.push(uid);
        record.uids.push((user, uid));
        given = true;
    }
    Ok(given)
}

/// Each installed extension's users and their uids, a name or uid held
/// already by one listed before passed over.
fn users(records: &[Record]) -> Vec<(&Record, &str, u32)> {
    let mut out: Vec<(&Record, &str, u32)> = Vec::new();
    for record in records {
        let Some(current) = &record.current else {
            continue;
        };
        for user in &current.grant.users {
            let Some(uid) = record.uid(user) else {
                continue;
            };
            if !out.iter().any(|&(_, n, u)| n == user || u == uid) {
                out.push((record, user, uid));
            }
        }
    }
    out
}

/// `/etc/passwd`: the base's entries, then each installed extension's users,
/// whose home is its state directory.
pub fn passwd(records: &[Record]) -> String {
    let mut out = BASE_PASSWD.to_string();
    for (record, user, uid) in users(records) {
        let name = &record.name;
        out.push_str(&format!(
            "{user}:x:{uid}:{uid}:{name} extension:/var/lib/{name}:{SHELL}\n"
        ));
    }
    out
}

/// `/etc/group`: the base's groups, then a group of each extension user's
/// own, with its uid for a gid and no members.
pub fn group(records: &[Record]) -> String {
    let mut out = BASE_GROUP.to_string();
    for (_, user, uid) in users(records) {
        out.push_str(&format!("{user}:x:{uid}:\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grant::Grant;
    use crate::record::Installed;
    use alloc::vec;
    use slopos_slibc_core::passwd::{Group, Passwd, lines};

    fn installed(name: &str, users: &[&str]) -> Record {
        let mut record = Record::new(name);
        record.install(Installed {
            version: 1,
            sha256: [1; 32],
            signer: None,
            grant: Grant {
                users: users.iter().map(|u| u.to_string()).collect(),
                ..Grant::default()
            },
        });
        record
    }

    #[test]
    fn a_uid_is_the_lowest_free_and_is_kept() {
        let mut hello = installed("hello", &["hello", "hello-worker"]);
        assert_eq!(allocate(&mut hello, []), Ok(true));
        assert_eq!(
            hello.uids,
            vec![("hello".into(), 100), ("hello-worker".into(), 101)]
        );
        assert_eq!(allocate(&mut hello, []), Ok(false));
        let mut tool = installed("tool", &[]);
        assert_eq!(allocate(&mut tool, [&hello]), Ok(true));
        assert_eq!(tool.uids, vec![("tool".into(), 102)]);
        hello.remove();
        let mut late = installed("late", &["late"]);
        assert_eq!(allocate(&mut late, [&hello, &tool]), Ok(true));
        assert_eq!(late.uid("late"), Some(103));
    }

    #[test]
    fn the_range_runs_out() {
        let mut full = installed("full", &[]);
        full.uids = (FIRST_UID..=LAST_UID)
            .map(|uid| (format!("u{uid}"), uid))
            .collect();
        let mut next = installed("next", &[]);
        assert!(allocate(&mut next, [&full]).is_err());
    }

    #[test]
    fn the_database_lists_installed_users_after_the_base() {
        let mut hello = installed("hello", &["hello", "hello-worker"]);
        allocate(&mut hello, []).unwrap();
        let mut gone = installed("gone", &["gone"]);
        allocate(&mut gone, [&hello]).unwrap();
        gone.remove();
        let records = [hello, gone];
        let passwd = passwd(&records);
        assert!(passwd.starts_with(BASE_PASSWD));
        let rows: Vec<Passwd<'_>> = lines(passwd.as_bytes())
            .map(|line| Passwd::parse(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[3].name, b"hello-worker");
        assert_eq!((rows[3].uid, rows[3].gid), (101, 101));
        assert_eq!(rows[3].dir, b"/var/lib/hello");
        let group = group(&records);
        let groups: Vec<Group<'_>> = lines(group.as_bytes())
            .map(|line| Group::parse(line).unwrap())
            .collect();
        assert_eq!(groups.len(), 4);
        assert_eq!((groups[2].name, groups[2].gid), (&b"hello"[..], 100));
    }

    #[test]
    fn a_name_or_uid_listed_twice_is_listed_once() {
        let mut one = installed("one", &["one"]);
        one.uids = vec![("one".into(), 100)];
        let mut two = installed("two", &["two"]);
        two.uids = vec![("two".into(), 100)];
        let passwd = passwd(&[one, two]);
        assert_eq!(lines(passwd.as_bytes()).count(), 3);
    }

    #[test]
    fn the_base_entries_name_the_session() {
        let user = lines(BASE_PASSWD.as_bytes())
            .filter_map(Passwd::parse)
            .find(|row| row.uid == slopos_abi::fs::SESSION_UID)
            .unwrap();
        assert_eq!(user.gid, slopos_abi::fs::SESSION_GID);
        assert_eq!(user.dir, slopos_abi::fs::SESSION_HOME.as_bytes());
    }
}
