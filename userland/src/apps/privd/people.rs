//! The people as privd judges a request by them: whom the accounts record
//! names, who is logged in, and whose desktop the screen shows.

use std::path::Path;

use slopos_abi::fs::SESSION_UID;
use slopos_priv_core::{People, Person};

use crate::accounts::Store;
use crate::init_control::{DESKTOP, Tasks};

/// The people as they stand now. A person is logged in while a process of
/// theirs carries the desktop's login tag; the desktop runs as the first
/// person until there is a login screen.
pub fn now(tasks: Option<&Tasks>) -> Result<People, String> {
    let tasks = tasks.ok_or("who is logged in is unknown: the tasks could not be listed")?;
    let accounts = Store::read(Path::new("/"))
        .accounts()
        .map_err(|why| format!("who the people are is unknown: {why}"))?;
    let people: Vec<Person> = accounts
        .people()
        .iter()
        .map(|p| Person {
            uid: p.uid,
            name: p.name.clone(),
            admin: p.admin,
            password: p.password.is_some(),
        })
        .collect();
    let logged_in = tasks
        .uids_of(&[DESKTOP])
        .into_iter()
        .filter(|uid| people.iter().any(|p| p.uid == *uid))
        .collect();
    Ok(People {
        people,
        logged_in,
        front: Some(SESSION_UID),
    })
}
