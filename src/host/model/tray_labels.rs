//! Bounded human metadata for already selected resident tray rows.
use super::*;
use crate::{
    agent_execution::HostedSession,
    model::{CardId, CardKind, TerminalId},
    workspace_model::views::{ManagedDisplayIndex, Scope},
};
use std::collections::BTreeMap;

pub(in crate::host) struct TrayLabel {
    pub name: Option<String>,
    pub scope: Vec<String>,
    pub directory: PathBuf,
}

impl Registry {
    /// One coherent read, at most20 indexed session/canonical-owner lookups.
    /// No all-agent, model, transcript or inventory-wide scan belongs here.
    pub(in crate::host) fn tray_labels(
        &self,
        sources: &[(TerminalId, Vec<HostedSession>)],
    ) -> Result<BTreeMap<TerminalId, TrayLabel>, String> {
        let transaction = self
            .db
            .unchecked_transaction()
            .map_err(|error| error.to_string())?;
        let mut labels = BTreeMap::new();
        for (session, sources) in sources.iter().take(crate::tray::MAX_WAITING_AGENTS) {
            let Some(agent) = self.agent_for_session(*session)? else {
                continue;
            };
            let canonical = CardId::from_uuid(agent.id.as_uuid());
            let owner: Option<String> = self
                .db
                .query_row(
                    "SELECT scope FROM shared_view_panes WHERE id=?1",
                    [canonical.to_string()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| error.to_string())?;
            let views = owner
                .map(|owner| {
                    let scope: Scope =
                        serde_json::from_str(&owner).map_err(|error| error.to_string())?;
                    super::views::read(&self.db, &scope)
                })
                .transpose()?;
            if views
                .as_ref()
                .is_some_and(|view| !view.panes.iter().any(|pane| pane.id == canonical))
            {
                continue;
            }
            let views = views.into_iter().collect();
            // Reuse the actual canonical-pane/saved-surface resolver. A moved
            // legacy pane's current scope outranks accepted execution placement.
            let model = WorkspaceModel {
                views,
                ..Default::default()
            };
            let index = ManagedDisplayIndex::new(&model);
            let Some(display) = index.activation_display(&agent, *session) else {
                continue;
            };
            if !sources
                .iter()
                .any(|source| source.same_process(&display.session))
            {
                continue;
            }
            let (repository, section, directory): (PathBuf, Option<String>, PathBuf) =
                match &display.display.scope {
                    Scope::Channel(channel) => {
                        let Some((repository, title)) = self
                            .db
                            .query_row(
                                "SELECT repository,title FROM channels WHERE id=?1",
                                [channel.to_string()],
                                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                            )
                            .optional()
                            .map_err(|error| error.to_string())?
                        else {
                            continue;
                        };
                        (repository.clone().into(), Some(title), repository.into())
                    }
                    Scope::Worktree { repository, path } => (
                        repository.clone(),
                        path.file_name()
                            .map(|part| part.to_string_lossy().into_owned()),
                        path.clone(),
                    ),
                };
            let Some(project) = self
                .db
                .query_row(
                    "SELECT name FROM projects WHERE repository=?1",
                    [repository.to_string_lossy().as_ref()],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|error| error.to_string())?
            else {
                continue;
            };
            let name = display
                .display
                .pane
                .as_ref()
                .and_then(|pane| pane.user_title.clone())
                .filter(|title| !title.trim().is_empty())
                .or(agent.name);
            let mut scope = vec![project];
            if let Some(section) = section.filter(|section| !section.trim().is_empty()) {
                scope.push(section);
            }
            labels.insert(
                *session,
                TrayLabel {
                    name,
                    scope,
                    directory,
                },
            );
        }
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(labels)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::Provider,
        agent_execution::{Agent, Origin, Placement, Spec, Surface, Target},
        chat::runtime::Handle,
        host::{
            chat::Record,
            state::{HostLock, HostState, SharedHost},
        },
        model::{AgentId, BootId, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe},
    };
    use std::sync::Arc;

    fn fixture() -> (
        tempfile::TempDir,
        SharedHost,
        Vec<Agent>,
        ChannelId,
        ChannelId,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let mut registry = Registry::at(&directory.path().join("tray.sqlite3")).unwrap();
        registry
            .apply(&ModelEdit::OpenProject {
                repository: "/repo/project".into(),
                name: "Workspace".into(),
                kind: Default::default(),
            })
            .unwrap();
        let backend = ChannelId::new();
        let frontend = ChannelId::new();
        for (id, title) in [(backend, "Backend"), (frontend, "Frontend")] {
            registry
                .apply(&ModelEdit::CreateChannel {
                    id,
                    repository: "/repo/project".into(),
                    title: title.into(),
                })
                .unwrap();
        }
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("tray-metadata-host"),
            boot_id: BootId::new("tray-metadata-boot"),
            host_incarnation: HostIncarnation::new("tray-metadata-incarnation"),
        };
        let mut state = HostState::new(proof.clone());
        let mut agents = Vec::new();
        for (channel, name) in [
            (backend, None),
            (frontend, None),
            (backend, Some("Release checks")),
        ] {
            let (handle, _) = Handle::fixture_remote_attachment_pair();
            let actor = handle.actor_identity().unwrap();
            let session = crate::chat::Session {
                id: actor.record.terminal,
                proof: Some(proof.clone()),
                terminal: None,
                native_continuity: Default::default(),
                owned_codex_listener: None,
                launch: crate::chat::Launch {
                    provider: Provider::Claude,
                    session_id: None,
                    recipe: LaunchRecipe {
                        program: "claude".into(),
                        arguments: vec![],
                        working_directory: "/repo/project".into(),
                        environment: BTreeMap::from([(
                            "DO_NOT_DISPLAY".into(),
                            "private-recipe-value".into(),
                        )]),
                        unset_environment: Default::default(),
                    },
                },
            };
            let agent = Agent {
                id: AgentId::new(),
                name: name.map(str::to_owned),
                origin: Origin::User,
                spec: Spec {
                    provider: Provider::Claude,
                    account: None,
                    target: Target {
                        repository: "/repo/project".into(),
                        directory: "/repo/project".into(),
                        placement: Some(Placement::Channel(channel)),
                    },
                    surface: Surface::Chat,
                    model: None,
                    permission_mode: None,
                    reasoning_effort: None,
                },
                placement: Placement::Channel(channel),
                session: Some(HostedSession::Chat(session.clone())),
                console: None,
                created_ms: 0,
                closed: false,
            };
            registry
                .db
                .execute(
                    "INSERT INTO agents(id,origin,record,session_id,closed) VALUES(?1,?2,?3,?4,0)",
                    params![
                        agent.id.to_string(),
                        serde_json::to_string(&agent.origin).unwrap(),
                        serde_json::to_string(&agent).unwrap(),
                        session.id.to_string()
                    ],
                )
                .unwrap();
            state.chats.insert(
                session.id,
                Record {
                    execution_instance: actor.instance,
                    console_recipe: None,
                    paired_terminal: None,
                    session,
                    handle,
                    source_process: None,
                    subscribers: Default::default(),
                    observations: Default::default(),
                    completion_observed: false,
                    returning_terminal: false,
                },
            );
            agents.push(agent);
        }
        state.install_model(registry);
        (
            directory,
            Arc::new(HostLock::new(state)),
            agents,
            backend,
            frontend,
        )
    }

