//! What it takes to bring the network to its settings, from what the kernel
//! says it is now. A plan is one phase — the networking switch, then which
//! adapters are up, then their addresses, then the DNS servers — since each
//! changes what the next finds; whoever carries one out reads the network
//! again and plans the next, until nothing is left.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use super::{Dns, GATEWAY_METRIC, Ipv4Method, Settings};
use slopos_net_core::{Cidr, Ipv4, Mac};

/// The network as the kernel reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Live {
    pub networking: bool,
    pub adapters: Vec<LiveAdapter>,
    pub dns: LiveDns,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveAdapter {
    pub ifindex: u32,
    pub name: String,
    pub mac: Mac,
    pub loopback: bool,
    /// Brought up, whether or not a cable is in.
    pub up: bool,
    pub carrier: bool,
    /// A DHCP client runs on it.
    pub dhcp: bool,
    /// That client has yet to be given a lease.
    pub leasing: bool,
    pub addresses: Vec<LiveAddress>,
    /// Through its default route, which an adapter has one of at most.
    pub gateway: Option<Ipv4>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiveAddress {
    pub address: Cidr,
    pub from_dhcp: bool,
    /// Link-local, which nothing here sets or takes away.
    pub link_local: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveDns {
    pub servers: Vec<Ipv4>,
    /// Set by hand, so no lease changes them.
    pub pinned: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    /// The switch, every adapter a block names and the DNS servers.
    Everything,
    /// The adapter the kernel calls this, as it appears.
    Adapter(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Networking(bool),
    Link {
        ifindex: u32,
        up: bool,
    },
    DhcpStart {
        ifindex: u32,
    },
    DhcpStop {
        ifindex: u32,
    },
    DhcpRenew {
        ifindex: u32,
    },
    AddressAdd {
        ifindex: u32,
        address: Cidr,
    },
    AddressRemove {
        ifindex: u32,
        address: Cidr,
    },
    GatewayAdd {
        ifindex: u32,
        gateway: Ipv4,
        metric: u32,
    },
    GatewayRemove {
        ifindex: u32,
    },
    /// None hands the DNS servers back to the leases.
    Dns(Vec<Ipv4>),
}

impl Step {
    pub fn ifindex(&self) -> Option<u32> {
        match *self {
            Step::Link { ifindex, .. }
            | Step::DhcpStart { ifindex }
            | Step::DhcpStop { ifindex }
            | Step::DhcpRenew { ifindex }
            | Step::AddressAdd { ifindex, .. }
            | Step::AddressRemove { ifindex, .. }
            | Step::GatewayAdd { ifindex, .. }
            | Step::GatewayRemove { ifindex } => Some(ifindex),
            Step::Networking(_) | Step::Dns(_) => None,
        }
    }

    /// What it does, in the terms `ip` prints, for the adapter called
    /// `name`.
    pub fn describe(&self, name: &str) -> String {
        alloc::format!("{}", Described(self, name))
    }
}

struct Described<'a>(&'a Step, &'a str);

impl fmt::Display for Described<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = self.1;
        match self.0 {
            Step::Networking(on) => write!(f, "turn networking {}", on_off(*on)),
            Step::Link { up, .. } => write!(f, "turn {name} {}", on_off(*up)),
            Step::DhcpStart { .. } => write!(f, "start DHCP on {name}"),
            Step::DhcpStop { .. } => write!(f, "stop DHCP on {name}"),
            Step::DhcpRenew { .. } => write!(f, "renew {name}'s lease"),
            Step::AddressAdd { address, .. } => write!(f, "add {address} to {name}"),
            Step::AddressRemove { address, .. } => write!(f, "remove {address} from {name}"),
            Step::GatewayAdd { gateway, .. } => write!(f, "route through {gateway} on {name}"),
            Step::GatewayRemove { .. } => write!(f, "remove {name}'s gateway"),
            Step::Dns(servers) if servers.is_empty() => f.write_str("take DNS servers from leases"),
            Step::Dns(servers) => {
                f.write_str("use DNS servers")?;
                for server in servers {
                    write!(f, " {server}")?;
                }
                Ok(())
            }
        }
    }
}

fn on_off(on: bool) -> &'static str {
    match on {
        true => "on",
        false => "off",
    }
}

