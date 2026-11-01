//! `/bin/netctl` — the network settings this computer keeps, in
//! [`network::PATH`]: whether networking is on, the DNS servers, and how
//! each adapter gets its address. `ip` changes the network until the next
//! start; netctl changes what it starts as, and brings it there now. Init
//! runs `netctl apply` at every start and `netctl apply ADAPTER` as each
//! adapter appears. Changing the settings is privd's
//! `org.slopos.network.settings`.

use slopos_settings_core::network::{
    self, Adapter, AdapterId, Dns, Ipv4Method, Live, LiveAdapter, Mac, Scope, Settings,
};

use slopos_abi::fs::O_NOFOLLOW;
use slopos_abi::syscall::posix::{LOCK_EX, LOCK_NB, O_CLOEXEC, O_NONBLOCK};

use crate::network_settings as kept;
use crate::privd::{self, Input};

const ACTION: &str = "org.slopos.network.settings";
const LOCK: &str = "/run/netctl.lock";
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

const USAGE: &str = "usage: netctl [status [ADAPTER]]
       netctl set ADAPTER auto
       netctl set ADAPTER manual ADDRESS/LENGTH [gateway ADDRESS]
       netctl set ADAPTER on|off
       netctl dns auto | SERVER [SERVER [SERVER]]
       netctl networking on|off
       netctl forget ADAPTER
       netctl apply [ADAPTER]
       netctl load < SETTINGS
       netctl cat
       netctl help";

const HELP: &str = "
An ADAPTER is named as `ip link` names it, such as eth0, or by its hardware
address. Settings for a present adapter are kept by its hardware address,
since a name passes to the next adapter once its holder leaves.

  auto      an address, gateway and DNS servers from DHCP, as every adapter
            gets until told otherwise
  manual    this address, with the length of its network's prefix, and the
            gateway packets for other networks leave through
  on|off    whether the adapter is brought up at all
  dns       DNS servers for every adapter, which leases then never change;
            auto takes them from the leases again
  forget    drop what is kept for the adapter, which goes back to DHCP
  apply     bring the network to its settings now, as every start does
  load      keep the settings standard input holds, in the form `cat`
            prints, in place of every one kept, and apply them

Each change takes effect at once and at every start. The settings are kept
in /etc/network, one line for each:

  networking off
  dns 1.1.1.1 9.9.9.9
  adapter 52:54:00:12:34:56
    ipv4 manual 192.168.1.20/24
    gateway 192.168.1.1
    off";

const EXIT_FAILED: i32 = 1;
const EXIT_USAGE: i32 = 2;

enum Command {
    Status(Option<String>),
    Set(String, Change),
    Dns(Dns),
    Networking(bool),
    Forget(String),
    Apply(Option<String>),
    Load,
    /// What `load` read from standard input.
    Loaded(Settings),
    Cat,
    Help,
}

enum Change {
    Ipv4(Ipv4Method),
    Enabled(bool),
}

fn usage(why: impl std::fmt::Display) -> String {
    format!("{why}\n{USAGE}")
}

fn checked<T>(read: Result<T, network::Invalid>) -> Result<T, String> {
    read.map_err(usage)
}

fn parse(args: &[String]) -> Result<Command, String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    if let ["set" | "forget" | "apply", adapter, ..] = words.as_slice() {
        checked(AdapterId::parse(adapter))?;
    }
    Ok(match words.as_slice() {
        [] | ["status"] => Command::Status(None),
        ["status", adapter] => Command::Status(Some(adapter.to_string())),
        ["set", adapter, "auto"] => {
            Command::Set(adapter.to_string(), Change::Ipv4(Ipv4Method::Auto))
        }
        ["set", adapter, "manual", address, rest @ ..] => {
            let address = checked(network::parse_address(address))?;
            let gateway = match rest {
                [] => None,
                ["gateway", gateway] => Some(checked(network::parse_gateway(gateway))?),
                _ => return Err(usage("A manual address takes only a gateway after it.")),
            };
            let method = checked(Ipv4Method::manual(address, gateway))?;
            Command::Set(adapter.to_string(), Change::Ipv4(method))
        }
        ["set", adapter, state @ ("on" | "off")] => {
            Command::Set(adapter.to_string(), Change::Enabled(*state == "on"))
        }
        ["dns", "auto"] => Command::Dns(Dns::Auto),
        ["dns", servers @ ..] if !servers.is_empty() => {
            Command::Dns(checked(network::parse_dns(&servers.join(" ")))?)
        }
        ["networking", state @ ("on" | "off")] => Command::Networking(*state == "on"),
        ["forget", adapter] => Command::Forget(adapter.to_string()),
        ["apply"] => Command::Apply(None),
        ["apply", adapter] => Command::Apply(Some(adapter.to_string())),
        ["load"] => Command::Load,
        ["cat"] => Command::Cat,
        ["help" | "-h" | "--help"] => Command::Help,
        _ => return Err(USAGE.to_string()),
    })
}

