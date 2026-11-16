//! Observe a fixture pane, or publish, prepare and perform its checked close.
//! This leaves the saved pane unchanged, so a normal UI relaunch can exercise
//! its recovery. Build against the same protocol as the fixture daemon.
use cagents::{
    agent::Provider,
    agent_execution::{Authority, HostedSession},
    chat::{Launch, NativeContinuity, Session},
    control::ControlClient,
    model::{CardId, CardKind, LaunchRecipe, TerminalId},
    workspace_model::{
        ModelEdit, WorkspaceModel,
        views::{Change, Pane, Scope},
    },
};
use serde::{Deserialize, Serialize};
use std::{
    error::Error,
    fs,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Target {
    task: PathBuf,
    socket: PathBuf,
    scope: Scope,
    pane: CardId,
    native: TerminalId,
    thread: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Prepared {
    target: Target,
    model_identity: Uuid,
    pane: Pane,
    session: Session,
}

#[derive(Debug, Serialize)]
struct Observation {
    target: Target,
    model_identity: Uuid,
    pane: Pane,
    actual_native: TerminalId,
    matching_inventory: Vec<Session>,
}

cagents::stamp_version!();

fn owned(path: &Path) -> Result<fs::Metadata, Box<dyn Error>> {
    let metadata = fs::symlink_metadata(path)?;
    // SAFETY: geteuid has no arguments or memory preconditions.
    if metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
        || metadata.file_type().is_symlink()
    {
        return Err("fixture path must be owned, not writable by others, and not a symlink".into());
    }
    Ok(metadata)
}

fn fixture_guard(target: &Target) -> Result<(), Box<dyn Error>> {
    // Never turn inherited session authority into user authority.
    if std::env::var_os("CAGENTS_AGENT_AUTHORITY").is_some() {
        return Err("run from a genuine user native-evidence env, not an agent session".into());
    }
    let task = target.task.canonicalize()?;
    if target.task != task {
        return Err("task must use its absolute canonical path".into());
    }
    if !owned(&task)?.is_dir() || !owned(&task.join(".native-evidence"))?.is_file() {
        return Err("expected a native-evidence task".into());
    }
    let fixture = task.join("fixture").canonicalize()?;
    let environment_fixture = PathBuf::from(
        std::env::var_os("CAGENTS_EVIDENCE_FIXTURE").ok_or("missing fixture environment")?,
    )
    .canonicalize()?;
    let home =
        PathBuf::from(std::env::var_os("HOME").ok_or("missing fixture HOME")?).canonicalize()?;
    let runtime =
        PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").ok_or("missing fixture runtime")?)
            .canonicalize()?;
    if environment_fixture != fixture
        || home != fixture.join("home").canonicalize()?
        || !runtime.starts_with(&task)
        || runtime == task
    {
        return Err("environment does not belong to this native-evidence task".into());
    }
    owned(&home)?;
    owned(&runtime)?;
    let socket = target.socket.canonicalize()?;
    if !socket.starts_with(&runtime) || !owned(&socket)?.file_type().is_socket() {
        return Err("control socket must belong to the fixture runtime".into());
    }
    Ok(())
}

fn canonical_pane<'a>(model: &'a WorkspaceModel, target: &Target) -> Result<&'a Pane, String> {
    let mut matches = model.views.iter().flat_map(|view| {
        view.panes
            .iter()
            .filter(move |pane| pane.id == target.pane)
            .map(move |pane| (&view.scope, pane))
    });
    let (scope, pane) = matches.next().ok_or("fixture pane is absent")?;
    if matches.next().is_some() || scope != &target.scope {
        return Err("fixture pane is ambiguous or moved".into());
    }
    Ok(pane)
}

fn selected_pane<'a>(model: &'a WorkspaceModel, target: &Target) -> Result<&'a Pane, String> {
    let pane = canonical_pane(model, target)?;
    let CardKind::Chat(source) = &pane.kind else {
        return Err("fixture pane is not a native chat".into());
    };
    validate_session(source, target)?;
    if model.agents.iter().any(|agent| {
        matches!(&agent.session, Some(HostedSession::Chat(session)) if session.id == target.native)
    }) {
        return Err("fixture chat must not belong to a managed agent".into());
    }
    Ok(pane)
}

