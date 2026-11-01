//! `keymap` — see and choose keyboard layouts.
//!
//! ```text
//! keymap                        the layout in use
//! keymap show                   the layout in use, yours and the computer's
//! keymap list [--titles]        the installed layouts, the one in use marked `*`
//! keymap set NAME               your layout, while your desktop is in front
//! keymap set --computer NAME    the computer's: the login screen's, the
//!                               console's and everyone's who chose none
//! keymap reset                  use the computer's layout again
//! keymap NAME                   the same as `keymap set --computer NAME`
//! ```
//!
//! Your own layout is your preference, `keyboard.layout`, which nothing
//! needs permission to change. The computer's is kept in `/etc/keymap` by
//! privd's `org.slopos.console.keymap`. Either way init loads the layout
//! wanted, as the desktop in front changes and when asked to.

use crate::init_control::{self, Command as InitCommand};
use crate::keymap::{Own, computer_layout, current_name, exists, keep_computer_layout, layouts};

const ACTION: &str = "org.slopos.console.keymap";

const USAGE: &str = "usage: keymap [show]
       keymap list [--titles]
       keymap set [--computer] NAME
       keymap reset";

enum Command {
    Current,
    Show,
    List { titles: bool },
    Own(Option<String>),
    Computer(String),
}

fn parse(args: &[String]) -> Result<Command, String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    Ok(match words.as_slice() {
        [] => Command::Current,
        ["show"] => Command::Show,
        ["list" | "-l" | "--list"] => Command::List { titles: false },
        ["list", "--titles"] => Command::List { titles: true },
        ["set", name] if !name.starts_with('-') => Command::Own(Some(name.to_string())),
        ["set", "--computer", name] => Command::Computer(name.to_string()),
        ["reset"] => Command::Own(None),
        [name] if !name.starts_with('-') && !name.is_empty() => Command::Computer(name.to_string()),
        _ => return Err(USAGE.to_string()),
    })
}

fn titled(name: &str) -> String {
    match layouts().into_iter().find(|l| l.name == name) {
        Some(layout) => format!("{name}  {}", layout.title),
        None => name.to_string(),
    }
}

fn have_init_follow() -> Result<(), String> {
    init_control::ask(InitCommand::Keymap)
        .map_err(|why| format!("kept, but it takes effect at the next login: {why}"))
}

fn run(command: Command) -> Result<String, String> {
    match command {
        Command::Current => current_name().ok_or_else(|| "cannot query the active layout".into()),
        Command::Show => {
            let in_use = current_name().unwrap_or_default();
            let own = crate::prefs::read_own()
                .keyboard_layout()
                .map_or_else(|| String::from("none, so the computer's"), titled);
            let computer = computer_layout()
                .map_or_else(|| titled(crate::keymap::BUILT_IN), |name| titled(&name));
            Ok(format!(
                "in use      {}\nyours       {own}\ncomputer's  {computer}",
                titled(&in_use)
            ))
        }
        Command::List { titles } => {
            let active = current_name();
            let mut layouts = layouts();
            if !titles {
                layouts.sort_by(|a, b| a.name.cmp(&b.name));
            }
            let lines: Vec<String> = layouts
                .into_iter()
                .map(|layout| {
                    let mark = if active.as_deref() == Some(layout.name.as_str()) {
                        '*'
                    } else {
                        ' '
                    };
                    match titles {
                        true => format!("{mark} {:<8} {}", layout.name, layout.title),
                        false => format!("{mark} {}", layout.name),
                    }
                })
                .collect();
            Ok(lines.join("\n"))
        }
        Command::Own(_) if std::env::var_os(slopos_priv_core::ENV_ACTION).is_some() => {
            Err("privd keeps only the computer's layout; set yours as yourself".into())
        }
        Command::Own(name) => {
            crate::keymap::keep_own(name.as_deref()).map_err(|why| match why {
                Own::NoSuchLayout => format!(
                    "no such layout: '{}'; `keymap list` names them",
                    name.as_deref().unwrap_or_default()
                ),
                Own::NoHome => String::from("you have no home to keep a layout in"),
                Own::NotKept(why) => why,
                Own::NotLoaded(why) => {
                    format!("kept, but it takes effect at the next login: {why}")
                }
            })?;
            Ok(match name {
                Some(name) => format!("keymap: yours is {name}"),
                None => String::from("keymap: yours is the computer's"),
            })
        }
        Command::Computer(name) => {
            if !exists(&name) {
                return Err(format!(
                    "no such layout: '{name}'; `keymap list` names them"
                ));
            }
            crate::privd::elevate(
                Some(ACTION),
                &crate::privd::own_args(),
                crate::privd::Input::Closed,
            );
            keep_computer_layout(&name)?;
            have_init_follow()?;
            Ok(format!("keymap: the computer's is {name}"))
        }
    }
}

pub fn keymap_user_main() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let said = parse(&args).and_then(run);
    match said {
        Ok(said) => {
            println!("{said}");
            0
        }
        Err(why) => {
            eprintln!("keymap: {why}");
            1
        }
    }
}
