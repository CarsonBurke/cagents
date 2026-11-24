//! What a phone needs to know to reach this computer, and whether one is
//! connected: read from the system, off the UI thread.
//!
//! Nothing here needs the daemon. A connected phone is a running
//! `cagents daemon bridge --mobile` of this user's, and `SSH_CONNECTION` in
//! its environment says where it connected from and to.

use std::{
    ffi::CStr,
    fs,
    net::Ipv4Addr,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use cagents::mobile::pairing::{self, AuthorizedKeys, Phone};
use serde::Deserialize;

mod firewall;

/// How long `tailscale status` may take before it counts as not answering.
const TAILSCALE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Tailscale {
    Connected {
        /// The MagicDNS name, when it has one.
        name: Option<String>,
        addresses: Vec<String>,
    },
    /// Installed, but not logged in or not running.
    NotConnected,
    NotInstalled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    Tailscale,
    Lan,
    Other,
}

/// A phone connected now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Connection {
    /// The phone's address.
    pub from: String,
    pub route: Route,
}

/// How a phone reaches this computer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Network {
    pub name: String,
    pub user: String,
    pub port: u16,
    /// Whether an SSH server listens on `port`.
    pub listening: bool,
    pub host_key: Option<String>,
    pub tailscale: Tailscale,
    /// Private IPv4 addresses on physical interfaces, where the firewall
    /// doesn't block SSH.
    pub lan: Vec<String>,
    /// Addresses like those in `lan` that the firewall blocks SSH on.
    pub blocked: Vec<String>,
    /// Whether the firewall lets SSH in over Tailscale while it blocks the
    /// `blocked` addresses, as a computer set up to allow SSH only over
    /// Tailscale does.
    pub only_tailscale: bool,
}

impl Network {
    /// Addresses in the order a phone should try them: Tailscale first,
    /// since it works away from home too.
    pub fn hosts(&self) -> Vec<String> {
        let mut hosts = Vec::new();
        if let Tailscale::Connected { name, addresses } = &self.tailscale {
            hosts.extend(name.iter().cloned());
            hosts.extend(addresses.iter().cloned());
        }
        hosts.extend(self.lan.iter().cloned());
        hosts
    }

    pub fn connect(&self) -> pairing::Connect {
        pairing::Connect {
            name: self.name.clone(),
            user: self.user.clone(),
            port: self.port,
            host_key: self.host_key.clone(),
            hosts: self.hosts(),
        }
    }
}

/// What the status bar and the popover show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Status {
    pub phones: Result<Vec<Phone>, String>,
    /// Why pairing can't change `authorized_keys`, if it can't.
    pub unwritable: Option<String>,
    pub connections: Vec<Connection>,
    /// Read only while the popover is open; `None` until then.
    pub network: Option<Network>,
}

/// Read the status; the network too when `network`.
pub(crate) fn probe(network: bool) -> Status {
    let fixture = fixture::read();
    let keys = AuthorizedKeys::of_user();
    let (phones, unwritable) = match &keys {
        Ok(keys) => (
            keys.phones()
                .map_err(|error| format!("Couldn't read {}: {error}", keys.path().display())),
            keys.unwritable(),
        ),
        Err(error) => (Err(error.to_string()), Some(error.to_string())),
    };
    let network = network.then(|| match &fixture {
        Some(fixture) => fixture.network(),
        None => read_network(),
    });
    let connections = match &fixture {
        Some(fixture) => fixture.connections(network.as_ref()),
        None => connections(Path::new("/proc"), network.as_ref()),
    };
    Status {
        phones,
        unwritable,
        connections,
        network,
    }
}

fn read_network() -> Network {
    let (port, listening) = ssh_port();
    let tailscale = tailscale();
    let interfaces = interface_addresses();
    let ufw = firewall::Ufw::read();
    let allowed = |interface: &str, address: Ipv4Addr| {
        ufw.as_ref()
            .is_none_or(|ufw| ufw.allows(interface, address, port))
    };
    let (lan, blocked): (Vec<_>, Vec<_>) = lan_addresses(&interfaces, &tailscale)
        .into_iter()
        .partition(|(interface, address)| allowed(interface, *address));
    let only_tailscale = !blocked.is_empty() && {
        // The Tailscale interface is the one holding its address; tailscale0
        // by default, and when it isn't connected.
        let address = match &tailscale {
            Tailscale::Connected { addresses, .. } => {
                addresses.iter().find_map(|address| address.parse().ok())
            }
            _ => None,
        }
        .unwrap_or(Ipv4Addr::new(100, 64, 0, 1));
        let interface = interfaces
            .iter()
            .find(|(_, own)| *own == address)
            .map_or("tailscale0", |(name, _)| name.as_str());
        allowed(interface, address)
    };
    Network {
        name: cagents::mobile::host_name().unwrap_or_else(|| "this computer".into()),
        user: user_name(),
        port,
        listening,
        host_key: host_key(Path::new(SSHD_CONFIG)),
        lan: lan
            .into_iter()
            .map(|(_, address)| address.to_string())
            .collect(),
        blocked: blocked
            .into_iter()
            .map(|(_, address)| address.to_string())
            .collect(),
        only_tailscale,
        tailscale,
    }
}

