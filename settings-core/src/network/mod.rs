//! The network settings `/bin/netctl` keeps in [`PATH`]: whether networking
//! is on, the DNS servers, and how each adapter it was told about gets its
//! address. An adapter no block names keeps what the kernel gives every
//! adapter as it appears, an address from DHCP.
//!
//! ```text
//! networking off
//! dns 1.1.1.1 9.9.9.9
//!
//! adapter 52:54:00:12:34:56
//!   ipv4 manual 192.168.1.20/24
//!   gateway 192.168.1.1
//!
//! adapter eth1
//!   ipv4 auto
//!   off
//! ```
//!
//! An adapter is named by its hardware address or by the name the kernel
//! gives it. A name is the next adapter's once its holder leaves, so a block
//! for a hardware address wins over one for a name.

mod plan;

#[cfg(test)]
mod tests;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use slopos_abi::net::{NET_IFNAMSIZ, NET_MAX_RESOLVERS};
pub use slopos_net_core::{Cidr, Ipv4, Mac};

pub use plan::{Live, LiveAdapter, LiveAddress, LiveDns, Scope, Step, plan};

pub const PATH: &str = "/etc/network";

/// The most the settings file holds, past what any computer's adapters need.
pub const FILE_MAX: usize = 64 * 1024;

/// What a default route through an adapter's gateway weighs, plus the
/// adapter's index, as a lease's does, so the adapter published first wins.
pub const GATEWAY_METRIC: u32 = 100;

pub const MAX_DNS: usize = NET_MAX_RESOLVERS;

const LOOPBACK: &str = "lo";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdapterId {
    Name(String),
    Mac(Mac),
}

impl AdapterId {
    /// A hardware address, `52:54:00:12:34:56`, or a name the kernel gives
    /// an adapter, `eth0`; never the loopback.
    pub fn parse(text: &str) -> Result<AdapterId, Invalid> {
        if let Some(mac) = Mac::from_str_bytes(text.as_bytes()) {
            return match mac.is_zero() {
                true => Err(Invalid::Adapter),
                false => Ok(AdapterId::Mac(mac)),
            };
        }
        if text == LOOPBACK {
            return Err(Invalid::Loopback);
        }
        let named = !text.is_empty()
            && text.len() < NET_IFNAMSIZ
            && text.starts_with(|c: char| c.is_ascii_lowercase())
            && text
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
        match named {
            true => Ok(AdapterId::Name(text.to_string())),
            false => Err(Invalid::Adapter),
        }
    }

    pub fn names(&self, name: &str, mac: Mac) -> bool {
        match self {
            AdapterId::Name(n) => n == name,
            AdapterId::Mac(m) => *m == mac,
        }
    }
}

impl fmt::Display for AdapterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AdapterId::Name(name) => f.write_str(name),
            AdapterId::Mac(mac) => write!(f, "{mac}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ipv4Method {
    /// From DHCP, as every adapter is until told otherwise.
    Auto,
    Manual {
        address: Cidr,
        gateway: Option<Ipv4>,
    },
}

