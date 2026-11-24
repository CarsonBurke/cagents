//! `cagents device`: links between this computer and its saved devices, and
//! whether sessions here may use them. See [`crate::device_trust`].

use super::*;
use crate::device_trust::{LinkKey, LinkStatus, Reply, Request};

#[derive(Debug, clap::Subcommand)]
pub enum DeviceCommand {
    /// List saved devices, which are linked, and which agents here may use.
    List,
    /// Link a saved device to this computer.
    #[command(
        after_help = "Run it in a system terminal outside cagents. Linking alone lets no agent use the device; add --allow-agents, or run `cagents device allow-agents NAME` later."
    )]
    Link {
        /// A device name from devices.json.
        name: String,
        /// Also let sessions on this computer start and drive agents there.
        #[arg(long)]
        allow_agents: bool,
    },
    /// Let sessions on this computer start and drive agents on a linked device.
    #[command(after_help = "Run it in a system terminal outside cagents.")]
    AllowAgents {
        /// A linked device's name.
        name: String,
    },
    /// Stop sessions on this computer from using a device.
    #[command(
        after_help = "Their agents there keep running; a person on that device can still reach them."
    )]
    DenyAgents {
        /// A linked device's name.
        name: String,
    },
    /// Forget a device's link on this computer and on the device.
    Unlink {
        /// A linked device's name.
        name: String,
    },
}

impl DeviceCommand {
    /// Whether only a person may run it: it changes what sessions can do.
    pub(super) fn changes_access(&self) -> bool {
        !matches!(self, Self::List)
    }
}

pub(super) fn execute(command: DeviceCommand) -> Result<CommandOutput, AppError> {
    // This computer's daemon first: selecting the device routes every later
    // connection there.
    let mut local = control_daemon_client()?;
    let mut trust = |request| local.device_trust(request).map_err(protocol_error);
    match command {
        DeviceCommand::List => list(&trust(Request::Links)?),
        DeviceCommand::Link { name, allow_agents } => {
            let device = saved(&name)?;
            let peer = link_peer(&links(trust(Request::Links)?), &name);
            let key = LinkKey::generate().map_err(|error| AppError::new("device", error))?;
            on_device(&name, |remote| {
                remote.device_trust(Request::RegisterPeer {
                    peer,
                    label: label(),
                    key: key.clone(),
                })
            })?;
            trust(Request::SaveLink {
                device: name.clone(),
                peer,
                key,
                agents_allowed: allow_agents,
            })?;
            Ok(CommandOutput {
                command: "device.link",
                data: json!({ "device": name, "host": device.host, "agents_allowed": allow_agents }),
                human: if allow_agents {
                    format!("Linked {name}. Sessions on this computer may use it.")
                } else {
                    format!(
                        "Linked {name}. No session here may use it yet; allow it with `cagents device allow-agents {name}`."
                    )
                },
            })
        }
        DeviceCommand::AllowAgents { name } => set_allowed(&mut trust, name, true),
        DeviceCommand::DenyAgents { name } => set_allowed(&mut trust, name, false),
        DeviceCommand::Unlink { name } => {
            let Reply::Removed { peer } = trust(Request::RemoveLink {
                device: name.clone(),
            })?
            else {
                return Err(AppError::new("device", "unexpected reply to unlinking"));
            };
            let Some(peer) = peer else {
                return Err(AppError::new(
                    "device",
                    format!("{name} is not linked to this computer"),
                ));
            };
            // The key is gone here, so nothing can sign for this link again;
            // forgetting it on the device only tidies up.
            let forgotten = on_device(&name, |remote| {
                remote.device_trust(Request::ForgetPeer { peer })
            });
            let human = match &forgotten {
                Ok(()) => format!("Unlinked {name}."),
                Err(error) => format!(
                    "Unlinked {name} here. {name} could not be reached to forget this computer ({}); it can no longer act for sessions here either way.",
                    error.message
                ),
            };
            Ok(CommandOutput {
                command: "device.unlink",
                data: json!({ "device": name, "device_forgot": forgotten.is_ok() }),
                human,
            })
        }
    }
}

fn set_allowed(
    trust: &mut impl FnMut(Request) -> Result<Reply, AppError>,
    name: String,
    allowed: bool,
) -> Result<CommandOutput, AppError> {
    trust(Request::SetAgentsAllowed {
        device: name.clone(),
        allowed,
    })?;
    Ok(CommandOutput {
        command: if allowed {
            "device.allow_agents"
        } else {
            "device.deny_agents"
        },
        data: json!({ "device": name, "agents_allowed": allowed }),
        human: if allowed {
            format!("Sessions on this computer may now use {name}.")
        } else {
            format!(
                "Sessions on this computer may no longer use {name}. Agents they started there keep running."
            )
        },
    })
}

