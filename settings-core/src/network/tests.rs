use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use super::*;

const MAC: Mac = Mac([0x52, 0x54, 0, 0x12, 0x34, 0x56]);
const OTHER_MAC: Mac = Mac([0x52, 0x54, 0, 0x12, 0x34, 0x57]);

fn cidr(text: &str) -> Cidr {
    Cidr::from_str_bytes(text.as_bytes()).unwrap()
}

fn ip(text: &str) -> Ipv4 {
    Ipv4::from_str_bytes(text.as_bytes()).unwrap()
}

fn manual(address: &str, gateway: Option<&str>) -> Ipv4Method {
    Ipv4Method::manual(cidr(address), gateway.map(ip)).unwrap()
}

const SAMPLE: &str = "\
networking off
dns 1.1.1.1 9.9.9.9

adapter 52:54:00:12:34:56
  ipv4 manual 192.168.1.20/24
  gateway 192.168.1.1

adapter eth1  # a dongle
  ipv4 auto
  off
";

#[test]
fn a_file_reads_as_it_says() {
    let settings = Settings::parse(SAMPLE).unwrap();
    assert!(!settings.networking);
    assert_eq!(
        settings.dns,
        Dns::Manual(vec![ip("1.1.1.1"), ip("9.9.9.9")])
    );
    assert_eq!(
        settings.adapters,
        vec![
            Adapter {
                id: AdapterId::Mac(MAC),
                enabled: true,
                ipv4: manual("192.168.1.20/24", Some("192.168.1.1")),
            },
            Adapter {
                id: AdapterId::Name("eth1".into()),
                enabled: false,
                ipv4: Ipv4Method::Auto,
            },
        ]
    );
}

#[test]
fn what_is_rendered_reads_back_the_same() {
    let settings = Settings::parse(SAMPLE).unwrap();
    let rendered = settings.render();
    assert_eq!(Settings::parse(&rendered).unwrap(), settings);
    assert_eq!(Settings::parse(&rendered).unwrap().render(), rendered);
    assert!(rendered.starts_with("# The network settings netctl keeps"));
}

#[test]
fn nothing_said_is_the_default() {
    assert_eq!(Settings::parse("").unwrap(), Settings::default());
    assert_eq!(
        Settings::parse(&Settings::default().render()).unwrap(),
        Settings::default()
    );
    let only_header = Settings::default().render();
    assert!(!only_header.contains("networking") || only_header.starts_with('#'));
    assert!(
        !only_header
            .lines()
            .any(|l| !l.starts_with('#') && !l.is_empty())
    );
}

#[test]
fn every_bad_line_is_named_with_why() {
    let cases: [(&str, usize, Invalid); 17] = [
        ("networking maybe\n", 1, Invalid::Line),
        ("networking on\nnetworking off\n", 2, Invalid::Repeated),
        ("dns 1.1.1.1\ndns auto\n", 2, Invalid::Repeated),
        (
            "dns 1.1.1.1 2.2.2.2 3.3.3.3 4.4.4.4\n",
            1,
            Invalid::TooManyDns,
        ),
        ("dns 127.0.0.1\n", 1, Invalid::DnsNotUnicast),
        ("  ipv4 auto\n", 1, Invalid::OutsideAdapter),
        ("adapter lo\n", 1, Invalid::Loopback),
        ("adapter Eth0\n", 1, Invalid::Adapter),
        (
            "adapter eth0\n  ipv4 manual 10.0.0.5\n",
            2,
            Invalid::NoPrefix,
        ),
        (
            "adapter eth0\n  ipv4 manual 10.0.0.0/24\n",
            2,
            Invalid::NetworkAddress,
        ),
        (
            "adapter eth0\n  ipv4 manual 10.0.0.5/24\n  gateway 10.0.1.1\n",
            3,
            Invalid::GatewayOutside,
        ),
        (
            "adapter eth0\n  gateway 10.0.0.1\n",
            2,
            Invalid::GatewayWithoutAddress,
        ),
        ("adapter eth0\nadapter eth0\n", 2, Invalid::TwoBlocks),
        ("adapter eth0\n  ipv4 sideways\n", 2, Invalid::Line),
        ("adapter eth0\n  gateway\n", 2, Invalid::Line),
        ("adapter eth0\n  on extra\n", 2, Invalid::Line),
        ("adapter eth0\n  off\n  on\n", 3, Invalid::Repeated),
    ];
    for (text, line, why) in cases {
        assert_eq!(
            Settings::parse(text),
            Err(ParseError { line, why }),
            "{text:?}"
        );
    }
}