impl Ipv4Method {
    /// A manual address, held to [`check_address`] and its gateway to
    /// [`check_gateway`].
    pub fn manual(address: Cidr, gateway: Option<Ipv4>) -> Result<Ipv4Method, Invalid> {
        check_address(address)?;
        if let Some(gateway) = gateway {
            check_gateway(address, gateway)?;
        }
        Ok(Ipv4Method::Manual { address, gateway })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Adapter {
    pub id: AdapterId,
    pub enabled: bool,
    pub ipv4: Ipv4Method,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dns {
    /// Whatever the leases name.
    Auto,
    Manual(Vec<Ipv4>),
}

impl Dns {
    pub fn manual(servers: Vec<Ipv4>) -> Result<Dns, Invalid> {
        if servers.is_empty() {
            return Ok(Dns::Auto);
        }
        if servers.len() > MAX_DNS {
            return Err(Invalid::TooManyDns);
        }
        if servers.iter().any(|&s| !unicast(s)) {
            return Err(Invalid::DnsNotUnicast);
        }
        Ok(Dns::Manual(servers))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub networking: bool,
    pub dns: Dns,
    pub adapters: Vec<Adapter>,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            networking: true,
            dns: Dns::Auto,
            adapters: Vec::new(),
        }
    }
}

/// Why a value is not a setting, as a sentence to the person who gave it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invalid {
    Adapter,
    Loopback,
    Address,
    NoPrefix,
    Prefix,
    NotUnicast,
    NetworkAddress,
    BroadcastAddress,
    Gateway,
    GatewayOutside,
    GatewayIsAddress,
    GatewayNotHost,
    DnsServer,
    DnsNotUnicast,
    TooManyDns,
    GatewayWithoutAddress,
    Line,
    OutsideAdapter,
    Repeated,
    TwoBlocks,
}

impl Invalid {
    pub const fn sentence(self) -> &'static str {
        match self {
            Invalid::Adapter => {
                "Name an adapter as the computer does, such as eth0, or by its hardware address."
            }
            Invalid::Loopback => "The loopback adapter keeps settings of its own.",
            Invalid::Address => "Type an address such as 192.168.1.20/24.",
            Invalid::NoPrefix => {
                "Give the address with the length of its network's prefix, such as 192.168.1.20/24."
            }
            Invalid::Prefix => "A prefix length is a number from 1 to 32.",
            Invalid::NotUnicast => {
                "The address can't be 0.0.0.0 or a loopback, multicast or broadcast address."
            }
            Invalid::NetworkAddress => {
                "That address names the whole network; choose one inside it, such as 192.168.1.20."
            }
            Invalid::BroadcastAddress => {
                "That is the network's broadcast address; choose one inside the network."
            }
            Invalid::Gateway => "Type a gateway such as 192.168.1.1.",
            Invalid::GatewayOutside => "The gateway has to be on the adapter's own network.",
            Invalid::GatewayIsAddress => "The gateway can't be the adapter's own address.",
            Invalid::GatewayNotHost => {
                "The gateway can't be the network's own address or its broadcast address."
            }
            Invalid::DnsServer => "Type DNS servers such as 1.1.1.1, separated by spaces.",
            Invalid::DnsNotUnicast => {
                "A DNS server can't be 0.0.0.0 or a loopback, multicast or broadcast address."
            }
            Invalid::TooManyDns => "There can be at most three DNS servers.",
            Invalid::GatewayWithoutAddress => {
                "A gateway goes with an address you set, so set the address too."
            }
            Invalid::Line => "This line isn't one the network settings have.",
            Invalid::OutsideAdapter => "An indented line belongs to an adapter above it.",
            Invalid::Repeated => "This line says again what a line above it said.",
            Invalid::TwoBlocks => "This adapter has settings above already.",
        }
    }
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.sentence())
    }
}

/// An [`Invalid`] line of the settings file, counted from 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub why: Invalid,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.why)
    }
}

fn unicast(addr: Ipv4) -> bool {
    let first = addr.0[0];
    !addr.is_unspecified() && !addr.is_loopback() && first < 224
}

/// An address an adapter may hold: a host on its network, never the network
/// or its broadcast where those are distinct from its hosts.
pub fn check_address(address: Cidr) -> Result<(), Invalid> {
    if address.prefix_len == 0 {
        return Err(Invalid::Prefix);
    }
    if !unicast(address.addr) {
        return Err(Invalid::NotUnicast);
    }
    if address.prefix_len <= 30 {
        if address.addr == address.network() {
            return Err(Invalid::NetworkAddress);
        }
        if address.addr == address.broadcast() {
            return Err(Invalid::BroadcastAddress);
        }
    }
    Ok(())
}

pub fn check_gateway(address: Cidr, gateway: Ipv4) -> Result<(), Invalid> {
    if !unicast(gateway) {
        return Err(Invalid::NotUnicast);
    }
    if gateway == address.addr {
        return Err(Invalid::GatewayIsAddress);
    }
    let gateway = Cidr {
        addr: gateway,
        prefix_len: address.prefix_len,
    };
    if gateway.network() != address.network() {
        return Err(Invalid::GatewayOutside);
    }
    match address.prefix_len <= 30
        && (gateway.addr == address.network() || gateway.addr == address.broadcast())
    {
        true => Err(Invalid::GatewayNotHost),
        false => Ok(()),
    }
}

