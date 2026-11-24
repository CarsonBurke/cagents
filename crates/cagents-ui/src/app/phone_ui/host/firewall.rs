//! Whether ufw lets SSH in on an interface, read from its saved rules.
//!
//! ufw keeps each rule a person added as a `### tuple ###` comment in
//! `/etc/ufw/user.rules`, which, like its other configuration, every user
//! can read. A computer set up so that only Tailscale reaches SSH (allow on
//! `tailscale0`, drop everything else) would otherwise offer a phone LAN
//! addresses that silently time out.
//!
//! This reads the configuration, not the loaded rules, and knows only ufw:
//! with no readable ufw configuration, or ufw turned off, it reports nothing
//! as blocked. Other firewalls (firewalld, hand-written nftables) are not
//! read, so a phone may still fail to reach an address this doesn't flag.

use std::{fs, net::Ipv4Addr, path::Path};

/// ufw's input rules for IPv4, in the order they apply.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Ufw {
    rules: Vec<Rule>,
    /// What happens to a connection no rule matches.
    default_allows: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct Rule {
    allow: bool,
    protocol: String,
    ports: String,
    destination: String,
    /// `None` for every interface.
    interface: Option<String>,
}

impl Ufw {
    /// The rules in effect, or `None` when ufw is off or unreadable.
    pub(crate) fn read() -> Option<Self> {
        Self::read_from(Path::new("/etc"))
    }

    fn read_from(etc: &Path) -> Option<Self> {
        let enabled = fs::read_to_string(etc.join("ufw/ufw.conf")).ok()?;
        if setting(&enabled, "ENABLED")? != "yes" {
            return None;
        }
        let rules = fs::read_to_string(etc.join("ufw/user.rules")).ok()?;
        let defaults = fs::read_to_string(etc.join("default/ufw")).unwrap_or_default();
        Some(Self::parse(
            &rules,
            setting(&defaults, "DEFAULT_INPUT_POLICY").unwrap_or("DROP"),
        ))
    }

    fn parse(rules: &str, default_policy: &str) -> Self {
        Self {
            rules: rules.lines().filter_map(Rule::parse).collect(),
            default_allows: default_policy == "ACCEPT",
        }
    }

    /// Whether a TCP connection to `address`:`port` arriving on `interface`
    /// gets in.
    pub(crate) fn allows(&self, interface: &str, address: Ipv4Addr, port: u16) -> bool {
        self.rules
            .iter()
            .find(|rule| rule.matches(interface, address, port))
            .map_or(self.default_allows, |rule| rule.allow)
    }
}

impl Rule {
    /// A line such as
    /// `### tuple ### allow tcp 22 0.0.0.0/0 any 0.0.0.0/0 in_tailscale0`,
    /// or with application names
    /// `### tuple ### allow tcp 22 0.0.0.0/0 any 0.0.0.0/0 OpenSSH - in`.
    fn parse(line: &str) -> Option<Self> {
        let words: Vec<&str> = line
            .strip_prefix("### tuple ###")?
            .split_whitespace()
            .collect();
        let (action, protocol, ports, destination) =
            (words.first()?, words.get(1)?, words.get(2)?, words.get(3)?);
        // A route rule filters forwarded traffic, not traffic to this computer.
        if action.starts_with("route:") {
            return None;
        }
        let direction = words
            .iter()
            .skip(6)
            .find(|word| **word == "in" || **word == "out" || word.starts_with("in_"))?;
        let interface = match *direction {
            "in" => None,
            "out" => return None,
            direction => Some(direction.strip_prefix("in_")?.to_owned()),
        };
        let allow = match action.split('_').next()? {
            "allow" | "limit" => true,
            "deny" | "reject" => false,
            _ => return None,
        };
        Some(Self {
            allow,
            protocol: protocol.to_string(),
            ports: ports.to_string(),
            destination: destination.to_string(),
            interface,
        })
    }

    fn matches(&self, interface: &str, address: Ipv4Addr, port: u16) -> bool {
        self.interface.as_deref().is_none_or(|own| own == interface)
            && matches!(self.protocol.as_str(), "any" | "tcp")
            && in_ports(&self.ports, port)
            && in_network(&self.destination, address)
    }
}

