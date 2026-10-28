use super::*;
use crate::editor_features::{self, Admission};

impl Cagents {
    fn feature_admission(&self) -> Admission {
        let upper = self.update_ui.confirmation.is_some()
            || self.form_dialog_is_open()
            || self.modal_over_form_dialogs_except_save_close();
        Admission {
            writable: !self.update_prepared
                && self.exit.is_none()
                && !crate::device::is_remote()
                && !self.operation_recovery_pending(),
            other_modal: upper || self.dirty_repository_close.is_some(),
            close_save: (!upper)
                .then(|| self.dirty_repository_close.as_ref().map(|close| close.card))
                .flatten(),
            choice_card: self.workspace.focused().filter(|card| {
                self.workspace.visibility(*card).is_some_and(VisibilityClass::is_mounted)
                    && self.workspace.pending_close().is_none_or(|closing| closing.unmounting != *card)
                    && matches!(self.cards.get(card), Some(CardRuntime::Repository(runtime)) if runtime.editor_focus_target(*card).is_some() && runtime.editor_feature_writable())
            }),
        }
    }
    pub(super) fn refresh_editor_feature_support(&mut self) {
        for runtime in self.cards.values_mut() {
            if let CardRuntime::Repository(runtime) = runtime {
                let capabilities = runtime
                    .lsp_document()
                    .map(|doc| self.lsp.capabilities(doc))
                    .unwrap_or_default();
                runtime.set_editor_feature_support(&capabilities);
            }
        }
    }
    pub(super) fn editor_format_before_save(
        &mut self,
        action: &RepositoryAction,
    ) -> Option<editor_features::Action> {
        let admission = self.feature_admission();
        if !admission.writable
            || (admission.other_modal && admission.close_save != Some(action.card()))
        {
            return None;
        }
        let CardRuntime::Repository(runtime) = self.cards.get_mut(&action.card())? else {
            return None;
        };
        editor_features::format_before_save(action, &self.lsp, runtime)
    }
    pub(super) fn update_editor_features(
        &mut self,
        action: editor_features::Action,
    ) -> Task<Message> {
        let admission = self.feature_admission();
        let mut buffers = self
            .cards
            .iter_mut()
            .filter_map(|(card, runtime)| match runtime {
                CardRuntime::Repository(runtime) => Some((*card, runtime.as_mut())),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut effects = self.editor_features.update(
            action,
            &mut self.lsp,
            &mut buffers,
            admission,
            &self.operation_recovery,
            self.settings.editor_syntax_theme,
        );
        drop(buffers);
        let mut tasks = vec![
            std::mem::replace(&mut effects.task, iced::Task::none())
                .map(Message::EditorFeatures)
                .map(cosmic::Action::App),
        ];
        tasks.extend(effects.repository_tasks.drain(..).map(repository_task));
        for card in effects.changed.drain(..) {
            tasks.push(self.drain_lsp_intents(card));
        }
        // The actual Change notifications must precede a positive server reply
        // and an admitted command, all within this reducer turn.
        if let Some(card) = effects.focus
            && let Some(CardRuntime::Repository(runtime)) = self.cards.get(&card)
            && let Some(target) = runtime.editor_focus_target(card)
        {
            tasks.push(iced::widget::operation::focus(target).map(cosmic::Action::App));
        }
        if let Some(card) = effects.choice_restore
            && let Some(CardRuntime::Repository(runtime)) = self.cards.get(&card)
            && let Some(target) = runtime.editor_focus_target(card)
        {
            tasks.push(
                editor_features::restore_choice_focus(target)
                    .map(Message::EditorFeatures)
                    .map(cosmic::Action::App),
            );
        }
        self.editor_features
            .finish_effects(&mut effects, &mut self.lsp);
        for card in effects.save {
            if self.prepare_editor_save(card)
                && let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&card)
            {
                tasks.push(repository_task(runtime.request_save(card, false)));
            }
        }
        for (level, message) in effects.notices {
            self.status = Some(match level {
                crate::lsp::LspMessageLevel::Error => TransientNotice::error(message),
                crate::lsp::LspMessageLevel::Warning => TransientNotice::warning(message),
                _ => TransientNotice::success(message),
            });
        }
        // A due timer may have been consumed while an existing language
        // dialog refused formatting. Its dirty buffer remains the obligation;
        // rearm every eligible native timer when that controller settles.
        if !self.editor_features.pending() {
            for (card, runtime) in &mut self.cards {
                if let CardRuntime::Repository(runtime) = runtime {
                    tasks.push(repository_task(runtime.auto_save_task(*card)));
                }
            }
        }
        self.refresh_editor_feature_support();
        Task::batch(tasks)
    }
}