impl Command {
    fn changes(&self) -> bool {
        matches!(
            self,
            Command::Set(..)
                | Command::Dns(_)
                | Command::Networking(_)
                | Command::Forget(_)
                | Command::Apply(_)
                | Command::Load
                | Command::Loaded(_)
        )
    }
}

/// The adapter `named` names now, by its name or its hardware address.
fn present<'a>(live: &'a Live, named: &str) -> Option<&'a LiveAdapter> {
    let mac = Mac::from_str_bytes(named.as_bytes());
    live.adapters
        .iter()
        .filter(|a| !a.loopback)
        .find(|a| a.name == named || Some(a.mac) == mac)
}

fn run(command: Command) -> Result<String, String> {
    if let Command::Help = command {
        return Ok(format!("{USAGE}\n{HELP}"));
    }
    if let Command::Cat = command {
        return Ok(kept::read()?.render().trim_end().to_string());
    }
    if let Command::Loaded(settings) = command {
        kept::write(&settings)?;
        return finish(&settings, &Scope::Everything, String::new());
    }
    let mut settings = kept::read()?;
    let live = kept::live()?;
    let (scope, said) = match command {
        Command::Status(adapter) => return status(&settings, &live, adapter.as_deref()),
        Command::Set(named, change) => {
            let id = AdapterId::parse(&named).map_err(|e| e.to_string())?;
            let now = present(&live, &named);
            let governing = now
                .and_then(|a| settings.governing(&a.name, a.mac))
                .or_else(|| settings.adapters.iter().find(|a| a.id == id))
                .cloned();
            let (mut enabled, mut ipv4) =
                governing.map_or((true, Ipv4Method::Auto), |a| (a.enabled, a.ipv4));
            match change {
                Change::Ipv4(method) => ipv4 = method,
                Change::Enabled(on) => enabled = on,
            }
            let said = format!("{named}: {}", described(enabled, ipv4));
            match now {
                Some(a) => {
                    settings.set_for(&a.name, a.mac, enabled, ipv4);
                    (Some(Scope::Adapter(a.name.clone())), said)
                }
                None => {
                    settings.set(Adapter { id, enabled, ipv4 });
                    (None, format!("{said}, once it is here"))
                }
            }
        }
        Command::Dns(dns) => {
            let said = match &dns {
                Dns::Auto => String::from("DNS servers from the leases"),
                Dns::Manual(servers) => format!("DNS servers {}", joined(servers)),
            };
            settings.dns = dns;
            (Some(Scope::Everything), said)
        }
        Command::Networking(on) => {
            settings.networking = on;
            let said = format!("networking {}", if on { "on" } else { "off" });
            (Some(Scope::Everything), said)
        }
        Command::Forget(named) => {
            if !forget(&mut settings, &live, &named) {
                return Err(format!("nothing is kept for {named}"));
            }
            let now = present(&live, &named);
            kept::write(&settings)?;
            let Some(a) = now else {
                return Ok(format!("{named}: forgotten"));
            };
            let mut back = settings.clone();
            back.set_for(&a.name, a.mac, true, Ipv4Method::Auto);
            return finish(
                &back,
                &Scope::Adapter(a.name.clone()),
                format!("{named}: forgotten, back to DHCP"),
            );
        }
        Command::Apply(adapter) => {
            let scope = match adapter {
                Some(named) => match present(&live, &named) {
                    Some(a) => Scope::Adapter(a.name.clone()),
                    None => return Err(format!("{named}: no such adapter")),
                },
                None => Scope::Everything,
            };
            return finish(&settings, &scope, String::new());
        }
        Command::Help | Command::Cat | Command::Load | Command::Loaded(_) => {
            unreachable!("answered above")
        }
    };
    kept::write(&settings)?;
    match scope {
        Some(scope) => finish(&settings, &scope, said),
        None => Ok(said),
    }
}