/// The next phase of bringing `live` to `settings` within `scope`; none once
/// it is there.
pub fn plan(settings: &Settings, live: &Live, scope: &Scope) -> Vec<Step> {
    let everything = *scope == Scope::Everything;
    if everything && settings.networking != live.networking {
        return vec![Step::Networking(settings.networking)];
    }
    if !live.networking {
        return Vec::new();
    }
    let governed: Vec<(&LiveAdapter, &super::Adapter)> = live
        .adapters
        .iter()
        .filter(|a| !a.loopback)
        .filter(|a| match scope {
            Scope::Everything => true,
            Scope::Adapter(name) => a.name == *name,
        })
        .filter_map(|a| Some((a, settings.governing(&a.name, a.mac)?)))
        .collect();

    let links: Vec<Step> = governed
        .iter()
        .filter(|(live, wanted)| live.up != wanted.enabled)
        .map(|(live, wanted)| Step::Link {
            ifindex: live.ifindex,
            up: wanted.enabled,
        })
        .collect();
    if !links.is_empty() {
        return links;
    }

    let addressing: Vec<Step> = governed
        .iter()
        .filter(|(_, wanted)| wanted.enabled)
        .flat_map(|(live, wanted)| address(live, wanted.ipv4))
        .collect();
    if !addressing.is_empty() || !everything {
        return addressing;
    }
    dns(&settings.dns, live)
}

fn address(live: &LiveAdapter, wanted: Ipv4Method) -> Vec<Step> {
    let ifindex = live.ifindex;
    let set_by_hand = live
        .addresses
        .iter()
        .filter(|a| !a.from_dhcp && !a.link_local)
        .map(|a| a.address);
    match wanted {
        Ipv4Method::Auto => {
            let mut steps: Vec<Step> = set_by_hand
                .map(|address| Step::AddressRemove { ifindex, address })
                .collect();
            if !live.dhcp {
                if live.gateway.is_some() {
                    steps.push(Step::GatewayRemove { ifindex });
                }
                steps.push(Step::DhcpStart { ifindex });
            }
            steps
        }
        Ipv4Method::Manual { .. } if live.dhcp => {
            // Ending a lease may take every route of the adapter with it, so
            // the rest waits for the network to be read again.
            let mut steps = vec![Step::DhcpStop { ifindex }];
            steps.extend(set_by_hand.map(|address| Step::AddressRemove { ifindex, address }));
            steps
        }
        Ipv4Method::Manual { address, gateway } => {
            let mut steps = Vec::new();
            let live_gateway = live.gateway;
            let held: Vec<Cidr> = live
                .addresses
                .iter()
                .filter(|a| !a.link_local)
                .map(|a| a.address)
                .collect();
            steps.extend(
                held.iter()
                    .filter(|&&held| held != address)
                    .map(|&address| Step::AddressRemove { ifindex, address }),
            );
            if !held.contains(&address) {
                steps.push(Step::AddressAdd { ifindex, address });
            }
            if live_gateway.is_some() && live_gateway != gateway {
                steps.push(Step::GatewayRemove { ifindex });
            }
            if let Some(gateway) = gateway.filter(|&g| live_gateway != Some(g)) {
                steps.push(Step::GatewayAdd {
                    ifindex,
                    gateway,
                    metric: GATEWAY_METRIC + ifindex,
                });
            }
            steps
        }
    }
}

fn dns(wanted: &Dns, live: &Live) -> Vec<Step> {
    match wanted {
        Dns::Auto if live.dns.pinned => {
            let mut steps = vec![Step::Dns(Vec::new())];
            steps.extend(
                live.adapters
                    .iter()
                    .filter(|a| a.dhcp && a.up)
                    .map(|a| Step::DhcpRenew { ifindex: a.ifindex }),
            );
            steps
        }
        Dns::Auto => Vec::new(),
        Dns::Manual(servers) if !live.dns.pinned || live.dns.servers != *servers => {
            vec![Step::Dns(servers.clone())]
        }
        Dns::Manual(_) => Vec::new(),
    }
}
