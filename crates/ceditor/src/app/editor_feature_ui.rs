use super::*;
use cagents::editor_features::{self, Admission};

impl Editor {
    fn feature_admission(&self) -> Admission {
        let upper = self.settings_open
            || self.quick_open
            || self.file_dialog.is_some()
            || self.checkout_dialog.is_some()
            || self.checkout_history_open
            || self.notices_open
            || self.pending_discard().is_some();
        Admission {
            // A committed window exit can still save its exact dirty-close
            // target. Other requests remain fenced by the closing modal and
            // the save_after + exact-card admission check.
            writable: !self.operation_recovery_pending()
                && (!self.exiting || self.closing.is_some())
                && !self
                    .tabs
                    .iter()
                    .any(|tab| tab.mutation == Some(MutationKind::Discard)),
            other_modal: upper || self.closing.is_some(),
            close_save: if upper { None } else { self.closing },
            // Only the current live tab is mounted in the standalone editor.
            // Keep this separate from ordinary Save/format admission.
            choice_card: self.active.filter(|card| {
                self.tab(*card)
                    .is_some_and(|tab| tab.runtime.editor_focus_target(*card).is_some())
            }),
        }
    }
    fn refresh_editor_feature_support(&mut self) {
        for tab in &mut self.tabs {
            let capabilities = tab
                .runtime
                .lsp_document()
                .map(|doc| self.lsp.capabilities(doc))
                .unwrap_or_default();
            tab.runtime.set_editor_feature_support(&capabilities);
        }
    }
    fn editor_format_before_save(
        &mut self,
        action: &RepositoryAction,
    ) -> Option<editor_features::Action> {
        let admission = self.feature_admission();
        if !admission.writable
            || (admission.other_modal && admission.close_save != Some(action.card()))
        {
            return None;
        }
        if self
            .tab(action.card())
            .is_some_and(|tab| self.tab_is_untitled(tab))
        {
            return None;
        }
        let lsp = &self.lsp;
        let tab = self.tabs.iter_mut().find(|tab| tab.id == action.card())?;
        editor_features::format_before_save(action, lsp, &mut tab.runtime)
    }
    fn update_editor_features(&mut self, action: editor_features::Action) -> Task<Message> {
        let admission = self.feature_admission();
        let mut buffers = self
            .tabs
            .iter_mut()
            .map(|tab| (tab.id, &mut tab.runtime))
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
            tasks.push(self.drain(card));
        }
        if let Some(card) = effects.focus
            && let Some(tab) = self.tab(card)
            && let Some(target) = tab.runtime.editor_focus_target(card)
        {
            tasks.push(iced::widget::operation::focus(target).map(cosmic::Action::App));
        }
        if let Some(card) = effects.choice_restore
            && let Some(tab) = self.tab(card)
            && let Some(target) = tab.runtime.editor_focus_target(card)
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
                && let Some(tab) = self.tab_mut(card)
            {
                tasks.push(repository_task(tab.runtime.request_save(card, false)));
            }
        }
        for (_, message) in effects.notices {
            self.notice = Some(message);
        }
        // A due timer may have been consumed while an existing language
        // dialog refused formatting. Its dirty buffer remains the obligation;
        // rearm every eligible native timer when that controller settles.
        if !self.editor_features.pending() {
            for tab in &mut self.tabs {
                tasks.push(repository_task(tab.runtime.auto_save_task(tab.id)));
            }
        }
        self.refresh_editor_feature_support();
        Task::batch(tasks)
    }
}
