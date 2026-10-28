//! `/bin/users` — the people the machine knows. Anyone may list them;
//! changing them is privd's: `org.slopos.accounts.password` changes the
//! requester's own password, asking the current one first, and
//! `org.slopos.accounts.manage` adds, removes and renames people, sets a
//! person's password and changes who administers the machine, for root, an
//! administrator, or a person an administrator's password answers for. The
//! add that makes a second person hands `/usr/local` to the system. Init
//! runs `users sync` at every boot, holding `Seal`, which writes the user
//! database and gives a root that has no record the one person's: `user`, its
//! first administrator.

use std::os::unix::fs::{DirBuilderExt, MetadataExt, lchown};
use std::path::{Path, PathBuf};
use std::time::Duration;

use slopos_abi::fs::{ADMIN_GROUP, OWNERS_RECORD, SESSION_HOME};
use slopos_accounts_core::{Accounts, Person};
use slopos_priv_core::{ENV_ACTION, ENV_UID};
use slopos_slibc_core::passwd::{BASE_GROUP, Group, lines};

use crate::accounts::{self, Store};
use crate::privd::{self, Input};
use crate::root_layout::{self, LOCAL, Owners};
use crate::secret;
use crate::syscall::core as sys_core;

const USAGE: &str = "usage: users [list]
       users add <name> [--admin] [--keep-autologin]
       users remove <name> [--home]
       users rename <name> <new name>
       users passwd [<name>]
       users promote <name>
       users demote <name>
       users autologin <name> | --off
       users remote <name> | --default";

const OWN_PASSWORD: &str = "org.slopos.accounts.password";
const MANAGE: &str = "org.slopos.accounts.manage";
/// What a wrong password costs its typist beside the hash, one at a time,
/// since privd runs the accounts' mechanisms one after another.
const WRONG_PASSWORD_DELAY: Duration = Duration::from_secs(2);

enum Request {
    List,
    Sync,
    OwnPassword,
    Manage(Change),
}

enum Change {
    Add {
        name: String,
        admin: bool,
        keep_autologin: bool,
    },
    Remove {
        name: String,
        home: bool,
    },
    Rename {
        old: String,
        new: String,
    },
    Password(String),
    Admin {
        name: String,
        admin: bool,
    },
    Autologin(Option<String>),
    /// Whom remoted runs as; `None` for the first administrator.
    Remote(Option<String>),
}

fn parse(args: &[String]) -> Result<Request, String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let flags = |rest: &[&str], known: &[&str]| -> Result<Vec<String>, String> {
        let mut seen = Vec::new();
        for flag in rest {
            if !known.contains(flag) || seen.iter().any(|s| s == flag) {
                return Err(format!("{flag}?\n{USAGE}"));
            }
            seen.push(flag.to_string());
        }
        Ok(seen)
    };
    let change = match words.as_slice() {
        [] | ["list"] => return Ok(Request::List),
        ["sync"] => return Ok(Request::Sync),
        ["passwd"] => return Ok(Request::OwnPassword),
        ["add", name, rest @ ..] => {
            let seen = flags(rest, &["--admin", "--keep-autologin"])?;
            Change::Add {
                name: name.to_string(),
                admin: seen.iter().any(|f| f == "--admin"),
                keep_autologin: seen.iter().any(|f| f == "--keep-autologin"),
            }
        }
        ["remove", name, rest @ ..] => Change::Remove {
            name: name.to_string(),
            home: !flags(rest, &["--home"])?.is_empty(),
        },
        ["rename", old, new] => Change::Rename {
            old: old.to_string(),
            new: new.to_string(),
        },
        ["passwd", name] => Change::Password(name.to_string()),
        ["promote", name] => Change::Admin {
            name: name.to_string(),
            admin: true,
        },
        ["demote", name] => Change::Admin {
            name: name.to_string(),
            admin: false,
        },
        ["autologin", "--off"] => Change::Autologin(None),
        ["autologin", name] => Change::Autologin(Some(name.to_string())),
        ["remote", "--default"] => Change::Remote(None),
        ["remote", name] => Change::Remote(Some(name.to_string())),
        _ => return Err(USAGE.to_owned()),
    };
    Ok(Request::Manage(change))
}

