//! Shared discussion state and widgets for the workspace and standalone editor.
use super::discussion::{Anchor, Authority, Mutation, Outcome};
use super::*;
use crate::github::{
    PullRequestConversation, PullRequestReviewThread, ReviewEvent, SubmitReviewRequest,
};
use cosmic::iced::widget::{markdown, text_editor};

#[derive(Debug, Clone)]
pub enum Action {
    Fenced {
        generation: u64,
        authority_revision: u64,
        action: Box<Action>,
    },
    Loaded {
        generation: u64,
        load: u64,
        result: Result<Arc<PullRequestConversation>, String>,
    },
    Begin(Anchor),
    Reply(String),
    Edit(String),
    Delete(String),
    Resolve {
        thread: String,
        resolved: bool,
    },
    ToggleThread(String),
    Composer(text_editor::Action),
    Preview,
    Cancel,
    Save,
    Summary(text_editor::Action),
    Event(ReviewEvent),
    PrepareSubmit,
    CancelSubmit,
    ConfirmSubmit,
    Finished {
        generation: u64,
        outcome: Outcome,
    },
    AcknowledgeUnknown,
    OpenLink(String),
}

#[derive(Debug, Clone)]
enum Destination {
    Line(Anchor),
    Reply(String),
    Edit(String),
}

struct Composer {
    authority: Authority,
    destination: Destination,
    content: text_editor::Content<cosmic::Renderer>,
    preview: bool,
    markdown: Vec<markdown::Item>,
}

#[derive(Default)]
struct FileThreads {
    leading: Vec<usize>,
    lines: HashMap<(crate::github::ReviewDiffSide, u64), Vec<usize>>,
    comments: usize,
    unresolved: usize,
}

pub(super) struct DiscussionState {
    revision: u64,
    conversation: Option<Arc<PullRequestConversation>>,
    threads: HashMap<String, FileThreads>,
    markdown: HashMap<String, Vec<markdown::Item>>,
    expanded: BTreeSet<String>,
    composer: Option<Composer>,
    summary: text_editor::Content<cosmic::Renderer>,
    event: ReviewEvent,
    confirmation: Option<(
        Authority,
        SubmitReviewRequest,
        Vec<discussion::PendingComment>,
    )>,
    busy: bool,
    submitting: bool,
    saving_composer: bool,
    unknown: Option<String>,
    loading: bool,
}

impl Default for DiscussionState {
    fn default() -> Self {
        Self {
            revision: 0,
            conversation: None,
            threads: HashMap::new(),
            markdown: HashMap::new(),
            expanded: BTreeSet::new(),
            composer: None,
            summary: text_editor::Content::new(),
            event: ReviewEvent::Comment,
            confirmation: None,
            busy: false,
            submitting: false,
            saving_composer: false,
            unknown: None,
            loading: false,
        }
    }
}

