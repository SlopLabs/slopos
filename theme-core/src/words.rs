//! The desktop's voice, where its words are data: what a screen a person
//! must act on never says, and the shapes a label and a sentence take. Each
//! surface adds the words of its own trade it keeps to its details.

/// Why `plain` refused a word of `jargon`.
pub const TRADE: &str = "the words use a term of the trade, which belongs in the details";

/// What a screen never says, and what the glossary says in its place.
pub const GLOSSARY: [(&str, &str); 15] = [
    ("machine", "computer"),
    ("drive", "disk"),
    ("medium", "installer"),
    ("volume", "partition"),
    ("region", "partition"),
    ("reboot", "restart"),
    ("halt", "power off"),
    ("shut down", "power off"),
    ("admin", "administrator"),
    ("end session", "log out"),
    ("switch person", "switch user"),
    ("program", "app"),
    ("client", "app"),
    ("boot order", "startup order"),
    ("firmware entry", "startup order"),
];

/// The words of a button, a menu entry, a heading or a passing status: a
/// capital first and no full stop.
pub fn label(text: &str, jargon: &[&str]) -> Result<(), &'static str> {
    if !text.starts_with(|c: char| c.is_uppercase()) {
        return Err("a label starts with a capital");
    }
    if text.ends_with('.') {
        return Err("a label ends without a full stop");
    }
    plain(text, jargon)
}

/// One or more sentences: a capital first and a full stop or a question
/// mark last.
pub fn sentence(text: &str, jargon: &[&str]) -> Result<(), &'static str> {
    if !text.starts_with(|c: char| c.is_uppercase()) || !text.ends_with(['.', '?']) {
        return Err("a sentence starts with a capital and ends with a full stop");
    }
    plain(text, jargon)
}

/// Whether `word` is `term` or a form of it: `reboots`, `rebooted`,
/// `rebooting`, `shutting`, `entries`.
fn inflects(word: &str, term: &str) -> bool {
    const ENDINGS: [&str; 6] = ["", "s", "es", "d", "ed", "ing"];
    let ends = |tail: &str| ENDINGS.iter().any(|end| tail.eq_ignore_ascii_case(end));
    if let Some(stem) = term.strip_suffix('y')
        && let Some((head, tail)) = word.split_at_checked(stem.len())
        && head.eq_ignore_ascii_case(stem)
        && (tail.eq_ignore_ascii_case("ies") || tail.eq_ignore_ascii_case("ied"))
    {
        return true;
    }
    let Some((head, tail)) = word.split_at_checked(term.len()) else {
        return false;
    };
    let doubled = term
        .bytes()
        .last()
        .is_some_and(|last| tail.as_bytes().first() == Some(&last))
        && ends(&tail[1..]);
    head.eq_ignore_ascii_case(term) && (ends(tail) || doubled)
}

/// No flag, no three dots for an ellipsis, no word the glossary replaces in
/// any of its forms and none of `jargon`, each a word or a word and an `s`.
pub fn plain(text: &str, jargon: &[&str]) -> Result<(), &'static str> {
    if text.contains("--") {
        return Err("the words name no flag");
    }
    if text.contains("...") {
        return Err("an ellipsis is one character, not three dots");
    }
    let mut previous = "";
    for word in text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
    {
        let is = |term: &str| {
            word.eq_ignore_ascii_case(term)
                || word
                    .strip_suffix('s')
                    .is_some_and(|stem| stem.eq_ignore_ascii_case(term))
        };
        let glossary = GLOSSARY.iter().any(|(term, _)| match term.split_once(' ') {
            Some((first, second)) => inflects(previous, first) && inflects(word, second),
            None => inflects(word, term),
        });
        if glossary {
            return Err("the words use one the glossary replaces");
        }
        if jargon.iter().any(|term| is(term)) {
            return Err(TRADE);
        }
        previous = word;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