// Observation does not authorize preparation or close. Fresh can legitimately
// replace the native ID, thread and listener descriptor of this same card.
fn observation(
    model: &WorkspaceModel,
    target: Target,
    inventory: Vec<Session>,
) -> Result<Observation, String> {
    let pane = canonical_pane(model, &target)?;
    let CardKind::Chat(source) = &pane.kind else {
        return Err("fixture pane is not a native chat".into());
    };
    if source.launch.provider != Provider::Codex {
        return Err("fixture pane is not Codex".into());
    }
    Ok(Observation {
        actual_native: source.id,
        matching_inventory: inventory
            .into_iter()
            .filter(|session| session.id == source.id)
            .collect(),
        target,
        model_identity: model.identity,
        pane: pane.clone(),
    })
}

fn validate_session(session: &Session, target: &Target) -> Result<(), String> {
    if session.id != target.native
        || session.launch.provider != Provider::Codex
        || session.launch.session_id.as_deref() != Some(target.thread.as_str())
        || session.terminal.is_some()
        || session.owned_codex_listener.is_some()
        || session.native_continuity != NativeContinuity::ReconnectOnly
    {
        return Err("expected the exact legacy native Codex conversation".into());
    }
    Ok(())
}

fn fixture_repository(target: &Target) -> Result<PathBuf, String> {
    let fixture = target
        .task
        .join("fixture")
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let project = target
        .task
        .join("fixture/project")
        .canonicalize()
        .map_err(|error| error.to_string())?;
    if !project.starts_with(&fixture) || project == fixture {
        return Err("project is outside the private fixture".into());
    }
    let location = cagents::project_location::discover(&project)?;
    if location.kind != cagents::model::ProjectKind::Git || !location.key.starts_with(&fixture) {
        return Err("expected the private fixture's Git repository".into());
    }
    Ok(location.key)
}

fn publish_revision(
    model: &WorkspaceModel,
    target: &Target,
    repository: &Path,
) -> Result<u64, String> {
    let Scope::Channel(id) = &target.scope else {
        return Err("publish requires a new ordinary fixture channel".into());
    };
    let channel = model.channel(*id).ok_or("fixture channel is absent")?;
    if channel.repository != repository {
        return Err("channel belongs to another repository".into());
    }
    let identity_conflict = model.views.iter().any(|view| {
        view.retired.contains(&target.pane) || view.panes.iter().any(|pane| {
            pane.id == target.pane
                || matches!(&pane.kind, CardKind::Chat(source) if source.id == target.native)
                || matches!(&pane.kind, CardKind::Terminal(source) if source.id == target.native)
        })
    });
    if !model.agents.is_empty() || identity_conflict {
        return Err("fixture has an agent or a conflicting pane/resource identity".into());
    }
    let mut views = model.views.iter().filter(|view| view.scope == target.scope);
    let Some(view) = views.next() else {
        return Ok(0);
    };
    if views.next().is_some() || !view.panes.is_empty() {
        return Err("publish requires one empty fixture view".into());
    }
    Ok(view.revision)
}

