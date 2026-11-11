//! Measurement-only canonical edits against this task's private native fixture.
cagents::stamp_version!();

use cagents::{control::ControlClient, model::ChannelId, workspace_model::ModelEdit};
use std::{
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};

fn now_ns() -> u64 {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut value) },
        0
    );
    value.tv_sec as u64 * 1_000_000_000 + value.tv_nsec as u64
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 4 {
        return Err("usage: frame-edits TASK ATTEMPT CHANNEL burst|restore".into());
    }
    let task = PathBuf::from(&args[0]).canonicalize()?;
    if task != PathBuf::from("/home/marvin/.local/state/cev/perf2440-followup")
        || !task.join(".native-evidence").is_file()
        || std::env::var_os("CAGENTS_AGENT_AUTHORITY").is_some()
        || std::env::var_os("HOME").map(PathBuf::from) != Some(task.join("fixture/home"))
        || std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) != Some(task.join("rt"))
        || !args[1]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
        || args[1].is_empty()
    {
        return Err("requires this task's genuine private native environment".into());
    }
    let out = task.join(&args[1]);
    if !out.is_dir() {
        return Err("observer attempt must exist first".into());
    }
    let id: ChannelId = serde_json::from_value(serde_json::Value::String(args[2].clone()))?;
    let mut client = ControlClient::connect(&task.join("rt/cagents-development/cagentsd.sock"))?;
    let path = out.join("frame-edit-record.json");
    let mut record = if args[3] == "burst" {
        let model = client.read_workspace_model()?;
        let channel = model
            .channels
            .iter()
            .find(|c| c.id == id)
            .ok_or("channel absent")?;
        let value = serde_json::json!({"channel": id, "original_title": channel.title, "original_title_source": channel.title_source, "edits": []});
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        serde_json::to_writer_pretty(&mut f, &value)?;
        f.flush()?;
        value
    } else if args[3] == "restore" {
        serde_json::from_slice(&std::fs::read(&path)?)?
    } else {
        return Err("expected burst or restore".into());
    };
    if record["channel"] != serde_json::to_value(id)? {
        return Err("record channel mismatch".into());
    }
    if args[3] == "burst" {
        let origin = Instant::now();
        for index in 0..20 {
            std::thread::sleep(
                (origin + Duration::from_millis(index * 100))
                    .saturating_duration_since(Instant::now()),
            );
            let title = format!("P2440 frame measurement {:02}", index + 1);
            let sent = now_ns();
            let model = client.edit_workspace_model(ModelEdit::RetitleChannel {
                id,
                title: title.clone(),
            })?;
            if !model
                .channels
                .iter()
                .any(|channel| channel.id == id && channel.title == title)
            {
                return Err("canonical title did not accept the requested edit".into());
            }
            record["edits"].as_array_mut().unwrap().push(serde_json::json!({"title":title,"sent_ns":sent,"accepted_ns":now_ns(),"sequence":model.sequence}));
            let temp = out.join("frame-edit-record.tmp");
            std::fs::write(&temp, serde_json::to_vec_pretty(&record)?)?;
            std::fs::rename(temp, &path)?;
        }
    } else {
        // A deliberate rename uses normal Manual title-source semantics.
        // Restore the original text; the common raw seed is restored per trial.
        let title = record["original_title"]
            .as_str()
            .ok_or("missing original title")?
            .to_owned();
        let model = client.edit_workspace_model(ModelEdit::RetitleChannel { id, title })?;
        if !model.channels.iter().any(|channel| {
            channel.id == id && channel.title == record["original_title"].as_str().unwrap()
        }) {
            return Err("canonical title did not restore the recorded text".into());
        }
        record["restored_sequence"] = serde_json::json!(model.sequence);
        record["restore_accepted_ns"] = serde_json::json!(now_ns());
        std::fs::write(&path, serde_json::to_vec_pretty(&record)?)?;
    }
    println!("{}", serde_json::to_string(&record)?);
    Ok(())
}
