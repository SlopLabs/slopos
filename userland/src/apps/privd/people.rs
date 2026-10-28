//! The people as privd judges a request by them: whom the accounts record
//! names, and who is logged in, as the session table says.

use std::path::Path;

use slopos_priv_core::{People, Person};

use super::sessions::Seen;
use crate::accounts::Store;

pub fn now(seen: &Seen) -> Result<People, String> {
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
    let logged_in = seen
        .logged_in()?
        .into_iter()
        .filter(|uid| people.iter().any(|p| p.uid == *uid))
        .collect();
    Ok(People { people, logged_in })
}
