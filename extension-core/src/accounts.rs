//! Extensions' users as the user database lists them, with the uids their
//! records keep. A record keeps a uid for its extension's own name whether
//! or not it declares that user: the extension's number, which init's login
//! tags for its services are made from.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use slopos_accounts_core::Entry;

use crate::record::Record;

/// The uids extensions' users take, Debian's range for dynamically allocated
/// system users. One is never given again, even once its extension is gone.
pub const FIRST_UID: u32 = *slopos_abi::fs::EXTENSION_UIDS.start();
pub const LAST_UID: u32 = *slopos_abi::fs::EXTENSION_UIDS.end();

/// Give `record`'s own name and each user its images are granted the lowest
/// uid that neither it nor `others` holds.
pub fn allocate<'a>(
    record: &mut Record,
    others: impl IntoIterator<Item = &'a Record>,
) -> Result<(), &'static str> {
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
    for user in wanted {
        if record.uid(&user).is_some() {
            continue;
        }
        let uid = (FIRST_UID..=LAST_UID)
            .find(|uid| !held.contains(uid))
            .ok_or("every uid for extensions' users is taken")?;
        held.push(uid);
        record.uids.push((user, uid));
    }
    Ok(())
}

/// Each installed extension's users, whose home is its state directory, as
/// the user database lists them beside the people.
pub fn entries(records: &[Record]) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    for record in records {
        let Some(current) = &record.current else {
            continue;
        };
        for user in &current.grant.users {
            let Some(uid) = record.uid(user) else {
                continue;
            };
            let name = &record.name;
            out.push(Entry {
                name: user.to_string(),
                uid,
                gecos: format!("{name} extension"),
                home: format!("/var/lib/{name}"),
            });
        }
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
        assert_eq!(allocate(&mut hello, []), Ok(()));
        let given = vec![("hello".into(), 100), ("hello-worker".into(), 101)];
        assert_eq!(hello.uids, given);
        assert_eq!(allocate(&mut hello, []), Ok(()));
        assert_eq!(hello.uids, given);
        let mut tool = installed("tool", &[]);
        assert_eq!(allocate(&mut tool, [&hello]), Ok(()));
        assert_eq!(tool.uids, vec![("tool".into(), 102)]);
        hello.remove();
        let mut late = installed("late", &["late"]);
        assert_eq!(allocate(&mut late, [&hello, &tool]), Ok(()));
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
    fn the_database_lists_installed_users_after_the_people() {
        use slopos_accounts_core::{Accounts, group, passwd};
        let mut hello = installed("hello", &["hello", "hello-worker"]);
        allocate(&mut hello, []).unwrap();
        let mut gone = installed("gone", &["gone"]);
        allocate(&mut gone, [&hello]).unwrap();
        gone.remove();
        let records = [hello, gone];
        let entries = entries(&records);
        assert_eq!(entries.len(), 2);
        let people = Accounts::one_person();
        let passwd = passwd(&people, &entries);
        let rows: Vec<Passwd<'_>> = lines(passwd.as_bytes())
            .map(|line| Passwd::parse(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[4].name, b"hello-worker");
        assert_eq!((rows[4].uid, rows[4].gid), (101, 101));
        assert_eq!(rows[4].dir, b"/var/lib/hello");
        assert_eq!(rows[4].gecos, b"hello extension");
        let group = group(&people, &entries);
        let groups: Vec<Group<'_>> = lines(group.as_bytes())
            .map(|line| Group::parse(line).unwrap())
            .collect();
        assert_eq!(groups.len(), 7);
        assert_eq!((groups[4].name, groups[4].gid), (&b"hello"[..], 100));
    }
}