impl Request {
    fn action(&self) -> Option<&'static str> {
        match self {
            Request::List | Request::Sync => None,
            Request::OwnPassword => Some(OWN_PASSWORD),
            Request::Manage(_) => Some(MANAGE),
        }
    }
}

fn requester() -> Result<u32, String> {
    if std::env::var_os(ENV_ACTION).is_none() {
        return Ok(crate::syscall::process::getuid());
    }
    std::env::var(ENV_UID)
        .ok()
        .and_then(|uid| uid.parse().ok())
        .ok_or_else(|| format!("privd named no requester in {ENV_UID}"))
}

fn administers(accounts: &Accounts, uid: u32) -> Result<(), String> {
    match accounts.by_uid(uid) {
        _ if uid == 0 => Ok(()),
        Some(person) if person.admin => Ok(()),
        Some(person) => Err(format!("{} does not administer this machine", person.name)),
        None => Err(format!("uid {uid} is nobody this machine knows")),
    }
}

fn new_password(name: &str) -> Result<String, String> {
    match secret::read_new(name)? {
        Some(secret) => secret.hash(),
        None => Err(format!("no password typed for {name}")),
    }
}

/// The current password of `person`, when they have one, typed and checked.
fn prove(person: &Person) -> Result<(), String> {
    if person.password.is_none() {
        return Ok(());
    }
    let typed = secret::read(&format!("Current password for {}: ", person.name))?
        .ok_or("no password typed")?;
    if crate::accounts::check(&person.name, typed.bytes())? {
        return Ok(());
    }
    std::thread::sleep(WRONG_PASSWORD_DELAY);
    Err(format!("that is not {}'s password", person.name))
}

/// A name nobody holds: no person, extension user or name of the base's.
fn free(accounts: &Accounts, name: &str) -> Result<(), String> {
    if !slopos_accounts_core::valid_name(name) {
        return Err(format!(
            "{name}: {}",
            slopos_accounts_core::record::BAD_NAME
        ));
    }
    if accounts.person(name).is_some()
        || crate::accounts::extension_users(Path::new("/"))?
            .iter()
            .any(|n| n == name)
    {
        return Err(format!(
            "{name}: {}",
            slopos_accounts_core::record::NAME_HELD
        ));
    }
    Ok(())
}

/// `/home/<name>`, the person's alone; refused where something is already,
/// and taken away again should it not become theirs.
fn make_home(person: &Person) -> Result<(), String> {
    let home = Path::new(&person.home);
    let shown = |e: std::io::Error| format!("{}: {e}", home.display());
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(home)
        .map_err(shown)?;
    lchown(home, Some(person.uid), Some(person.uid)).map_err(|e| {
        let _ = std::fs::remove_dir(home);
        shown(e)
    })
}

/// Take away `person`'s home, when it is a directory of theirs, links never
/// followed.
fn remove_home(person: &Person) -> Result<String, String> {
    let home = Path::new(&person.home);
    let theirs =
        std::fs::symlink_metadata(home).is_ok_and(|meta| meta.is_dir() && meta.uid() == person.uid);
    if !theirs || person.home == slopos_abi::fs::SESSION_HOME {
        return Ok(format!(
            "; {} is not theirs alone, so it stays",
            home.display()
        ));
    }
    std::fs::remove_dir_all(home).map_err(|e| format!("{}: {e}", home.display()))?;
    Ok(format!("; {} is gone", home.display()))
}

/// What a change leaves for once the record holds it: a home made for it,
/// taken away again should the record not take it, or one to take away.
enum Home {
    Kept,
    Made(PathBuf),
    Removed(Person),
}

/// The hashes a change sets, typed and checked before the store is locked,
/// so nobody's typing holds up another writer.
#[derive(Default)]
struct Typed {
    /// The requester's own password, which an administrator without one
    /// sets before adding someone.
    adder: Option<String>,
    password: Option<String>,
}