    fn menu(host: &SharedHost) -> BTreeMap<TerminalId, String> {
        let snapshot = crate::host::tray_attention::waiting_snapshot_for_test(host);
        assert_eq!(snapshot.tally.waiting, 3);
        let rows = crate::tray::waiting_menu_for_test(snapshot)
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        for label in rows.values() {
            assert!(!label.contains("DO_NOT_DISPLAY"));
            assert!(!label.contains("private-recipe-value"));
        }
        rows
    }

    #[test]
    fn resident_menu_uses_real_named_channels_and_agent_names_for_identical_paths_and_titles() {
        let (_directory, host, agents, _, _) = fixture();
        let rows = menu(&host);
        assert_eq!(
            rows[&agents[0].session.as_ref().unwrap().id()],
            "Claude Code · Review — Workspace/Backend"
        );
        assert_eq!(
            rows[&agents[1].session.as_ref().unwrap().id()],
            "Claude Code · Review — Workspace/Frontend"
        );
        assert_eq!(
            rows[&agents[2].session.as_ref().unwrap().id()],
            "Claude Code · Release checks — Workspace/Backend"
        );
        assert!(rows.values().all(|label| !label.contains(" · Chat ")));
    }

    #[test]
    fn identical_human_scope_names_use_actual_display_paths_instead_of_accepted_directory() {
        let (_directory, host, agents, _, _) = fixture();
        let registry = host.lock().unwrap().execution_registry.clone().unwrap();
        {
            let mut registry = registry.lock().unwrap();
            for (agent, repository) in [
                (&agents[0], "/left/project"),
                (&agents[1], "/right/project"),
            ] {
                registry
                    .apply(&ModelEdit::OpenProject {
                        repository: repository.into(),
                        name: "Project".into(),
                        kind: Default::default(),
                    })
                    .unwrap();
                let channel = ChannelId::new();
                registry
                    .apply(&ModelEdit::CreateChannel {
                        id: channel,
                        repository: repository.into(),
                        title: "Review".into(),
                    })
                    .unwrap();
                let scope = serde_json::to_string(&Scope::Channel(channel)).unwrap();
                let Some(HostedSession::Chat(chat)) = &agent.session else {
                    unreachable!()
                };
                let pane = crate::workspace_model::views::Pane {
                    id: CardId::from_uuid(agent.id.as_uuid()),
                    title: "Review".into(),
                    user_title: None,
                    pending_terminal_start: false,
                    width_fraction: 1.0,
                    kind: CardKind::Chat(chat.clone()),
                    scope: None,
                };
                registry
                    .db
                    .execute(
                        "INSERT INTO shared_view_scopes(scope,channel) VALUES(?1,?2)",
                        params![scope, channel.to_string()],
                    )
                    .unwrap();
                registry.db.execute("INSERT INTO shared_views(scope,revision,panes,retired) VALUES(?1,1,?2,'[]')", params![scope,serde_json::to_string(&vec![pane.clone()]).unwrap()]).unwrap();
                registry
                    .db
                    .execute(
                        "INSERT INTO shared_view_panes(id,scope) VALUES(?1,?2)",
                        params![pane.id.to_string(), scope],
                    )
                    .unwrap();
            }
        }
        let rows = menu(&host);
        assert_eq!(
            rows[&agents[0].session.as_ref().unwrap().id()],
            "Claude Code · Review — Project/Review/left/project"
        );
        assert_eq!(
            rows[&agents[1].session.as_ref().unwrap().id()],
            "Claude Code · Review — Project/Review/right/project"
        );
        assert!(rows.values().all(|label| !label.contains(" · Chat ")));
    }