#[test]
fn an_address_is_typed_with_its_network() {
    assert_eq!(
        parse_address("192.168.1.20/24"),
        Ok(cidr("192.168.1.20/24"))
    );
    assert_eq!(
        parse_address(" 192.168.1.20/255.255.255.0 "),
        Ok(cidr("192.168.1.20/24"))
    );
    assert_eq!(parse_address("192.168.1.20"), Err(Invalid::NoPrefix));
    assert_eq!(parse_address("192.168.1.20/33"), Err(Invalid::Prefix));
    assert_eq!(parse_address("192.168.1.20/0"), Err(Invalid::Prefix));
    assert_eq!(
        parse_address("192.168.1.20/255.0.255.0"),
        Err(Invalid::Prefix)
    );
    assert_eq!(
        parse_address("192.168.1.255/24"),
        Err(Invalid::BroadcastAddress)
    );
    assert_eq!(parse_address("224.0.0.1/4"), Err(Invalid::NotUnicast));
    assert_eq!(parse_address("home"), Err(Invalid::Address));
    assert_eq!(parse_address("10.0.0.0/31"), Ok(cidr("10.0.0.0/31")));
}

#[test]
fn a_gateway_lies_on_the_network() {
    let address = cidr("10.0.2.15/24");
    assert_eq!(check_gateway(address, ip("10.0.2.2")), Ok(()));
    assert_eq!(
        check_gateway(address, ip("10.0.3.2")),
        Err(Invalid::GatewayOutside)
    );
    assert_eq!(
        check_gateway(address, ip("10.0.2.15")),
        Err(Invalid::GatewayIsAddress)
    );
    assert_eq!(
        check_gateway(address, ip("0.0.0.0")),
        Err(Invalid::NotUnicast)
    );
    for not_host in ["10.0.2.0", "10.0.2.255"] {
        assert_eq!(
            check_gateway(address, ip(not_host)),
            Err(Invalid::GatewayNotHost)
        );
    }
    assert_eq!(check_gateway(cidr("10.0.2.15/31"), ip("10.0.2.14")), Ok(()));
}

#[test]
fn dns_servers_are_typed_apart_by_spaces_or_commas() {
    assert_eq!(parse_dns(""), Ok(Dns::Auto));
    assert_eq!(
        parse_dns("1.1.1.1, 9.9.9.9"),
        Ok(Dns::Manual(vec![ip("1.1.1.1"), ip("9.9.9.9")]))
    );
    assert_eq!(parse_dns("1.1.1.1 dns.example"), Err(Invalid::DnsServer));
}

#[test]
fn an_address_block_wins_over_a_name_block() {
    let mut settings = Settings::default();
    settings.set(Adapter {
        id: AdapterId::Name("eth0".into()),
        enabled: true,
        ipv4: Ipv4Method::Auto,
    });
    settings.set(Adapter {
        id: AdapterId::Mac(MAC),
        enabled: false,
        ipv4: Ipv4Method::Auto,
    });
    assert!(!settings.governing("eth0", MAC).unwrap().enabled);
    assert!(settings.governing("eth0", OTHER_MAC).unwrap().enabled);
    assert!(settings.governing("eth1", OTHER_MAC).is_none());
}

#[test]
fn setting_an_adapter_leaves_one_block_keyed_by_its_address() {
    let mut settings = Settings::parse("adapter eth0\n  off\n").unwrap();
    settings.set_for("eth0", MAC, true, manual("10.0.0.5/24", None));
    assert_eq!(settings.adapters.len(), 1);
    assert_eq!(settings.adapters[0].id, AdapterId::Mac(MAC));
    assert!(settings.forget("eth0", MAC));
    assert!(settings.adapters.is_empty());
    assert!(!settings.forget("eth0", MAC));
    settings.set_for("tap0", Mac::ZERO, true, Ipv4Method::Auto);
    assert_eq!(settings.adapters[0].id, AdapterId::Name("tap0".into()));
}

fn adapter(ifindex: u32, name: &str, mac: Mac) -> LiveAdapter {
    LiveAdapter {
        ifindex,
        name: name.into(),
        mac,
        loopback: false,
        up: true,
        carrier: true,
        dhcp: true,
        leasing: false,
        addresses: vec![LiveAddress {
            address: cidr("10.0.2.15/24"),
            from_dhcp: true,
            link_local: false,
        }],
        gateway: Some(ip("10.0.2.2")),
    }
}