fn legacy_pane(target: &Target) -> Pane {
    Pane {
        id: target.pane,
        title: "Codex Fresh fixture".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 1.0,
        kind: CardKind::Chat(Session {
            id: target.native,
            launch: Launch {
                provider: Provider::Codex,
                recipe: LaunchRecipe {
                    program: target.task.join("fixture/bin/codex").display().to_string(),
                    arguments: vec![
                        "--remote".into(),
                        format!(
                            "unix://{}/old/cagents-codex-{}/control.sock",
                            target.task.display(),
                            target.native
                        ),
                        "-c".into(),
                        "model_reasoning_effort=\"high\"".into(),
                    ],
                    working_directory: target.task.join("fixture/project"),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: Some(target.thread.clone()),
            },
            proof: None,
            terminal: None,
            owned_codex_listener: None,
            native_continuity: NativeContinuity::ReconnectOnly,
        }),
        scope: None,
    }
}

fn publish(client: &mut ControlClient, target: &Target) -> Result<Pane, Box<dyn Error>> {
    Uuid::parse_str(&target.thread)?;
    let model = client.read_workspace_model()?;
    let repository = fixture_repository(target)?;
    let expected_revision = publish_revision(&model, target, &repository)?;
    if !client.list_chats()?.is_empty() {
        return Err("publish requires an empty hosted chat inventory".into());
    }
    let pane = legacy_pane(target);
    pane.validate()?;
    let model = client.edit_workspace_model(ModelEdit::ChangeView {
        scope: target.scope.clone(),
        expected_revision,
        operation: Uuid::new_v4(),
        change: Change::Insert {
            pane: pane.clone(),
            before: None,
        },
    })?;
    if selected_pane(&model, target)? != &pane {
        return Err("publish did not return the exact fixture pane".into());
    }
    Ok(pane)
}

fn snapshot(client: &mut ControlClient, target: Target) -> Result<Prepared, Box<dyn Error>> {
    let model = client.read_workspace_model()?;
    let pane = selected_pane(&model, &target)?.clone();
    let mut sessions = client
        .list_chats()?
        .into_iter()
        .filter(|session| session.id == target.native);
    let session = sessions.next().ok_or("fixture actor is not hosted")?;
    if sessions.next().is_some() {
        return Err("fixture actor inventory is ambiguous".into());
    }
    validate_session(&session, &target)?;
    if !session
        .proof
        .as_ref()
        .is_some_and(|proof| proof.is_complete())
    {
        return Err("fixture inventory did not issue a host proof".into());
    }
    let CardKind::Chat(saved) = &pane.kind else {
        unreachable!()
    };
    if saved != &session {
        return Err("saved pane and live inventory have not converged exactly".into());
    }
    if !session
        .launch
        .recipe
        .working_directory
        .canonicalize()?
        .starts_with(target.task.join("fixture").canonicalize()?)
    {
        return Err("fixture conversation is outside the fixture".into());
    }
    Ok(Prepared {
        target,
        model_identity: model.identity,
        pane,
        session,
    })
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let mode = args
        .next()
        .ok_or("usage: native-codex-close publish|observe|prepare|close REQUEST.json")?;
    let path = args.next().ok_or("missing request file")?;
    if args.next().is_some() {
        return Err("unexpected arguments".into());
    }
    let bytes = fs::read(path)?;
    if bytes.len() > 1024 * 1024 {
        return Err("fixture request exceeds 1 MiB".into());
    }
    if mode == "publish" || mode == "prepare" || mode == "observe" {
        let target: Target = serde_json::from_slice(&bytes)?;
        fixture_guard(&target)?;
        let mut client = ControlClient::connect(&target.socket)?;
        client.authenticate(Authority::default())?;
        if mode == "publish" {
            let pane = publish(&mut client, &target)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "pane_published": true, "target": target, "pane": pane,
                }))?
            );
        } else if mode == "observe" {
            let model = client.read_workspace_model()?;
            let observed = observation(&model, target, client.list_chats()?)?;
            println!("{}", serde_json::to_string_pretty(&observed)?);
        } else {
            let prepared = snapshot(&mut client, target)?;
            println!("{}", serde_json::to_string_pretty(&prepared)?);
        }
    } else if mode == "close" {
        let prepared: Prepared = serde_json::from_slice(&bytes)?;
        fixture_guard(&prepared.target)?;
        let mut client = ControlClient::connect(&prepared.target.socket)?;
        client.authenticate(Authority::default())?;
        let current = snapshot(&mut client, prepared.target.clone())?;
        if current.model_identity != prepared.model_identity
            || current.pane != prepared.pane
            || current.session != prepared.session
        {
            return Err("fixture source changed after preparation; nothing closed".into());
        }
        // The production API attaches using this unmodified issued descriptor
        // and requires ChatClosed, including provider teardown, before success.
        client.close_chat(current.session)?;
        if client
            .list_chats()?
            .iter()
            .any(|session| session.id == prepared.target.native)
        {
            return Err("closed actor remains in inventory".into());
        }
        let model = client.read_workspace_model()?;
        if model.identity != prepared.model_identity
            || selected_pane(&model, &prepared.target)? != &prepared.pane
        {
            return Err("actor closed but saved pane changed; inspect before continuing".into());
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "chat_closed_acknowledged": true,
                "inventory_absent": true,
                "saved_pane_unchanged": true,
                "prepared": prepared,
            }))?
        );
    } else {
        return Err("mode must be publish, observe, prepare or close".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (WorkspaceModel, Target) {
        let target = Target {
            task: "/fixture/task".into(),
            socket: "/fixture/task/rt/control.sock".into(),
            scope: Scope::Channel(Default::default()),
            pane: CardId::new(),
            native: TerminalId::new(),
            thread: Uuid::new_v4().to_string(),
        };
        let session = Session {
            id: target.native,
            launch: Launch {
                provider: Provider::Codex,
                recipe: LaunchRecipe {
                    program: "codex".into(),
                    arguments: vec![
                        "--remote".into(),
                        "unix:///fixture/task/old/control.sock".into(),
                    ],
                    working_directory: "/fixture/task/fixture/project".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: Some(target.thread.clone()),
            },
            proof: None,
            terminal: None,
            owned_codex_listener: None,
            native_continuity: NativeContinuity::ReconnectOnly,
        };
        let pane = Pane {
            id: target.pane,
            title: "Fixture Codex".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 1.0,
            kind: CardKind::Chat(session),
            scope: None,
        };
        let model = WorkspaceModel {
            views: vec![cagents::workspace_model::views::View {
                scope: target.scope.clone(),
                revision: 1,
                panes: vec![pane],
                retired: vec![],
            }],
            ..Default::default()
        };
        (model, target)
    }

    #[test]
    fn observe_refuses_missing_moved_ambiguous_and_non_codex_panes() {
        let (model, mut target) = fixture();
        assert!(observation(&model, target.clone(), vec![]).is_ok());
        target.pane = CardId::new();
        assert!(observation(&model, target, vec![]).is_err());
        let (model, mut target) = fixture();
        target.scope = Scope::Channel(Default::default());
        assert!(observation(&model, target, vec![]).is_err());
        let (mut model, target) = fixture();
        model.views.push(model.views[0].clone());
        assert!(observation(&model, target, vec![]).is_err());
        let (mut model, target) = fixture();
        let CardKind::Chat(session) = &mut model.views[0].panes[0].kind else {
            unreachable!()
        };
        session.launch.provider = Provider::Claude;
        assert!(observation(&model, target, vec![]).is_err());
    }

    #[test]
    fn observe_reports_current_fresh_metadata_without_authorizing_legacy_close() {
        let (mut model, target) = fixture();
        let CardKind::Chat(original) = &model.views[0].panes[0].kind else {
            unreachable!()
        };
        let old = original.clone();
        let mut fresh = old.clone();
        fresh.id = TerminalId::new();
        fresh.launch.session_id = None;
        fresh.launch.recipe.arguments = vec!["--remote".into(), "unix://".into()];
        let proof = cagents::model::HostSessionProof {
            host_session_id: cagents::model::HostSessionId::new("fixture-host"),
            boot_id: cagents::model::BootId::new("fixture-boot"),
            host_incarnation: cagents::model::HostIncarnation::new("fixture-incarnation"),
        };
        assert!(fresh.claim_owned_codex_listener(Path::new("/fixture/task/new"), &proof));
        fresh.launch.session_id = Some(Uuid::new_v4().to_string());
        fresh.proof = Some(proof);
        fresh.native_continuity = NativeContinuity::Durable {
            provider_version: "0.167.0".into(),
        };
        model.views[0].panes[0].kind = CardKind::Chat(fresh.clone());
        assert!(selected_pane(&model, &target).is_err());
        let observed =
            observation(&model, target.clone(), vec![old.clone(), fresh.clone()]).unwrap();
        assert_eq!(observed.model_identity, model.identity);
        assert_eq!(observed.pane, model.views[0].panes[0]);
        assert_eq!(observed.actual_native, fresh.id);
        assert_eq!(observed.matching_inventory, vec![fresh.clone()]);
        // No matching inventory is a fact, not an invented hosted/ready result.
        let absent = observation(&model, target.clone(), vec![old]).unwrap();
        assert!(absent.matching_inventory.is_empty());
        let mut current_target = target;
        current_target.native = fresh.id;
        current_target.thread = fresh.launch.session_id.clone().unwrap();
        assert!(selected_pane(&model, &current_target).is_err());
    }

    #[test]
    fn refuses_missing_moved_or_ambiguous_panes_before_close() {
        let (model, mut target) = fixture();
        assert!(selected_pane(&model, &target).is_ok());
        target.pane = CardId::new();
        assert!(selected_pane(&model, &target).is_err());
        let (mut model, target) = fixture();
        model.views[0].scope = Scope::Channel(Default::default());
        assert!(selected_pane(&model, &target).is_err());
        let (mut model, target) = fixture();
        model.views.push(model.views[0].clone());
        assert!(selected_pane(&model, &target).is_err());
    }

    #[test]
    fn refuses_another_native_thread_or_provider_before_close() {
        for change in 0..3 {
            let (mut model, target) = fixture();
            let CardKind::Chat(session) = &mut model.views[0].panes[0].kind else {
                unreachable!()
            };
            match change {
                0 => session.id = TerminalId::new(),
                1 => session.launch.session_id = Some(Uuid::new_v4().to_string()),
                _ => session.launch.provider = Provider::Claude,
            }
            assert!(selected_pane(&model, &target).is_err());
        }
    }

    #[test]
    fn refuses_sources_that_need_retained_owner_recovery() {
        for continuity in [
            NativeContinuity::Durable {
                provider_version: "0.167.0".into(),
            },
            NativeContinuity::RetainedClaudeActor { protocol: 1 },
        ] {
            let (mut model, target) = fixture();
            let CardKind::Chat(session) = &mut model.views[0].panes[0].kind else {
                unreachable!()
            };
            session.native_continuity = continuity;
            assert!(selected_pane(&model, &target).is_err());
        }
    }

    #[test]
    fn publish_requires_the_existing_fixture_channel_and_empty_view() {
        let (mut model, mut target) = fixture();
        assert!(
            publish_revision(&model, &target, &target.task.join("fixture/project/.git")).is_err()
        );
        let Scope::Channel(id) = target.scope else {
            unreachable!()
        };
        model.channels.push(cagents::workspace_model::ModelChannel {
            id,
            repository: target.task.join("fixture/project/.git"),
            title: "Fixture".into(),
            title_source: Default::default(),
            ordinal: 1,
        });
        // An existing target is never an overwrite capability.
        assert!(
            publish_revision(&model, &target, &target.task.join("fixture/project/.git")).is_err()
        );
        model.views[0].panes.clear();
        assert_eq!(
            publish_revision(&model, &target, &target.task.join("fixture/project/.git")).unwrap(),
            1
        );
        model.views.clear();
        assert_eq!(
            publish_revision(&model, &target, &target.task.join("fixture/project/.git")).unwrap(),
            0
        );
        model.channels[0].repository = "/other/project".into();
        assert!(
            publish_revision(&model, &target, &target.task.join("fixture/project/.git")).is_err()
        );
        target.scope = Scope::Channel(Default::default());
        assert!(
            publish_revision(&model, &target, &target.task.join("fixture/project/.git")).is_err()
        );
    }

    #[test]
    fn publish_refuses_a_resource_alias_in_another_view() {
        let (mut model, target) = fixture();
        let Scope::Channel(id) = target.scope else {
            unreachable!()
        };
        model.channels.push(cagents::workspace_model::ModelChannel {
            id,
            repository: target.task.join("fixture/project/.git"),
            title: "Fixture".into(),
            title_source: Default::default(),
            ordinal: 1,
        });
        model.views[0].scope = Scope::Channel(Default::default());
        model.views[0].panes[0].id = CardId::new();
        assert!(
            publish_revision(&model, &target, &target.task.join("fixture/project/.git")).is_err()
        );
    }

    #[test]
    fn publish_uses_the_same_git_identity_as_channel_creation() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("fixture/project");
        fs::create_dir_all(&project).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet", "--initial-branch=main"])
                .arg(&project)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .status()
                .unwrap()
                .success()
        );
        let (mut model, mut target) = fixture();
        target.task = temporary.path().to_path_buf();
        model.views.clear();
        let Scope::Channel(id) = target.scope else {
            unreachable!()
        };
        let repository = fixture_repository(&target).unwrap();
        assert_ne!(repository, project);
        model.channels.push(cagents::workspace_model::ModelChannel {
            id,
            repository: repository.clone(),
            title: "Fixture".into(),
            title_source: Default::default(),
            ordinal: 1,
        });
        assert_eq!(publish_revision(&model, &target, &repository).unwrap(), 0);
        assert!(publish_revision(&model, &target, &project).is_err());
    }
}
