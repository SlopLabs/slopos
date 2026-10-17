//! What an extension is granted: the capabilities its services and actions
//! hold, the actions every person's processes run unasked, the users made
//! for it and the commands put on everyone's `PATH`. A first install's
//! command line spells it out, and the person reads exactly that:
//!
//! ```text
//! grant Power,NetAdmin unasked greet users hello,hello-worker commands hello
//! ```

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::words::{parse_caps, parse_names, render_caps, render_list};

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Grant {
    pub caps: u64,
    pub unasked: Vec<String>,
    pub users: Vec<String>,
    pub commands: Vec<String>,
}

const WORDS: [&str; 4] = ["grant", "unasked", "users", "commands"];

impl Grant {
    /// The eight words a command line carries it as.
    pub fn words(&self) -> [String; 8] {
        let list = |names: &[String]| render_list(names.iter().map(String::as_str));
        [
            WORDS[0].to_string(),
            render_caps(self.caps),
            WORDS[1].to_string(),
            list(&self.unasked),
            WORDS[2].to_string(),
            list(&self.users),
            WORDS[3].to_string(),
            list(&self.commands),
        ]
    }

    pub fn parse(words: &[&str]) -> Option<Grant> {
        let [g, caps, u, unasked, us, users, c, commands] = words else {
            return None;
        };
        if [*g, *u, *us, *c] != WORDS {
            return None;
        }
        let any = |name: &str| !name.is_empty() && name.bytes().all(|b| b.is_ascii_graphic());
        Some(Grant {
            caps: parse_caps(caps)?,
            unasked: parse_names(unasked, any)?,
            users: parse_names(users, any)?,
            commands: parse_names(commands, any)?,
        })
    }

    /// The same grant, whatever order its lists were written in.
    pub fn same(&self, other: &Grant) -> bool {
        let same = |a: &[String], b: &[String]| {
            a.len() == b.len() && a.iter().all(|name| b.contains(name))
        };
        self.caps == other.caps
            && same(&self.unasked, &other.unasked)
            && same(&self.users, &other.users)
            && same(&self.commands, &other.commands)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn a_grant_reads_back_as_written() {
        let grant = Grant {
            caps: slopos_abi::caps::POWER | slopos_abi::caps::NET_ADMIN,
            unasked: vec!["greet".into()],
            users: vec!["hello".into(), "hello-worker".into()],
            commands: Vec::new(),
        };
        let words = grant.words();
        assert_eq!(
            words.join(" "),
            "grant Power,NetAdmin unasked greet users hello,hello-worker commands -"
        );
        let borrowed: Vec<&str> = words.iter().map(String::as_str).collect();
        assert_eq!(Grant::parse(&borrowed), Some(grant.clone()));
        let reordered = Grant::parse(&[
            "grant",
            "NetAdmin,Power",
            "unasked",
            "greet",
            "users",
            "hello-worker,hello",
            "commands",
            "-",
        ])
        .unwrap();
        assert!(reordered.same(&grant));
        assert!(!Grant::default().same(&grant));
    }

    #[test]
    fn a_line_out_of_shape_is_none() {
        for line in [
            "grant - unasked - users -",
            "grant - unasked - users - commands - more",
            "caps - unasked - users - commands -",
            "grant Wizard unasked - users - commands -",
            "grant Power,Power unasked - users - commands -",
            "grant - unasked a,a users - commands -",
            "grant , unasked - users - commands -",
        ] {
            let words: Vec<&str> = line.split(' ').collect();
            assert_eq!(Grant::parse(&words), None, "{line}");
        }
    }
}
