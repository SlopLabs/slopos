//! `/bin/session restart` asks init to end the desktop session and start it
//! again; `session end`, which only uid 0 may ask, leaves it ended.

use crate::init_control::{self, Command};

const USAGE: &str = "usage: session restart|end";

pub fn session_main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["restart"] => Command::RestartSession,
        ["end"] => Command::EndSession,
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(why) = init_control::ask(command) {
        eprintln!("session: {why}");
        std::process::exit(1);
    }
}
