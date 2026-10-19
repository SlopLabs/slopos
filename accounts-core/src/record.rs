//! The accounts record: the people the machine knows, which of them
//! administer it, their passwords' hashes, whom the desktop logs in by
//! itself, and the uid the next person is given, so none is given twice.
//!
//! ```text
//! next 1002
//! autologin user
//! person 1000 user /home/user admin -
//! person 1001 bob /home/bob - $argon2id$v=19$m=65536,t=3,p=4$<salt>$<tag>
//! ```
//!
//! A person's words are their uid, name, home, `admin` or `-`, and their
//! password's hash or `-` for none. A record names an administrator, and has
//! one spelling, the one [`Accounts::render`] writes. A root without a record
//! has [`Accounts::one_person`]'s.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use slopos_abi::fs::{PERSON_UIDS, SESSION_HOME, SESSION_UID};

use crate::password::Hash;
use crate::valid_name;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Person {
    pub uid: u32,
    pub name: String,
    pub home: String,
    pub admin: bool,
    /// The PHC string of the password's hash, `None` for no password.
    pub password: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Accounts {
    next: u32,
    autologin: Option<String>,
    people: Vec<Person>,
}

pub const NO_SUCH_PERSON: &str = "no such person";
pub const BAD_NAME: &str = "a name is a lowercase letter, then lowercase letters, digits, - and _, at most 32 bytes, and none the base holds";
pub const NAME_HELD: &str = "the name is held already";
pub const LAST_ADMIN: &str = "the machine's last administrator stays one";
pub const DESKTOP_PERSON: &str = "the desktop runs as the first person, who stays";
pub const NO_UIDS: &str = "every uid for people has been given";
const BAD_HASH: &str = "not a password hash this system writes";

/// A home's path: absolute, printable, and without the database's
/// separators or whitespace.
fn valid_home(home: &str) -> bool {
    home.starts_with('/')
        && home.len() <= 255
        && home.bytes().all(|b| b.is_ascii_graphic() && b != b':')
}

impl Accounts {
    /// A root's record before anyone wrote one: one person, `user`, the first
    /// administrator, with no password and logged in by the desktop.
    pub fn one_person() -> Accounts {
        Accounts::first("user", None).expect("user is a person's name")
    }

    /// A new root's record: its one person, `name`, the first administrator
    /// at the desktop's home, with `password` if it is given.
    pub fn first(name: &str, password: Option<String>) -> Result<Accounts, &'static str> {
        if !valid_name(name) {
            return Err(BAD_NAME);
        }
        if password
            .as_deref()
            .is_some_and(|h| Hash::parse(h).is_none())
        {
            return Err(BAD_HASH);
        }
        Ok(Accounts {
            next: SESSION_UID + 1,
            autologin: Some(name.to_string()),
            people: alloc::vec![Person {
                uid: SESSION_UID,
                name: name.to_string(),
                home: SESSION_HOME.to_string(),
                admin: true,
                password,
            }],
        })
    }

    pub fn people(&self) -> &[Person] {
        &self.people
    }

    pub fn person(&self, name: &str) -> Option<&Person> {
        self.people.iter().find(|p| p.name == name)
    }

    pub fn by_uid(&self, uid: u32) -> Option<&Person> {
        self.people.iter().find(|p| p.uid == uid)
    }

    pub fn admins(&self) -> impl Iterator<Item = &Person> {
        self.people.iter().filter(|p| p.admin)
    }

    /// The person the desktop logs in by itself, if any.
    pub fn autologin(&self) -> Option<&Person> {
        self.autologin.as_deref().and_then(|name| self.person(name))
    }

    fn at(&mut self, name: &str) -> Result<&mut Person, &'static str> {
        self.people
            .iter_mut()
            .find(|p| p.name == name)
            .ok_or(NO_SUCH_PERSON)
    }

    fn free_name(&self, name: &str) -> Result<(), &'static str> {
        if !valid_name(name) {
            return Err(BAD_NAME);
        }
        if self.person(name).is_some() {
            return Err(NAME_HELD);
        }
        Ok(())
    }

    /// Give `name` the next uid and a home of their own; their uid.
    pub fn add(&mut self, name: &str, admin: bool) -> Result<u32, &'static str> {
        self.free_name(name)?;
        if !PERSON_UIDS.contains(&self.next) {
            return Err(NO_UIDS);
        }
        let uid = self.next;
        self.next += 1;
        self.people.push(Person {
            uid,
            name: name.to_string(),
            home: format!("/home/{name}"),
            admin,
            password: None,
        });
        Ok(uid)
    }

    /// Take `name` away, their uid with them for good.
    pub fn remove(&mut self, name: &str) -> Result<Person, &'static str> {
        let person = self.person(name).ok_or(NO_SUCH_PERSON)?;
        if person.uid == SESSION_UID {
            return Err(DESKTOP_PERSON);
        }
        if person.admin && self.admins().count() == 1 {
            return Err(LAST_ADMIN);
        }
        if self.autologin.as_deref() == Some(name) {
            self.autologin = None;
        }
        let at = self.people.iter().position(|p| p.name == name);
        Ok(self.people.remove(at.ok_or(NO_SUCH_PERSON)?))
    }

    /// Call `old` `new`, keeping their uid and home.
    pub fn rename(&mut self, old: &str, new: &str) -> Result<(), &'static str> {
        self.person(old).ok_or(NO_SUCH_PERSON)?;
        self.free_name(new)?;
        if self.autologin.as_deref() == Some(old) {
            self.autologin = Some(new.to_string());
        }
        self.at(old)?.name = new.to_string();
        Ok(())
    }

    pub fn set_admin(&mut self, name: &str, admin: bool) -> Result<(), &'static str> {
        let person = self.person(name).ok_or(NO_SUCH_PERSON)?;
        if person.admin && !admin && self.admins().count() == 1 {
            return Err(LAST_ADMIN);
        }
        self.at(name)?.admin = admin;
        Ok(())
    }

    /// Set `name`'s password to the hash `password` spells, or none.
    pub fn set_password(
        &mut self,
        name: &str,
        password: Option<String>,
    ) -> Result<(), &'static str> {
        if password
            .as_deref()
            .is_some_and(|h| Hash::parse(h).is_none())
        {
            return Err(BAD_HASH);
        }
        self.at(name)?.password = password;
        Ok(())
    }

    /// Have the desktop log `name` in by itself, or nobody.
    pub fn set_autologin(&mut self, name: Option<&str>) -> Result<(), &'static str> {
        if let Some(name) = name {
            self.person(name).ok_or(NO_SUCH_PERSON)?;
        }
        self.autologin = name.map(str::to_string);
        Ok(())
    }

    pub fn render(&self) -> String {
        let mut out = format!(
            "next {}\nautologin {}\n",
            self.next,
            self.autologin.as_deref().unwrap_or("-")
        );
        for p in &self.people {
            out.push_str(&format!(
                "person {} {} {} {} {}\n",
                p.uid,
                p.name,
                p.home,
                if p.admin { "admin" } else { "-" },
                p.password.as_deref().unwrap_or("-"),
            ));
        }
        out
    }

    pub fn parse(text: &str) -> Option<Accounts> {
        let mut lines = text.lines();
        let next: u32 = number(lines.next()?.strip_prefix("next ")?)?;
        let autologin = match lines.next()?.strip_prefix("autologin ")? {
            "-" => None,
            name => Some(name.to_string()),
        };
        let mut people: Vec<Person> = Vec::new();
        for line in lines {
            let mut words = line.strip_prefix("person ")?.split(' ');
            let mut word = || words.next();
            let uid = number(word()?)?;
            let name = word()?.to_string();
            let home = word()?.to_string();
            let admin = match word()? {
                "admin" => true,
                "-" => false,
                _ => return None,
            };
            let password = match word()? {
                "-" => None,
                hash => Some(hash.to_string()),
            };
            let fresh = !people.iter().any(|p| p.uid == uid || p.name == name);
            let sound = valid_name(&name)
                && valid_home(&home)
                && PERSON_UIDS.contains(&uid)
                && uid < next
                && password.as_deref().is_none_or(|h| Hash::parse(h).is_some());
            if word().is_some() || !fresh || !sound {
                return None;
            }
            people.push(Person {
                uid,
                name,
                home,
                admin,
                password,
            });
        }
        let accounts = Accounts {
            next,
            autologin,
            people,
        };
        let sound = next <= PERSON_UIDS.end() + 1
            && accounts.admins().next().is_some()
            && accounts
                .autologin
                .as_deref()
                .is_none_or(|name| accounts.person(name).is_some());
        (sound && accounts.render() == text).then_some(accounts)
    }
}

/// A decimal number in its one spelling.
fn number(text: &str) -> Option<u32> {
    let canonical = text == "0" || (!text.starts_with('0') && !text.is_empty());
    (canonical && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}
