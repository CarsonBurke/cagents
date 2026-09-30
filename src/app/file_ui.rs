use super::*;
use crate::document_link::{self, DocumentLink};

impl Cagents {
    /// All file links share the editor path, independently of web-link preferences.
    pub(super) fn open_document_link(
        &mut self,
        source: Option<CardId>,
        target: &str,
    ) -> Task<Message> {
        if source.is_some_and(|card| self.workspace.card(card).is_none()) {
            return Task::none();
        }
        let document = source.and_then(|card| match self.cards.get(&card) {
            Some(CardRuntime::Repository(runtime)) => runtime
                .selected_path()
                .map(|path| runtime.descriptor.worktree.join(path)),
            Some(CardRuntime::Chat(runtime)) => Some(
                runtime
                    .session
                    .launch
                    .recipe
                    .working_directory
                    .join(".chat"),
            ),
            _ => None,
        });
        // Chat paths use the conversation's working directory; document
        // paths use the open file's directory. Explicit file URLs work in both.
        let (target, position) = document_link::file_position(target);
        let resolved = if document.is_none()
            && !Url::parse(target)
                .is_ok_and(|url| matches!(url.scheme(), "http" | "https" | "file"))
        {
            Err("This link needs an absolute HTTP, HTTPS, or local file URL".into())
        } else {
            document_link::resolve(document.as_deref().unwrap_or(Path::new("/")), target)
        };
        match resolved {
            Ok(DocumentLink::Local { path, fragment }) => {
                self.open_local_file(source, path, position.or(fragment))
            }
            Ok(DocumentLink::External(url)) => self.open_clicked_link(url),
            Err(error) => {
                self.status = Some(TransientNotice::error(error));
                Task::none()
            }
        }
    }