fn live(adapters: Vec<LiveAdapter>) -> Live {
    Live {
        networking: true,
        adapters,
        dns: LiveDns {
            servers: vec![ip("10.0.2.3")],
            pinned: false,
        },
    }
}

/// Plan, carry it out on the model as the kernel would, and plan again,
/// until nothing is left; every phase on the way.
fn settle(settings: &Settings, mut now: Live, scope: &Scope) -> (Live, Vec<Vec<Step>>) {
    let mut phases = Vec::new();
    for _ in 0..8 {
        let steps = plan(settings, &now, scope);
        if steps.is_empty() {
            return (now, phases);
        }
        for step in &steps {
            kernel(&mut now, step);
        }
        phases.push(steps);
    }
    panic!("never settled: {phases:?}");
}

/// What the kernel's calls do to the model, as `slopos-net` does them.
fn kernel(now: &mut Live, step: &Step) {
    let at = step
        .ifindex()
        .and_then(|i| now.adapters.iter().position(|a| a.ifindex == i));
    match (step, at) {
        (Step::Networking(on), _) => now.networking = *on,
        (Step::Link { up, .. }, Some(at)) => now.adapters[at].up = *up,
        (Step::DhcpStart { .. }, Some(at)) => {
            let a = &mut now.adapters[at];
            a.dhcp = true;
            a.addresses.push(LiveAddress {
                address: cidr("10.0.2.15/24"),
                from_dhcp: true,
                link_local: false,
            });
            a.gateway = Some(ip("10.0.2.2"));
        }
        (Step::DhcpStop { .. }, Some(at)) => {
            let a = &mut now.adapters[at];
            assert!(a.dhcp, "stop with no client");
            a.dhcp = false;
            if a.addresses.iter().any(|x| x.from_dhcp) {
                a.addresses.retain(|x| !x.from_dhcp);
                a.gateway = None;
            }
        }
        (Step::DhcpRenew { .. }, Some(at)) => assert!(now.adapters[at].dhcp),
        (Step::AddressAdd { address, .. }, Some(at)) => {
            let a = &mut now.adapters[at];
            assert!(!a.addresses.iter().any(|x| x.address == *address));
            a.addresses.push(LiveAddress {
                address: *address,
                from_dhcp: false,
                link_local: false,
            });
        }
        (Step::AddressRemove { address, .. }, Some(at)) => {
            let a = &mut now.adapters[at];
            let before = a.addresses.len();
            a.addresses.retain(|x| x.address != *address);
            assert_ne!(before, a.addresses.len(), "removed what was not there");
        }
        (Step::GatewayAdd { gateway, .. }, Some(at)) => {
            assert!(now.adapters[at].gateway.is_none(), "a second default route");
            now.adapters[at].gateway = Some(*gateway);
        }
        (Step::GatewayRemove { .. }, Some(at)) => {
            assert!(now.adapters[at].gateway.take().is_some());
        }
        (Step::Dns(servers), _) => {
            now.dns = LiveDns {
                servers: servers.clone(),
                pinned: !servers.is_empty(),
            };
        }
        (step, None) => panic!("{step:?} names no adapter"),
    }
}

#[test]
fn an_adapter_no_block_names_is_left_as_it_is() {
    let now = live(vec![adapter(2, "eth0", MAC)]);
    assert!(plan(&Settings::default(), &now, &Scope::Everything).is_empty());
}

#[test]
fn a_manual_address_ends_the_lease_then_takes_its_place() {
    let mut settings = Settings::default();
    settings.set_for(
        "eth0",
        MAC,
        true,
        manual("192.168.1.20/24", Some("192.168.1.1")),
    );
    let (now, phases) = settle(
        &settings,
        live(vec![adapter(2, "eth0", MAC)]),
        &Scope::Everything,
    );
    assert_eq!(
        phases,
        vec![
            vec![Step::DhcpStop { ifindex: 2 }],
            vec![
                Step::AddressAdd {
                    ifindex: 2,
                    address: cidr("192.168.1.20/24")
                },
                Step::GatewayAdd {
                    ifindex: 2,
                    gateway: ip("192.168.1.1"),
                    metric: GATEWAY_METRIC + 2
                },
            ],
        ]
    );
    let eth0 = &now.adapters[0];
    assert!(!eth0.dhcp);
    assert_eq!(eth0.gateway, Some(ip("192.168.1.1")));
}

