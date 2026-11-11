//! Bounded synthetic workspace edit driver. See docs/performance.md.
//! Run only against a native-evidence fixture, with its matched development build.

cagents::stamp_version!();

#[cfg(feature = "development")]
#[path = "support/workspace_fixture.rs"]
mod workspace_fixture;

#[cfg(not(feature = "development"))]
fn main() {
    eprintln!("workspace-edit-workload requires --features development");
    std::process::exit(2);
}

#[cfg(feature = "development")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use cagents::{
        control::ControlClient,
        model::{BrowserId, CardId, CardKind, ChannelId},
        workspace_model::{
            ModelEdit,
            views::{Change, Pane, Scope},
        },
    };
    use std::{
        path::PathBuf,
        time::{Duration, Instant},
    };

    fn prepared_pane(id: CardId, browser: BrowserId) -> Result<Pane, Box<dyn std::error::Error>> {
        workspace_fixture::browser_pane(id, browser, "Edit measurement")
    }

    let arguments: Vec<_> = std::env::args().skip(1).collect();
    if arguments.len() != 3 {
        return Err("usage: workspace-edit-workload FIXTURE_ROOT EDITS INTERVAL_MS | FIXTURE_ROOT focused-transfer WORKTREE_PATH".into());
    }
    let fixture = workspace_fixture::Fixture::open(PathBuf::from(&arguments[0]))?;
    if arguments[1] == "focused-transfer" {
        return workspace_fixture::focused_transfer(&fixture, PathBuf::from(&arguments[2]));
    }
    let root = fixture.root.clone();
    let task = &fixture.task;
    let socket = fixture.socket.clone();

    let edits: usize = arguments[1].parse()?;
    let interval = Duration::from_millis(arguments[2].parse()?);
    if !(1..=2000).contains(&edits)
        || interval < Duration::from_millis(20)
        || interval > Duration::from_secs(10)
    {
        return Err(
            "use 1-2000 edits and a 20-10000 ms interval; also wrap the driver in scripts/bounded"
                .into(),
        );
    }
    let mut client = ControlClient::connect(&socket)?;
    let model = client.read_workspace_model()?;
    let project = model
        .projects
        .first()
        .ok_or("seed the fixture workspace first")?;
    let (channel, scope, mut pane, mut revision) =
        if let Some(resume) = std::env::var_os("CAGENTS_WORKSPACE_EDIT_RESUME") {
            let resume = PathBuf::from(resume).canonicalize()?;
            if !resume.starts_with(task) || resume.starts_with(&root) {
                return Err("resume marker must belong to this task outside its fixture".into());
            }
            let ready: serde_json::Value = serde_json::from_slice(&std::fs::read(resume)?)?;
            if ready.get("event").and_then(serde_json::Value::as_str) != Some("prepared") {
                return Err("resume marker is not a prepared event".into());
            }
            let channel: ChannelId = serde_json::from_value(ready["channel"].clone())?;
            let id: CardId = serde_json::from_value(ready["pane"].clone())?;
            let metadata = model
                .channels
                .iter()
                .find(|entry| entry.id == channel)
                .ok_or("prepared measurement channel is absent")?;
            if metadata.title != "Workspace edit measurement"
                || metadata.title_source != cagents::model::ChannelTitleSource::Manual
                || metadata.repository != project.repository
            {
                return Err(
                    "resume requires the same manual measurement channel and repository".into(),
                );
            }
            let scope = Scope::Channel(channel);
            let view = model
                .views
                .iter()
                .find(|view| view.scope == scope)
                .ok_or("prepared scope is absent")?;
            if !view.retired.is_empty() {
                return Err("resume requires a fresh prepared scope without retired panes".into());
            }
            let [pane] = view.panes.as_slice() else {
                return Err("resume requires exactly one active prepared pane".into());
            };
            let CardKind::Browser(browser) = &pane.kind else {
                return Err("prepared pane is not a browser".into());
            };
            if pane.id != id || *pane != prepared_pane(id, browser.id)? {
                return Err("resume requires the exact unchanged prepared pane descriptor".into());
            }
            let pane = pane.clone();
            (channel, scope, pane, view.revision)
        } else {
            let channel = ChannelId::new();
            client.edit_workspace_model(ModelEdit::CreateChannel {
                id: channel,
                repository: project.repository.clone(),
                title: "Workspace edit measurement".into(),
                title_source: cagents::model::ChannelTitleSource::Manual,
            })?;
            let scope = Scope::Channel(channel);
            let pane = prepared_pane(CardId::new(), BrowserId::new())?;
            let initial = client.edit_workspace_model(ModelEdit::ChangeView {
                scope: scope.clone(),
                expected_revision: 0,
                operation: uuid::Uuid::new_v4(),
                change: Change::Insert {
                    pane: pane.clone(),
                    before: None,
                },
            })?;
            let revision = initial
                .views
                .iter()
                .find(|view| view.scope == scope)
                .ok_or("inserted scope missing")?
                .revision;
            (channel, scope, pane, revision)
        };
    let resumed = model.views.iter().any(|view| view.scope == scope);
    let prepared_views = model.views.len() + usize::from(!resumed);
    let prepared_panes = model
        .views
        .iter()
        .map(|view| view.panes.len())
        .sum::<usize>()
        + usize::from(!resumed);
    // A native observer can select this pane before timed deadlines exist.
    // The optional marker belongs to this task, outside the restored fixture.
    if let Some(ready) = std::env::var_os("CAGENTS_WORKSPACE_EDIT_READY") {
        let ready = PathBuf::from(ready);
        let parent = ready
            .parent()
            .ok_or("ready marker has no parent")?
            .canonicalize()?;
        if !parent.starts_with(task) || parent.starts_with(&root) {
            return Err("the ready marker must be under this task and outside the fixture".into());
        }
        let setup = serde_json::json!({"event":"prepared", "pane":pane.id.to_string(), "channel":channel.to_string()});
        workspace_fixture::publish_json(&ready, &setup)?;
        println!("{setup}");
        let deadline = Instant::now() + Duration::from_secs(120);
        while !ready.with_extension("go").is_file() {
            if Instant::now() >= deadline {
                return Err("the native pane was not selected within 120 seconds".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    std::thread::sleep(Duration::from_secs(3));
    let origin = Instant::now();
    println!(
        "{}",
        serde_json::json!({"event":"begin", "pane":pane.id.to_string(), "edits":edits, "interval_ms":interval.as_millis(), "views":prepared_views, "panes":prepared_panes, "agents":model.agents.len(), "resumed":resumed})
    );
    for index in 0..edits {
        let deadline = origin + interval * index as u32;
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
        let mut next = pane.clone();
        next.title = format!("Measurement {index}");
        next.user_title = Some(next.title.clone());
        let edit = ModelEdit::ChangeView {
            scope: scope.clone(),
            expected_revision: revision,
            operation: uuid::Uuid::new_v4(),
            change: Change::Update {
                before: pane.clone(),
                pane: next.clone(),
            },
        };
        let started = Instant::now();
        let receipt = client.apply_workspace_edit(edit)?;
        revision = receipt
            .effects
            .views
            .iter()
            .find(|view| view.scope == scope)
            .and_then(|view| view.after)
            .ok_or("edit has no scope revision")?;
        let elapsed = started.elapsed();
        println!(
            "{}",
            serde_json::json!({"event":"edit", "index":index, "elapsed_ns":elapsed.as_nanos(), "late_ns":started.saturating_duration_since(deadline).as_nanos(), "sequence":receipt.sequence})
        );
        pane = next;
    }
    println!(
        "{}",
        serde_json::json!({"event":"end", "elapsed_ns":origin.elapsed().as_nanos()})
    );
    Ok(())
}