fn user_name() -> String {
    // SAFETY: getpwuid returns a pointer into static storage or null; it is
    // read at once, on this thread.
    let name = unsafe {
        let entry = libc::getpwuid(libc::geteuid());
        (!entry.is_null() && !(*entry).pw_name.is_null()).then(|| {
            CStr::from_ptr((*entry).pw_name)
                .to_string_lossy()
                .into_owned()
        })
    };
    name.or_else(|| std::env::var("USER").ok())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// SSH server
// ---------------------------------------------------------------------------

const SSHD_CONFIG: &str = "/etc/ssh/sshd_config";

/// The fingerprint of sshd's Ed25519 host key: the first `HostKey` that is
/// one, or the default key when none is configured.
fn host_key(config: &Path) -> Option<String> {
    let configured = sshd_settings(config, "HostKey");
    let candidates = if configured.is_empty() {
        vec![PathBuf::from(pairing::HOST_KEY)]
    } else {
        configured
            .into_iter()
            .map(|private| PathBuf::from(format!("{private}.pub")))
            .collect()
    };
    candidates.iter().find_map(|public| {
        pairing::host_key(public)
            .ok()
            .filter(pairing::PublicKey::is_ed25519)
            .map(|key| key.fingerprint())
    })
}

/// The port sshd is configured for, and whether something listens there.
fn ssh_port() -> (u16, bool) {
    let configured = configured_ports(Path::new(SSHD_CONFIG));
    let listening = listening_ports();
    configured
        .iter()
        .find(|port| listening.contains(port))
        .map(|port| (*port, true))
        .unwrap_or((configured[0], false))
}

/// The `Port` lines of `config` and the files it includes, in order; 22
/// when there are none.
fn configured_ports(config: &Path) -> Vec<u16> {
    let mut ports: Vec<u16> = sshd_settings(config, "Port")
        .iter()
        .filter_map(|port| port.parse().ok())
        .collect();
    if ports.is_empty() {
        ports.push(22);
    }
    ports
}

/// The values of `wanted` in `config`, with the files it includes read
/// where they are included, up to the first `Match`: after it, settings
/// apply only to some connections.
fn sshd_settings(config: &Path, wanted: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut files = 0;
    read_sshd_settings(config, wanted, &mut values, &mut files);
    values
}

/// Adds `file`'s values of `wanted` to `values`; false once a `Match` ends
/// the settings that apply to every connection.
fn read_sshd_settings(
    file: &Path,
    wanted: &str,
    values: &mut Vec<String>,
    files: &mut usize,
) -> bool {
    // sshd refuses deeper nesting; this bounds a loop of includes.
    *files += 1;
    if *files > 64 {
        return true;
    }
    let Ok(text) = fs::read_to_string(file) else {
        return true;
    };
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let Some(keyword) = words.next().filter(|word| !word.starts_with('#')) else {
            continue;
        };
        if keyword.eq_ignore_ascii_case("Match") {
            return false;
        }
        if keyword.eq_ignore_ascii_case(wanted) {
            values.extend(words.next().map(str::to_owned));
        } else if keyword.eq_ignore_ascii_case("Include") {
            for pattern in words {
                let pattern = if Path::new(pattern).is_absolute() {
                    PathBuf::from(pattern)
                } else {
                    Path::new("/etc/ssh").join(pattern)
                };
                let mut matched: Vec<PathBuf> = glob::glob(&pattern.to_string_lossy())
                    .into_iter()
                    .flatten()
                    .flatten()
                    .collect();
                matched.sort();
                for included in matched {
                    if !read_sshd_settings(&included, wanted, values, files) {
                        return false;
                    }
                }
            }
        }
    }
    true
}