impl State {
    pub(super) fn load_discussion(&mut self) -> ActionTask {
        let Some(target) = self.target.clone() else {
            return ActionTask::none();
        };
        let Ok(key) = PullRequestKey::new(target.repository, target.pull_request.number) else {
            return ActionTask::none();
        };
        let generation = self.generation;
        let load = self.load_generation;
        let api = Arc::clone(&self.api);
        let cancellation = self.cancellation.clone();
        self.discussion.loading = true;
        ActionTask::perform(
            async move {
                let result = tokio::task::spawn_blocking(move || {
                    api.conversation(&key, &cancellation)
                        .map(Arc::new)
                        .map_err(|e| e.to_string())
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
                super::Action::Discussion(Action::Loaded {
                    generation,
                    load,
                    result,
                })
            },
            |action| action,
        )
    }

    fn authority(&self) -> Option<Authority> {
        Some(Authority {
            target: self.target.clone()?,
            context: self.context.clone()?,
        })
    }

    fn can_mutate_discussion(&self) -> bool {
        self.load == LoadState::Ready
            && !self.refreshing
            && !self.discussion.loading
            && !self.discussion.busy
            && self.discussion.unknown.is_none()
    }

    fn send_discussion(&mut self, authority: Authority, mutation: Mutation) -> ActionTask {
        if !self.can_mutate_discussion() {
            return ActionTask::none();
        }
        self.discussion.busy = true;
        self.discussion.submitting = matches!(mutation, Mutation::Submit { .. });
        self.discussion.saving_composer = matches!(
            mutation,
            Mutation::Comment { .. } | Mutation::Reply { .. } | Mutation::Edit { .. }
        );
        // A load started before this write must not install an older pending count.
        self.load_generation = self.load_generation.wrapping_add(1);
        let generation = self.generation;
        let api = Arc::clone(&self.api);
        let git = self.git.clone();
        let cancellation = self.cancellation.clone();
        ActionTask::perform(
            async move {
                let outcome = tokio::task::spawn_blocking(move || {
                    discussion::execute(api.as_ref(), &git, &authority, &mutation, &cancellation)
                })
                .await
                .unwrap_or_else(|e| Outcome::Unknown(e.to_string()));
                super::Action::Discussion(Action::Finished {
                    generation,
                    outcome,
                })
            },
            |action| action,
        )
    }

    pub(super) fn update_discussion(&mut self, action: Action) -> ActionTask {
        self.discussion.revision = self.discussion.revision.wrapping_add(1);
        match action {
            Action::Fenced {
                generation,
                authority_revision,
                action,
            } => {
                if generation != self.generation || authority_revision != self.authority_revision {
                    return ActionTask::none();
                }
                return self.update_discussion(*action);
            }
            Action::Loaded {
                generation,
                load,
                result,
            } => {
                if generation != self.generation || load != self.load_generation {
                    return ActionTask::none();
                }
                self.discussion.loading = false;
                match result {
                    Ok(conversation) => {
                        self.discussion.threads.clear();
                        for (index, thread) in conversation.threads.iter().enumerate() {
                            let file = self
                                .discussion
                                .threads
                                .entry(thread.path.clone())
                                .or_default();
                            file.comments += thread.comments.len();
                            file.unresolved += usize::from(!thread.resolved);
                            if let Some(line) = thread.line.filter(|_| !thread.outdated) {
                                file.lines
                                    .entry((thread.side, line))
                                    .or_default()
                                    .push(index);
                            } else {
                                file.leading.push(index);
                            }
                        }
                        self.discussion.markdown = conversation
                            .threads
                            .iter()
                            .flat_map(|t| &t.comments)
                            .filter_map(|c| {
                                c.id.as_ref()
                                    .map(|id| (id.clone(), markdown::parse(&c.body).collect()))
                            })
                            .collect();
                        self.discussion.conversation = Some(conversation);
                    }
                    Err(error) => {
                        self.notice = Some(format!("Could not load review discussion. {error}"))
                    }
                }
            }
            Action::Finished {
                generation,
                outcome,
            } => {
                if generation != self.generation {
                    return ActionTask::none();
                }
                self.discussion.busy = false;
                self.discussion.confirmation = None;
                match outcome {
                    Outcome::Applied => {
                        if self.discussion.saving_composer {
                            self.discussion.composer = None;
                        }
                        if self.discussion.submitting {
                            self.discussion.summary = text_editor::Content::new();
                        }
                        self.notice = Some("Review updated on GitHub.".into());
                    }
                    Outcome::Refused(message) => self.notice = Some(message),
                    Outcome::Unknown(message) => self.discussion.unknown = Some(message),
                }
                self.refreshing = true;
                return self.start_load();
            }
            Action::AcknowledgeUnknown if !self.discussion.busy => {
                self.discussion.unknown = None;
                self.discussion.composer = None;
                self.discussion.confirmation = None;
                return self.refresh();
            }
            Action::ToggleThread(id) => {
                if !self.discussion.expanded.remove(&id) {
                    self.discussion.expanded.insert(id);
                }
            }
            Action::CancelSubmit => self.discussion.confirmation = None,
            Action::OpenLink(raw) => {
                if let Ok(url) = Url::parse(&raw)
                    && matches!(url.scheme(), "https" | "http")
                {
                    self.effects.push(Effect::OpenUrl(url));
                }
            }
            _ if !self.can_mutate_discussion() => return ActionTask::none(),
            Action::Begin(anchor) => {
                if self
                    .selected
                    .as_ref()
                    .is_none_or(|path| path.display() != anchor.path)
                {
                    return ActionTask::none();
                }
                let authority = self.authority().expect("loaded review authority");
                if let Some(composer) = &mut self.discussion.composer {
                    if matches!(composer.destination, Destination::Line(_))
                        && composer.authority == authority
                    {
                        composer.destination = Destination::Line(anchor);
                    } else {
                        self.notice = Some("Save or cancel the open comment first.".into());
                    }
                    return ActionTask::none();
                }
                self.discussion.composer = Some(Composer {
                    authority,
                    destination: Destination::Line(anchor),
                    content: text_editor::Content::new(),
                    preview: false,
                    markdown: Vec::new(),
                });
            }
            Action::Reply(id) => {
                if self.discussion.composer.is_some() {
                    self.notice = Some("Save or cancel the open comment first.".into());
                    return ActionTask::none();
                }
                self.discussion.expanded.insert(id.clone());
                self.discussion.composer = Some(Composer {
                    authority: self.authority().expect("loaded review authority"),
                    destination: Destination::Reply(id),
                    content: text_editor::Content::new(),
                    preview: false,
                    markdown: Vec::new(),
                });
            }
            Action::Edit(id) => {
                if self.discussion.composer.is_some() {
                    self.notice = Some("Save or cancel the open comment first.".into());
                    return ActionTask::none();
                }
                let Some(comment) = self.discussion.conversation.as_ref().and_then(|c| {
                    c.threads.iter().flat_map(|t| &t.comments).find(|c| {
                        c.id.as_ref() == Some(&id) && c.viewer_did_author && c.is_pending()
                    })
                }) else {
                    return ActionTask::none();
                };
                self.discussion.composer = Some(Composer {
                    authority: self.authority().expect("loaded review authority"),
                    destination: Destination::Edit(id),
                    content: text_editor::Content::with_text(&comment.body),
                    preview: false,
                    markdown: markdown::parse(&comment.body).collect(),
                });
            }
            Action::Composer(action) => {
                if let Some(composer) = &mut self.discussion.composer {
                    composer.content.perform(action);
                    composer.markdown = markdown::parse(&composer.content.text()).collect();
                }
            }
            Action::Preview => {
                if let Some(composer) = &mut self.discussion.composer {
                    composer.preview = !composer.preview;
                }
            }
            Action::Cancel => self.discussion.composer = None,
            Action::Summary(action) => self.discussion.summary.perform(action),
            Action::Event(event) => {
                self.discussion.event = event;
                self.discussion.confirmation = None;
            }
            Action::Save => {
                let Some(composer) = &self.discussion.composer else {
                    return ActionTask::none();
                };
                let authority = composer.authority.clone();
                let body = composer.content.text();
                if body.trim().is_empty() {
                    return ActionTask::none();
                }
                let mutation = match &composer.destination {
                    Destination::Line(anchor) => Mutation::Comment {
                        anchor: anchor.clone(),
                        body,
                    },
                    Destination::Reply(thread) => Mutation::Reply {
                        thread: thread.clone(),
                        body,
                    },
                    Destination::Edit(comment) => Mutation::Edit {
                        comment: comment.clone(),
                        body,
                    },
                };
                return self.send_discussion(authority, mutation);
            }
            Action::Delete(comment) => {
                if let Some(authority) = self.authority() {
                    return self.send_discussion(authority, Mutation::Delete { comment });
                }
            }
            Action::Resolve { thread, resolved } => {
                if let Some(authority) = self.authority() {
                    return self.send_discussion(authority, Mutation::Resolve { thread, resolved });
                }
            }
            Action::PrepareSubmit => {
                if self.discussion.composer.is_some() {
                    self.notice = Some(
                        "Save or cancel the open comment before submitting your review.".into(),
                    );
                    return ActionTask::none();
                }
                if let Some(authority) = self.authority() {
                    let request = SubmitReviewRequest {
                        pull_request_id: authority.context.pull_request_id.clone(),
                        review_id: authority.context.pending.as_ref().map(|p| p.id.clone()),
                        commit_oid: authority.context.head_oid.clone(),
                        event: self.discussion.event,
                        body: self.discussion.summary.text(),
                    };
                    let empty = PullRequestConversation::default();
                    let conversation = self.discussion.conversation.as_deref().unwrap_or(&empty);
                    let comments = match discussion::pending_snapshot(
                        conversation,
                        authority.context.pending.as_ref(),
                    ) {
                        Ok(comments) => comments,
                        Err(Outcome::Refused(message) | Outcome::Unknown(message)) => {
                            self.notice = Some(message);
                            return ActionTask::none();
                        }
                        Err(Outcome::Applied) => unreachable!(),
                    };
                    self.discussion.confirmation = Some((authority, request, comments));
                }
            }
            Action::ConfirmSubmit => {
                if let Some((authority, request, comments)) = self.discussion.confirmation.take() {
                    return self.send_discussion(authority, Mutation::Submit { request, comments });
                }
            }
            Action::AcknowledgeUnknown => {}
        }
        ActionTask::none()
    }

    pub fn review_bar(&self, modal_open: bool) -> Element<'_, super::Action> {
        let enabled = !modal_open && self.can_mutate_discussion();
        let mut rows: Vec<Element<'_, Action>> = Vec::new();
        let pending = self
            .context
            .as_ref()
            .and_then(|c| c.pending.as_ref())
            .map_or(0, |p| p.comment_count);
        rows.push(design::heading(format!("Review · {pending} pending comments")).into());
        if let Some(message) = &self.discussion.unknown {
            rows.push(design::body(format!("GitHub may have received the last change. Check the discussion before continuing. {message}")).into());
            rows.push(
                button::text("I checked GitHub; continue")
                    .on_press(Action::AcknowledgeUnknown)
                    .into(),
            );
        }
        if let Some((authority, request, _)) = &self.discussion.confirmation {
            rows.push(
                design::body(format!(
                    "{} on {}/{} #{}\nCommit {}\nIncludes {} pending comments.",
                    request.event.label(),
                    authority.target.repository.owner,
                    authority.target.repository.name,
                    authority.target.pull_request.number,
                    request.commit_oid,
                    authority
                        .context
                        .pending
                        .as_ref()
                        .map_or(0, |p| p.comment_count)
                ))
                .into(),
            );
            rows.push(design::body(request.body.clone()).into());
            rows.push(
                widget::row::with_children(vec![
                    button::suggested("Submit review")
                        .on_press_maybe(enabled.then_some(Action::ConfirmSubmit))
                        .into(),
                    button::text("Back").on_press(Action::CancelSubmit).into(),
                ])
                .spacing(8)
                .into(),
            );
        } else {
            let own = self.context.as_ref().is_some_and(|c| c.viewer_is_author);
            rows.push(
                widget::row::with_children(
                    [
                        ReviewEvent::Comment,
                        ReviewEvent::Approve,
                        ReviewEvent::RequestChanges,
                    ]
                    .into_iter()
                    .map(|event| {
                        button::text(if event == self.discussion.event {
                            format!("✓ {}", event.label())
                        } else {
                            event.label().into()
                        })
                        .on_press_maybe(
                            (enabled && (!own || event == ReviewEvent::Comment))
                                .then_some(Action::Event(event)),
                        )
                        .into()
                    })
                    .collect::<Vec<_>>(),
                )
                .spacing(4)
                .into(),
            );
            let mut editor = text_editor::TextEditor::new(&self.discussion.summary)
                .placeholder("Review summary (optional)")
                .height(90);
            if enabled {
                editor = editor.on_action(Action::Summary);
            }
            rows.push(editor.into());
            rows.push(
                button::suggested(if self.discussion.busy {
                    "Saving…"
                } else {
                    "Review and submit"
                })
                .on_press_maybe(enabled.then_some(Action::PrepareSubmit))
                .into(),
            );
        }
        {
            let element: Element<'_, Action> =
                widget::container(widget::column::with_children(rows).spacing(8))
                    .padding(8)
                    .width(Length::Fill)
                    .into();
            element.map(|action| self.review_action(action))
        }
    }
}

impl State {
    pub fn review_action(&self, action: Action) -> super::Action {
        super::Action::Discussion(Action::Fenced {
            generation: self.generation,
            authority_revision: self.authority_revision,
            action: Box::new(action),
        })
    }

