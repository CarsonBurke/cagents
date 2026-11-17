//! Dispatch for a repository pane's own actions: editing, saving, review and
//! Git operations started from inside the pane.
use super::*;

impl Cagents {
    pub(super) fn update_repository(&mut self, action: RepositoryAction) -> Task<Message> {
        self.retain_repository_operation(&action);
        if matches!(
            &action,
            RepositoryAction::Save { .. } | RepositoryAction::AutoSaveDue { .. }
        ) && !self.prepare_editor_save(action.card())
        {
            if let RepositoryAction::AutoSaveDue {
                identity,
                revision,
                ticket,
            } = &action
                && let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&identity.card)
            {
                runtime.retire_auto_save_due(identity, *revision, *ticket);
            }
            return Task::none();
        }
        if matches!(
            &action,
            RepositoryAction::Clipboard {
                op: repository_widget::ClipboardOp::Copy | repository_widget::ClipboardOp::Cut,
                ..
            }
        ) {
            chat_ui::invalidate_copy_intent();
        }
        if let RepositoryAction::DiffAnnotations {
            identity,
            review_revision,
            ..
        } = &action
            && !matches!(self.cards.get(&identity.card), Some(CardRuntime::PullRequests(pane)) if pane.review.annotation_revision() == *review_revision)
        {
            return Task::none();
        }
        if self.modal_is_open() && !repository_action_allowed_while_modal(&action) {
            return Task::none();
        }
        let card = action.card();
        let find_preferences_changed = matches!(
            &action,
            RepositoryAction::FindOptionsChanged { .. }
                | RepositoryAction::PreserveCaseChanged { .. }
        );
        let completion_character = match &action {
            RepositoryAction::EditorAction {
                action:
                    repository_widget::EditorAction::Edit(repository_widget::EditorEdit::Insert(
                        character,
                    )),
                ..
            } => Some(*character),
            _ => None,
        };
        let basis_resolved = matches!(action, RepositoryAction::DiffBasisResolved { .. });
        let save_finished = matches!(
            action,
            RepositoryAction::SaveFinished { .. } | RepositoryAction::RetainedSaveFinished { .. }
        );
        let saved = matches!(
            &action,
            RepositoryAction::SaveFinished { result: Ok(_), .. }
                | RepositoryAction::RetainedSaveFinished { result: Ok(_), .. }
        );
        // A parked source-control navigation is answered by the very
        // file list it is waiting on, and only after the card has taken
        // that list.
        let files_loaded = match &action {
            RepositoryAction::FilesLoaded { identity, .. } => Some(identity.clone()),
            _ => None,
        };
        let closing_card = self.dirty_repository_close.as_ref().map(|close| close.card);
        let task = match self.cards.get_mut(&card) {
            Some(CardRuntime::Repository(runtime)) => runtime
                .update(action)
                .map(move |action| match action {
                    RepositoryAction::ApplyDiffBasis { card, basis } if basis_resolved => {
                        Message::ResolvedDiffBasis { card, basis }
                    }
                    action => Message::Repository(action),
                })
                .map(cosmic::Action::App),
            // The pane's runtime only ever shows committed content
            // from a fetched range, so the working-tree bookkeeping
            // below (source-control reloads, dirty closes) has
            // nothing to do for it.
            Some(CardRuntime::PullRequests(pane)) => repository_task(pane.runtime.update(action)),
            _ => Task::none(),
        };
        // Every buffer transition the action caused is recorded on the
        // runtime and has so far reached nothing. Draining here rather
        // than at the individual actions is what makes that true of all
        // of them, including the ones a later action adds.
        let mut tasks = vec![task, self.drain_lsp_intents(card)];
        if find_preferences_changed {
            let preferences = match self.cards.get(&card) {
                Some(CardRuntime::Repository(runtime)) => Some(runtime.find_preferences()),
                Some(CardRuntime::PullRequests(pane)) => Some(pane.runtime.find_preferences()),
                _ => None,
            };
            if let Some((options, preserve)) = preferences {
                self.settings.editor_find_options = options;
                self.settings.editor_replace_preserve_case = preserve;
                for (target, runtime) in &mut self.cards {
                    let task = match runtime {
                        CardRuntime::Repository(runtime) => {
                            runtime.update_find_preferences(*target, options, preserve)
                        }
                        CardRuntime::PullRequests(pane) => pane
                            .runtime
                            .update_find_preferences(*target, options, preserve),
                        _ => iced::Task::none(),
                    };
                    tasks.push(repository_task(task));
                }
                tasks.push(self.save_settings_task(false));
            }
        }
        tasks.push(self.restore_repository_editor_focus());
        if let Some(character) = completion_character
            && let Some(doc) = self.lsp_document(card)
            && self.lsp.completion_trigger(doc, character)
            && let Some(version) = self.lsp.document_version(doc)
            && let Some(CardRuntime::Repository(runtime)) = self.cards.get(&card)
            && let Some(at) = runtime.cursor_position()
        {
            tasks.push(cosmic::task::future(async move {
                tokio::time::sleep(Duration::from_millis(150)).await;
                Message::Repository(RepositoryAction::Language {
                    card,
                    action: repository_widget::EditorLspAction::CompleteAt { doc, version, at },
                })
            }));
        }
        let selected_view = match self.cards.get(&card) {
            Some(CardRuntime::Repository(runtime))
                if runtime.descriptor.selected_file.is_some() =>
            {
                Some(runtime.descriptor.clone())
            }
            _ => None,
        };
        if let Some(view) = selected_view
            && let Some(state) = self.card_state_mut(card)
            && state.kind != CardKind::Repository(view.clone())
        {
            state.kind = CardKind::Repository(view);
            self.persist();
        }
        if let Some(CardRuntime::Repository(runtime)) = self.cards.get(&card) {
            let navigation = runtime.navigation();
            let stored = self.durable.repository_navigation.get(&card);
            if stored != navigation.as_ref() {
                let saved = &mut Arc::make_mut(&mut self.durable).repository_navigation;
                if let Some(navigation) = navigation {
                    saved.insert(card, navigation);
                } else {
                    saved.remove(&card);
                }
                // No view reads where an editor is.
                tasks.push(self.save_snapshot_soon());
            }
        }
        // A queued refresh discards this result and advances the list
        // generation. Keep navigation parked until its replacement
        // lands, rather than opening a file without its Git status.
        if files_loaded.as_ref().is_some_and(|identity| {
            identity.path == Path::new(".")
                && matches!(self.cards.get(&card), Some(CardRuntime::Repository(runtime)) if runtime.accepts_file_list(identity))
        }) {
            tasks.push(self.flush_pending_panel_open(card));
        }
        if saved
            && let Some(key) = self.sidebar_state.active_worktree().cloned()
            && self.card_state(card).is_some_and(|state| {
                matches!(&state.kind, CardKind::Repository(view)
                    if view.worktree == key.canonical_path)
            })
        {
            tasks.push(self.reload_source_control(&key));
        }
        if save_finished && closing_card == Some(card) {
            tasks.push(self.advance_dirty_close());
        }
        if !self.project_ui.closing.is_empty() {
            tasks.push(self.complete_closed_projects());
        }
        Task::batch(tasks)
    }
}
