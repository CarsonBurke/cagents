//! Whole-group intents resolve complete paths at their ordered queue turn.

use super::*;
use crate::repository::{ChangeArea, PathSelection};
use sidebar::ChangeGroupCommand;

impl Cagents {
    pub(super) fn enqueue_change_group(
        &mut self,
        key: ActiveWorktreeKey,
        area: ChangeArea,
        command: ChangeGroupCommand,
    ) -> Task<Message> {
        let allowed = match command {
            ChangeGroupCommand::Stage => area != ChangeArea::Staged,
            ChangeGroupCommand::Unstage => area == ChangeArea::Staged,
            ChangeGroupCommand::Discard => matches!(area, ChangeArea::Unstaged | ChangeArea::Untracked),
        };
        if !allowed { return Task::none(); }
        let Some(group) = self.sidebar_model.source_control.as_ref()
            .filter(|source| source.key == key).and_then(|source| source.group(area))
        else { return Task::none(); };
        let preview = group.rows().iter().map(sidebar::ChangeRow::selection).collect();
        self.enqueue_git_mutation(key, QueuedGitMutation::Area { area, command, preview })
    }

    pub(super) fn queued_group_task(
        &self,
        key: ActiveWorktreeKey,
        operation: uuid::Uuid,
        area: ChangeArea,
        command: ChangeGroupCommand,
    ) -> Task<Message> {
        let cancellation = self.source_control_mutations.get(&key.canonical_path)
            .filter(|mutation| mutation.id == operation)
            .map(|mutation| mutation.cancellation.clone()).unwrap_or_default();
        cosmic::task::future(async move {
            let target = key.clone();
            tokio::task::spawn_blocking(move || {
                let service = RepositoryService::system("git");
                let worktree = target.canonical_path.clone();
                let complete = |report| Message::SourceControlMutated {
                    key: target.clone(), operation, report, committed: false,
                };
                let selection = match service.status(&worktree, &cancellation) {
                    Ok(status) => status.area(area).map(PathSelection::from_changed_path).collect::<Vec<_>>(),
                    Err(error) => return complete(Some(format!("Could not read all paths for this action: {error}"))),
                };
                if selection.is_empty() { return complete(None); }
                match command {
                    ChangeGroupCommand::Stage => {
                        let request = StageRequest { worktree, selection };
                        match service.check_stage_markers(&request, &Default::default(), &cancellation) {
                            Ok(Some(markers)) => Message::SourceControlStageChecked {
                                key: target, operation, selection: request.selection, markers,
                            },
                            Ok(None) => complete(mutation_report("Stage", service.stage(&request, &cancellation))),
                            Err(error) => complete(Some(format!("Stage could not run: {error}"))),
                        }
                    }
                    ChangeGroupCommand::Unstage => complete(mutation_report("Unstage", service.unstage(
                        &UnstageRequest { worktree, selection }, &cancellation,
                    ))),
                    ChangeGroupCommand::Discard => Message::SourceControlGroupSelection {
                        key: target, operation, area, command, result: Ok(selection),
                    },
                }
            }).await.unwrap_or_else(|error| Message::SourceControlMutated {
                key, operation, report: Some(format!("Group action worker failed: {error}")), committed: false,
            })
        })
    }

    pub(super) fn finish_group_selection(
        &mut self,
        key: ActiveWorktreeKey,
        operation: uuid::Uuid,
        area: ChangeArea,
        command: ChangeGroupCommand,
        result: Result<Vec<PathSelection>, String>,
    ) -> Task<Message> {
        if !self.source_control_mutations.get(&key.canonical_path)
            .is_some_and(|mutation| mutation.id == operation)
        { return Task::none(); }
        let (selection, report) = match result {
            Ok(selection) => (Some(selection), None),
            Err(error) => (None, Some(error)),
        };
        // Release the read's ordered turn before presenting its explicit path
        // set. Confirmation never authorizes files discovered by a later read.
        let release = self.update(Message::SourceControlMutated {
            key: key.clone(), operation, report, committed: false,
        });
        if !self.source_control_is_bound(&key) || self.modal_is_open() { return release; }
        let Some(action) = selection.and_then(|selection| sidebar::resolve_group_selection(&key, area, command, selection))
        else { return release; };
        Task::batch([release, self.update(Message::Sidebar(action))])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::futures::StreamExt;
    use std::{fs, path::Path, process::Command};

    fn git(root: &Path, args: &[&str]) -> Vec<u8> {
        let output = Command::new("git").arg("-C").arg(root).args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Fixture").env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture").env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .output().unwrap();
        assert!(output.status.success(), "{args:?}: {}", String::from_utf8_lossy(&output.stderr));
        output.stdout
    }