/// An address as a person types it: `192.168.1.20/24` or
/// `192.168.1.20/255.255.255.0`, never a bare address, whose network would
/// be guessed.
pub fn parse_address(text: &str) -> Result<Cidr, Invalid> {
    let text = text.trim();
    let Some((addr, prefix)) = text.split_once('/') else {
        return match Ipv4::from_str_bytes(text.as_bytes()) {
            Some(_) => Err(Invalid::NoPrefix),
            None => Err(Invalid::Address),
        };
    };
    let addr = Ipv4::from_str_bytes(addr.as_bytes()).ok_or(Invalid::Address)?;
    let prefix_len = match Ipv4::from_str_bytes(prefix.as_bytes()) {
        Some(mask) => slopos_net_core::mask_to_prefix_len(mask.0).ok_or(Invalid::Prefix)?,
        None => prefix
            .parse::<u8>()
            .ok()
            .filter(|&p| (1..=32).contains(&p))
            .ok_or(Invalid::Prefix)?,
    };
    let address = Cidr::new(addr, prefix_len).ok_or(Invalid::Prefix)?;
    check_address(address)?;
    Ok(address)
}

pub fn parse_gateway(text: &str) -> Result<Ipv4, Invalid> {
    Ipv4::from_str_bytes(text.trim().as_bytes()).ok_or(Invalid::Gateway)
}

/// DNS servers as a person types them, separated by spaces or commas; none
/// is [`Dns::Auto`].
pub fn parse_dns(text: &str) -> Result<Dns, Invalid> {
    let servers = text
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|word| !word.is_empty())
        .map(|word| Ipv4::from_str_bytes(word.as_bytes()).ok_or(Invalid::DnsServer))
        .collect::<Result<Vec<_>, _>>()?;
    Dns::manual(servers)
}

const HEADER: &str = "\
# The network settings netctl keeps, applied at every start and as each
# adapter appears. `netctl help` says what each line means.
";

impl Settings {
    pub fn parse(text: &str) -> Result<Settings, ParseError> {
        let mut settings = Settings::default();
        let mut said = Said::default();
        let mut block: Option<Block> = None;
        for (at, raw) in text.lines().enumerate() {
            let line = at + 1;
            let fail = |why| ParseError { line, why };
            let content = raw.split('#').next().unwrap_or("");
            if content.trim().is_empty() {
                continue;
            }
            let words: Vec<&str> = content.split_whitespace().collect();
            if content.starts_with(char::is_whitespace) {
                let Some(open) = block.as_mut() else {
                    return Err(fail(Invalid::OutsideAdapter));
                };
                open.take(line, &words).map_err(fail)?;
                continue;
            }
            if let Some(done) = block.take() {
                settings.push(done.finish()?)?;
            }
            match words.as_slice() {
                ["networking", ..] if said.networking => return Err(fail(Invalid::Repeated)),
                ["dns", ..] if said.dns => return Err(fail(Invalid::Repeated)),
                ["networking", state] => {
                    settings.networking = on_off(state).ok_or(fail(Invalid::Line))?;
                    said.networking = true;
                }
                ["dns", servers @ ..] if !servers.is_empty() => {
                    settings.dns = match servers {
                        ["auto"] => Dns::Auto,
                        _ => parse_dns(&servers.join(" ")).map_err(fail)?,
                    };
                    said.dns = true;
                }
                ["adapter", id] => {
                    block = Some(Block::new(line, AdapterId::parse(id).map_err(fail)?));
                }
                _ => return Err(fail(Invalid::Line)),
            }
        }
        if let Some(done) = block.take() {
            settings.push(done.finish()?)?;
        }
        Ok(settings)
    }

    fn push(&mut self, (line, adapter): (usize, Adapter)) -> Result<(), ParseError> {
        if self.adapters.iter().any(|a| a.id == adapter.id) {
            return Err(ParseError {
                line,
                why: Invalid::TwoBlocks,
            });
        }
        self.adapters.push(adapter);
        Ok(())
    }

    pub fn render(&self) -> String {
        let mut out = String::from(HEADER);
        let mut top = String::new();
        if !self.networking {
            top.push_str("networking off\n");
        }
        if let Dns::Manual(servers) = &self.dns {
            let servers: Vec<String> = servers.iter().map(Ipv4::to_string).collect();
            top.push_str(&format!("dns {}\n", servers.join(" ")));
        }
        if !top.is_empty() {
            out.push('\n');
            out.push_str(&top);
        }
        for adapter in &self.adapters {
            out.push_str(&format!("\nadapter {}\n", adapter.id));
            match adapter.ipv4 {
                Ipv4Method::Auto => out.push_str("  ipv4 auto\n"),
                Ipv4Method::Manual { address, gateway } => {
                    out.push_str(&format!("  ipv4 manual {address}\n"));
                    if let Some(gateway) = gateway {
                        out.push_str(&format!("  gateway {gateway}\n"));
                    }
                }
            }
            if !adapter.enabled {
                out.push_str("  off\n");
            }
        }
        out
    }