    fn open_local_file(
        &mut self,
        source: Option<CardId>,
        path: PathBuf,
        fragment: Option<String>,
    ) -> Task<Message> {
        if self.workspace_inputs_blocked() || self.refuse_in_deleting_worktree(&path) {
            return Task::none();
        }
        let Some((directory, name)) = path.parent().zip(path.file_name()) else {
            self.status = Some(TransientNotice::error(
                "The link does not name a local file",
            ));
            return Task::none();
        };
        let source = source.or(self.workspace.focused());
        let channel = self
            .durable
            .channels
            .iter()
            .find(|channel| source.is_some_and(|source| channel.cards.contains(&source)))
            .map(|channel| (channel.id, channel.project_id));
        let scope = if channel.is_some() {
            None
        } else {
            source
                .and_then(|id| self.card_state(id))
                .and_then(|card| self.card_worktree(card))
                .map(Path::to_path_buf)
                .or_else(|| self.active_workspace.clone())
        };
        let project = channel
            .map(|(_, project)| project)
            .or_else(|| {
                source
                    .and_then(|id| self.card_state(id))
                    .and_then(|card| match &card.kind {
                        CardKind::Repository(view) => Some(view.project),
                        _ => None,
                    })
            })
            .or_else(|| {
                scope.as_deref().and_then(|scope| {
                    self.sidebar_model
                        .worktree_at(scope)
                        .map(|key| key.project_id)
                        .or_else(|| {
                            self.sidebar_model
                                .projects
                                .iter()
                                .find(|project| project.repository == scope)
                                .map(|project| project.id)
                        })
                })
            })
            .unwrap_or_else(ProjectId::new);
        // Prefer the exact file, even when dirty. Only dedicated, clean file
        // panes are candidates for replacement; repository browsing is separate.
        let candidates = self.workspace.order().filter_map(|card| {
            // A newly focused editor may still be entering the viewport. Its
            // reveal animation must not turn a rapid second link into another
            // pane; unrelated offscreen panes remain ineligible.
            if self.workspace.focused() != Some(card)
                && self.workspace.visibility(card) != Some(VisibilityClass::Visible)
            {
                return None;
            }
            let owner = self
                .durable
                .channels
                .iter()
                .find(|channel| channel.cards.contains(&card));
            if owner.map(|channel| channel.id) != channel.map(|(id, _)| id)
                || (channel.is_none()
                    && self
                        .card_state(card)
                        .and_then(|state| self.card_worktree(state))
                        != scope.as_deref())
            {
                return None;
            }
            let CardRuntime::Repository(runtime) = self.cards.get(&card)? else {
                return None;
            };
            let exact = runtime
                .selected_path()
                .is_some_and(|selected| runtime.descriptor.worktree.join(selected) == path);
            (exact || (runtime.descriptor.selected_file.is_some() && !runtime.blocks_navigation()))
                .then_some((card, exact))
        });
        let candidate = candidates.max_by_key(|(_, exact)| *exact);
        if let Some((card, true)) = candidate {
            let jump = match (self.cards.get_mut(&card), fragment) {
                (Some(CardRuntime::Repository(runtime)), Some(fragment)) => {
                    repository_task(runtime.preview_fragment(card, &fragment))
                }
                _ => Task::none(),
            };
            let update = self
                .workspace
                .focus(card)
                .unwrap_or_else(|_| ViewportUpdate::unchanged());
            return Task::batch([jump, self.apply_viewport(update, true, false)]);
        }
        let view = RepositoryView {
            project,
            repository: directory.to_path_buf(),
            worktree: directory.to_path_buf(),
            selected_file: Some(name.into()),
        };
        let card = candidate.map_or_else(CardId::new, |(card, _)| card);
        let title = normalize_card_name(&name.to_string_lossy()).unwrap_or_else(|| "File".into());
        self.retire_lsp_card(card);
        if candidate.is_none() {
            let mut runtime = RepositoryRuntime::new_directory(view.clone());
            runtime.set_auto_save(self.settings.editor_auto_save);
            runtime.set_wrap_lines(self.settings.editor_wrap_lines);
            runtime.set_syntax_theme(self.settings.editor_syntax_theme);
            runtime.set_external_editors(self.sidebar_model.external_editors.clone());
            self.cards
                .insert(card, CardRuntime::Repository(Box::new(runtime)));
        }
        let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&card) else {
            return Task::none();
        };
        let load = repository_task(runtime.retarget_file(card, view.clone()));
        let jump = fragment.map_or_else(Task::none, |fragment| {
            repository_task(runtime.preview_fragment(card, &fragment))
        });
        let update = if candidate.is_some() {
            if let Some(state) = self.card_state_mut(card) {
                state.title = title.clone();
                state.kind = CardKind::Repository(view);
            }
            let _ = self.workspace.rename_card(card, title);
            self.workspace.focus(card)
        } else {
            let (index, durable_index) = source
                .and_then(|source| {
                    Some((
                        self.workspace.order().position(|id| id == source)? + 1,
                        self.durable
                            .cards
                            .iter()
                            .position(|state| state.id == source)?
                            + 1,
                    ))
                })
                .unwrap_or_else(|| self.terminal_insertion_indices());
            let width = self.settings.default_native_pane_fraction;
            let state = CardState {
                id: card,
                generation: self.durable.generation,
                title: title.clone(),
                width_fraction: width,
                existence: Existence::Open,
                exposure: Exposure::Visible,
                residency: Residency::Hot,
                kind: CardKind::Repository(view),
            };
            let durable = Arc::make_mut(&mut self.durable);
            durable.cards.insert(durable_index, state);
            if let Some(scope) = scope {
                durable.card_scopes.insert(card, scope.clone());
                self.worktree_owners.insert(card, scope);
            }
            if let Some((owner, _)) = channel
                && let Some(channel) = Arc::make_mut(&mut self.durable)
                    .channels
                    .iter_mut()
                    .find(|channel| channel.id == owner)
            {
                channel.cards.push(card);
                channel.focused = Some(card);
            }
            self.workspace.add_card(
                WorkspaceCard::new(card, title, CardWidth::Fraction(width))
                    .expect("file pane is valid"),
                index,
                true,
            )
        }
        .unwrap_or_else(|_| ViewportUpdate::unchanged());
        Task::batch([load, jump, self.apply_viewport(update, true, true)])
    }

    pub(super) fn preview_local_file(&mut self, card: CardId) -> Task<Message> {
        let Some(CardRuntime::Repository(runtime)) = self.cards.get(&card) else {
            return Task::none();
        };
        let Some(path) = runtime.selected_path() else {
            return Task::none();
        };
        if runtime.blocks_navigation() {
            self.status = Some(TransientNotice::warning(
                "Save the file before previewing it",
            ));
            return Task::none();
        }
        let Ok(url) = Url::from_file_path(runtime.descriptor.worktree.join(path)) else {
            return Task::none();
        };
        // Preview is explicit; bypass the normal file-link route back to source.
        self.add_browser(url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::futures::StreamExt;

    fn app() -> Cagents {
        let mut app = super::super::tests::context_inspector_test_app(true);
        app.context_inspector_open = false;
        app.settings.editor_auto_save = false;
        app.settings.link_open_destination = LinkOpenDestination::SystemDefault;
        app.workspace.resize_viewport(1200.0);
        app
    }

    async fn settle(app: &mut Cagents, task: Task<Message>) {
        let mut pending = vec![task];
        while let Some(task) = pending.pop() {
            if let Some(mut stream) = iced::runtime::task::into_stream(task) {
                while let Some(action) = stream.next().await {
                    if let iced::runtime::Action::Output(cosmic::Action::App(
                        message @ Message::Repository(_),
                    )) = action
                    {
                        pending.push(app.update(message));
                    }
                }
            }
        }
    }

    fn runtime(app: &Cagents, card: CardId) -> &RepositoryRuntime {
        let CardRuntime::Repository(runtime) = &app.cards[&card] else {
            panic!("expected editor")
        };
        runtime
    }

    #[tokio::test]
    async fn file_links_open_source_beside_the_terminal_reuse_and_restore_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("report one.html");
        let second = directory.path().join("next.txt");
        std::fs::write(&first, "<h1>Local result</h1>").unwrap();
        std::fs::write(&second, "second file").unwrap();
        let mut app = app();
        let terminal = app.workspace.focused().unwrap();
        let scope = PathBuf::from("/source-worktree");
        Arc::make_mut(&mut app.durable)
            .card_scopes
            .insert(terminal, scope.clone());
        app.worktree_owners.insert(terminal, scope.clone());
        let task = app.update(Message::OpenLink(
            terminal,
            Url::from_file_path(&first).unwrap(),
            LinkOpenGesture::ControlClick,
        ));
        settle(&mut app, task).await;
        let editor = app.workspace.focused().unwrap();
        assert_ne!(editor, terminal);
        assert_eq!(
            app.workspace.order().collect::<Vec<_>>(),
            vec![terminal, editor]
        );
        assert_eq!(
            runtime(&app, editor).editor_text().as_deref(),
            Some("<h1>Local result</h1>")
        );
        assert!(runtime(&app, editor).is_directory_only());
        assert_eq!(app.durable.card_scopes.get(&editor), Some(&scope));
        assert_eq!(
            app.card_state(editor).unwrap().width_fraction,
            app.settings.default_native_pane_fraction
        );
        assert_ne!(
            app.workspace.visibility(editor),
            Some(VisibilityClass::Visible),
            "exercise reuse before the focused editor's reveal animation finishes"
        );

        let task = app.update(Message::OpenDocumentLink(
            terminal,
            Url::from_file_path(&second).unwrap().into(),
        ));
        settle(&mut app, task).await;
        assert_eq!(app.workspace.focused(), Some(editor));
        assert_eq!(app.workspace.cards().len(), 2);
        assert_eq!(
            runtime(&app, editor).editor_text().as_deref(),
            Some("second file")
        );

        let saved: WorkspaceState =
            serde_json::from_slice(&serde_json::to_vec(&*app.durable).unwrap()).unwrap();
        let CardKind::Repository(view) = saved
            .cards
            .iter()
            .find(|state| state.id == editor)
            .unwrap()
            .kind
            .clone()
        else {
            panic!()
        };
        assert_eq!(view.selected_file, Some(PathBuf::from("next.txt")));
        let mut restored = RepositoryRuntime::new(view);
        let task = restored.initial_load(editor);
        app.cards
            .insert(editor, CardRuntime::Repository(Box::new(restored)));
        settle(&mut app, repository_task(task)).await;
        assert_eq!(
            runtime(&app, editor).editor_text().as_deref(),
            Some("second file")
        );
    }

    #[tokio::test]
    async fn dirty_file_links_preserve_buffers_and_channel_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.md");
        let second = directory.path().join("second.txt");
        std::fs::write(&first, "first").unwrap();
        std::fs::write(&second, "second").unwrap();
        let mut app = app();
        let terminal = app.workspace.focused().unwrap();
        let channel = crate::model::ChannelId::new();
        let durable = Arc::make_mut(&mut app.durable);
        durable.channels.push(crate::model::ProjectChannel {
            title_source: Default::default(),
            id: channel,
            project_id: ProjectId::new(),
            title: "Results".into(),
            ordinal: 0,
            shared: false,
            cards: vec![terminal],
            focused: Some(terminal),
        });
        durable.active_channel = Some(channel);
        let task = app.open_document_link(
            Some(terminal),
            Url::from_file_path(&first).unwrap().as_str(),
        );
        settle(&mut app, task).await;
        let first_card = app.workspace.focused().unwrap();
        drop(
            app.update(Message::Repository(RepositoryAction::EditorAction {
                card: first_card,
                action: repository_widget::EditorAction::Edit(
                    repository_widget::EditorEdit::Insert('!'),
                ),
            })),
        );
        assert!(runtime(&app, first_card).is_dirty());
        let dirty = runtime(&app, first_card).editor_text();
        let task = app.open_document_link(
            Some(terminal),
            Url::from_file_path(&first).unwrap().as_str(),
        );
        settle(&mut app, task).await;
        assert_eq!(app.workspace.focused(), Some(first_card));
        assert_eq!(runtime(&app, first_card).editor_text(), dirty);
        let task = app.open_document_link(
            Some(terminal),
            Url::from_file_path(&second).unwrap().as_str(),
        );
        settle(&mut app, task).await;
        let second_card = app.workspace.focused().unwrap();
        assert_ne!(first_card, second_card);
        assert_eq!(runtime(&app, first_card).editor_text(), dirty);
        assert_eq!(std::fs::read_to_string(first).unwrap(), "first");
        let channel = &app.durable.channels[0];
        assert!(channel.cards.contains(&first_card) && channel.cards.contains(&second_card));
    }

    #[tokio::test]
    async fn local_document_links_resolve_relative_paths_but_chat_rejects_remote_files() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.md");
        std::fs::write(&first, "[next](next%20file.md#result)").unwrap();
        std::fs::write(directory.path().join("next file.md"), "# Result\nEvidence").unwrap();
        let mut app = app();
        let terminal = app.workspace.focused().unwrap();
        let task =
            app.open_document_link(Some(terminal), Url::from_file_path(first).unwrap().as_str());
        settle(&mut app, task).await;
        let editor = app.workspace.focused().unwrap();
        let task = app.update(Message::Repository(RepositoryAction::MarkdownLink {
            card: editor,
            uri: "next%20file.md#result".into(),
        }));
        settle(&mut app, task).await;
        assert_eq!(
            runtime(&app, editor).editor_text().as_deref(),
            Some("# Result\nEvidence")
        );
        assert_eq!(
            runtime(&app, editor).inspector_context().markdown_mode,
            Some("Preview")
        );
        for link in [
            "file://remote/etc/passwd",
            "javascript:alert(1)",
            "relative.txt",
        ] {
            let task = app.update(Message::OpenDocumentLink(terminal, link.into()));
            settle(&mut app, task).await;
            assert_eq!(app.workspace.cards().len(), 2);
            assert_eq!(
                runtime(&app, editor).editor_text().as_deref(),
                Some("# Result\nEvidence")
            );
            assert!(!app.notices.entries.is_empty());
        }
    }

    #[tokio::test]
    async fn source_ownership_beats_sidebar_selection_and_other_visible_file_scopes() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("one.txt");
        let second = directory.path().join("two.txt");
        std::fs::write(&first, "one").unwrap();
        std::fs::write(&second, "two").unwrap();
        let mut app = app();
        app.workspace.resize_viewport(1600.0);
        let source_a = app.workspace.focused().unwrap();
        app.workspace
            .set_card_width(source_a, CardWidth::Fixed(180.0))
            .unwrap();
        let [project_a, project_b] = [ProjectId::new(), ProjectId::new()];
        let [scope_a, scope_b] = [PathBuf::from("/scope-a"), PathBuf::from("/scope-b")];
        for (project, scope) in [(project_a, &scope_a), (project_b, &scope_b)] {
            app.sidebar_model.projects.push(SidebarProject::new(
                project,
                RepositoryId::new(),
                "Project".into(),
                scope.clone(),
                vec![SidebarWorktree::from_durable(
                    crate::model::WorktreeId::new(),
                    scope.clone(),
                    None,
                )],
                Vec::new(),
            ));
        }
        Arc::make_mut(&mut app.durable)
            .card_scopes
            .insert(source_a, scope_a.clone());
        app.worktree_owners.insert(source_a, scope_a.clone());
        drop(app.add_repository_card(project_b, scope_b.clone(), scope_b.clone()));
        let browser = app.workspace.focused().unwrap();
        app.workspace
            .set_card_width(browser, CardWidth::Fixed(180.0))
            .unwrap();
        let key = app.sidebar_model.worktree_at(&scope_b).unwrap();
        app.sidebar_state.apply(&SidebarAction::OpenWorktree {
            project_id: key.project_id,
            worktree_id: key.worktree_id,
            path: scope_b.clone(),
        });
        let task =
            app.open_document_link(Some(source_a), Url::from_file_path(first).unwrap().as_str());
        settle(&mut app, task).await;
        let file_a = app.workspace.focused().unwrap();
        assert_ne!(file_a, browser);
        assert!(runtime(&app, browser).selected_path().is_none());
        assert_eq!(runtime(&app, file_a).descriptor.project, project_a);
        assert_eq!(app.durable.card_scopes.get(&file_a), Some(&scope_a));
        assert_eq!(
            app.workspace.visibility(file_a),
            Some(VisibilityClass::Visible)
        );
        let task =
            app.open_document_link(Some(browser), Url::from_file_path(second).unwrap().as_str());
        settle(&mut app, task).await;
        let file_b = app.workspace.focused().unwrap();
        assert_ne!(
            file_a, file_b,
            "a file pane owned by another scope cannot be repurposed"
        );
        assert_eq!(runtime(&app, file_a).editor_text().as_deref(), Some("one"));
        assert_eq!(runtime(&app, file_b).editor_text().as_deref(), Some("two"));
        assert_eq!(runtime(&app, file_b).descriptor.project, project_b);
        assert_eq!(app.durable.card_scopes.get(&file_b), Some(&scope_b));
    }
}