    fn key(root: &Path) -> ActiveWorktreeKey {
        ActiveWorktreeKey { project_id: ProjectId::new(), worktree_id: crate::model::WorktreeId::new(), canonical_path: root.to_owned() }
    }

    async fn run(task: Task<Message>) -> Message {
        let mut stream = iced::runtime::task::into_stream(task).unwrap();
        loop {
            if let iced::runtime::Action::Output(message) = stream.next().await.expect("group result") { return message; }
        }
    }

    #[tokio::test]
    async fn whole_group_stage_and_unstage_resolve_paths_at_execution_time() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-b", "main"]);
        let app = super::super::tests::context_inspector_test_app(false);
        let key = key(root.path());
        fs::write(root.path().join("first.txt"), "first\n").unwrap();
        let task = app.queued_group_task(key.clone(), uuid::Uuid::new_v4(), ChangeArea::Untracked, ChangeGroupCommand::Stage);
        // These paths arrived after the user queued the group command. They
        // are absent from its rendered/capped preview, but belong to "all".
        fs::write(root.path().join("later.txt"), "later\n").unwrap();
        fs::write(root.path().join("last.txt"), "last\n").unwrap();
        assert!(matches!(run(task).await, Message::SourceControlMutated { report: None, .. }));
        assert_eq!(git(root.path(), &["ls-files", "-z"]).split(|byte| *byte == 0).filter(|path| !path.is_empty()).count(), 3);
        let task = app.queued_group_task(key, uuid::Uuid::new_v4(), ChangeArea::Staged, ChangeGroupCommand::Unstage);
        fs::write(root.path().join("staged-later.txt"), "another\n").unwrap();
        git(root.path(), &["add", "staged-later.txt"]);
        assert!(matches!(run(task).await, Message::SourceControlMutated { report: None, .. }));
        assert!(git(root.path(), &["ls-files", "-z"]).is_empty());
        assert!(root.path().join("staged-later.txt").exists());
    }

    #[tokio::test]
    async fn whole_group_stage_preserves_marker_confirmation_and_discard_binds_exact_paths() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-b", "main"]);
        fs::write(root.path().join("safe.txt"), "safe\n").unwrap();
        fs::write(root.path().join("markers.txt"), "<<<<<<< ours\nleft\n=======\nright\n>>>>>>> theirs\n").unwrap();
        let app = super::super::tests::context_inspector_test_app(false);
        let key = key(root.path());
        let stage = app.queued_group_task(key.clone(), uuid::Uuid::new_v4(), ChangeArea::Untracked, ChangeGroupCommand::Stage);
        let Message::SourceControlStageChecked { selection, markers, .. } = run(stage).await else { panic!("marker confirmation required"); };
        assert_eq!(selection.len(), 2);
        assert!(!markers.is_empty());
        assert!(git(root.path(), &["ls-files", "-z"]).is_empty());
        let discard = app.queued_group_task(key.clone(), uuid::Uuid::new_v4(), ChangeArea::Untracked, ChangeGroupCommand::Discard);
        let Message::SourceControlGroupSelection { result: Ok(selection), .. } = run(discard).await else { panic!("explicit selection required"); };
        fs::write(root.path().join("unconfirmed.txt"), "preserve\n").unwrap();
        let Some(SidebarAction::RequestDiscard { request, .. }) = sidebar::resolve_group_selection(&key, ChangeArea::Untracked, ChangeGroupCommand::Discard, selection) else { panic!("confirmation required"); };
        assert_eq!(request.untracked.len(), 2);
        assert!(!request.untracked.iter().any(|path| path.as_path() == Path::new("unconfirmed.txt")));
        assert!(root.path().join("safe.txt").exists(), "a selection read must never discard directly");
    }
}
