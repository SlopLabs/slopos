//! The network settings in [`network::PATH`] as `/bin/netctl` and Settings
//! read and keep them, and carrying them out: the network read from the
//! kernel, one phase of [`network::plan`] carried out, and read again, until
//! nothing is left to do.

use std::ffi::CString;

use slopos_abi::net::{
    NET_ADDR_ORIGIN_DHCP, NET_ADDR_ORIGIN_LINKLOCAL, NET_ADDR_SCOPE_GLOBAL, NET_IFINDEX_GLOBAL,
    NET_IFINDEX_NONE, NET_IFKIND_LOOPBACK, NET_IFOP_DHCP_RENEW, NET_IFOP_DHCP_START,
    NET_IFOP_DHCP_STOP, NET_IFOP_SET_ADMIN_UP, NET_IFOP_SET_ENABLED, NET_Q_ADDRS, NET_Q_DHCP,
    NET_Q_GLOBAL, NET_Q_RESOLVER, NET_Q_ROUTES, NET_RESOLVER_SRC_STATIC, UserAddr, UserAddrReq,
    UserDhcpStatus, UserIface, UserNetGlobal, UserResolver, UserResolverReq, UserRoute,
    UserRouteReq,
};
use slopos_settings_core::network::{
    self, Cidr, Ipv4, Live, LiveAdapter, LiveAddress, LiveDns, Mac, Scope, Settings, Step,
};

use crate::net_query::{self, Ifaces};
use crate::syscall::SyscallError;
use crate::syscall::net::{net_addr_ctl, net_iface_ctl, net_resolver_set, net_route_ctl};

/// The settings kept, the default ones while none are.
pub fn read() -> Result<Settings, String> {
    let path = network::PATH;
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::default()),
        Err(e) => return Err(format!("{path}: {e}")),
        Ok(_) => {}
    }
    let bytes = crate::regular_file::read(path, network::FILE_MAX + 1, Some(0))
        .ok_or_else(|| format!("{path}: not a file uid 0 keeps"))?;
    if bytes.len() > network::FILE_MAX {
        return Err(format!("{path}: longer than any settings are"));
    }
    let text = String::from_utf8(bytes).map_err(|_| format!("{path}: not text"))?;
    Settings::parse(&text).map_err(|e| format!("{path}: {e}"))
}

pub fn write(settings: &Settings) -> Result<(), String> {
    let path = CString::new(network::PATH).map_err(|e| e.to_string())?;
    crate::syscall::fs::write_durable(&path, settings.render().as_bytes())
        .map_err(|e| format!("{}: {e}", network::PATH))
}

fn queried<T: Copy + Default>(what: u32) -> Result<Vec<T>, String> {
    net_query::fetch::<T>(what, NET_IFINDEX_NONE)
        .map(|q| q.records)
        .map_err(|e| format!("reading the network: {e}"))
}

/// The network as the kernel reports it now.
pub fn live() -> Result<Live, String> {
    let ifaces = Ifaces::fetch().map_err(|e| format!("reading the network: {e}"))?;
    let addrs: Vec<UserAddr> = queried(NET_Q_ADDRS)?;
    let routes: Vec<UserRoute> = queried(NET_Q_ROUTES)?;
    let dhcp: Vec<UserDhcpStatus> = queried(NET_Q_DHCP)?;
    let global: Vec<UserNetGlobal> = queried(NET_Q_GLOBAL)?;
    let resolver: Vec<UserResolver> = queried(NET_Q_RESOLVER)?;
    let adapters = ifaces
        .rows
        .iter()
        .map(|iface| adapter(iface, &addrs, &routes, &dhcp))
        .collect();
    let dns = resolver.first().map_or(
        LiveDns {
            servers: Vec::new(),
            pinned: false,
        },
        |r| {
            let servers: Vec<Ipv4> = r.servers[..(r.n_servers as usize).min(r.servers.len())]
                .iter()
                .map(|&s| Ipv4(s))
                .collect();
            LiveDns {
                pinned: r.source == NET_RESOLVER_SRC_STATIC && !servers.is_empty(),
                servers,
            }
        },
    );
    Ok(Live {
        networking: global.first().is_none_or(|g| g.enabled != 0),
        adapters,
        dns,
    })
}

