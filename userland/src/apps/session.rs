//! `/bin/session` — a person's own session, as init keeps it: `restart` ends
//! it and starts it again, `end` logs its person out, `lock` locks its
//! desktop and `greeter` brings the login screen in front, from which another
//! person logs in; `status` lists every session. Uid 0 also restarts, ends
//! and brings in front a session by its tag, and from outside every session
//! restarts or ends them all.
//!
//! privd runs two mechanisms of it, each reading the password from standard
//! input: `login desktop|console <name>` for the login screen and the
//! console's prompt, and `unlock` for a locked desktop's own person.

use std::path::Path;
use std::time::Duration;

use slopos_priv_core::ENV_UID;

use crate::init_control::{self, At, Command, Name};
use crate::privd::{Input, elevate, own_args};
use crate::syscall::fs;

const USAGE: &str = "usage: session restart|end|lock|greeter|status
       session restart|end|switch <tag>
       session login desktop|console <name>
       session unlock";

const LOGIN: &str = "org.slopos.login";
const UNLOCK: &str = "org.slopos.session.unlock";

/// What a wrong password costs, as PAM's own delay on a failure, doubling
/// with each one since that person's last right one, as privd's checks do.
const FIRST_DELAY: Duration = Duration::from_secs(2);
const LONGEST_DELAY: Duration = Duration::from_secs(60);
/// Each person's wrong passwords since their last right one, in memory
/// only uid 0 reads, since each mechanism lives for one check.
const FAILURES: &str = "/run/slopos-auth";

pub fn session_main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let tag = |word: &str| word.parse::<u32>().ok().filter(|tag| *tag != 0);
    let command = match words[..] {
        ["restart"] => Command::RestartSession,
        ["end"] => Command::EndSession,
        ["lock"] => Command::Lock,
        ["greeter"] => Command::Greeter,
        ["status"] => return status(),
        ["restart", t] => tag(t).map(Command::Restart).unwrap_or_else(|| usage()),
        ["end", t] => tag(t).map(Command::End).unwrap_or_else(|| usage()),
        ["switch", t] => tag(t).map(Command::Switch).unwrap_or_else(|| usage()),
        ["login", at, name] => {
            let at = match at {
                "desktop" => At::Desktop,
                "console" => At::Console,
                _ => usage(),
            };
            let name = Name::new(name).unwrap_or_else(|| usage());
            elevate(Some(LOGIN), &own_args(), Input::Closed);
            return log_in(at, name);
        }
        ["unlock"] => {
            elevate(Some(UNLOCK), &own_args(), Input::Closed);
            return unlock();
        }
        _ => usage(),
    };
    if let Err(why) = init_control::ask(command) {
        fail(&why);
    }
}

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2);
}

fn fail(why: &str) -> ! {
    eprintln!("session: {why}");
    std::process::exit(1);
}

fn status() {
    for entry in crate::sessions::read() {
        let mut line = format!(
            "{:>8} {:<12} {:<10}",
            entry.tag,
            entry.user,
            entry.class.word()
        );
        if entry.active {
            line += " in front";
        }
        if entry.locked {
            line += " locked";
        }
        if let Some(tty) = &entry.tty {
            line += &format!(" {tty}");
        }
        println!("{line}");
    }
}

/// The password on standard input, one line.
fn typed() -> crate::secret::Secret {
    match crate::secret::read("") {
        Ok(Some(secret)) => secret,
        Ok(None) => fail("no password was given"),
        Err(why) => fail(&why),
    }
}

/// Whether `secret` lets `name` in: their password, or nothing for a person
/// who has none.
fn admits(name: &str, secret: &crate::secret::Secret) -> bool {
    let record = crate::accounts::Store::read(Path::new("/")).accounts();
    let Some(person) = record.as_ref().ok().and_then(|r| r.person(name)) else {
        return false;
    };
    match &person.password {
        None => secret.is_empty(),
        Some(_) => crate::accounts::check(name, secret.bytes()).unwrap_or_else(|why| fail(&why)),
    }
}

fn proved(name: &str) {
    let secret = typed();
    let path = Path::new(FAILURES).join(name);
    if admits(name, &secret) {
        let _ = std::fs::remove_file(&path);
        return;
    }
    let failures = std::fs::read_to_string(&path)
        .ok()
        .and_then(|n| n.trim().parse::<u32>().ok())
        .unwrap_or(0)
        .saturating_add(1);
    if std::fs::create_dir_all(FAILURES).is_ok() {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(FAILURES, std::fs::Permissions::from_mode(0o700));
        let _ = std::fs::write(&path, failures.to_string());
    }
    let delay = FIRST_DELAY.saturating_mul(1 << failures.min(6).saturating_sub(1));
    std::thread::sleep(delay.min(LONGEST_DELAY));
    fail(&format!("the password for {name} is wrong"));
}

/// Log `name` in once their password is proved: their desktop in front, or
/// a shell on the terminal at standard output, which init takes.
fn log_in(at: At, name: Name) {
    proved(name.as_str());
    let handed = match at {
        At::Desktop => None,
        At::Console if fs::isatty(1) => Some(1),
        At::Console => fail("a console login runs its shell on standard output, no terminal"),
    };
    if let Err(why) = init_control::ask_handing(Command::Login(at, name), handed) {
        fail(&why);
    }
}

/// Answer whether the person asking typed their own password.
fn unlock() {
    let uid = std::env::var(ENV_UID)
        .ok()
        .and_then(|uid| uid.parse::<u32>().ok())
        .unwrap_or_else(crate::syscall::process::getuid);
    let record = crate::accounts::Store::read(Path::new("/")).accounts();
    let Some(name) = record
        .ok()
        .and_then(|record| Some(record.by_uid(uid)?.name.clone()))
    else {
        fail("the person asking is no one the accounts record names");
    };
    proved(&name);
}
