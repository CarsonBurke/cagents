use super::*;
use crate::agent::ActivityState;
use crate::model::{ProjectThread, ThreadId};
use crate::sidebar::SidebarThread;

#[derive(Debug, Clone, Copy)]
pub enum Action {
    Create(ProjectId),
    Open(ThreadId),
}

impl Cagents {
    pub(super) fn new_project_thread(&mut self) -> Task<Message> {
        let project = self
            .durable
            .active_thread
            .and_then(|id| self.durable.threads.iter().find(|thread| thread.id == id))
            .map(|thread| thread.project_id)
            .or_else(|| {
                self.active_workspace.as_deref().and_then(|path| {
                    self.sidebar_model
                        .worktree_at(path)
                        .map(|key| key.project_id)
                })
            })
            .or_else(|| {
                sidebar::selected_project(&self.sidebar_model, &self.sidebar_state)
                    .map(|project| project.id)
            })
            .or_else(|| {
                self.sidebar_model
                    .projects
                    .first()
                    .map(|project| project.id)
            });
        match project {
            Some(project) => self.handle_thread_ui(Action::Create(project)),
            None => cosmic::task::message(Message::OpenRepository),
        }
    }

    pub(super) fn handle_thread_ui(&mut self, action: Action) -> Task<Message> {
        if self.update_prepared
            || self.modal_is_open()
            || self.focused_repository_is_saving()
            || self.project_ui.deleting()
        {
            return Task::none();
        }
        match action {
            Action::Create(project_id) => {
                if !self
                    .sidebar_model
                    .projects
                    .iter()
                    .any(|project| project.id == project_id)
                {
                    return Task::none();
                }
                let id = ThreadId::new();
                Arc::make_mut(&mut self.durable)
                    .threads
                    .push(ProjectThread {
                        id,
                        project_id,
                        // Empty means awaiting the conversation's first meaningful title.
                        // Nonempty names, including user edits, are never replaced here.
                        title: String::new(),
                        cards: Vec::new(),
                        focused: None,
                    });
                self.sidebar_state.reveal_project(project_id);
                self.show_workspace_for_action();
                let scope = self.select_thread(id);
                let chat = self.new_native_chat();
                self.persist();
                Task::batch([scope, chat])
            }
            Action::Open(id) => {
                self.show_workspace_for_action();
                self.select_thread(id)
            }
        }
    }

    /// Refresh cached rows after state changes, never during rendering. A thread's
    /// conversation supplies its initial name, but cannot overwrite a saved name.
    pub(super) fn refresh_sidebar_threads(&mut self) {
        let mut renamed = false;
        for index in 0..self.durable.threads.len() {
            let thread = &self.durable.threads[index];
            if !thread.title.is_empty() {
                continue;
            }
            let title = thread.cards.iter().find_map(|id| {
                let card = self.card_state(*id)?;
                if !card.existence.is_open()
                    || !matches!(&card.kind, CardKind::Chat(_))
                    || matches!(card.title.trim(), "" | "Native chat" | "Agent" | "Terminal")
                {
                    return None;
                }
                normalize_card_name(&card.title)
            });
            if let Some(title) = title {
                Arc::make_mut(&mut self.durable).threads[index].title = title;
                renamed = true;
            }
        }
        if renamed {
            self.persist();
        }

        let durable = &self.durable;
        let state = &self.sidebar_state;
        for project in &mut self.sidebar_model.projects {
            let mut index = 0;
            for thread in durable
                .threads
                .iter()
                .filter(|thread| thread.project_id == project.id)
            {
                let title = if thread.title.is_empty() {
                    "New thread"
                } else {
                    &thread.title
                };
                let selected = durable.active_thread == Some(thread.id);
                let mut activity = None;
                let mut unread = false;
                for id in &thread.cards {
                    if !durable
                        .cards
                        .iter()
                        .any(|card| card.id == *id && card.existence.is_open())
                    {
                        continue;
                    }
                    unread |= state.has_card_update(*id);
                    match state.card_activity(*id).map(|activity| activity.state) {
                        Some(ActivityState::Waiting) => activity = Some(ActivityState::Waiting),
                        Some(ActivityState::Working)
                            if activity != Some(ActivityState::Waiting) =>
                        {
                            activity = Some(ActivityState::Working);
                        }
                        _ => {}
                    }
                }
                let unchanged = project.threads.get(index).is_some_and(|row| {
                    row.id == thread.id
                        && row.title == title
                        && row.selected == selected
                        && row.activity == activity
                        && row.unread == unread
                });
                if !unchanged {
                    let row =
                        SidebarThread::new(thread.id, title.to_owned(), selected, activity, unread);
                    if index < project.threads.len() {
                        project.threads[index] = row;
                    } else {
                        project.threads.push(row);
                    }
                }
                index += 1;
            }
            project.threads.truncate(index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_waits_for_a_conversation_name_and_preserves_saved_names() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let project = ProjectId::new();
        let id = ThreadId::new();
        let durable = Arc::make_mut(&mut app.durable);
        durable.cards[0].title = "Native chat".into();
        // Thread naming reads the conversation card: the fixture's default
        // terminal card never qualifies.
        durable.cards[0].kind = crate::model::CardKind::Chat(crate::chat::Session {
            id: crate::model::TerminalId::new(),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Omp,
                recipe: crate::model::LaunchRecipe {
                    program: "/bin/sh".into(),
                    arguments: Vec::new(),
                    working_directory: std::path::PathBuf::from("/tmp"),
                    environment: Default::default(),
                },
                session_id: None,
            },
            proof: None,
            terminal: None,
        });
        durable.threads.push(ProjectThread {
            id,
            project_id: project,
            title: String::new(),
            cards: vec![card],
            focused: Some(card),
        });
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "project".into(),
            "/repo".into(),
            Vec::new(),
            Vec::new(),
        ));

        app.refresh_sidebar_threads();
        assert_eq!(app.sidebar_model.projects[0].threads[0].title, "New thread");
        Arc::make_mut(&mut app.durable).cards[0].title = "Explain the parser".into();
        app.refresh_sidebar_threads();
        assert_eq!(
            app.sidebar_model.projects[0].threads[0].title,
            "Explain the parser"
        );

        let durable = Arc::make_mut(&mut app.durable);
        durable.threads[0].title = "My investigation".into();
        durable.cards[0].title = "A later conversation title".into();
        app.refresh_sidebar_threads();
        assert_eq!(
            app.sidebar_model.projects[0].threads[0].title,
            "My investigation"
        );
        assert_eq!(app.durable.threads[0].title, "My investigation");
    }
}