#[test]
fn a_client_is_ended_before_an_address_set_by_hand_is_put_back() {
    let mut settings = Settings::default();
    settings.set_for(
        "eth0",
        MAC,
        true,
        manual("192.168.1.20/24", Some("192.168.1.1")),
    );
    let mut eth0 = adapter(2, "eth0", MAC);
    eth0.addresses = vec![LiveAddress {
        address: cidr("192.168.1.20/24"),
        from_dhcp: false,
        link_local: false,
    }];
    eth0.gateway = Some(ip("192.168.1.1"));
    let address = cidr("192.168.1.20/24");
    let (_, phases) = settle(&settings, live(vec![eth0]), &Scope::Everything);
    assert_eq!(
        phases,
        vec![
            vec![
                Step::DhcpStop { ifindex: 2 },
                Step::AddressRemove {
                    ifindex: 2,
                    address
                }
            ],
            vec![Step::AddressAdd {
                ifindex: 2,
                address
            }],
        ],
        "its route may have gone with the client, which binding as it is asked to end takes"
    );
}

#[test]
fn a_manual_address_already_in_place_asks_for_nothing() {
    let mut settings = Settings::default();
    settings.set_for(
        "eth0",
        MAC,
        true,
        manual("192.168.1.20/24", Some("192.168.1.1")),
    );
    let (now, _) = settle(
        &settings,
        live(vec![adapter(2, "eth0", MAC)]),
        &Scope::Everything,
    );
    assert!(plan(&settings, &now, &Scope::Everything).is_empty());
    assert!(plan(&settings, &now, &Scope::Adapter("eth0".into())).is_empty());
}

#[test]
fn a_new_manual_address_and_gateway_replace_the_old() {
    let mut settings = Settings::default();
    settings.set_for(
        "eth0",
        MAC,
        true,
        manual("192.168.1.20/24", Some("192.168.1.1")),
    );
    let (now, _) = settle(
        &settings,
        live(vec![adapter(2, "eth0", MAC)]),
        &Scope::Everything,
    );
    settings.set_for(
        "eth0",
        MAC,
        true,
        manual("192.168.7.9/16", Some("192.168.0.1")),
    );
    let (now, phases) = settle(&settings, now, &Scope::Everything);
    assert_eq!(phases.len(), 1);
    assert_eq!(
        now.adapters[0]
            .addresses
            .iter()
            .map(|a| a.address)
            .collect::<Vec<_>>(),
        vec![cidr("192.168.7.9/16")]
    );
    assert_eq!(now.adapters[0].gateway, Some(ip("192.168.0.1")));
}

#[test]
fn automatic_again_takes_the_hand_set_address_away_and_starts_dhcp() {
    let mut settings = Settings::default();
    settings.set_for(
        "eth0",
        MAC,
        true,
        manual("192.168.1.20/24", Some("192.168.1.1")),
    );
    let (now, _) = settle(
        &settings,
        live(vec![adapter(2, "eth0", MAC)]),
        &Scope::Everything,
    );
    settings.set_for("eth0", MAC, true, Ipv4Method::Auto);
    let (now, phases) = settle(&settings, now, &Scope::Everything);
    assert_eq!(
        phases,
        vec![vec![
            Step::AddressRemove {
                ifindex: 2,
                address: cidr("192.168.1.20/24")
            },
            Step::GatewayRemove { ifindex: 2 },
            Step::DhcpStart { ifindex: 2 },
        ]]
    );
    assert!(now.adapters[0].dhcp);
}

#[test]
fn an_adapter_turned_off_goes_down_first_and_comes_back_up() {
    let mut settings = Settings::default();
    settings.set_for("eth0", MAC, false, Ipv4Method::Auto);
    let (now, phases) = settle(
        &settings,
        live(vec![adapter(2, "eth0", MAC)]),
        &Scope::Everything,
    );
    assert_eq!(
        phases,
        vec![vec![Step::Link {
            ifindex: 2,
            up: false
        }]]
    );
    settings.set_for("eth0", MAC, true, manual("10.1.0.2/24", None));
    let (_, phases) = settle(&settings, now, &Scope::Everything);
    assert_eq!(
        phases[0],
        vec![Step::Link {
            ifindex: 2,
            up: true
        }]
    );
    assert_eq!(phases.len(), 3, "the link, ending its lease, its address");
}