/// `ports` is `any`, or numbers and `FIRST:LAST` ranges separated by commas.
fn in_ports(ports: &str, port: u16) -> bool {
    ports == "any"
        || ports.split(',').any(|part| match part.split_once(':') {
            Some((first, last)) => first
                .parse::<u16>()
                .ok()
                .zip(last.parse::<u16>().ok())
                .is_some_and(|(first, last)| (first..=last).contains(&port)),
            None => part.parse() == Ok(port),
        })
}

/// `network` is an address or `ADDRESS/BITS`.
fn in_network(network: &str, address: Ipv4Addr) -> bool {
    let (base, bits) = network.split_once('/').unwrap_or((network, "32"));
    let (Ok(base), Ok(bits)) = (base.parse::<Ipv4Addr>(), bits.parse::<u32>()) else {
        return false;
    };
    let mask = u32::MAX.checked_shl(32 - bits.min(32)).unwrap_or(0);
    u32::from(base) & mask == u32::from(address) & mask
}

/// The value of `NAME=value` or `NAME="value"` in a shell-style file.
fn setting<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let value = line.trim().strip_prefix(name)?.strip_prefix('=')?;
        Some(value.trim().trim_matches('"'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAN: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 20);

    #[test]
    fn ssh_allowed_only_on_tailscale_is_blocked_on_the_lan() {
        let rules = "\
*filter
### tuple ### allow tcp 8787 0.0.0.0/0 any 0.0.0.0/0 in
-A ufw-user-input -p tcp --dport 8787 -j ACCEPT
### tuple ### allow any 53 0.0.0.0/0 any 0.0.0.0/0 in
### tuple ### allow any any 0.0.0.0/0 any 0.0.0.0/0 in_tailscale0
### tuple ### route:allow any any 0.0.0.0/0 any 0.0.0.0/0 in_wlan0!out_tailscale0
";
        let ufw = Ufw::parse(rules, "DROP");
        assert!(ufw.allows("tailscale0", Ipv4Addr::new(100, 1, 2, 3), 22));
        assert!(!ufw.allows("wlan0", LAN, 22));
        assert!(ufw.allows("wlan0", LAN, 8787));
    }

    #[test]
    fn the_first_matching_rule_decides() {
        let rules = "\
### tuple ### deny tcp 22 0.0.0.0/0 any 0.0.0.0/0 in_eth0
### tuple ### allow tcp 22 0.0.0.0/0 any 0.0.0.0/0 OpenSSH - in
";
        let ufw = Ufw::parse(rules, "DROP");
        assert!(!ufw.allows("eth0", LAN, 22));
        assert!(ufw.allows("wlan0", LAN, 22));
    }

    #[test]
    fn ports_ranges_destinations_and_the_default_policy() {
        let rules = "\
### tuple ### limit tcp 2000:2300,80 192.168.1.0/24 any 10.0.0.0/8 in comment=6c616e
### tuple ### allow udp 22 0.0.0.0/0 any 0.0.0.0/0 in
";
        let ufw = Ufw::parse(rules, "DROP");
        assert!(ufw.allows("eth0", LAN, 2222));
        assert!(!ufw.allows("eth0", Ipv4Addr::new(10, 0, 0, 2), 2222));
        assert!(
            !ufw.allows("eth0", LAN, 22),
            "a UDP rule leaves TCP dropped"
        );
        assert!(Ufw::parse("", "ACCEPT").allows("eth0", LAN, 22));
    }

    #[test]
    fn ufw_turned_off_blocks_nothing() {
        let etc = tempfile::tempdir().unwrap();
        fs::create_dir_all(etc.path().join("ufw")).unwrap();
        fs::create_dir_all(etc.path().join("default")).unwrap();
        fs::write(etc.path().join("ufw/user.rules"), "").unwrap();
        fs::write(
            etc.path().join("default/ufw"),
            "DEFAULT_INPUT_POLICY=\"DROP\"\n",
        )
        .unwrap();
        fs::write(etc.path().join("ufw/ufw.conf"), "# comment\nENABLED=no\n").unwrap();
        assert_eq!(Ufw::read_from(etc.path()), None);
        fs::write(etc.path().join("ufw/ufw.conf"), "ENABLED=yes\n").unwrap();
        let ufw = Ufw::read_from(etc.path()).unwrap();
        assert!(!ufw.allows("eth0", LAN, 22));
        assert_eq!(Ufw::read_from(&etc.path().join("missing")), None);
    }
}