/// Whatever `request` needs typed: the current password of the requester
/// changing their own, and every new one.
fn typed(accounts: &Accounts, uid: u32, request: &Request) -> Result<Typed, String> {
    let mut typed = Typed::default();
    match request {
        Request::Manage(change) => {
            // privd runs it only for root, an administrator, or a person an
            // administrator's password answered for.
            if std::env::var_os(ENV_ACTION).is_none() {
                administers(accounts, uid)?;
            }
            match change {
                Change::Add { .. } => {
                    let adder = accounts
                        .by_uid(uid)
                        .filter(|p| p.admin && p.password.is_none());
                    if let Some(adder) = adder {
                        println!(
                            "{} has no password, which an administrator of a shared machine needs",
                            adder.name
                        );
                        typed.adder = Some(new_password(&adder.name)?);
                    }
                }
                Change::Password(name) => {
                    let person = accounts
                        .person(name)
                        .ok_or_else(|| format!("{name}: no such person"))?;
                    let _quiet = secret::quiet()?;
                    if person.uid == uid {
                        prove(person)?;
                    }
                    typed.password = Some(new_password(name)?);
                }
                _ => {}
            }
        }
        _ => {
            let person = accounts
                .by_uid(uid)
                .ok_or_else(|| format!("uid {uid} has no account; name one"))?;
            let _quiet = secret::quiet()?;
            prove(person)?;
            typed.password = Some(new_password(&person.name)?);
        }
    }
    Ok(typed)
}

fn change(request: Request) -> Result<String, String> {
    let uid = requester()?;
    let top = Path::new("/");
    let before = Store::read(top).accounts()?;
    let typed = typed(&before, uid, &request)?;
    let mut store = Store::write(top)?;
    let mut accounts = store.accounts()?;
    if accounts != before {
        return Err("the accounts changed while this was typed; ask again".into());
    }
    let (mut said, home) = match request {
        Request::Manage(change) => manage(&mut accounts, uid, change, typed)?,
        _ => {
            let name = accounts
                .by_uid(uid)
                .map(|p| p.name.clone())
                .unwrap_or_default();
            accounts.set_password(&name, typed.password)?;
            (format!("{name}'s password is changed"), Home::Kept)
        }
    };
    let second = matches!(home, Home::Made(_)) && accounts.people().len() > 1;
    let shared = match second {
        true => share_for_second(top),
        false => Ok(false),
    };
    let recorded = shared.and_then(|shared| {
        store.record(&accounts).map_err(|why| match shared {
            true => format!("{why}; nobody was added, though {LOCAL} is the system's now"),
            false => why,
        })?;
        if shared {
            said.push_str(&format!(
                "; {LOCAL} is the system's now, installed into through privd, and \
                 {SESSION_HOME} closed to others"
            ));
        }
        Ok(())
    });
    if let Err(why) = recorded {
        if let Home::Made(home) = home {
            let _ = std::fs::remove_dir(home);
        }
        return Err(why);
    }
    let synced = store.sync();
    store.unlock();
    if matches!(home, Home::Made(_))
        && !root_layout::settled()
        && Owners::read(top).is_some_and(|owners| !owners.holds(LOCAL))
    {
        said.push_str(&format!(
            "; restart before they log in, so nothing opened in {LOCAL} before is open still"
        ));
    }
    if let Home::Removed(person) = home {
        let gone = remove_home(&person).unwrap_or_else(|why| format!("; its home stays: {why}"));
        said.push_str(&gone);
    }
    synced.map(|()| said.clone()).map_err(|why| {
        format!("{said}, but the user database was not written: {why}; the next boot writes it")
    })
}

/// Share the root for a second person the add records: whether this add
/// did, and a refusal where the root's owners record cannot be read.
fn share_for_second(top: &Path) -> Result<bool, String> {
    let Some(owners) = Owners::read(top) else {
        return Err(format!(
            "{OWNERS_RECORD} cannot be read, which a boot that lays out the root writes; nobody was added"
        ));
    };
    if !owners.holds(LOCAL) {
        return Ok(false);
    }
    root_layout::share(top)
        .map(|()| true)
        .map_err(|why| match root_layout::sharing(top) {
            true => format!("{why}; nobody was added, and the next boot finishes the hand-over"),
            false => format!("{why}; nobody was added"),
        })
}

