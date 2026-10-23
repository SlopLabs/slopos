use alloc::string::String;

/// Where `/src/slopos` fetches from unless the choices say.
pub const DEFAULT_REMOTE: &str = "https://github.com/SlopLabs/slopos";
/// What the installed kernel boots with unless the choices say: a slot that
/// panics resets to the default, which is how a tried system falls back.
pub const DEFAULT_CMDLINE: &str = "panic=reboot";
/// A new root's first administrator unless the choices name one.
pub const FIRST_PERSON: &str = "user";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Erase,
    /// SlopOS's partitions new, in the free region of that index or the
    /// largest.
    Free {
        region: Option<usize>,
    },
    /// The root over partition `root`, keeping what it holds when `keep`; the
    /// disk's own ESP, boot and crash partitions, and new ones in the free
    /// region where it has none.
    Reuse {
        root: u32,
        region: Option<usize>,
        keep: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Person {
    pub name: String,
    /// Their password's hash, when they have a password.
    pub password: Option<String>,
}

/// What to install, as asked: each `None` is the engine's default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choices {
    pub disk: String,
    pub mode: Mode,
    /// A new root's size, which erase and free-space installs take.
    pub root_size: Option<u64>,
    /// Where `/src/slopos` fetches from, when the medium carries a clone.
    pub remote: Option<String>,
    /// Whether SlopOS goes first in the firmware's boot order.
    pub first: bool,
    /// A new root's first administrator; a kept root keeps its own people.
    pub person: Option<Person>,
    /// What the installed kernel boots with.
    pub cmdline: Option<String>,
}

impl Choices {
    /// `mode` on `disk`, everything else the engine's default.
    pub fn new(disk: &str, mode: Mode) -> Choices {
        Choices {
            disk: disk.into(),
            mode,
            root_size: None,
            remote: None,
            first: false,
            person: None,
            cmdline: None,
        }
    }
}
