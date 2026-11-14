//! Renames one shared channel in a timed burst through the daemon's ordinary
//! model edit API, then restores its name, as another window renaming it
//! would. Windows watching the registry receive each rename as an edge, which
//! makes this the workload for measuring how they absorb metadata edits.
//!
//! Usage: rename-channel-burst SOCKET [CHANNEL] [COUNT] [INTERVAL_MS]
//!
//! Without CHANNEL, the first shared channel is renamed. Each line printed is
//! one accepted edit: its index, when it was sent and when its reply arrived,
//! in milliseconds from the start of the burst.
use cagents::{control::ControlClient, model::ChannelId, workspace_model::ModelEdit};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

cagents::stamp_version!();

const USAGE: &str = "usage: rename-channel-burst SOCKET [CHANNEL] [COUNT] [INTERVAL_MS]";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let socket = PathBuf::from(args.next().ok_or(USAGE)?);
    let channel = args.next().filter(|channel| !channel.is_empty());
    let count = args.next().map_or(Ok(20), |count| count.parse::<u64>())?;
    let interval = Duration::from_millis(
        args.next()
            .map_or(Ok(100), |interval| interval.parse::<u64>())?,
    );
    if args.next().is_some() {
        return Err(USAGE.into());
    }
    let mut client = ControlClient::connect(&socket)?;
    let model = client.read_workspace_model()?;
    let original = match &channel {
        Some(channel) => {
            let id: ChannelId = serde_json::from_value(serde_json::Value::String(channel.clone()))?;
            model.channel(id).ok_or("no such shared channel")?
        }
        None => model.channels.first().ok_or("no shared channel")?,
    }
    .clone();
    let ms = |at: Instant, origin: Instant| at.duration_since(origin).as_secs_f64() * 1e3;
    let origin = Instant::now();
    for index in 0..count {
        let due = origin + interval * u32::try_from(index)?;
        std::thread::sleep(due.saturating_duration_since(Instant::now()));
        let title = format!("Rename burst {:02}", index + 1);
        let sent = Instant::now();
        let reply = client.edit_workspace_model(ModelEdit::RetitleChannel {
            id: original.id,
            title: title.clone(),
        })?;
        let accepted = Instant::now();
        if reply
            .channel(original.id)
            .map(|channel| channel.title.as_str())
            != Some(&title)
        {
            return Err(format!("edit {index} was not applied").into());
        }
        println!(
            "edit {index} sent {:.3} ms accepted {:.3} ms",
            ms(sent, origin),
            ms(accepted, origin)
        );
    }
    // The burst named the channel by hand, so its old name returns the same
    // way, and stays a manual name from here on.
    let restore = ModelEdit::RetitleChannel {
        id: original.id,
        title: original.title.clone(),
    };
    client.edit_workspace_model(restore)?;
    println!("restored {} to {:?}", original.id, original.title);
    Ok(())
}