fn manage(
    accounts: &mut Accounts,
    uid: u32,
    change: Change,
    typed: Typed,
) -> Result<(String, Home), String> {
    let said = match change {
        Change::Add {
            name,
            admin,
            keep_autologin,
        } => {
            free(accounts, &name)?;
            if let Some(adder) = accounts.by_uid(uid).map(|p| p.name.clone())
                && typed.adder.is_some()
            {
                accounts.set_password(&adder, typed.adder)?;
            }
            let given = accounts.add(&name, admin)?;
            let person = accounts.person(&name).ok_or("the person just added")?;
            make_home(person)?;
            let home = PathBuf::from(&person.home);
            let mut said = format!("{name} is uid {given}, at home in {}", home.display());
            if !keep_autologin && accounts.autologin().is_some() {
                accounts.set_autologin(None)?;
                said.push_str("; no one is logged in by the desktop by itself now");
            }
            return Ok((said, Home::Made(home)));
        }
        Change::Remove { name, home } => {
            let person = accounts.remove(&name).map_err(|e| format!("{name}: {e}"))?;
            let said = format!(
                "{name} is removed; uid {} is given to nobody again",
                person.uid
            );
            let home = if home {
                Home::Removed(person)
            } else {
                Home::Kept
            };
            return Ok((said, home));
        }
        Change::Rename { old, new } => {
            free(accounts, &new)?;
            accounts
                .rename(&old, &new)
                .map_err(|e| format!("{old}: {e}"))?;
            format!("{old} is {new} now")
        }
        Change::Password(name) => {
            accounts.set_password(&name, typed.password)?;
            format!("{name}'s password is changed")
        }
        Change::Admin { name, admin } => {
            accounts
                .set_admin(&name, admin)
                .map_err(|e| format!("{name}: {e}"))?;
            match admin {
                true => format!("{name} administers this machine"),
                false => format!("{name} no longer administers this machine"),
            }
        }
        Change::Autologin(name) => {
            accounts.set_autologin(name.as_deref())?;
            let who = name.as_deref().unwrap_or("no one");
            format!("{who} is logged in by the desktop by itself")
        }
        Change::Remote(name) => {
            accounts.set_remote(name.as_deref())?;
            let who = name.as_deref().unwrap_or("the first administrator");
            format!("remoted runs as {who} from its next start")
        }
    };
    Ok((said, Home::Kept))
}

/// Write the database, and the one person's record on a root without one.
fn sync() -> Result<String, String> {
    let top = Path::new("/");
    if crate::accounts::in_memory(top) {
        return Ok(String::new());
    }
    let store = Store::write(top)?;
    if store.recorded()?.is_some() {
        store.sync()?;
        return Ok(String::new());
    }
    store.record(&Accounts::one_person())?;
    store.sync()?;
    Ok(
        "user is this machine's first administrator, with no password, logged in by the desktop"
            .into(),
    )
}

/// Every person, from the record when this process may read it and from the
/// database anyone reads otherwise.
fn list() -> Result<String, String> {
    if let Ok(Some(accounts)) = Store::read(Path::new("/")).recorded() {
        let autologin = accounts.autologin().map(|p| p.uid);
        let remote = accounts.remote().map(|p| p.uid);
        return Ok(accounts
            .people()
            .iter()
            .map(|p| {
                let mut line = row(&p.name, p.uid, p.admin, &p.home);
                line.push_str(if p.password.is_some() {
                    "  password"
                } else {
                    "  no password"
                });
                if autologin == Some(p.uid) {
                    line.push_str("  automatic login");
                }
                if remote == Some(p.uid) {
                    line.push_str("  remote");
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n"));
    }
    let group = accounts::database("/etc/group", BASE_GROUP);
    let admins = lines(&group)
        .filter_map(Group::parse)
        .find(|g| g.name == ADMIN_GROUP.as_bytes());
    Ok(accounts::people()
        .iter()
        .map(|p| {
            let admin = admins.as_ref().is_some_and(|g| g.lists(p.name.as_bytes()));
            row(&p.name, p.uid, admin, &p.home)
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

fn row(name: &str, uid: u32, admin: bool, home: &str) -> String {
    let role = if admin { "admin" } else { "-" };
    format!("{name:<16} {uid:>5}  {role:<5}  {home}")
}

fn run(args: &[String]) -> Result<String, String> {
    let request = parse(args)?;
    privd::elevate(request.action(), &privd::own_args(), Input::Terminal);
    secret::claim_terminal();
    match request {
        Request::List => list(),
        Request::Sync => sync(),
        request => change(request),
    }
}

pub fn users_main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(said) if said.is_empty() => {}
        Ok(said) => println!("{said}"),
        Err(why) => {
            eprintln!("users: {why}");
            sys_core::exit_with_code(1);
        }
    }
}
