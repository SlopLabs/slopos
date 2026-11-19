//! The uids packages' users take, which their records keep, and the user
//! database written from the records and the people's.

use std::path::Path;

use slopos_package_core::accounts;
use slopos_package_core::record::Record;

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

/// Give `record` each uid its name and its images' users lack, when its
/// images are granted any user.
pub fn give_uids(store: &Store, record: &mut Record) -> Result<(), String> {
    let others: Vec<Record> = records(store)?
        .into_iter()
        .filter(|other| other.name != record.name)
        .collect();
    accounts::allocate(record, &others).map_err(|e| format!("{}: {e}", record.name))
}

/// Write the database the records describe beside the people; a RAM root
/// keeps none.
pub fn sync() -> Result<(), String> {
    let top = Path::new("/");
    if crate::accounts::in_memory(top) {
        return Ok(());
    }
    crate::accounts::Store::write(top)?.sync()
}