    pub(super) fn discussion_counts(&self, path: &str) -> (usize, usize) {
        self.discussion
            .threads
            .get(path)
            .map_or((0, 0), |file| (file.comments, file.unresolved))
    }

    pub fn annotation_revision(&self) -> (u64, u64) {
        (self.generation, self.discussion.revision)
    }

    fn composer_on(&self, path: &str, old: Option<u32>, new: Option<u32>, leading: bool) -> bool {
        let Some(composer) = &self.discussion.composer else {
            return false;
        };
        match &composer.destination {
            Destination::Line(anchor) => {
                !leading
                    && anchor.path == path
                    && match anchor.side {
                        crate::github::ReviewDiffSide::Left => {
                            old.map(u64::from) == Some(anchor.line)
                        }
                        crate::github::ReviewDiffSide::Right => {
                            new.map(u64::from) == Some(anchor.line)
                        }
                    }
            }
            Destination::Reply(id) => self
                .threads_on(path, old, new, leading)
                .any(|t| &t.id == id),
            Destination::Edit(id) => self
                .threads_on(path, old, new, leading)
                .any(|t| t.comments.iter().any(|c| c.id.as_ref() == Some(id))),
        }
    }

    fn threads_on<'a>(
        &'a self,
        path: &'a str,
        old: Option<u32>,
        new: Option<u32>,
        leading: bool,
    ) -> impl Iterator<Item = &'a PullRequestReviewThread> {
        use crate::github::ReviewDiffSide::{Left, Right};
        let file = self.discussion.threads.get(path);
        let top = file
            .filter(|_| leading)
            .map(|file| file.leading.as_slice())
            .unwrap_or_default();
        let left = file
            .filter(|_| !leading)
            .and_then(|file| file.lines.get(&(Left, u64::from(old?))));
        let right = file
            .filter(|_| !leading)
            .and_then(|file| file.lines.get(&(Right, u64::from(new?))));
        top.iter()
            .chain(left.into_iter().flatten())
            .chain(right.into_iter().flatten())
            .filter_map(|index| self.discussion.conversation.as_ref()?.threads.get(*index))
    }

    pub fn has_annotation(
        &self,
        path: &str,
        old: Option<u32>,
        new: Option<u32>,
        leading: bool,
    ) -> bool {
        self.threads_on(path, old, new, leading).next().is_some()
            || self.composer_on(path, old, new, leading)
    }

    /// Natural-height content; the diff measures this before reserving space.
    pub fn annotation<'a>(
        &'a self,
        path: &'a str,
        old: Option<u32>,
        new: Option<u32>,
        leading: bool,
    ) -> Element<'a, super::Action> {
        let enabled = self.can_mutate_discussion();
        let mut rows: Vec<Element<'a, Action>> = Vec::new();
        for thread in self.threads_on(path, old, new, leading) {
            let expanded = !thread.resolved || self.discussion.expanded.contains(&thread.id);
            let label = format!(
                "{}{} · {} comments",
                if thread.outdated { "Outdated · " } else { "" },
                if thread.resolved {
                    "Resolved"
                } else {
                    "Discussion"
                },
                thread.comments.len()
            );
            rows.push(
                button::text(label)
                    .on_press(Action::ToggleThread(thread.id.clone()))
                    .into(),
            );
            if !expanded {
                continue;
            }
            for comment in &thread.comments {
                rows.push(
                    design::caption(format!(
                        "{}{}",
                        comment
                            .author
                            .as_ref()
                            .map_or("Deleted account", |a| a.login.as_str()),
                        if comment.is_pending() {
                            " · Pending"
                        } else {
                            ""
                        }
                    ))
                    .into(),
                );
                if let Some(items) = comment
                    .id
                    .as_ref()
                    .and_then(|id| self.discussion.markdown.get(id))
                {
                    rows.push(
                        markdown::view(items, markdown::Settings::from(iced::Theme::Dark))
                            .map(|uri| Action::OpenLink(uri.to_string())),
                    );
                } else {
                    rows.push(design::body(&comment.body).into());
                }
                if comment.viewer_did_author
                    && comment.is_pending()
                    && let Some(id) = &comment.id
                {
                    rows.push(
                        widget::row::with_children(vec![
                            button::text("Edit")
                                .on_press_maybe(
                                    (enabled && comment.viewer_can_update)
                                        .then_some(Action::Edit(id.clone())),
                                )
                                .into(),
                            button::text("Delete draft")
                                .on_press_maybe(
                                    (enabled && comment.viewer_can_delete)
                                        .then_some(Action::Delete(id.clone())),
                                )
                                .into(),
                        ])
                        .spacing(8)
                        .into(),
                    );
                }
            }
            let can_resolve = if thread.resolved {
                thread.viewer_can_unresolve
            } else {
                thread.viewer_can_resolve
            };
            rows.push(
                widget::row::with_children(vec![
                    button::text("Reply")
                        .on_press_maybe(enabled.then_some(Action::Reply(thread.id.clone())))
                        .into(),
                    button::text(if thread.resolved { "Reopen" } else { "Resolve" })
                        .on_press_maybe((enabled && can_resolve).then_some(Action::Resolve {
                            thread: thread.id.clone(),
                            resolved: !thread.resolved,
                        }))
                        .into(),
                ])
                .spacing(8)
                .into(),
            );
            if thread.truncated {
                rows.push(design::caption("More comments are available on GitHub.").into());
            }
        }
        if self.composer_on(path, old, new, leading)
            && let Some(composer) = &self.discussion.composer
        {
            let label = match &composer.destination {
                Destination::Line(anchor) if anchor.start_line != anchor.line => {
                    format!("New comment · lines {}–{}", anchor.start_line, anchor.line)
                }
                Destination::Line(anchor) => format!("New comment · line {}", anchor.line),
                Destination::Reply(_) => "Reply in pending review".into(),
                Destination::Edit(_) => "Edit pending comment".into(),
            };
            rows.push(design::heading(label).into());
            if composer.preview {
                rows.push(
                    markdown::view(
                        &composer.markdown,
                        markdown::Settings::from(iced::Theme::Dark),
                    )
                    .map(|uri| Action::OpenLink(uri.to_string())),
                );
            } else {
                let mut editor = text_editor::TextEditor::new(&composer.content)
                    .placeholder("Leave a review comment…")
                    .height(140);
                if enabled {
                    editor = editor.on_action(Action::Composer);
                }
                rows.push(editor.into());
            }
            rows.push(
                widget::row::with_children(vec![
                    button::suggested("Save pending comment")
                        .on_press_maybe(
                            (enabled && !composer.content.text().trim().is_empty())
                                .then_some(Action::Save),
                        )
                        .into(),
                    button::text(if composer.preview { "Write" } else { "Preview" })
                        .on_press_maybe(enabled.then_some(Action::Preview))
                        .into(),
                    button::text("Cancel")
                        .on_press_maybe(enabled.then_some(Action::Cancel))
                        .into(),
                ])
                .spacing(8)
                .into(),
            );
        }
        let element: Element<'_, Action> =
            widget::container(widget::column::with_children(rows).spacing(8))
                .padding(12)
                .width(Length::Fill)
                .into();
        element.map(|action| self.review_action(action))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::ReviewDiffSide;
    use crate::pull_request_review::tests::{entry, loaded, target};

    fn ready() -> State {
        let mut state = State::new(
            Arc::new(ScriptedApi::new()),
            RepositoryService::system("git"),
        );
        state.target = Some(target(1));
        state.adopt(&loaded(
            vec![entry("file.rs", ChangeKind::Modified)],
            vec![],
        ));
        state.selected = Some(RepoPath::parse("file.rs").unwrap());
        state
    }
    fn anchor() -> Anchor {
        Anchor {
            path: "file.rs".into(),
            side: ReviewDiffSide::Right,
            start_line: 4,
            line: 7,
        }
    }

    #[test]
    fn queued_gutter_action_cannot_create_a_draft_for_a_replaced_target() {
        let mut state = ready();
        let old = state.review_action(Action::Begin(anchor()));
        state.reset_target();
        state.target = Some(target(2));
        state.adopt(&loaded(
            vec![entry("file.rs", ChangeKind::Modified)],
            vec![],
        ));
        state.selected = Some(RepoPath::parse("file.rs").unwrap());
        drop(state.update(old));
        assert!(state.discussion.composer.is_none());
    }

    #[test]
    fn queued_gutter_action_cannot_move_to_a_refreshed_head_of_the_same_pr() {
        let mut state = ready();
        let old = state.review_action(Action::Begin(anchor()));
        let mut next = loaded(vec![entry("file.rs", ChangeKind::Modified)], vec![]);
        next.context.head_oid = "c".repeat(40);
        state.adopt(&next);
        drop(state.update(old));
        assert!(state.discussion.composer.is_none());
    }

    #[test]
    fn draft_retains_original_commit_authority_across_background_refresh() {
        let mut state = ready();
        drop(state.update_discussion(Action::Begin(anchor())));
        let mut next = loaded(vec![entry("file.rs", ChangeKind::Modified)], vec![]);
        next.context.head_oid = "c".repeat(40);
        state.adopt(&next);
        assert_eq!(
            state
                .discussion
                .composer
                .as_ref()
                .unwrap()
                .authority
                .context
                .head_oid,
            "a".repeat(40)
        );
        assert_eq!(state.context.as_ref().unwrap().head_oid, "c".repeat(40));
    }

    #[test]
    fn changing_comment_range_preserves_text_and_cannot_retarget_a_refreshed_head() {
        let mut state = ready();
        drop(state.update_discussion(Action::Begin(anchor())));
        let composer = state.discussion.composer.as_mut().unwrap();
        composer.content = text_editor::Content::with_text("Keep this draft");
        composer.preview = true;
        let mut extended = anchor();
        extended.line = 9;
        drop(state.update_discussion(Action::Begin(extended.clone())));
        let composer = state.discussion.composer.as_ref().unwrap();
        assert_eq!(composer.content.text(), "Keep this draft");
        assert!(composer.preview);
        assert!(matches!(&composer.destination, Destination::Line(a) if a.line == 9));
        drop(state.update_discussion(Action::Reply("another-thread".into())));
        assert!(matches!(
            state.discussion.composer.as_ref().unwrap().destination,
            Destination::Line(_)
        ));
        let mut next = loaded(vec![entry("file.rs", ChangeKind::Modified)], vec![]);
        next.context.head_oid = "c".repeat(40);
        state.adopt(&next);
        extended.line = 10;
        drop(state.update_discussion(Action::Begin(extended)));
        let composer = state.discussion.composer.as_ref().unwrap();
        assert_eq!(composer.content.text(), "Keep this draft");
        assert_eq!(composer.authority.context.head_oid, "a".repeat(40));
        assert!(matches!(&composer.destination, Destination::Line(a) if a.line == 9));
    }

    #[test]
    fn unknown_write_does_not_offer_an_automatic_retry() {
        let mut state = ready();
        drop(state.update_discussion(Action::Begin(anchor())));
        state.discussion.busy = true;
        drop(state.update_discussion(Action::Finished {
            generation: state.generation,
            outcome: Outcome::Unknown("reply lost".into()),
        }));
        assert!(state.discussion.unknown.is_some());
        assert!(!state.can_mutate_discussion());
        drop(state.update_discussion(Action::Save));
        assert!(!state.discussion.busy);
        assert!(
            state.discussion.composer.is_some(),
            "keep the draft visible for comparison with GitHub"
        );
    }

    #[test]
    fn submission_requires_the_open_draft_to_be_saved_or_cancelled() {
        let mut state = ready();
        drop(state.update_discussion(Action::Begin(anchor())));
        drop(state.update_discussion(Action::PrepareSubmit));
        assert!(state.discussion.confirmation.is_none());
        assert!(state.notice.as_deref().unwrap().contains("Save or cancel"));
    }

    #[test]
    fn thread_index_keeps_sides_and_outdated_discussions_separate() {
        let mut state = ready();
        let thread = |id: &str, side, outdated| PullRequestReviewThread {
            id: id.into(),
            path: "file.rs".into(),
            line: Some(7),
            start_line: None,
            side,
            start_side: None,
            outdated,
            resolved: false,
            collapsed: false,
            viewer_can_resolve: true,
            viewer_can_unresolve: false,
            comments: vec![],
            truncated: false,
        };
        let conversation = PullRequestConversation {
            threads: vec![
                thread("left", ReviewDiffSide::Left, false),
                thread("right", ReviewDiffSide::Right, false),
                thread("outdated", ReviewDiffSide::Right, true),
            ],
            ..Default::default()
        };
        drop(state.update_discussion(Action::Loaded {
            generation: state.generation,
            load: state.load_generation,
            result: Ok(Arc::new(conversation)),
        }));
        let ids = |old, new, leading| {
            state
                .threads_on("file.rs", old, new, leading)
                .map(|t| t.id.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(Some(7), None, false), ["left"]);
        assert_eq!(ids(None, Some(7), false), ["right"]);
        assert_eq!(ids(None, None, true), ["outdated"]);
    }
}
