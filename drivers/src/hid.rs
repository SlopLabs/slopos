//! A HID report descriptor parsed into heap storage, for the transports that
//! carry one: I²C-HID and USB.

use slopos_hid_core::{Descriptor, Error, Field, Parsed, parse};
use slopos_ostd::{KBox, KVec};

pub struct ReportMap {
    fields: KVec<Field>,
    usages: KVec<u32>,
    parsed: KBox<Parsed>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapError {
    NoMemory,
    Malformed(Error),
}

impl ReportMap {
    /// At most `fields` fields and `usages` listed usages.
    pub fn parse(desc: &[u8], fields: usize, usages: usize) -> Result<Self, MapError> {
        let mut field_store = KVec::new();
        field_store
            .resize(fields, Field::default())
            .map_err(|_| MapError::NoMemory)?;
        let mut usage_store = KVec::new();
        usage_store
            .resize(usages, 0)
            .map_err(|_| MapError::NoMemory)?;
        let parsed =
            parse(desc, &mut field_store, &mut usage_store).map_err(MapError::Malformed)?;
        Ok(Self {
            fields: exact(&field_store[..parsed.fields])?,
            usages: exact(&usage_store[..parsed.usages])?,
            parsed: KBox::try_new(parsed).map_err(|_| MapError::NoMemory)?,
        })
    }

    pub fn descriptor(&self) -> Descriptor<'_> {
        self.parsed.descriptor(&self.fields, &self.usages)
    }
}

fn exact<T: Copy>(items: &[T]) -> Result<KVec<T>, MapError> {
    let mut kept = KVec::with_capacity(items.len()).map_err(|_| MapError::NoMemory)?;
    kept.extend_from_slice(items)
        .map_err(|_| MapError::NoMemory)?;
    Ok(kept)
}
