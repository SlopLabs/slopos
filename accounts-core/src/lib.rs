//! The people a machine knows, as data: the accounts record the store at
//! [`slopos_abi::fs::ACCOUNTS_STORE`] keeps, the user database rendered from
//! it and from extensions' users — `/etc/passwd`, `/etc/group` and
//! `/etc/shadow` — and passwords as Argon2id hashes. `/bin/users`,
//! `/sbin/authcheck`, the installer and init do the I/O.
//!
//! The database lists root, then the people, then the system users beside
//! them, then `nobody`; an entry whose name or uid one listed before holds is
//! passed over. A person's group is their own, its gid their uid, and the
//! administrators are the members of [`ADMIN_GROUP`].

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod password;
pub mod record;

pub use record::{Accounts, Person};

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

pub use slopos_abi::fs::{ADMIN_GID, ADMIN_GROUP, NOBODY_UID};

/// The longest name a person may have.
pub const NAME_MAX: usize = 32;
/// The base's own names, which no person takes.
pub const RESERVED: [&str; 4] = ["root", "nobody", ADMIN_GROUP, "greeter"];
/// A person's shell.
pub const SHELL: &str = "/bin/shell";
/// The shell of a user that logs in nowhere.
pub const NO_SHELL: &str = "/bin/false";
const NOBODY_HOME: &str = "/nonexistent";

/// A person's name: a lowercase letter, then lowercase letters, digits, `-`
/// and `_`, at most [`NAME_MAX`] bytes, and none of [`RESERVED`].
pub fn valid_name(name: &str) -> bool {
    name.len() <= NAME_MAX
        && name.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        && !RESERVED.contains(&name)
}

/// A system user the database lists beside the people: an extension's.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    pub name: String,
    pub uid: u32,
    pub gecos: String,
    pub home: String,
}

struct Row<'a> {
    name: &'a str,
    uid: u32,
    gecos: &'a str,
    home: &'a str,
    shell: &'a str,
}

fn rows<'a>(accounts: &'a Accounts, others: &'a [Entry]) -> Vec<Row<'a>> {
    let root = Row {
        name: "root",
        uid: 0,
        gecos: "root",
        home: "/root",
        shell: SHELL,
    };
    let people = accounts.people().iter().map(|p| Row {
        name: &p.name,
        uid: p.uid,
        gecos: &p.name,
        home: &p.home,
        shell: SHELL,
    });
    let system = others.iter().map(|e| Row {
        name: &e.name,
        uid: e.uid,
        gecos: &e.gecos,
        home: &e.home,
        shell: NO_SHELL,
    });
    let mut out: Vec<Row<'a>> = Vec::new();
    for row in core::iter::once(root).chain(people).chain(system) {
        let held = out.iter().any(|r| r.name == row.name || r.uid == row.uid)
            || [ADMIN_GROUP, "nobody"].contains(&row.name)
            || row.uid == NOBODY_UID;
        if !held {
            out.push(row);
        }
    }
    out.push(Row {
        name: "nobody",
        uid: NOBODY_UID,
        gecos: "nobody",
        home: NOBODY_HOME,
        shell: NO_SHELL,
    });
    out
}

/// `/etc/passwd`.
pub fn passwd(accounts: &Accounts, others: &[Entry]) -> String {
    rows(accounts, others)
        .iter()
        .map(|r| {
            format!(
                "{}:x:{}:{}:{}:{}:{}\n",
                r.name, r.uid, r.uid, r.gecos, r.home, r.shell
            )
        })
        .collect()
}

/// `/etc/group`: root's, the administrators', then every other entry's own.
pub fn group(accounts: &Accounts, others: &[Entry]) -> String {
    let admins: Vec<&str> = accounts.admins().map(|p| p.name.as_str()).collect();
    let mut out = format!(
        "root:x:0:\n{ADMIN_GROUP}:x:{ADMIN_GID}:{}\n",
        admins.join(",")
    );
    for row in rows(accounts, others).iter().skip(1) {
        out.push_str(&format!("{}:x:{}:\n", row.name, row.uid));
    }
    out
}

/// `/etc/shadow`: each person's password hash, empty for none.
pub fn shadow(accounts: &Accounts) -> String {
    accounts
        .people()
        .iter()
        .map(|p| {
            format!(
                "{}:{}:::::::\n",
                p.name,
                p.password.as_deref().unwrap_or("")
            )
        })
        .collect()
}

/// What `/etc/shadow` holds for `name`: its hash, empty for no password, or
/// `None` when it lists no such person.
pub fn shadow_hash<'a>(shadow: &'a str, name: &str) -> Option<&'a str> {
    shadow.lines().find_map(|line| {
        let mut fields = line.split(':');
        (fields.next()? == name).then_some(())?;
        fields.next()
    })
}

#[cfg(test)]
mod tests;
