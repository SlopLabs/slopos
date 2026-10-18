//! The user database, `/etc/passwd` and `/etc/group`: the base's entries and
//! the installed extensions' users, written sealed from the records, which
//! keep the uids. slibc believes either only while it is sealed.

use std::path::Path;

use slopos_extension_core::accounts;
use slopos_extension_core::record::Record;

use super::store::{Store, replace_sealed};

const PASSWD: &str = "/etc/passwd";
const GROUP: &str = "/etc/group";

/// Every record the store keeps. One it cannot read is an error: the uids it
/// holds would be given again.
fn records(store: &Store) -> Result<Vec<Record>, String> {
    let mut out = Vec::new();
    for name in store.names()? {
        out.extend(store.record(&name)?);
    }
    Ok(out)
}

/// Give `record` each uid its name and its images' users lack.
pub fn give_uids(store: &Store, record: &mut Record) -> Result<bool, String> {
    let others: Vec<Record> = records(store)?
        .into_iter()
        .filter(|other| other.name != record.name)
        .collect();
    accounts::allocate(record, &others).map_err(|e| format!("{}: {e}", record.name))
}

/// Write the database the store's records describe.
pub fn sync(store: &Store) -> Result<(), String> {
    let records = records(store)?;
    replace_sealed(Path::new(PASSWD), accounts::passwd(&records).as_bytes())?;
    replace_sealed(Path::new(GROUP), accounts::group(&records).as_bytes())
}