fn finish(settings: &Settings, scope: &Scope, said: String) -> Result<String, String> {
    let failed = kept::apply(settings, scope);
    match failed.is_empty() {
        true => Ok(said),
        false => Err(match said.is_empty() {
            true => format!("the settings are kept, but {}", failed.join("; ")),
            false => format!("{said} is kept, but {}", failed.join("; ")),
        }),
    }
}

fn joined(servers: &[network::Ipv4]) -> String {
    servers
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

fn described(enabled: bool, ipv4: Ipv4Method) -> String {
    let method = match ipv4 {
        Ipv4Method::Auto => String::from("auto"),
        Ipv4Method::Manual {
            address,
            gateway: Some(gateway),
        } => format!("manual {address} gateway {gateway}"),
        Ipv4Method::Manual { address, .. } => format!("manual {address}"),
    };
    match enabled {
        true => method,
        false => format!("{method}, off"),
    }
}

fn status(settings: &Settings, live: &Live, only: Option<&str>) -> Result<String, String> {
    let mut out = Vec::new();
    if only.is_none() {
        out.push(format!(
            "networking  {}{}",
            if live.networking { "on" } else { "off" },
            match settings.networking == live.networking {
                true => "",
                false => " (kept otherwise)",
            }
        ));
        let dns = match &settings.dns {
            Dns::Auto => "from the leases",
            Dns::Manual(_) => "kept",
        };
        let servers = match live.dns.servers.is_empty() {
            true => String::from("none"),
            false => joined(&live.dns.servers),
        };
        out.push(format!("dns         {servers} ({dns})"));
    }
    let shown: Vec<&LiveAdapter> = live
        .adapters
        .iter()
        .filter(|a| !a.loopback)
        .filter(|a| {
            only.is_none_or(|named| {
                a.name == named || Mac::from_str_bytes(named.as_bytes()) == Some(a.mac)
            })
        })
        .collect();
    if let Some(named) = only
        && shown.is_empty()
    {
        return Err(format!("{named}: no such adapter"));
    }
    for adapter in shown {
        out.push(String::new());
        let link = match (adapter.up, adapter.carrier) {
            (false, _) => "off",
            (true, true) => "connected",
            (true, false) => "no cable",
        };
        out.push(format!("{}  {}  {link}", adapter.name, adapter.mac));
        let kept = match settings.governing(&adapter.name, adapter.mac) {
            Some(a) => described(a.enabled, a.ipv4),
            None => String::from("auto (nothing kept)"),
        };
        out.push(format!("  settings  {kept}"));
        let addresses: Vec<String> = adapter
            .addresses
            .iter()
            .filter(|a| !a.link_local)
            .map(|a| {
                let from = if a.from_dhcp { " (DHCP)" } else { "" };
                format!("{}{from}", a.address)
            })
            .collect();
        if !addresses.is_empty() {
            out.push(format!("  address   {}", addresses.join(", ")));
        }
        match adapter.gateway {
            Some(gateway) if gateway.is_unspecified() => out.push("  gateway   on-link".into()),
            Some(gateway) => out.push(format!("  gateway   {gateway}")),
            None => {}
        }
        let due = network::plan(settings, live, &Scope::Adapter(adapter.name.clone()));
        if !due.is_empty() {
            let steps: Vec<String> = due.iter().map(|s| s.describe(&adapter.name)).collect();
            out.push(format!("  not yet   {}", steps.join("; ")));
        }
    }
    if only.is_none() {
        for absent in settings
            .adapters
            .iter()
            .filter(|kept| !live.adapters.iter().any(|a| kept.id.names(&a.name, a.mac)))
        {
            out.push(String::new());
            out.push(format!("{}  not here", absent.id));
            out.push(format!(
                "  settings  {}",
                described(absent.enabled, absent.ipv4)
            ));
        }
    }
    Ok(out.join("\n"))
}

/// Drop what `settings` keeps for the adapter `named`, by whatever names it
/// now when it is here: whether anything was.
fn forget(settings: &mut Settings, live: &Live, named: &str) -> bool {
    match (present(live, named), AdapterId::parse(named)) {
        (Some(a), _) => settings.forget(&a.name, a.mac),
        (None, Ok(id)) => {
            let before = settings.adapters.len();
            settings.adapters.retain(|a| a.id != id);
            settings.adapters.len() != before
        }
        (None, Err(_)) => false,
    }
}

/// Why `command` cannot do what it asks, found before anyone is asked to
/// allow it.
fn refused(command: &Command) -> Option<(i32, String)> {
    match command {
        Command::Forget(named) => {
            let (Ok(mut settings), Ok(live)) = (kept::read(), kept::live()) else {
                return None;
            };
            (!forget(&mut settings, &live, named))
                .then(|| (EXIT_FAILED, format!("nothing is kept for {named}")))
        }
        Command::Load if privd::stands_in(0) => Some((
            EXIT_USAGE,
            usage("load reads the settings from a file or a pipe, such as netctl load < FILE"),
        )),
        Command::Apply(Some(named)) => match kept::live() {
            Ok(live) if present(&live, named).is_some() => None,
            Ok(_) => Some((EXIT_FAILED, format!("{named}: no such adapter"))),
            Err(why) => Some((EXIT_FAILED, why)),
        },
        _ => None,
    }
}

/// The settings standard input holds, for `load`.
fn given() -> Result<Settings, String> {
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(
        &mut std::io::Read::take(std::io::stdin().lock(), network::FILE_MAX as u64 + 1),
        &mut bytes,
    )
    .map_err(|e| format!("reading the settings: {e}"))?;
    if bytes.len() > network::FILE_MAX {
        return Err(String::from("the settings given are longer than any kept"));
    }
    let text = String::from_utf8(bytes).map_err(|_| "the settings given are not text")?;
    Settings::parse(&text).map_err(|e| format!("the settings given: {e}"))
}

/// Held while this netctl changes the network, so that init's and a
/// person's never interleave their reads and writes: a regular file of
/// uid 0's, waited on for as long as an apply takes.
fn exclusive() -> Result<std::fs::File, String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .mode(0o600)
        .custom_flags((O_NOFOLLOW as u64 | O_NONBLOCK | O_CLOEXEC) as i32)
        .open(LOCK)
        .map_err(|e| format!("{LOCK}: {e}"))?;
    let meta = file.metadata().map_err(|e| format!("{LOCK}: {e}"))?;
    if !meta.is_file() || meta.uid() != 0 {
        return Err(format!("{LOCK}: not a file uid 0 keeps"));
    }
    let start = std::time::Instant::now();
    while crate::syscall::fs::flock(file.as_raw_fd(), LOCK_EX | LOCK_NB).is_err() {
        if start.elapsed() > LOCK_WAIT {
            return Err(String::from(
                "another netctl is changing the network; try again",
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Ok(file)
}

pub fn netctl_main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match parse(&args) {
        Ok(command) => command,
        Err(why) => {
            eprintln!("netctl: {why}");
            std::process::exit(EXIT_USAGE);
        }
    };
    if let Some((status, why)) = refused(&command) {
        eprintln!("netctl: {why}");
        std::process::exit(status);
    }
    if command.changes() {
        privd::elevate(Some(ACTION), &privd::own_args(), Input::Closed);
    }
    let command = match command {
        Command::Load => given().map(Command::Loaded),
        command => Ok(command),
    };
    let ran = command.and_then(|command| {
        let _held = command.changes().then(exclusive).transpose()?;
        run(command)
    });
    match ran {
        Ok(said) if said.is_empty() => {}
        Ok(said) => println!("{said}"),
        Err(why) => {
            eprintln!("netctl: {why}");
            std::process::exit(EXIT_FAILED);
        }
    }
}