fn adapter(
    iface: &UserIface,
    addrs: &[UserAddr],
    routes: &[UserRoute],
    dhcp: &[UserDhcpStatus],
) -> LiveAdapter {
    let ifindex = iface.ifindex;
    let addresses = addrs
        .iter()
        .filter(|a| a.ifindex == ifindex)
        .filter_map(|a| {
            Some(LiveAddress {
                address: Cidr::new(Ipv4(a.addr), a.prefix_len)?,
                from_dhcp: a.origin == NET_ADDR_ORIGIN_DHCP,
                link_local: a.origin == NET_ADDR_ORIGIN_LINKLOCAL,
            })
        })
        .collect();
    let gateway = routes
        .iter()
        .find(|r| r.ifindex == ifindex && r.prefix_len == 0)
        .map(|r| Ipv4(r.gateway));
    LiveAdapter {
        ifindex,
        name: net_query::name_of(iface).to_string(),
        mac: Mac(iface.mac),
        loopback: iface.kind == NET_IFKIND_LOOPBACK,
        up: iface.admin_up != 0,
        carrier: iface.carrier != 0,
        dhcp: dhcp.iter().any(|d| d.ifindex == ifindex),
        addresses,
        gateway,
    }
}

pub fn carry_out(step: &Step) -> Result<(), SyscallError> {
    match step {
        Step::Networking(on) => {
            net_iface_ctl(NET_IFINDEX_GLOBAL, NET_IFOP_SET_ENABLED, u64::from(*on))
        }
        Step::Link { ifindex, up } => {
            net_iface_ctl(*ifindex, NET_IFOP_SET_ADMIN_UP, u64::from(*up))
        }
        Step::DhcpStart { ifindex } => net_iface_ctl(*ifindex, NET_IFOP_DHCP_START, 0),
        Step::DhcpStop { ifindex } => net_iface_ctl(*ifindex, NET_IFOP_DHCP_STOP, 0),
        Step::DhcpRenew { ifindex } => net_iface_ctl(*ifindex, NET_IFOP_DHCP_RENEW, 0),
        Step::AddressAdd { ifindex, address } | Step::AddressRemove { ifindex, address } => {
            let mut req = UserAddrReq::default();
            req.ifindex = *ifindex;
            req.addr = address.addr.octets();
            req.prefix_len = address.prefix_len;
            req.family = slopos_abi::net::AF_INET as u8;
            req.scope = NET_ADDR_SCOPE_GLOBAL;
            net_addr_ctl(&req, matches!(step, Step::AddressAdd { .. }))
        }
        Step::GatewayAdd {
            ifindex,
            gateway,
            metric,
        } => {
            let mut req = UserRouteReq::default();
            req.ifindex = *ifindex;
            req.gateway = gateway.octets();
            req.metric = *metric;
            net_route_ctl(&req, true)
        }
        Step::GatewayRemove { ifindex } => {
            let mut req = UserRouteReq::default();
            req.ifindex = *ifindex;
            net_route_ctl(&req, false)
        }
        Step::Dns(servers) => {
            let mut req = UserResolverReq::default();
            for (slot, server) in req.servers.iter_mut().zip(servers) {
                *slot = server.octets();
            }
            req.n_servers = servers.len().min(req.servers.len()) as u8;
            req.source = NET_RESOLVER_SRC_STATIC;
            net_resolver_set(&req)
        }
    }
}

/// Every phase it takes before nothing is left: the switch, the adapters,
/// their addresses and the DNS servers, with room to read each again.
const MOST_ROUNDS: usize = 8;

/// What failed of bringing the network to `settings` within `scope`, a line
/// each; nothing once it is there.
pub fn apply(settings: &Settings, scope: &Scope) -> Vec<String> {
    let mut failed = Vec::new();
    for _ in 0..MOST_ROUNDS {
        let now = match live() {
            Ok(now) => now,
            Err(why) => return vec![why],
        };
        let steps = network::plan(settings, &now, scope);
        if steps.is_empty() {
            return failed;
        }
        for step in &steps {
            if let Err(e) = carry_out(step) {
                let name = step
                    .ifindex()
                    .and_then(|i| now.adapters.iter().find(|a| a.ifindex == i))
                    .map_or("", |a| a.name.as_str());
                failed.push(format!("cannot {}: {e}", step.describe(name)));
            }
        }
        if !failed.is_empty() {
            return failed;
        }
    }
    vec![String::from(
        "the network did not settle on its settings; `netctl` shows how it stands",
    )]
}
