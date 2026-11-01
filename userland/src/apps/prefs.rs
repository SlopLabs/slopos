//! `/bin/prefs` — your preferences, which Settings changes too: the
//! desktop's background and clock and your keyboard layout, kept in
//! `~/.config/slopos/prefs`. Your desktop takes a change within a moment;
//! nothing here needs anyone's permission.

use std::time::Duration;

use slopos_settings_core::prefs::{Key, Kind, Prefs};

use crate::init_control::{self, Command as InitCommand};

const USAGE: &str = "usage: prefs [list]
       prefs get KEY
       prefs set KEY VALUE
       prefs reset KEY | --all
       prefs describe [KEY]
       prefs monitor";

const EXIT_FAILED: i32 = 1;
const EXIT_USAGE: i32 = 2;

/// How often `monitor` looks at the file.
const LOOK_EVERY: Duration = Duration::from_millis(250);

enum Command {
    List,
    Get(Key),
    Set(Key, String),
    Reset(Option<Key>),
    Describe(Option<Key>),
    Monitor,
}

fn key(name: &str) -> Result<Key, String> {
    Key::named(name).ok_or_else(|| {
        format!(
            "{name}: {}",
            slopos_settings_core::prefs::Refused::NoSuchKey
        )
    })
}

fn parse(args: &[String]) -> Result<Command, String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    Ok(match words.as_slice() {
        [] | ["list"] => Command::List,
        ["get", name] => Command::Get(key(name)?),
        ["set", name, value @ ..] if !value.is_empty() => Command::Set(key(name)?, value.join(" ")),
        ["reset", "--all"] => Command::Reset(None),
        ["reset", name] => Command::Reset(Some(key(name)?)),
        ["describe"] => Command::Describe(None),
        ["describe", name] => Command::Describe(Some(key(name)?)),
        ["monitor"] => Command::Monitor,
        _ => return Err(USAGE.to_string()),
    })
}

fn listed(prefs: &Prefs, key: Key) -> String {
    let default = match prefs.is_set(key) {
        true => "",
        false => "  (default)",
    };
    format!("{} = {}{default}", key.name(), prefs.get(key))
}

fn described(key: Key) -> String {
    let takes = match key.kind() {
        Kind::Choice(choices) => choices.join(", "),
        Kind::Path => String::from("the whole path of a PNG picture, or nothing"),
        Kind::Layout => String::from("a name `keymap list` prints, or nothing"),
    };
    let default = match key.default_value() {
        "" => "nothing",
        value => value,
    };
    format!(
        "{}\n  {}\n  takes: {takes}\n  default: {default}",
        key.name(),
        key.summary()
    )
}

/// Refuse what the key's kind admits but this computer cannot use.
fn usable(key: Key, value: &str) -> Result<(), String> {
    match key {
        Key::BackgroundPicture if !value.is_empty() => match crate::regular_file::picture(value) {
            Some(_) => Ok(()),
            None => Err(format!(
                "{value}: not a file holding a PNG picture this computer can show"
            )),
        },
        Key::KeyboardLayout if !value.is_empty() => {
            match crate::keymap::layout_path(value)
                .is_some_and(|p| std::path::Path::new(&p).is_file())
            {
                true => Ok(()),
                false => Err(format!("{value}: no such layout; `keymap list` names them")),
            }
        }
        _ => Ok(()),
    }
}

/// Have init load the layout now, should it be this person's in front.
fn keyboard_changed() -> Result<(), String> {
    init_control::ask(InitCommand::Keymap)
        .map_err(|why| format!("kept, but the layout changes at the next login: {why}"))
}

fn run(command: Command) -> Result<String, String> {
    let path = crate::prefs::own_path().ok_or("you have no home to keep preferences in")?;
    let mut prefs = crate::prefs::read(&path);
    let changed: Vec<Key> = match command {
        Command::List => {
            let lines: Vec<String> = Key::ALL.iter().map(|&k| listed(&prefs, k)).collect();
            return Ok(lines.join("\n"));
        }
        Command::Get(key) => return Ok(prefs.get(key).to_string()),
        Command::Describe(only) => {
            let keys: Vec<Key> = only.map_or(Key::ALL.to_vec(), |k| vec![k]);
            let said: Vec<String> = keys.into_iter().map(described).collect();
            return Ok(said.join("\n\n"));
        }
        Command::Monitor => return monitor(path, prefs),
        Command::Set(key, value) => {
            prefs
                .set(key, &value)
                .map_err(|why| format!("{}: {why}", key.name()))?;
            usable(key, &value)?;
            vec![key]
        }
        Command::Reset(Some(key)) => {
            prefs.reset(key);
            vec![key]
        }
        Command::Reset(None) => {
            let set: Vec<Key> = Key::ALL.into_iter().filter(|&k| prefs.is_set(k)).collect();
            for &key in &set {
                prefs.reset(key);
            }
            set
        }
    };
    crate::prefs::write(&path, &prefs)?;
    if changed.contains(&Key::KeyboardLayout) {
        keyboard_changed()?;
    }
    Ok(String::new())
}

fn monitor(path: String, mut seen: Prefs) -> Result<String, String> {
    let mut watch = crate::prefs::Watch::new(path.clone());
    loop {
        std::thread::sleep(LOOK_EVERY);
        if !watch.changed() {
            continue;
        }
        let now = crate::prefs::read(&path);
        for key in Key::ALL {
            if now.get(key) != seen.get(key) {
                println!("{}", listed(&now, key));
            }
        }
        seen = now;
    }
}

pub fn prefs_main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match parse(&args) {
        Ok(command) => command,
        Err(why) => {
            eprintln!("prefs: {why}");
            std::process::exit(EXIT_USAGE);
        }
    };
    match run(command) {
        Ok(said) if said.is_empty() => {}
        Ok(said) => println!("{said}"),
        Err(why) => {
            eprintln!("prefs: {why}");
            std::process::exit(EXIT_FAILED);
        }
    }
}