/// The identity this computer links to `device` with. Linking again
/// replaces only the key: this computer keeps its identity on the device, so
/// the agents its sessions started there stay theirs.
fn link_peer(links: &[LinkStatus], device: &str) -> uuid::Uuid {
    links
        .iter()
        .find(|link| link.device == device)
        .map_or_else(uuid::Uuid::new_v4, |link| link.peer)
}

fn links(reply: Reply) -> Vec<LinkStatus> {
    match reply {
        Reply::Links(links) => links,
        _ => Vec::new(),
    }
}

fn saved(name: &str) -> Result<crate::device::Device, AppError> {
    crate::device::list()
        .map_err(|error| AppError::new("device", error))?
        .into_iter()
        .find(|device| device.name == name)
        .ok_or_else(|| {
            AppError::new(
                "device",
                format!(
                    "No saved device is named {name}. Add it with Connect to a device… in the app, or to {}.",
                    crate::device::configuration_path()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|_| "devices.json".into())
                ),
            )
        })
}

/// Runs `request` as the user on the device `name`, over its SSH bridge.
fn on_device<T>(
    name: &str,
    request: impl FnOnce(&mut ControlClient) -> Result<T, ControlError>,
) -> Result<(), AppError> {
    crate::device::select(Some(name)).map_err(|error| AppError::new("device", error))?;
    crate::device::prepare().map_err(|error| AppError::new("device", error))?;
    let mut remote = control_daemon_client()?;
    request(&mut remote).map_err(protocol_error)?;
    Ok(())
}

/// How the device names this computer.
fn label() -> String {
    let mut buffer = [0u8; 256];
    // SAFETY: the pointer and length describe `buffer`, which outlives the
    // call; gethostname writes at most that many bytes into it.
    if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } != 0 {
        return "another computer".into();
    }
    std::ffi::CStr::from_bytes_until_nul(&buffer)
        .ok()
        .and_then(|name| name.to_str().ok())
        .filter(|name| !name.is_empty())
        .map_or_else(|| "another computer".into(), str::to_owned)
}

fn list(reply: &Reply) -> Result<CommandOutput, AppError> {
    let Reply::Links(links) = reply else {
        return Err(AppError::new("device", "unexpected reply to listing links"));
    };
    let devices = crate::device::list().map_err(|error| AppError::new("device", error))?;
    let rows: Vec<Value> = devices
        .iter()
        .map(|device| {
            let link = links.iter().find(|link| link.device == device.name);
            json!({
                "name": device.name,
                "host": device.host,
                "linked": link.is_some(),
                "agents_allowed": link.is_some_and(|link| link.agents_allowed),
            })
        })
        .collect();
    let human = if devices.is_empty() {
        "No saved devices. Add one with Connect to a device… in the app.".to_owned()
    } else {
        let mut lines = vec![format!(
            "{:<16}  {:<24}  {}",
            "DEVICE", "SSH HOST", "AGENTS"
        )];
        for device in &devices {
            let link = links.iter().find(|link| link.device == device.name);
            lines.push(format!(
                "{:<16}  {:<24}  {}",
                device.name,
                device.host,
                match link {
                    None => "not linked",
                    Some(link) if link.agents_allowed => "allowed",
                    Some(_) => "not allowed",
                }
            ));
        }
        lines.join("\n")
    };
    Ok(CommandOutput {
        command: "device.list",
        data: json!({ "devices": rows }),
        human,
    })
}

#[cfg(all(test, not(any(feature = "host", feature = "ui"))))]
mod tests {
    use super::*;

    #[test]
    fn linking_again_keeps_this_computers_identity_on_the_device() {
        let link = |device: &str| LinkStatus {
            device: device.into(),
            peer: uuid::Uuid::new_v4(),
            agents_allowed: true,
            linked_at: 0,
        };
        let links = [link("laptop"), link("fw16")];
        assert_eq!(link_peer(&links, "fw16"), links[1].peer);
        let fresh = link_peer(&links, "devx");
        assert!(links.iter().all(|link| link.peer != fresh));
        assert_ne!(fresh, link_peer(&links, "devx"));
    }
}
