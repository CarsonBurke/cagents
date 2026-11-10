//! Bounded synthetic workspace edit driver. See docs/performance.md.
//! Run only against a native-evidence fixture, with its matched development build.

cagents::stamp_version!();

#[cfg(not(feature = "development"))]
fn main() {
    eprintln!("workspace-edit-workload requires --features development");
    std::process::exit(2);
}

#[cfg(feature = "development")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use cagents::{
        control::ControlClient,
        model::{BrowserId, BrowserRestoreFidelity, BrowserSession, CardId, CardKind, ChannelId},
        terminal::ipc::daemon_socket_path,
        workspace_model::{
            ModelEdit,
            views::{Change, Pane, Scope},
        },
    };
    use std::{
        path::PathBuf,
        time::{Duration, Instant},
    };

    let arguments: Vec<_> = std::env::args().skip(1).collect();
    if arguments.len() != 3 {
        return Err("usage: workspace-edit-workload FIXTURE_ROOT EDITS INTERVAL_MS".into());
    }
    let root = PathBuf::from(&arguments[0]).canonicalize()?;
    let task = root.parent().ok_or("fixture has no parent")?;
    if !task.join(".native-evidence").is_file()
        || root.file_name().is_none_or(|name| name != "fixture")
    {
        return Err("the root must be a native-evidence task's fixture directory".into());
    }
    let socket = daemon_socket_path()?;
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR")?);
    let namespace = match std::env::var("CAGENTS_DEVELOPMENT_INSTANCE") {
        Ok(name)
            if !name.is_empty()
                && name.len() <= 24
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-') =>
        {
            Some(format!("cagents-development-{name}"))
        }
        Err(std::env::VarError::NotPresent) => Some("cagents-development".to_owned()),
        _ => None,
    };
    let nested_fixture = namespace.is_some_and(|namespace| {
        runtime.file_name().is_some_and(|part| {
            let part = part.to_string_lossy();
            part.starts_with("nh-") && part.len() == 11
        }) && socket == runtime.join(namespace).join("cagentsd.sock")
    });
    if !(socket.starts_with(task) || nested_fixture)
        || !PathBuf::from(std::env::var_os("XDG_DATA_HOME").ok_or("XDG_DATA_HOME")?)
            .starts_with(&root)
    {
        return Err("the selected daemon and data directory must belong to the fixture".into());
    }
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
    let channel = ChannelId::new();
    client.edit_workspace_model(ModelEdit::CreateChannel {
        id: channel,
        repository: project.repository.clone(),
        title: "Workspace edit measurement".into(),
        title_source: cagents::model::ChannelTitleSource::Manual,
    })?;
    let scope = Scope::Channel(channel);
    let mut pane = Pane {
        id: CardId::new(),
        title: "Edit measurement".into(),
        user_title: Some("Edit measurement".into()),
        pending_terminal_start: false,
        width_fraction: 0.5,
        scope: None,
        kind: CardKind::Browser(BrowserSession {
            id: BrowserId::new(),
            current_url: "about:blank".parse()?,
            history: vec![],
            history_index: 0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            fidelity: BrowserRestoreFidelity::UrlOnly,
        }),
    };
    let initial = client.edit_workspace_model(ModelEdit::ChangeView {
        scope: scope.clone(),
        expected_revision: 0,
        operation: uuid::Uuid::new_v4(),
        change: Change::Insert {
            pane: pane.clone(),
            before: None,
        },
    })?;
    let mut revision = initial
        .views
        .iter()
        .find(|view| view.scope == scope)
        .ok_or("inserted scope missing")?
        .revision;
    // A native observer can select this pane before timed deadlines exist.
    // The optional marker belongs to this task, outside the restored fixture.
    if let Some(ready) = std::env::var_os("CAGENTS_WORKSPACE_EDIT_READY") {
        use std::io::Write as _;
        let ready = PathBuf::from(ready);
        let parent = ready
            .parent()
            .ok_or("ready marker has no parent")?
            .canonicalize()?;
        if !parent.starts_with(task) || parent.starts_with(&root) {
            return Err("the ready marker must be under this task and outside the fixture".into());
        }
        let setup = serde_json::json!({"event":"prepared", "pane":pane.id.to_string(), "channel":channel.to_string()});
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&ready)?;
        writeln!(file, "{setup}")?;
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
        serde_json::json!({"event":"begin", "pane":pane.id.to_string(), "edits":edits, "interval_ms":interval.as_millis(), "views":model.views.len(), "panes":model.views.iter().map(|view|view.panes.len()).sum::<usize>(), "agents":model.agents.len()})
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
