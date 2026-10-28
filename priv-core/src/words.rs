//! What an action says to the person it asks: no capability name, none of
//! the policy's own words, no flag and no `...`.

use slopos_theme_core::words;

use crate::CAPABILITIES;

pub const DESCRIBE_MAX: usize = 72;
pub const DETAIL_MAX: usize = 160;
pub const VERB: &str = "Allow";
pub const ADMIN_NEEDED: &str = "This needs an administrator's password.";
pub const ASKED_BY: &str = "Asked by";
pub const RUNS: &str = "Runs";
pub const IN: &str = "In";
pub const EXTENSION: &str = "Extension";

// The dialog's own controls, which no verb may say: what has focus is found by
// what it says.
pub const CANCEL: &str = "Cancel";
pub const SHOW_DETAILS: &str = "Show details";
pub const SHOW_LESS: &str = "Show less";
pub const PASSWORD: &str = "Password";

/// What an allowing button may say: each does what it says, and none reads
/// as refusing, as more to read, or as another of the dialog's controls.
pub const VERBS: [&str; 31] = [
    "Add",
    "Allow",
    "Change",
    "Connect",
    "Continue",
    "Copy",
    "Delete",
    "Erase",
    "Format",
    "Go back",
    "Install",
    "Keep",
    "Log in",
    "Mount",
    "Open",
    "Power off",
    "Remove",
    "Replace",
    "Restart",
    "Run",
    "Save",
    "Send",
    "Set up",
    "Share",
    "Start",
    "Trust",
    "Turn off",
    "Turn on",
    "Unlock",
    "Update",
    "Use",
];

/// The policy's vocabulary, which no screen shows, beside the desktop's
/// glossary.
const JARGON: [&str; 8] = [
    "requester",
    "mechanism",
    "principal",
    "privd",
    "slot",
    "base",
    "sealed",
    "uid",
];

pub fn describe(text: &str) -> Result<(), &'static str> {
    if text.len() > DESCRIBE_MAX {
        return Err("describe is at most 72 characters");
    }
    if !text.starts_with(|c: char| c.is_ascii_lowercase()) {
        return Err("describe is a verb phrase in lower case that completes \"... wants to\"");
    }
    if text.ends_with(|c: char| c.is_ascii_punctuation()) {
        return Err("describe ends without punctuation");
    }
    plain(text)
}

pub fn detail(text: &str) -> Result<(), &'static str> {
    if text.len() > DETAIL_MAX {
        return Err("detail is at most 160 characters");
    }
    if !text.starts_with(|c: char| c.is_ascii_uppercase()) || !text.ends_with('.') {
        return Err("detail is a sentence: a capital first and a full stop last");
    }
    plain(text)
}

pub fn verb(text: &str) -> Result<(), &'static str> {
    match VERBS.contains(&text) {
        true => Ok(()),
        false => Err("verb is one of slopos_priv_core::words::VERBS: Allow, Install, Run…"),
    }
}

fn plain(text: &str) -> Result<(), &'static str> {
    words::plain(text, &JARGON).map_err(|why| match why {
        words::TRADE => "the words use the policy's own, which no part of the dialog names",
        why => why,
    })?;
    no_capability(text)
}

fn no_capability(text: &str) -> Result<(), &'static str> {
    let capability = text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .enumerate()
        .filter(|(at, word)| *at > 0 || word.bytes().skip(1).any(|b| b.is_ascii_uppercase()))
        .any(|(_, word)| CAPABILITIES.iter().any(|(name, _)| *name == word));
    if capability {
        return Err("the words name no capability");
    }
    Ok(())
}

/// What privd tells a requester it refused, beside [`crate::Refused`] and
/// [`crate::consent::Refusal`].
pub mod privd {
    pub const LAPSED: &str = "Nothing ran, because the app that asked changed after it asked.";
    pub const RESTARTING: &str = "The system is being updated. Try again in a moment.";
    pub const NO_ACTION: &str = "This computer doesn't offer that.";
    pub const NOT_THAT_WAY: &str = "That can't be asked with those arguments.";
    pub const NO_FOLDER: &str = "This runs in your current folder, and none came with it.";
    pub const UNWANTED_FOLDER: &str = "This runs in no folder, and one came with it.";
    pub const FOLDER_GONE: &str = "Your current folder no longer exists.";
    pub const NOT_A_FOLDER: &str = "What came as your current folder isn't a folder.";
    pub const FOLDER_MOVED: &str =
        "The folder this was asked in moved while you were asked. Try again.";
    pub const UNREADABLE: &str = "That request couldn't be read.";
    pub const CROWDED: &str = "Too many requests are waiting. Try again in a moment.";
    pub const BUSY: &str = "Something like this is running already. Try again once it's done.";
    pub const INHERITED: &str = "Something started before the system was updated is still running. Try again once it's done.";
    pub const TOO_LONG: &str = "The command is too long to show in full, so it can't be allowed.";
    pub const INCOMPLETE: &str = "The answer came back incomplete. Try again.";
    pub const UNCHECKED: &str = "Can't check the password right now. Try again in a moment.";
    pub const ANSWERED_ELSEWHERE: &str =
        "Who can allow this changed while you were asked. Try again.";
    pub const NO_ACCOUNT: &str = "The account that asked no longer exists.";
    pub const EXTENSION_CHANGED: &str = "The extension that runs this has changed. Try again.";
    pub const FAULT: &str = "Something went wrong. Try again in a moment.";
}

#[cfg(test)]
mod tests;