    #[test]
    fn resident_menu_follows_saved_canonical_scope_and_user_title_after_legacy_move_and_rename() {
        let (_directory, host, agents, _, _) = fixture();
        let registry = host.lock().unwrap().execution_registry.clone().unwrap();
        let moved = ChannelId::new();
        let source = agents[0].session.as_ref().unwrap();
        {
            let mut registry = registry.lock().unwrap();
            registry
                .apply(&ModelEdit::OpenProject {
                    repository: "/other/project".into(),
                    name: "Other workspace".into(),
                    kind: Default::default(),
                })
                .unwrap();
            registry
                .apply(&ModelEdit::CreateChannel {
                    id: moved,
                    repository: "/other/project".into(),
                    title: "Moved channel".into(),
                })
                .unwrap();
            // Published legacy cross-project display ownership is intentionally
            // separate from unchanged accepted execution placement. Use the real
            // stored view format, then resolve through the actual shared helper.
            let scope = Scope::Channel(moved);
            let encoded = serde_json::to_string(&scope).unwrap();
            let HostedSession::Chat(chat) = source else {
                unreachable!()
            };
            let pane = crate::workspace_model::views::Pane {
                id: CardId::from_uuid(agents[0].id.as_uuid()),
                title: "Review".into(),
                user_title: Some("Actual pane task".into()),
                pending_terminal_start: false,
                width_fraction: 1.0,
                kind: CardKind::Chat(chat.clone()),
                scope: None,
            };
            registry
                .db
                .execute(
                    "INSERT INTO shared_view_scopes(scope,channel) VALUES(?1,?2)",
                    params![encoded, moved.to_string()],
                )
                .unwrap();
            registry
                .db
                .execute(
                    "INSERT INTO shared_views(scope,revision,panes,retired) VALUES(?1,1,?2,'[]')",
                    params![encoded, serde_json::to_string(&vec![pane.clone()]).unwrap()],
                )
                .unwrap();
            registry
                .db
                .execute(
                    "INSERT INTO shared_view_panes(id,scope) VALUES(?1,?2)",
                    params![pane.id.to_string(), encoded],
                )
                .unwrap();
            let model = registry.model().unwrap();
            let resolved = ManagedDisplayIndex::new(&model)
                .activation_display(&agents[0], source.id())
                .unwrap();
            assert_eq!(resolved.display.scope, scope);
            assert_eq!(resolved.session, *source);
            assert_ne!(agents[0].placement, scope);
        }
        assert_eq!(
            menu(&host)[&source.id()],
            "Claude Code · Actual pane task — Other workspace/Moved channel"
        );
        {
            let mut registry = registry.lock().unwrap();
            registry
                .apply(&ModelEdit::RetitleChannel {
                    id: moved,
                    title: "Renamed channel".into(),
                })
                .unwrap();
            registry
                .apply(&ModelEdit::OpenProject {
                    repository: "/other/project".into(),
                    name: "Renamed workspace".into(),
                    kind: Default::default(),
                })
                .unwrap();
        }
        assert_eq!(
            menu(&host)[&source.id()],
            "Claude Code · Actual pane task — Renamed workspace/Renamed channel"
        );
        // A same-ID model successor cannot lend its renamed scope to this old
        // resident. The ordinary path/title fallback stays usable and truthful.
        {
            let registry = registry.lock().unwrap();
            let mut successor = registry.agent(agents[0].id).unwrap();
            let Some(HostedSession::Chat(chat)) = &mut successor.session else {
                unreachable!()
            };
            chat.launch
                .recipe
                .arguments
                .push("different-process".into());
            registry
                .db
                .execute(
                    "UPDATE agents SET record=?2 WHERE id=?1",
                    params![
                        successor.id.to_string(),
                        serde_json::to_string(&successor).unwrap()
                    ],
                )
                .unwrap();
        }
        assert_eq!(menu(&host)[&source.id()], "Claude Code · Review — project");
    }
}