    /// The block that governs the adapter the kernel calls `name`, whose
    /// hardware address is `mac`: its address's, else its name's.
    pub fn governing(&self, name: &str, mac: Mac) -> Option<&Adapter> {
        let by_mac = self
            .adapters
            .iter()
            .find(|a| matches!(a.id, AdapterId::Mac(m) if m == mac && !mac.is_zero()));
        by_mac.or_else(|| {
            self.adapters
                .iter()
                .find(|a| a.id == AdapterId::Name(name.to_string()))
        })
    }

    /// Settle what governs the adapter called `name` with hardware address
    /// `mac`: one block, keyed by its address when it has one, in place of
    /// whatever named it before.
    pub fn set_for(&mut self, name: &str, mac: Mac, enabled: bool, ipv4: Ipv4Method) {
        self.forget(name, mac);
        let id = match mac.is_zero() {
            true => AdapterId::Name(name.to_string()),
            false => AdapterId::Mac(mac),
        };
        self.adapters.push(Adapter { id, enabled, ipv4 });
    }

    /// Drop every block that names the adapter `name` or `mac`; whether any
    /// did.
    pub fn forget(&mut self, name: &str, mac: Mac) -> bool {
        let before = self.adapters.len();
        self.adapters.retain(|a| match &a.id {
            AdapterId::Name(n) => n != name,
            AdapterId::Mac(m) => mac.is_zero() || *m != mac,
        });
        self.adapters.len() != before
    }

    /// Set the block for `id` itself, in place of one keyed the same.
    pub fn set(&mut self, adapter: Adapter) {
        match self.adapters.iter_mut().find(|a| a.id == adapter.id) {
            Some(held) => *held = adapter,
            None => self.adapters.push(adapter),
        }
    }
}

fn on_off(word: &str) -> Option<bool> {
    match word {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

fn once<T>(slot: &mut Option<T>, value: T) -> Result<(), Invalid> {
    match slot.replace(value) {
        None => Ok(()),
        Some(_) => Err(Invalid::Repeated),
    }
}

#[derive(Default)]
struct Said {
    networking: bool,
    dns: bool,
}

/// An adapter's lines as they are read.
struct Block {
    line: usize,
    id: AdapterId,
    enabled: Option<bool>,
    ipv4: Option<(usize, Ipv4Method)>,
    gateway: Option<(usize, Ipv4)>,
}

impl Block {
    fn new(line: usize, id: AdapterId) -> Block {
        Block {
            line,
            id,
            enabled: None,
            ipv4: None,
            gateway: None,
        }
    }

    fn take(&mut self, line: usize, words: &[&str]) -> Result<(), Invalid> {
        match words {
            [state @ ("on" | "off")] => once(&mut self.enabled, *state == "on"),
            ["ipv4", "auto"] => once(&mut self.ipv4, (line, Ipv4Method::Auto)),
            ["ipv4", "manual", address] => {
                let address = parse_address(address)?;
                let method = Ipv4Method::Manual {
                    address,
                    gateway: None,
                };
                once(&mut self.ipv4, (line, method))
            }
            ["gateway", gateway] => once(&mut self.gateway, (line, parse_gateway(gateway)?)),
            _ => Err(Invalid::Line),
        }
    }

    fn finish(self) -> Result<(usize, Adapter), ParseError> {
        let ipv4 = match (self.ipv4, self.gateway) {
            (Some((_, Ipv4Method::Manual { address, .. })), Some((line, gateway))) => {
                Ipv4Method::manual(address, Some(gateway))
                    .map_err(|why| ParseError { line, why })?
            }
            (_, Some((line, _))) => {
                return Err(ParseError {
                    line,
                    why: Invalid::GatewayWithoutAddress,
                });
            }
            (Some((_, method)), None) => method,
            (None, None) => Ipv4Method::Auto,
        };
        let adapter = Adapter {
            id: self.id,
            enabled: self.enabled.unwrap_or(true),
            ipv4,
        };
        Ok((self.line, adapter))
    }
}
