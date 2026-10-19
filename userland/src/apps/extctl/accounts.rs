//! The uids extensions' users take, which their records keep, and the user
//! database written from the records and the people's.

use std::path::Path;

use slopos_extension_core::accounts;
use slopos_extension_core::record::Record;

use super::store::Store;

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

/// Write the database the records describe beside the people.
pub fn sync() -> Result<(), String> {
    crate::accounts::Store::write(Path::new("/"))?.sync()
}