/// TCP ports something listens on, from `/proc/net/tcp` and `tcp6`.
fn listening_ports() -> Vec<u16> {
    ["/proc/net/tcp", "/proc/net/tcp6"]
        .iter()
        .filter_map(|path| fs::read_to_string(path).ok())
        .flat_map(|table| {
            table
                .lines()
                .skip(1)
                .filter_map(|line| {
                    let fields: Vec<&str> = line.split_whitespace().collect();
                    // State 0A is LISTEN.
                    (fields.get(3) == Some(&"0A"))
                        .then(|| fields.get(1)?.rsplit_once(':'))
                        .flatten()
                        .and_then(|(_, port)| u16::from_str_radix(port, 16).ok())
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Addresses
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TailscaleStatus {
    backend_state: String,
    #[serde(rename = "Self")]
    this: Option<TailscaleSelf>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TailscaleSelf {
    #[serde(rename = "DNSName", default)]
    dns_name: String,
    #[serde(default)]
    tailscale_i_ps: Option<Vec<String>>,
}

fn tailscale() -> Tailscale {
    let Ok(mut child) = Command::new("tailscale")
        .args(["status", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Tailscale::NotInstalled;
    };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < TAILSCALE_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Tailscale::NotConnected;
            }
        }
    }
    let output = child
        .wait_with_output()
        .map(|output| output.stdout)
        .unwrap_or_default();
    parse_tailscale(&output)
}

fn parse_tailscale(output: &[u8]) -> Tailscale {
    let Ok(status) = serde_json::from_slice::<TailscaleStatus>(output) else {
        return Tailscale::NotConnected;
    };
    let Some(this) = status.this.filter(|_| status.backend_state == "Running") else {
        return Tailscale::NotConnected;
    };
    let mut addresses = this.tailscale_i_ps.unwrap_or_default();
    // IPv4 first: it is shorter to read and to fit in the code.
    addresses.sort_by_key(|address| address.contains(':'));
    addresses.truncate(1);
    let name = Some(this.dns_name.trim_end_matches('.').to_owned()).filter(|name| !name.is_empty());
    if name.is_none() && addresses.is_empty() {
        return Tailscale::NotConnected;
    }
    Tailscale::Connected { name, addresses }
}

/// Private IPv4 addresses on interfaces that are up and backed by hardware,
/// which leaves out container bridges and VPN tunnels a phone can't use,
/// with their interfaces.
fn lan_addresses(
    interfaces: &[(String, Ipv4Addr)],
    tailscale: &Tailscale,
) -> Vec<(String, Ipv4Addr)> {
    let tailscale = match tailscale {
        Tailscale::Connected { addresses, .. } => addresses.clone(),
        _ => Vec::new(),
    };
    interfaces
        .iter()
        .filter(|(name, address)| {
            address.is_private()
                && !tailscale.contains(&address.to_string())
                && Path::new("/sys/class/net")
                    .join(name)
                    .join("device")
                    .exists()
        })
        .cloned()
        .collect()
}

fn interface_addresses() -> Vec<(String, Ipv4Addr)> {
    let mut addresses = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list` with a linked list that freeifaddrs
    // releases; each entry is read before that.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return addresses;
        }
        let mut entry = list;
        while !entry.is_null() {
            let interface = &*entry;
            entry = interface.ifa_next;
            let up = interface.ifa_flags & libc::IFF_UP as u32 != 0;
            let loopback = interface.ifa_flags & libc::IFF_LOOPBACK as u32 != 0;
            if !up
                || loopback
                || interface.ifa_addr.is_null()
                || i32::from((*interface.ifa_addr).sa_family) != libc::AF_INET
            {
                continue;
            }
            let address = &*(interface.ifa_addr as *const libc::sockaddr_in);
            let name = CStr::from_ptr(interface.ifa_name)
                .to_string_lossy()
                .into_owned();
            addresses.push((name, Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr))));
        }
        libc::freeifaddrs(list);
    }
    addresses
}

// ---------------------------------------------------------------------------
// Connected phones
// ---------------------------------------------------------------------------

/// This user's running mobile bridges under `proc`, with where each phone
/// connected from.
fn connections(proc: &Path, network: Option<&Network>) -> Vec<Connection> {
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    let Ok(entries) = fs::read_dir(proc) else {
        return Vec::new();
    };
    let mut connections: Vec<Connection> = entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.bytes().all(|byte| byte.is_ascii_digit()))
        })
        .filter(|entry| entry.metadata().is_ok_and(|meta| meta.uid() == uid))
        .filter(|entry| {
            fs::read(entry.path().join("cmdline")).is_ok_and(|cmdline| is_mobile_bridge(&cmdline))
        })
        .filter_map(|entry| {
            let environ = fs::read(entry.path().join("environ")).ok()?;
            let (from, to) = ssh_connection(&environ)?;
            Some(Connection {
                route: route(&to, &from, network),
                from,
            })
        })
        .collect();
    connections.sort_by(|a, b| a.from.cmp(&b.from));
    connections
}

fn is_mobile_bridge(cmdline: &[u8]) -> bool {
    let words: Vec<&[u8]> = cmdline.split(|byte| *byte == 0).collect();
    words.len() >= 4
        && Path::new(std::ffi::OsStr::from_bytes(words[0]))
            .file_name()
            .is_some_and(|name| name == "cagents")
        && words[1..4] == [b"daemon".as_slice(), b"bridge", b"--mobile"]
}

/// `SSH_CONNECTION`'s client and server addresses.
fn ssh_connection(environ: &[u8]) -> Option<(String, String)> {
    let value = environ
        .split(|byte| *byte == 0)
        .find_map(|variable| variable.strip_prefix(b"SSH_CONNECTION="))?;
    let text = std::str::from_utf8(value).ok()?;
    let words: Vec<&str> = text.split_whitespace().collect();
    Some((words.first()?.to_string(), words.get(2)?.to_string()))
}

fn route(server: &str, client: &str, network: Option<&Network>) -> Route {
    let in_tailscale_range = |address: &str| {
        address
            .parse::<Ipv4Addr>()
            .is_ok_and(|address| address.octets()[0] == 100 && address.octets()[1] & 0xc0 == 64)
            || address.starts_with("fd7a:115c:a1e0:")
    };
    if in_tailscale_range(server) || in_tailscale_range(client) {
        return Route::Tailscale;
    }
    if network.is_some_and(|network| network.lan.iter().any(|address| address == server))
        || server
            .parse::<Ipv4Addr>()
            .is_ok_and(|address| address.is_private())
    {
        return Route::Lan;
    }
    Route::Other
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A development fixture that stands in for the network, the SSH server and
/// the connected phones, so native evidence can show each state without a
/// phone, Tailscale or sshd. `authorized_keys` is still the fixture HOME's
/// own file.
///
/// It is compiled into debug builds only, and read only when `HOME` lies
/// inside a native-evidence task (an ancestor holds the task's
/// `.native-evidence` marker) and holds [`fixture::MARKER`], a JSON object
/// whose fields each replace one probe; see docs/fixtures.md.
mod fixture {
    use super::*;

    pub(super) const MARKER: &str = ".cagents-fixture-mobile-host";

    #[derive(Deserialize, Default)]
    #[serde(default)]
    pub(super) struct Fixture {
        name: Option<String>,
        user: Option<String>,
        port: Option<u16>,
        listening: Option<bool>,
        host_key: Option<String>,
        /// `null` for installed but not connected; absent for not installed.
        tailscale: Option<Option<FixtureTailscale>>,
        lan: Vec<String>,
        /// Network addresses the firewall blocks SSH on, while it allows SSH
        /// over Tailscale.
        blocked: Vec<String>,
        /// The phones' addresses.
        connections: Vec<String>,
    }

    #[derive(Deserialize)]
    struct FixtureTailscale {
        name: Option<String>,
        #[serde(default)]
        addresses: Vec<String>,
    }

    impl Fixture {
        pub(super) fn network(&self) -> Network {
            Network {
                name: self.name.clone().unwrap_or_else(|| "workstation".into()),
                user: self.user.clone().unwrap_or_else(user_name),
                port: self.port.unwrap_or(22),
                listening: self.listening.unwrap_or(true),
                host_key: self.host_key.clone(),
                tailscale: match &self.tailscale {
                    None => Tailscale::NotInstalled,
                    Some(None) => Tailscale::NotConnected,
                    Some(Some(tailscale)) => Tailscale::Connected {
                        name: tailscale.name.clone(),
                        addresses: tailscale.addresses.clone(),
                    },
                },
                lan: self.lan.clone(),
                only_tailscale: !self.blocked.is_empty(),
                blocked: self.blocked.clone(),
            }
        }

        pub(super) fn connections(&self, network: Option<&Network>) -> Vec<Connection> {
            self.connections
                .iter()
                .map(|from| Connection {
                    route: route("", from, network),
                    from: from.clone(),
                })
                .collect()
        }
    }

    pub(super) fn read() -> Option<Fixture> {
        if !cfg!(debug_assertions) {
            return None;
        }
        let home = std::env::var_os("HOME")?;
        let home = Path::new(&home).canonicalize().ok()?;
        if !home
            .ancestors()
            .skip(1)
            .any(|path| path.join(".native-evidence").is_file())
        {
            return None;
        }
        serde_json::from_slice(&fs::read(home.join(MARKER)).ok()?).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailscale_counts_only_when_running() {
        let running = br#"{"BackendState":"Running","Self":{"DNSName":"box.tail1.ts.net.","TailscaleIPs":["fd7a:115c:a1e0::1","100.101.1.2"]}}"#;
        assert_eq!(
            parse_tailscale(running),
            Tailscale::Connected {
                name: Some("box.tail1.ts.net".into()),
                addresses: vec!["100.101.1.2".into()],
            }
        );
        let logged_out =
            br#"{"BackendState":"NeedsLogin","Self":{"DNSName":"","TailscaleIPs":null}}"#;
        assert_eq!(parse_tailscale(logged_out), Tailscale::NotConnected);
        assert_eq!(parse_tailscale(b"garbage"), Tailscale::NotConnected);
    }

    #[test]
    fn sshd_ports_follow_includes_and_stop_at_match() {
        let root = tempfile::tempdir().unwrap();
        let included = root.path().join("conf.d");
        fs::create_dir(&included).unwrap();
        fs::write(included.join("10-port.conf"), "Port 2222\n").unwrap();
        let config = root.path().join("sshd_config");
        fs::write(
            &config,
            format!(
                "# Port 1\nInclude {}/*.conf\nport 22\nMatch User other\nPort 3333\n",
                included.display()
            ),
        )
        .unwrap();
        assert_eq!(configured_ports(&config), [2222, 22]);
        assert_eq!(configured_ports(&root.path().join("missing")), [22]);
    }

    #[test]
    fn the_host_key_is_the_configured_ed25519_one() {
        let root = tempfile::tempdir().unwrap();
        let rsa = root.path().join("rsa_key");
        let ed25519 = root.path().join("ed25519_key");
        fs::write(
            rsa.with_extension("pub"),
            "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQ host\n",
        )
        .unwrap();
        let key = pairing::PublicKey::ed25519(&[3; 32]);
        fs::write(
            root.path().join("ed25519_key.pub"),
            format!("{} root@host\n", key.openssh()),
        )
        .unwrap();
        let config = root.path().join("sshd_config");
        fs::write(
            &config,
            format!("HostKey {}\nHostKey {}\n", rsa.display(), ed25519.display()),
        )
        .unwrap();
        assert_eq!(host_key(&config), Some(key.fingerprint()));
        fs::write(&config, format!("HostKey {}\n", rsa.display())).unwrap();
        assert_eq!(host_key(&config), None);
    }

    #[test]
    fn connected_bridges_are_found_with_their_addresses() {
        let proc = tempfile::tempdir().unwrap();
        let process = |pid: &str, cmdline: &[u8], environ: &[u8]| {
            let directory = proc.path().join(pid);
            fs::create_dir(&directory).unwrap();
            fs::write(directory.join("cmdline"), cmdline).unwrap();
            fs::write(directory.join("environ"), environ).unwrap();
        };
        process(
            "100",
            b"/home/a/.local/bin/cagents\0daemon\0bridge\0--mobile\0",
            b"HOME=/home/a\0SSH_CONNECTION=100.88.1.4 50000 100.101.1.2 22\0",
        );
        process(
            "101",
            b"/x/cagents\0daemon\0bridge\0--mobile\0",
            b"SSH_CONNECTION=10.0.0.249 41000 10.0.0.113 22\0",
        );
        // A desktop device bridge, and a bridge started without SSH.
        process(
            "102",
            b"/x/cagents\0daemon\0bridge\0",
            b"SSH_CONNECTION=10.0.0.5 1 10.0.0.113 22\0",
        );
        process(
            "103",
            b"/x/cagents\0daemon\0bridge\0--mobile\0",
            b"HOME=/\0",
        );
        fs::create_dir(proc.path().join("self")).unwrap();
        assert_eq!(
            connections(proc.path(), None),
            [
                Connection {
                    from: "10.0.0.249".into(),
                    route: Route::Lan,
                },
                Connection {
                    from: "100.88.1.4".into(),
                    route: Route::Tailscale,
                },
            ]
        );
    }

    #[test]
    fn hosts_put_tailscale_first() {
        let network = Network {
            name: "box".into(),
            user: "a".into(),
            port: 22,
            listening: true,
            host_key: None,
            tailscale: Tailscale::Connected {
                name: Some("box.ts.net".into()),
                addresses: vec!["100.1.2.3".into()],
            },
            lan: vec!["10.0.0.2".into()],
            blocked: vec!["192.168.1.20".into()],
            only_tailscale: true,
        };
        assert_eq!(network.hosts(), ["box.ts.net", "100.1.2.3", "10.0.0.2"]);
    }
}
