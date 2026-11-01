//! The people as `users list --porcelain` prints them for another program:
//! one line each, `name`, `uid`, `home` and the words of what holds for
//! them, apart by tabs, so Settings reads what the record says without
//! reading the record. Who has a password, logs in by themselves or runs
//! remoted is told to root and the administrators, and to anyone else of
//! themselves alone.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::Accounts;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    pub uid: u32,
    pub home: String,
    pub admin: bool,
    pub password: bool,
    /// The desktop logs them in by itself.
    pub autologin: bool,
    /// remoted runs as them.
    pub remote: bool,
    /// Whether they have a password, log in by themselves or run remoted is
    /// not the viewer's to know, so those read false.
    pub withheld: bool,
}

const ADMIN: &str = "admin";
const PASSWORD: &str = "password";
const AUTOLOGIN: &str = "autologin";
const REMOTE: &str = "remote";
const WITHHELD: &str = "withheld";
const NONE: &str = "-";

/// The people as `viewer` may see them.
pub fn render(accounts: &Accounts, viewer: u32) -> String {
    let autologin = accounts.autologin().map(|p| p.uid);
    let remote = accounts.remote().map(|p| p.uid);
    let sees_all = viewer == 0 || accounts.by_uid(viewer).is_some_and(|p| p.admin);
    accounts
        .people()
        .iter()
        .map(|p| {
            let told = sees_all || p.uid == viewer;
            let held = [
                (p.admin, ADMIN),
                (told && p.password.is_some(), PASSWORD),
                (told && autologin == Some(p.uid), AUTOLOGIN),
                (told && remote == Some(p.uid), REMOTE),
                (!told, WITHHELD),
            ];
            let words: Vec<&str> = held.iter().filter(|(on, _)| *on).map(|(_, w)| *w).collect();
            let words = match words.is_empty() {
                true => NONE.to_string(),
                false => words.join(","),
            };
            format!("{}\t{}\t{}\t{words}\n", p.name, p.uid, p.home)
        })
        .collect()
}

/// `None` for any line that is not one [`render`] writes.
pub fn parse(text: &str) -> Option<Vec<Listed>> {
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let [name, uid, home, words] = line.split('\t').collect::<Vec<_>>()[..] else {
                return None;
            };
            let words: Vec<&str> = match words {
                NONE => Vec::new(),
                _ => words.split(',').collect(),
            };
            if words
                .iter()
                .any(|w| ![ADMIN, PASSWORD, AUTOLOGIN, REMOTE, WITHHELD].contains(w))
            {
                return None;
            }
            Some(Listed {
                name: name.to_string(),
                uid: uid.parse().ok()?,
                home: home.to_string(),
                admin: words.contains(&ADMIN),
                password: words.contains(&PASSWORD),
                autologin: words.contains(&AUTOLOGIN),
                remote: words.contains(&REMOTE),
                withheld: words.contains(&WITHHELD),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_rendered_reads_back() {
        let mut accounts = Accounts::one_person();
        accounts.add("ada", false).unwrap();
        let hash =
            "$argon2id$v=19$m=256,t=2,p=1$c29tZXNhbHQ$nf65EOgLrQMR/uIPnA4rEsF5h7TKyQwu9U1bMCHGi/4";
        accounts.set_password("ada", Some(hash.into())).unwrap();
        let text = render(&accounts, 1000);
        let listed = parse(&text).unwrap();
        assert_eq!(listed.len(), 2);
        let first = &listed[0];
        assert_eq!(
            (
                first.name.as_str(),
                first.uid,
                first.admin,
                first.autologin,
                first.remote
            ),
            ("user", 1000, true, true, true)
        );
        assert!(!first.password);
        assert!(!listed[1].admin && !listed[1].autologin && listed[1].password);
        assert_eq!(listed[1].home, "/home/ada");
    }

    #[test]
    fn a_standard_user_is_told_of_themselves_alone() {
        let mut accounts = Accounts::one_person();
        accounts.add("ada", false).unwrap();
        let ada = accounts.person("ada").unwrap().uid;
        let listed = parse(&render(&accounts, ada)).unwrap();
        let user = &listed[0];
        assert!(user.admin && user.withheld && !user.autologin && !user.remote);
        assert!(!listed[1].withheld);
        assert!(
            parse(&render(&accounts, 0))
                .unwrap()
                .iter()
                .all(|p| !p.withheld)
        );
    }

    #[test]
    fn a_line_of_anything_else_is_refused() {
        assert_eq!(parse(""), Some(Vec::new()));
        assert!(parse("user 1000 /home/user admin\n").is_none());
        assert!(parse("user\t1000\t/home/user\tadmin,root\n").is_none());
        assert!(parse("user\tten\t/home/user\t-\n").is_none());
    }
}