#[test]
fn networking_off_is_all_there_is_to_do() {
    let settings = Settings::parse("networking off\ndns 1.1.1.1\nadapter eth0\n  off\n").unwrap();
    let (now, phases) = settle(
        &settings,
        live(vec![adapter(2, "eth0", MAC)]),
        &Scope::Everything,
    );
    assert_eq!(phases, vec![vec![Step::Networking(false)]]);
    assert!(!now.networking);
}

#[test]
fn an_adapter_appearing_is_set_and_nothing_else() {
    let settings =
        Settings::parse("networking off\ndns 1.1.1.1\nadapter eth1\n  ipv4 manual 10.9.0.2/24\n")
            .unwrap();
    let now = live(vec![adapter(2, "eth0", MAC), adapter(3, "eth1", OTHER_MAC)]);
    let scope = Scope::Adapter("eth1".into());
    let (now, phases) = settle(&settings, now, &scope);
    assert!(now.networking, "the switch is no adapter's");
    assert_eq!(now.dns.servers, vec![ip("10.0.2.3")]);
    assert!(phases.iter().flatten().all(|s| s.ifindex() == Some(3)));
}

#[test]
fn dns_set_by_hand_then_handed_back_to_the_leases() {
    let mut settings = Settings::parse("dns 1.1.1.1\n").unwrap();
    let (now, phases) = settle(
        &settings,
        live(vec![adapter(2, "eth0", MAC)]),
        &Scope::Everything,
    );
    assert_eq!(phases, vec![vec![Step::Dns(vec![ip("1.1.1.1")])]]);
    settings.dns = Dns::Auto;
    let (_, phases) = settle(&settings, now, &Scope::Everything);
    assert_eq!(
        phases,
        vec![vec![Step::Dns(Vec::new()), Step::DhcpRenew { ifindex: 2 }]]
    );
}

#[test]
fn the_loopback_is_never_planned_for() {
    let mut lo = adapter(1, "lo", Mac::ZERO);
    lo.loopback = true;
    lo.dhcp = false;
    let mut settings = Settings::default();
    settings.adapters.push(Adapter {
        id: AdapterId::Name("lo".into()),
        enabled: false,
        ipv4: Ipv4Method::Auto,
    });
    assert!(plan(&settings, &live(vec![lo]), &Scope::Everything).is_empty());
}

#[test]
fn every_step_says_what_it_does() {
    let steps = [
        (Step::Networking(false), "turn networking off"),
        (
            Step::Link {
                ifindex: 2,
                up: true,
            },
            "turn eth0 on",
        ),
        (Step::DhcpStop { ifindex: 2 }, "stop DHCP on eth0"),
        (
            Step::AddressAdd {
                ifindex: 2,
                address: cidr("10.0.0.2/24"),
            },
            "add 10.0.0.2/24 to eth0",
        ),
        (
            Step::GatewayAdd {
                ifindex: 2,
                gateway: ip("10.0.0.1"),
                metric: 102,
            },
            "route through 10.0.0.1 on eth0",
        ),
        (Step::Dns(Vec::new()), "take DNS servers from leases"),
        (
            Step::Dns(vec![ip("1.1.1.1"), ip("9.9.9.9")]),
            "use DNS servers 1.1.1.1 9.9.9.9",
        ),
    ];
    for (step, said) in steps {
        assert_eq!(step.describe("eth0"), said);
    }
}

#[test]
fn every_refusal_is_a_sentence_in_the_desktop_s_words() {
    let all = [
        Invalid::Adapter,
        Invalid::Loopback,
        Invalid::Address,
        Invalid::NoPrefix,
        Invalid::Prefix,
        Invalid::NotUnicast,
        Invalid::NetworkAddress,
        Invalid::BroadcastAddress,
        Invalid::Gateway,
        Invalid::GatewayOutside,
        Invalid::GatewayIsAddress,
        Invalid::GatewayNotHost,
        Invalid::DnsServer,
        Invalid::DnsNotUnicast,
        Invalid::TooManyDns,
        Invalid::GatewayWithoutAddress,
        Invalid::Line,
        Invalid::OutsideAdapter,
        Invalid::Repeated,
        Invalid::TwoBlocks,
    ];
    let mut seen: Vec<String> = Vec::new();
    for why in all {
        let said = why.to_string();
        slopos_theme_core::words::sentence(&said, &[]).unwrap_or_else(|e| panic!("{said}: {e}"));
        assert!(!seen.contains(&said), "{said} twice");
        seen.push(said);
    }
}
