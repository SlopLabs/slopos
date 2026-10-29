//! What the apps say when the computer refuses to open something: what was
//! refused and why.

use slopos_appkit::Refusal;

/// `what` is how the app names it.
pub fn to_open(what: &str, refusal: Refusal) -> String {
    match refusal {
        Refusal::System => {
            format!("You don't have permission to open {what}, which is part of the system.")
        }
        Refusal::Extension => {
            format!("You don't have permission to open {what}, which is part of an extension.")
        }
        Refusal::ReadOnlyDisk | Refusal::Sealed | Refusal::Denied => {
            format!("You don't have permission to open {what}.")
        }
    }
}
