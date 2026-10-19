//! `/sbin/authcheck` — whether a secret is a person's password, the one place
//! a hash is read back. It takes `<name>\n<secret>\n` on standard input and
//! answers by its status alone: 0 for the password, 1 for anything else — a
//! wrong secret, a person with no password or nobody of that name — and 2
//! when it cannot tell. It believes `/etc/shadow` only sealed, which only uid
//! 0 reads, so whoever asks it never holds a hash.

use std::io::Read;
use std::path::Path;

use slopos_accounts_core::password::Hash;
use slopos_accounts_core::shadow_hash;

use crate::sealed::{SMALL_MAX, read_sealed};
use crate::syscall::core as sys_core;

const SHADOW: &str = "/etc/shadow";
/// A name, a password and their two newlines.
const INPUT_MAX: u64 = 4096;

fn check(input: &[u8]) -> Result<bool, String> {
    let text = input
        .strip_suffix(b"\n")
        .ok_or("the input ends with no newline")?;
    let at = text
        .iter()
        .position(|&b| b == b'\n')
        .ok_or("the input is not a name and a secret")?;
    let (name, secret) = (&text[..at], &text[at + 1..]);
    if secret.contains(&b'\n') {
        return Err("the input is more than a name and a secret".into());
    }
    let name = std::str::from_utf8(name).map_err(|_| "a name that is not UTF-8")?;
    let shadow = read_sealed(Path::new(SHADOW), SMALL_MAX)?
        .ok_or_else(|| format!("{SHADOW} is not written"))?;
    let shadow = std::str::from_utf8(&shadow).map_err(|_| format!("{SHADOW} is not UTF-8"))?;
    match shadow_hash(shadow, name) {
        None | Some("") => Ok(false),
        Some(hash) => Hash::parse(hash)
            .ok_or_else(|| format!("{SHADOW} holds a hash this system does not write"))?
            .matches(secret)
            .map_err(|e| format!("checking: {e:?}")),
    }
}

pub fn authcheck_main() {
    let mut input = Vec::new();
    let verdict = std::io::stdin()
        .take(INPUT_MAX)
        .read_to_end(&mut input)
        .map_err(|e| format!("reading: {e}"))
        .and_then(|_| check(&input));
    slopos_tls_core::ct::wipe(&mut input);
    sys_core::exit_with_code(match verdict {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(why) => {
            eprintln!("authcheck: {why}");
            2
        }
    });
}
