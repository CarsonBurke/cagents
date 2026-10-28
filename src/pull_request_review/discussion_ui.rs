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
    /// Starts a comment on the whole of the selected file.
    BeginFile(String),
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
    InsertSuggestion,
    SuggestionLines {
        generation: u64,
        composer_revision: u64,
        head: String,
        anchor: Anchor,
        original: String,
        result: Result<String, String>,
    },
    PreviewSuggestion {
        comment: String,
        index: usize,
    },
    SuggestionPreview {
        generation: u64,
        head: String,
        comment: String,
        index: usize,
        expected_body: String,
        result: Result<String, String>,
    },
    ApplySuggestion {
        comment: String,
        index: usize,
    },
    SuggestionApplied {
        generation: u64,
        outcome: Outcome,
    },
    Cancel,
    Save,
    SaveNow,
    Reanchor,
    Reanchored {
        generation: u64,
        composer_revision: u64,
        context: ReviewContext,
        result: Result<Anchor, String>,
    },
    PrepareDiscard,
    ConfirmDiscard,
    CancelDiscard,
    Summary(text_editor::Action),
    Event(ReviewEvent),
    /// Shows or hides the review submission popover.
    ToggleReview,
    CloseReview,
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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Destination {
    Line(Anchor),
    File(String),
    Reply(String),
    Edit(String),
}

struct Composer {
    authority: Authority,
    range: Option<PullRequestRange>,
    destination: Destination,
    original_body: Option<String>,
    content: text_editor::Content<cosmic::Renderer>,
    preview: bool,
    markdown: Vec<markdown::Item>,
}

#[derive(Default)]
struct FileThreads {
    leading: Vec<usize>,
    /// Preserve GitHub's order when anchors must fall back above the file.
    anchored: Vec<usize>,
    lines: HashMap<(crate::github::ReviewDiffSide, u64), Vec<usize>>,
    comments: usize,
    unresolved: usize,
}

pub(super) struct DiscussionState {
    revision: u64,
    pub(super) conversation: Option<Arc<PullRequestConversation>>,
    threads: HashMap<String, FileThreads>,
    markdown: HashMap<String, Vec<markdown::Item>>,
    expanded: BTreeSet<String>,
    composer: Option<Composer>,
    /// The submission surface is disclosed on demand rather than parked
    /// under every diff.
    review_open: bool,
    discard_confirmation: Option<(Authority, String)>,
    /// Why GitHub refused the last submission, shown inside the popover.
    submit_error: Option<String>,
    summary: text_editor::Content<cosmic::Renderer>,
    event: ReviewEvent,
    confirmation: Option<(
        Authority,
        SubmitReviewRequest,
        Vec<discussion::PendingComment>,
    )>,
    pub(super) busy: bool,
    submitting: bool,
    saving_composer: bool,
    suggestion_loading: bool,
    suggestion_previews: BTreeMap<(String, usize), (String, String, String)>,
    pub(super) unknown: Option<String>,
    loading: bool,
    drafts_saved: bool,
    /// Fences asynchronous anchor mapping to the draft and destination it read.
    composer_revision: u64,
    cache_version: Option<String>,
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
            review_open: false,
            discard_confirmation: None,
            submit_error: None,
            summary: text_editor::Content::new(),
            event: ReviewEvent::Comment,
            confirmation: None,
            busy: false,
            submitting: false,
            saving_composer: false,
            suggestion_loading: false,
            suggestion_previews: BTreeMap::new(),
            unknown: None,
            loading: false,
            drafts_saved: true,
            composer_revision: 0,
            cache_version: None,
        }
    }
}

impl DiscussionState {
    /// Park only local input; conversations and rendered threads reload on return.
    pub(super) fn into_draft_state(mut self) -> Self {
        Self {
            composer: self.composer.take(),
            summary: std::mem::take(&mut self.summary),
            event: self.event,
            review_open: self.review_open,
            drafts_saved: self.drafts_saved,
            cache_version: self.cache_version,
            ..Self::default()
        }
    }

    pub(super) fn has_conversation(&self) -> bool {
        self.conversation.is_some()
    }

    pub(super) fn has_local_work(&self) -> bool {
        self.composer.is_some() || !self.summary.text().is_empty()
    }
}

impl State {
    pub(super) fn discussion_is_busy(&self) -> bool {
        self.discussion.busy
    }
    pub(super) fn drafts_need_save(&self) -> bool {
        !self.discussion.drafts_saved
    }

    pub(super) fn draft_cache_basis(&self) -> Option<String> {
        self.discussion.cache_version.clone()
    }
    pub(super) fn drafts_cache_saved(&mut self, version: Option<String>) {
        self.discussion.drafts_saved = true;
        if version.is_some() {
            self.discussion.cache_version = version;
        }
    }

    pub(super) fn local_drafts(&self) -> Option<cache::Drafts> {
        (self.discussion.has_local_work() || self.discussion.unknown.is_some()).then(|| {
            cache::Drafts {
                composer: self
                    .discussion
                    .composer
                    .as_ref()
                    .map(|composer| cache::Draft {
                        context: composer.authority.context.clone(),
                        range: composer.range.clone(),
                        destination: composer.destination.clone(),
                        body: composer.content.text(),
                        original_body: composer.original_body.clone(),
                    }),
                summary: self.discussion.summary.text(),
                event: self.discussion.event,
                unknown: self.discussion.unknown.clone(),
            }
        })
    }
    pub(super) fn restore_drafts(
        &mut self,
        drafts: Option<cache::Drafts>,
        version: Option<String>,
    ) {
        if self.discussion.has_local_work() {
            return;
        }
        self.discussion.cache_version = version;
        let (Some(drafts), Some(target)) = (drafts, self.target.clone()) else {
            return;
        };
        self.discussion.summary = text_editor::Content::with_text(&drafts.summary);
        self.discussion.event = drafts.event;
        self.discussion.unknown = drafts.unknown;
        self.discussion.composer = drafts.composer.map(|draft| Composer {
            authority: Authority {
                target,
                context: draft.context,
                merge_base: draft.range.as_ref().map(|range| range.merge_base.clone()),
            },
            range: draft.range,
            destination: draft.destination,
            original_body: draft.original_body,
            content: text_editor::Content::with_text(&draft.body),
            preview: false,
            markdown: markdown::parse(&draft.body).collect(),
        });
    }

    /// Local review input and write receipts must survive until explicitly resolved.
    pub fn restart_blocker(&self) -> Option<&'static str> {
        if self.existing_checkout.busy() {
            Some(
                "Wait for checkout reconciliation to settle before closing or updating the window.",
            )
        } else if let Some(reason) = self.completion.blocker() {
            Some(reason)
        } else if self.discussion.busy {
            Some("Wait for the GitHub review write to finish before closing the window.")
        } else if self.discussion.unknown.is_some() {
            Some(
                "Check the unresolved review write on GitHub and acknowledge its outcome before closing the window.",
            )
        } else if (self.discussion.has_local_work() && self.cache.is_none())
            || (!self.discussion.drafts_saved && self.cache.is_some())
            || self.parked_discussions.iter().any(|(_, discussion)| {
                discussion.has_local_work() && (!discussion.drafts_saved || self.cache.is_none())
            })
        {
            Some(
                "Local review drafts are not saved yet. Wait for the cache write or keep this window open until its save error is resolved.",
            )
        } else {
            None
        }
    }

    pub(super) fn block_discussion_navigation(&mut self) -> bool {
        if self.existing_checkout.busy() {
            self.notice=Some("Wait for checkout reconciliation to settle before leaving this review. You can request cancellation below.".into());
            return true;
        }
        if let Some(reason) = self.completion.blocker() {
            self.notice = Some(reason.into());
            return true;
        }
        let message = if self.cache.is_some() && !self.discussion.drafts_saved {
            Some("Wait for the local review draft to finish saving before leaving this review.")
        } else if self.discussion.busy {
            Some("Wait for the GitHub write to finish before leaving this review.")
        } else if self.discussion.unknown.is_some() {
            Some(
                "Check the unresolved write on GitHub and acknowledge its outcome before leaving this review.",
            )
        } else {
            None
        };
        if let Some(message) = message {
            self.notice = Some(message.into());
            true
        } else {
            false
        }
    }
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
            merge_base: self.range.as_ref().map(|range| range.merge_base.clone()),
        })
    }

    fn can_mutate_discussion(&self) -> bool {
        self.load == LoadState::Ready
            && !self.discussion.busy
            && self.discussion.unknown.is_none()
            && self.completion.blocker().is_none()
    }

    fn can_discard_draft(&self) -> bool {
        !self.discussion.busy && self.discussion.unknown.is_none()
    }

    fn send_discussion(&mut self, authority: Authority, mutation: Mutation) -> ActionTask {
        if !self.can_mutate_discussion() {
            return ActionTask::none();
        }
        self.force_conversation = true;
        self.discussion.busy = true;
        self.discussion.submitting = matches!(mutation, Mutation::Submit { .. });
        self.discussion.saving_composer = matches!(
            mutation,
            Mutation::Comment { .. }
                | Mutation::FileComment { .. }
                | Mutation::Reply { .. }
                | Mutation::Edit { .. }
        );
        // A load started before this write must not install an older pending count.
        self.load_generation = self.load_generation.wrapping_add(1);
        self.refreshing = false;
        self.discussion.loading = false;
        self.pages.loading = false;
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
        if matches!(
            &action,
            Action::Cancel
                | Action::Begin(_)
                | Action::BeginFile(_)
                | Action::Reply(_)
                | Action::Edit(_)
                | Action::Reanchor
                | Action::AcknowledgeUnknown
        ) {
            self.discussion.composer_revision = self.discussion.composer_revision.wrapping_add(1);
            self.discussion.suggestion_loading = false;
        }
        if matches!(
            &action,
            Action::Composer(_)
                | Action::Summary(_)
                | Action::Cancel
                | Action::Finished { .. }
                | Action::AcknowledgeUnknown
                | Action::Event(_)
                | Action::Reanchored { .. }
                | Action::Begin(_)
                | Action::BeginFile(_)
                | Action::Reply(_)
                | Action::Edit(_)
        ) {
            self.cache_dirty = true;
            self.discussion.drafts_saved = false;
        }
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
                                file.anchored.push(index);
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
                        if let Some(target) = &self.target {
                            crate::github::polling::succeeded(&target.repository.host);
                        }
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
                        self.notice = Some(if self.discussion.submitting {
                            self.discussion.summary = text_editor::Content::new();
                            self.discussion.event = ReviewEvent::Comment;
                            self.discussion.review_open = false;
                            "Review submitted to GitHub.".into()
                        } else {
                            "Review updated on GitHub.".into()
                        });
                    }
                    // A refused submission is reported where it was made; the
                    // pane's notice would sit underneath the open popover.
                    Outcome::Refused(message) if self.discussion.submitting => {
                        self.discussion.submit_error = Some(message)
                    }
                    Outcome::Refused(message) => self.notice = Some(message),
                    // Every review write is blocked until this is resolved,
                    // and its resolution lives in the pane's notice slot.
                    Outcome::Unknown(message) => {
                        self.discussion.review_open = false;
                        self.discussion.unknown = Some(message);
                    }
                }
                if self.target.as_ref().is_some_and(|target| {
                    crate::github::polling::ready(&target.repository.host, false)
                }) {
                    self.refreshing = true;
                    return self.start_load();
                }
                self.refreshing = false;
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
            Action::CancelSubmit if !self.discussion.submitting || !self.discussion.busy => {
                self.discussion.confirmation = None;
            }
            Action::ToggleReview if !self.discussion.submitting || !self.discussion.busy => {
                self.discussion.review_open = !self.discussion.review_open;
                self.discussion.confirmation = None;
                self.discussion.submit_error = None;
            }
            Action::CloseReview if !self.discussion.submitting || !self.discussion.busy => {
                self.discussion.review_open = false;
                self.discussion.confirmation = None;
                self.discussion.submit_error = None;
            }
            Action::BeginFile(path) if self.drafting_file(&path) => {
                // The file's draft is already open: take the reader back to it.
                self.effects.push(Effect::RevealFileDiscussion);
            }
            // Drafting is local until it is saved, so a background refresh
            // must not swallow keystrokes. Only an in-flight write, whose
            // completion replaces the draft, holds them back.
            Action::Composer(action) if !self.discussion.busy => {
                if let Some(composer) = &mut self.discussion.composer {
                    composer.content.perform(action);
                    composer.markdown = markdown::parse(&composer.content.text()).collect();
                }
            }
            Action::Preview if !self.discussion.busy => {
                if let Some(composer) = &mut self.discussion.composer {
                    composer.preview = !composer.preview;
                }
            }
            Action::Summary(action) if !self.discussion.busy => {
                self.discussion.summary.perform(action)
            }
            // Discarding a draft sends nothing, so a refresh does not hold it.
            // A write whose outcome was lost does: the draft is what the
            // reader compares with GitHub.
            Action::Cancel if self.can_discard_draft() => self.discussion.composer = None,
            Action::OpenLink(raw) => {
                if let Ok(url) = Url::parse(&raw)
                    && matches!(url.scheme(), "https" | "http")
                {
                    self.effects.push(Effect::OpenUrl(url));
                }
            }
            Action::SuggestionLines {
                generation,
                composer_revision,
                head,
                anchor,
                original,
                result,
            } => {
                if generation != self.generation
                    || composer_revision != self.discussion.composer_revision
                {
                    return ActionTask::none();
                }
                self.discussion.suggestion_loading = false;
                if self
                    .context
                    .as_ref()
                    .is_none_or(|context| context.head_oid != head)
                {
                    self.notice = Some(
                        "The PR head changed. Re-anchor the draft before inserting selected lines."
                            .into(),
                    );
                    return ActionTask::none();
                }
                match result {
                    Ok(lines) => {
                        if let Some(composer) = &mut self.discussion.composer {
                            if composer.authority.context.head_oid == head
                                && matches!(&composer.destination, Destination::Line(current) if *current == anchor)
                                && composer.content.text() == original
                            {
                                let block =
                                    crate::github::suggested_changes::suggestion_block(&lines);
                                let body = if original.is_empty() {
                                    block
                                } else {
                                    format!("{original}\n\n{block}")
                                };
                                composer.content = text_editor::Content::with_text(&body);
                                composer.markdown = markdown::parse(&body).collect();
                                self.discussion.composer_revision =
                                    self.discussion.composer_revision.wrapping_add(1);
                                self.discussion.drafts_saved = false;
                                self.cache_dirty = true;
                            } else {
                                self.notice = Some("The draft changed while loading the selected lines. Insert the suggestion again.".into());
                            }
                        }
                    }
                    Err(error) => self.notice = Some(error),
                }
            }
            Action::SuggestionPreview {
                generation,
                head,
                comment,
                index,
                expected_body,
                result,
            } => {
                if generation != self.generation {
                    return ActionTask::none();
                }
                match result {
                    Ok(source) => {
                        self.discussion
                            .suggestion_previews
                            .insert((comment, index), (expected_body, head, source));
                    }
                    Err(error) => self.notice = Some(error),
                }
            }
            Action::SuggestionApplied {
                generation,
                outcome,
            } => {
                if generation != self.generation {
                    return ActionTask::none();
                }
                self.discussion.busy = false;
                match outcome {
                    Outcome::Applied => {
                        self.notice = Some("Suggestion committed to the PR branch.".into())
                    }
                    Outcome::Refused(error) => self.notice = Some(error),
                    Outcome::Unknown(error) => {
                        self.discussion.unknown = Some(error);
                        self.discussion.drafts_saved = false;
                        self.cache_dirty = true;
                    }
                }
                return self.refresh();
            }
            Action::Reanchored {
                generation,
                composer_revision,
                context,
                result,
            } => {
                if generation != self.generation
                    || composer_revision != self.discussion.composer_revision
                    || self
                        .context
                        .as_ref()
                        .is_none_or(|current| !same_diff_authority(current, &context))
                {
                    return ActionTask::none();
                }
                match result {
                    Ok(anchor) => {
                        if let Some(composer) = &mut self.discussion.composer {
                            composer.destination = Destination::Line(anchor);
                            composer.authority.context = context;
                            composer.authority.merge_base =
                                self.range.as_ref().map(|range| range.merge_base.clone());
                            composer.range = self.range.clone();
                            self.notice = Some("Draft re-anchored to the current head. Check its line before posting.".into());
                        }
                    }
                    Err(error) => self.notice = Some(error),
                }
            }
            Action::CancelDiscard => self.discussion.discard_confirmation = None,
            _ if !self.can_mutate_discussion() => return ActionTask::none(),
            Action::InsertSuggestion => {
                if self.discussion.suggestion_loading {
                    return ActionTask::none();
                }
                let Some(composer) = &self.discussion.composer else {
                    return ActionTask::none();
                };
                let Destination::Line(anchor) = &composer.destination else {
                    return ActionTask::none();
                };
                if anchor.side != crate::github::ReviewDiffSide::Right {
                    self.notice = Some(
                        "Suggestions can replace lines on the current right side of the diff."
                            .into(),
                    );
                    return ActionTask::none();
                }
                if self
                    .context
                    .as_ref()
                    .is_none_or(|context| context.head_oid != composer.authority.context.head_oid)
                {
                    self.notice=Some("Re-anchor the draft to the current PR head before inserting selected lines.".into());
                    return ActionTask::none();
                }
                let anchor = anchor.clone();
                let original = composer.content.text();
                let authority = composer.authority.clone();
                let head = authority.context.head_oid.clone();
                self.discussion.composer_revision =
                    self.discussion.composer_revision.wrapping_add(1);
                let composer_revision = self.discussion.composer_revision;
                let generation = self.generation;
                let api = Arc::clone(&self.api);
                let cancellation = self.cancellation.clone();
                self.discussion.suggestion_loading = true;
                return iced::Task::perform(
                    async move {
                        let requested = anchor.clone();
                        let result = tokio::task::spawn_blocking(move || {
                            let key = PullRequestKey::new(
                                authority.target.repository,
                                authority.target.pull_request.number,
                            )
                            .map_err(|error| error.to_string())?;
                            api.suggestion_lines(
                                &key,
                                &requested.path,
                                &authority.context.head_oid,
                                requested.start_line,
                                requested.line,
                                &cancellation,
                            )
                            .map_err(|error| error.to_string())
                        })
                        .await
                        .unwrap_or_else(|error| Err(error.to_string()));
                        Action::SuggestionLines {
                            generation,
                            composer_revision,
                            head,
                            anchor,
                            original,
                            result,
                        }
                    },
                    |action| super::Action::Discussion(action),
                );
            }
            Action::PreviewSuggestion { comment, index } => {
                let Some(authority) = self.authority() else {
                    return ActionTask::none();
                };
                let Some((thread, body)) =
                    self.discussion
                        .conversation
                        .as_ref()
                        .and_then(|conversation| {
                            conversation.threads.iter().find_map(|thread| {
                                thread
                                    .comments
                                    .iter()
                                    .find(|candidate| candidate.id.as_deref() == Some(&comment))
                                    .map(|comment| (thread.clone(), comment.body.clone()))
                            })
                        })
                else {
                    return ActionTask::none();
                };
                let Some((start, end)) = crate::github::suggested_changes::suggestions(&body)
                    .get(index)
                    .and_then(|suggestion| {
                        crate::github::suggested_changes::suggestion_range(&thread, suggestion)
                    })
                else {
                    return ActionTask::none();
                };
                let generation = self.generation;
                let api = Arc::clone(&self.api);
                let cancellation = self.cancellation.clone();
                let head = authority.context.head_oid.clone();
                return iced::Task::perform(
                    async move {
                        let result = tokio::task::spawn_blocking(move || {
                            let key = PullRequestKey::new(
                                authority.target.repository,
                                authority.target.pull_request.number,
                            )
                            .map_err(|error| error.to_string())?;
                            api.suggestion_lines(
                                &key,
                                &thread.path,
                                &authority.context.head_oid,
                                start,
                                end,
                                &cancellation,
                            )
                            .map_err(|error| error.to_string())
                        })
                        .await
                        .unwrap_or_else(|error| Err(error.to_string()));
                        Action::SuggestionPreview {
                            generation,
                            head,
                            comment,
                            index,
                            expected_body: body,
                            result,
                        }
                    },
                    |action| super::Action::Discussion(action),
                );
            }
            Action::ApplySuggestion { comment, index } => {
                if self.discussion.composer.is_some() {
                    self.notice = Some(
                        "Save or cancel the open comment before applying a suggestion.".into(),
                    );
                    return ActionTask::none();
                }
                let Some(authority) = self.authority() else {
                    return ActionTask::none();
                };
                if !authority.context.viewer_is_author {
                    return ActionTask::none();
                }
                let Some(body) = self
                    .discussion
                    .conversation
                    .as_ref()
                    .and_then(|conversation| {
                        conversation
                            .threads
                            .iter()
                            .filter(|thread| {
                                !thread.outdated
                                    && thread.side == crate::github::ReviewDiffSide::Right
                            })
                            .flat_map(|thread| &thread.comments)
                            .find(|candidate| {
                                candidate.id.as_deref() == Some(&comment) && !candidate.is_pending()
                            })
                    })
                    .map(|comment| comment.body.clone())
                else {
                    return ActionTask::none();
                };
                if crate::github::suggested_changes::suggestions(&body)
                    .get(index)
                    .is_none()
                {
                    return ActionTask::none();
                }
                let generation = self.generation;
                let api = Arc::clone(&self.api);
                self.discussion.busy = true;
                self.load_generation = self.load_generation.wrapping_add(1);
                self.discussion.loading = false;
                return iced::Task::perform(
                    async move {
                        let outcome = tokio::task::spawn_blocking(move || {
                            let key = PullRequestKey::new(
                                authority.target.repository,
                                authority.target.pull_request.number,
                            )
                            .map_err(|error| error.to_string())?;
                            api.apply_suggestion(
                                &key,
                                &crate::github::suggested_changes::SuggestionRequest {
                                    comment,
                                    expected_body: body,
                                    index,
                                    expected_head: authority.context.head_oid,
                                },
                                &CancellationToken::new(),
                            )
                            .map_err(|error| error.to_string())
                        })
                        .await
                        .unwrap_or_else(|error| Err(error.to_string()));
                        let outcome = match outcome {
                            Ok(ReviewMutationOutcome::Applied(())) => Outcome::Applied,
                            Ok(ReviewMutationOutcome::NotApplied { failure }) => {
                                Outcome::Refused(failure.to_string())
                            }
                            Ok(ReviewMutationOutcome::OutcomeUnknown { failure }) => {
                                Outcome::Unknown(failure.to_string())
                            }
                            Err(error) => Outcome::Refused(error),
                        };
                        Action::SuggestionApplied {
                            generation,
                            outcome,
                        }
                    },
                    |action| super::Action::Discussion(action),
                );
            }
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
                        && discussion::same_identity(
                            &composer.authority.context,
                            &authority.context,
                        )
                        .is_ok()
                    {
                        composer.destination = Destination::Line(anchor);
                        composer.authority = authority;
                        composer.range = self.range.clone();
                        self.cache_dirty = true;
                    } else {
                        self.notice = Some("Save or cancel the open comment first.".into());
                    }
                    return ActionTask::none();
                }
                self.discussion.composer = Some(Composer {
                    authority,
                    range: self.range.clone(),
                    destination: Destination::Line(anchor),
                    original_body: None,
                    content: text_editor::Content::new(),
                    preview: false,
                    markdown: Vec::new(),
                });
            }
            Action::BeginFile(path) => {
                if self
                    .selected
                    .as_ref()
                    .is_none_or(|selected| selected.as_path().to_str() != Some(path.as_str()))
                {
                    return ActionTask::none();
                }
                if self.discussion.composer.is_some() {
                    self.notice = Some("Save or cancel the open comment first.".into());
                    return ActionTask::none();
                }
                self.discussion.composer = Some(Composer {
                    authority: self.authority().expect("loaded review authority"),
                    range: self.range.clone(),
                    destination: Destination::File(path),
                    original_body: None,
                    content: text_editor::Content::new(),
                    preview: false,
                    markdown: Vec::new(),
                });
                // File discussions sit above the first diff row.
                self.effects.push(Effect::RevealFileDiscussion);
            }
            Action::Reply(id) => {
                if self.discussion.composer.is_some() {
                    self.notice = Some("Save or cancel the open comment first.".into());
                    return ActionTask::none();
                }
                self.discussion.expanded.insert(id.clone());
                self.discussion.composer = Some(Composer {
                    authority: self.authority().expect("loaded review authority"),
                    range: self.range.clone(),
                    destination: Destination::Reply(id),
                    original_body: None,
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
                        c.id.as_ref() == Some(&id) && c.viewer_did_author && c.viewer_can_update
                    })
                }) else {
                    return ActionTask::none();
                };
                self.discussion.composer = Some(Composer {
                    authority: self.authority().expect("loaded review authority"),
                    range: self.range.clone(),
                    destination: Destination::Edit(id),
                    original_body: Some(comment.body.clone()),
                    content: text_editor::Content::with_text(&comment.body),
                    preview: false,
                    markdown: markdown::parse(&comment.body).collect(),
                });
            }
            Action::Composer(_) | Action::Preview | Action::Summary(_) | Action::Cancel => {}
            Action::Event(event) => {
                self.discussion.event = event;
                self.discussion.confirmation = None;
                self.discussion.submit_error = None;
            }
            Action::Save | Action::SaveNow => {
                let single = matches!(action, Action::SaveNow);
                let Some(composer) = &self.discussion.composer else {
                    return ActionTask::none();
                };
                if matches!(
                    &composer.destination,
                    Destination::Line(_) | Destination::File(_)
                ) && (self
                    .context
                    .as_ref()
                    .is_none_or(|current| current.head_oid != composer.authority.context.head_oid)
                    || composer
                        .range
                        .as_ref()
                        .zip(self.range.as_ref())
                        .is_none_or(|(old, new)| old.merge_base != new.merge_base))
                {
                    self.notice = Some("The draft's diff changed. Re-anchor it to the current head, or select its new line in the gutter.".into());
                    return ActionTask::none();
                }
                let authority = composer.authority.clone();
                let body = composer.content.text();
                if body.trim().is_empty() {
                    return ActionTask::none();
                }
                let mutation = match &composer.destination {
                    Destination::Line(anchor) => Mutation::Comment {
                        anchor: anchor.clone(),
                        body,
                        single,
                    },
                    Destination::File(path) => Mutation::FileComment {
                        path: path.clone(),
                        body,
                        single,
                    },
                    Destination::Reply(thread) => Mutation::Reply {
                        thread: thread.clone(),
                        body,
                        add_to_review: !single,
                    },
                    Destination::Edit(comment) => Mutation::Edit {
                        comment: comment.clone(),
                        body,
                        expected_body: composer.original_body.clone(),
                    },
                };
                return self.send_discussion(authority, mutation);
            }
            Action::PrepareDiscard => {
                if let Some(authority) = self.authority()
                    && let Some(pending) = &authority.context.pending
                {
                    self.discussion.discard_confirmation =
                        Some((authority.clone(), pending.id.clone()));
                }
            }
            Action::ConfirmDiscard => {
                if let Some((authority, review_id)) = self.discussion.discard_confirmation.take() {
                    return self.send_discussion(authority, Mutation::Discard { review_id });
                }
            }
            Action::Reanchor => {
                let Some(composer) = &self.discussion.composer else {
                    return ActionTask::none();
                };
                if !discussion::same_identity(
                    &composer.authority.context,
                    &self.context.clone().expect("loaded context"),
                )
                .is_ok()
                {
                    self.notice = Some("The account or pull request changed. Restore that account before using this draft.".into());
                    return ActionTask::none();
                }
                let Some(authority) = self.authority() else {
                    return ActionTask::none();
                };
                if matches!(composer.destination, Destination::File(_)) {
                    let composer = self.discussion.composer.as_mut().unwrap();
                    composer.authority = authority;
                    composer.range = self.range.clone();
                    self.cache_dirty = true;
                    self.discussion.drafts_saved = false;
                    return ActionTask::none();
                }
                let Destination::Line(anchor) = &composer.destination else {
                    return ActionTask::none();
                };
                let (Some(old), Some(new)) = (composer.range.clone(), self.range.clone()) else {
                    return ActionTask::none();
                };
                let anchor = anchor.clone();
                let git = self.git.clone();
                let cancellation = self.cancellation.clone();
                let worktree = authority.target.worktree.clone();
                let composer_revision = self.discussion.composer_revision;
                let generation = self.generation;
                let context = authority.context;
                return ActionTask::perform(
                    async move {
                        let result = tokio::task::spawn_blocking(move || {
                            discussion::reanchor(
                                &git,
                                &worktree,
                                &old,
                                &new,
                                &anchor,
                                &cancellation,
                            )
                        })
                        .await
                        .unwrap_or_else(|error| Err(error.to_string()));
                        super::Action::Discussion(Action::Reanchored {
                            generation,
                            composer_revision,
                            context,
                            result,
                        })
                    },
                    |action| action,
                );
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
                self.discussion.submit_error = None;
                if self.discussion.composer.is_some() {
                    self.notice = Some(
                        "Save or cancel the open comment before submitting your review.".into(),
                    );
                    return ActionTask::none();
                }
                if self.discussion.event == ReviewEvent::Comment
                    && self.discussion.summary.text().trim().is_empty()
                    && self.pending_count() == 0
                {
                    self.discussion.submit_error = Some("Write a summary or add a pending comment before submitting a Comment review.".into());
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
                // The confirmation stays on screen, reading "Submitting…",
                // until the outcome arrives and clears it.
                if let Some((authority, request, comments)) = self.discussion.confirmation.clone() {
                    self.discussion.submit_error = None;
                    return self.send_discussion(authority, Mutation::Submit { request, comments });
                }
            }
            Action::AcknowledgeUnknown
            | Action::ToggleReview
            | Action::CloseReview
            | Action::CancelSubmit => {}
        }
        ActionTask::none()
    }

    fn pending_count(&self) -> u64 {
        self.context
            .as_ref()
            .and_then(|context| context.pending.as_ref())
            .map_or(0, |pending| pending.comment_count)
    }

    /// The pull request's review entry point. Submission is disclosed on
    /// demand: a quiet button until comments are pending, then a primary
    /// one that carries their count, like a forge's "Finish your review".
    pub fn review_control(&self, modal_open: bool) -> Element<'_, super::Action> {
        let pending = self.pending_count();
        let available = !modal_open && self.authority().is_some();
        let mut label: Vec<Element<'_, Action>> = vec![
            design::body(if pending == 0 {
                "Review"
            } else {
                "Finish review"
            })
            .wrapping(iced::widget::text::Wrapping::None)
            .into(),
        ];
        if pending > 0 {
            label.push(count_pill(pending));
        }
        let button = button::custom(
            widget::row::with_children(label)
                .spacing(6)
                .align_y(Alignment::Center),
        )
        .class(if pending > 0 {
            cosmic::theme::Button::Suggested
        } else {
            cosmic::theme::Button::Standard
        })
        .padding([3, 10])
        .selected(self.discussion.review_open)
        .on_press_maybe(available.then_some(Action::ToggleReview));
        let tip = if self.authority().is_none() {
            "Reviewing needs this pull request loaded from GitHub"
        } else if pending == 0 {
            "Approve, request changes, or comment on the pull request"
        } else {
            "Submit your pending comments as a review"
        };
        let anchor = crate::interaction::tooltip(button, tip);
        let popup = (self.discussion.review_open && !modal_open).then(|| self.review_popup());
        let element = crate::ui_controls::popover(
            anchor,
            popup,
            widget::popover::Position::Point(iced::Point::new(0.0, REVIEW_POPUP_OFFSET)),
            Action::CloseReview,
        );
        element.map(|action| self.review_action(action))
    }

    fn review_popup(&self) -> Element<'_, Action> {
        let enabled = self.can_mutate_discussion();
        let pending = self.pending_count();
        let mut rows: Vec<Element<'_, Action>> = Vec::new();
        if let Some((authority, _)) = &self.discussion.discard_confirmation {
            rows.push(design::heading("Discard pending review?").into());
            rows.push(design::body(format!("This deletes the pending review and all {} draft comments from GitHub. Your local composer is kept.", authority.context.pending.as_ref().map_or(0, |pending| pending.comment_count))).into());
            rows.push(popup_actions(
                crate::ui_controls::button::secondary("Keep review")
                    .on_press(Action::CancelDiscard),
                crate::ui_controls::button::primary("Discard review")
                    .on_press_maybe(enabled.then_some(Action::ConfirmDiscard)),
            ));
        } else if let Some((authority, request, _)) = &self.discussion.confirmation {
            let comments = match authority
                .context
                .pending
                .as_ref()
                .map_or(0, |p| p.comment_count)
            {
                0 => "no pending comments".to_owned(),
                1 => "1 pending comment".to_owned(),
                count => format!("{count} pending comments"),
            };
            rows.push(design::heading(format!("{}?", request.event.label())).into());
            rows.push(
                design::body(format!(
                    "Submits to {}/{} #{} at commit {} with {comments}.",
                    authority.target.repository.owner,
                    authority.target.repository.name,
                    authority.target.pull_request.number,
                    request.commit_oid.get(..7).unwrap_or(&request.commit_oid),
                ))
                .wrapping(iced::widget::text::Wrapping::Word)
                .into(),
            );
            if !request.body.trim().is_empty() {
                rows.push(
                    widget::container(
                        design::body(request.body.as_str())
                            .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
                    )
                    .padding([space::S, space::M])
                    .width(Length::Fill)
                    .max_height(160)
                    .class(cosmic::theme::Container::custom(|theme| {
                        widget::container::Style {
                            background: Some(iced::Background::Color(design::sunken(theme))),
                            border: iced::Border {
                                radius: design::RADIUS_XS.into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        }
                    }))
                    .into(),
                );
            }
            rows.push(popup_actions(
                crate::ui_controls::button::secondary("Back")
                    .on_press_maybe((!self.discussion.busy).then_some(Action::CancelSubmit)),
                crate::ui_controls::button::primary(if self.discussion.busy {
                    "Submitting…"
                } else {
                    "Submit review"
                })
                .on_press_maybe(enabled.then_some(Action::ConfirmSubmit)),
            ));
        } else {
            rows.push(design::heading("Finish your review").into());
            if let Some(pending) = self
                .context
                .as_ref()
                .and_then(|context| context.pending.as_ref())
            {
                let commit = pending.commit_oid.as_deref().unwrap_or("unknown");
                let stale = self.context.as_ref().is_some_and(|context| {
                    Some(context.head_oid.as_str()) != pending.commit_oid.as_deref()
                });
                rows.push(design::body(format!("You have a pending review with {} comments on {}.{}", pending.comment_count, commit.get(..7).unwrap_or(commit), if stale { " This review belongs to an old head. Discard it to start a review on the current head." } else { "" })).into());
                rows.push(
                    crate::ui_controls::button::secondary("Discard pending review…")
                        .on_press_maybe(enabled.then_some(Action::PrepareDiscard))
                        .into(),
                );
            }
            rows.push(
                design::caption(match pending {
                    0 => "No pending comments. Comment on a line from the diff gutter, or on a whole file from its toolbar.".to_owned(),
                    1 => "1 pending comment will be published with this review.".to_owned(),
                    count => format!("{count} pending comments will be published with this review."),
                })
                .wrapping(iced::widget::text::Wrapping::Word)
                .into(),
            );
            let own = self.context.as_ref().is_some_and(|c| c.viewer_is_author);
            if own {
                rows.push(design::caption("GitHub allows comments on your own pull request, but not approval or requests for changes.").into());
            }
            let mut editor = text_editor::TextEditor::new(&self.discussion.summary)
                .placeholder("Leave a comment on the pull request (optional)")
                .padding(space::M)
                .height(96);
            if !self.discussion.busy {
                editor = editor.on_action(Action::Summary);
            }
            rows.push(editor.into());
            let mut choices: Vec<Element<'_, Action>> = [
                (
                    ReviewEvent::Comment,
                    "Submit general feedback without explicit approval.",
                ),
                (
                    ReviewEvent::Approve,
                    "Give your approval to merge these changes.",
                ),
                (
                    ReviewEvent::RequestChanges,
                    "Submit feedback that must be addressed before merging.",
                ),
            ]
            .into_iter()
            .map(|(event, description)| {
                event_choice(
                    event,
                    description,
                    event == self.discussion.event,
                    enabled && (!own || event == ReviewEvent::Comment),
                )
            })
            .collect();
            if own {
                choices.push(
                    design::caption(
                        "You can’t approve or request changes on your own pull request.",
                    )
                    .wrapping(iced::widget::text::Wrapping::Word)
                    .into(),
                );
            }
            rows.push(
                widget::column::with_children(choices)
                    .spacing(space::XXS)
                    .into(),
            );
            let drafting = self.discussion.composer.is_some();
            if drafting {
                if let Some(composer) = &self.discussion.composer {
                    let mut editor = text_editor::TextEditor::new(&composer.content).height(100);
                    if !self.discussion.busy {
                        editor = editor.on_action(Action::Composer);
                    }
                    rows.push(editor.into());
                    rows.push(
                        crate::ui_controls::button::quiet("Discard local comment")
                            .on_press_maybe(self.can_discard_draft().then_some(Action::Cancel))
                            .into(),
                    );
                }
                rows.push(
                    design::caption("Save or cancel the comment you’re writing before submitting.")
                        .class(cosmic::theme::Text::Custom(design::warning_text))
                        .wrapping(iced::widget::text::Wrapping::Word)
                        .into(),
                );
            }
            let empty_comment = self.discussion.event == ReviewEvent::Comment
                && self.discussion.summary.text().trim().is_empty()
                && pending == 0;
            if empty_comment {
                rows.push(
                    design::caption(
                        "Write a summary or add a pending comment to submit a Comment review.",
                    )
                    .into(),
                );
            }
            rows.push(popup_actions(
                crate::ui_controls::button::secondary("Cancel").on_press(Action::CloseReview),
                crate::interaction::tooltip(
                    crate::ui_controls::button::primary(if self.discussion.busy {
                        "Saving…"
                    } else {
                        "Submit review"
                    })
                    .on_press_maybe(
                        (enabled && !drafting && !empty_comment).then_some(Action::PrepareSubmit),
                    ),
                    if empty_comment {
                        "Write a summary or add a pending comment to submit a Comment review."
                    } else {
                        "Confirm and submit this review"
                    },
                ),
            ));
        }
        if let Some(error) = &self.discussion.submit_error {
            // Just above the actions, where the eye returns after submitting.
            rows.insert(
                rows.len() - 1,
                design::caption(format!("GitHub didn’t accept the review. {error}"))
                    .class(cosmic::theme::Text::Custom(design::warning_text))
                    .wrapping(iced::widget::text::Wrapping::Word)
                    .into(),
            );
        }
        widget::container(widget::column::with_children(rows).spacing(space::M))
            .padding(space::S)
            .width(Length::Fixed(REVIEW_POPUP_WIDTH))
            .into()
    }

    /// Starts a discussion on the selected file as a whole. Line comments
    /// begin in the diff gutter; this is the file header's counterpart.
    ///
    /// `diff_can_show` says whether the diff beside it can render a file
    /// discussion. Where it cannot, a new draft would be invisible yet hold
    /// every other comment and the submission, so the control is withheld;
    /// an existing draft keeps its control so it is never lost track of.
    pub fn file_comment_control(
        &self,
        modal_open: bool,
        diff_can_show: bool,
    ) -> Option<Element<'_, super::Action>> {
        let path = self.selected.as_ref()?.as_path().to_str()?.to_owned();
        let drafting_here = self.drafting_file(&path);
        if !diff_can_show && !drafting_here {
            return None;
        }
        // While this file's draft is open the control stays live and leads
        // back to it; any other open draft must be finished first.
        let enabled = !modal_open
            && self.authority().is_some()
            && (drafting_here
                || (self.can_mutate_discussion() && self.discussion.composer.is_none()));
        let control: Element<'_, Action> = crate::ui_controls::icon_button(
            Icon::Comment,
            if drafting_here {
                "Show the comment on this file"
            } else {
                "Comment on this file"
            },
        )
        .selected(drafting_here)
        .on_press_maybe(enabled.then_some(Action::BeginFile(path)))
        .into();
        Some(control.map(|action| self.review_action(action)))
    }

    fn drafting_file(&self, path: &str) -> bool {
        self.discussion.composer.as_ref().is_some_and(
            |composer| matches!(&composer.destination, Destination::File(file) if file == path),
        )
    }

    /// A write whose outcome was lost blocks every other review write until
    /// the reader has compared the draft with GitHub, so it is shown with the
    /// pane's notices rather than inside a surface that may be closed.
    pub(super) fn discussion_notice(&self, modal_open: bool) -> Option<Element<'_, super::Action>> {
        let message = self.discussion.unknown.as_ref()?;
        let element: Element<'_, Action> = widget::column::with_children(vec![
            design::caption(format!(
                "GitHub may have received the last review change. Check the discussion before continuing. {message}"
            ))
            .class(cosmic::theme::Text::Custom(design::warning_text))
            .wrapping(iced::widget::text::Wrapping::Word)
            .width(Length::Fill)
            .into(),
            crate::ui_controls::button::quiet("I checked GitHub; continue")
                .on_press_maybe(
                    (!modal_open && !self.discussion.busy).then_some(Action::AcknowledgeUnknown),
                )
                .into(),
        ])
        .spacing(space::XS)
        .padding(Padding::from([space::XS, space::M]))
        .into();
        Some(element.map(|action| self.review_action(action)))
    }
}

/// Where the submission popover opens, below its anchor button.
const REVIEW_POPUP_OFFSET: f32 = 30.0;
const REVIEW_POPUP_WIDTH: f32 = 380.0;

fn count_pill<'a>(count: u64) -> Element<'a, Action> {
    widget::container(
        design::caption(count.to_string())
            .wrapping(iced::widget::text::Wrapping::None)
            .class(cosmic::theme::Text::Default),
    )
    .padding([0, 5])
    .class(cosmic::theme::Container::custom(|_| {
        widget::container::Style {
            // The pill sits on the accent fill of a primary button, so it
            // shades that fill rather than adding a hue of its own.
            background: Some(iced::Background::Color(iced::Color::from_rgba(
                0.0, 0.0, 0.0, 0.2,
            ))),
            border: iced::Border {
                radius: design::RADIUS_L.into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }))
    .into()
}

/// One review verdict: a radio mark, its name, and what it tells the author.
fn event_choice<'a>(
    event: ReviewEvent,
    description: &'a str,
    selected: bool,
    enabled: bool,
) -> Element<'a, Action> {
    let mark = widget::container(
        widget::container(widget::Space::new())
            .width(6)
            .height(6)
            .class(cosmic::theme::Container::custom(move |theme| {
                widget::container::Style {
                    background: selected
                        .then(|| iced::Background::Color(design::accent_color(theme))),
                    border: iced::Border {
                        radius: 3.0.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                }
            })),
    )
    .center(Length::Fixed(14.0))
    .class(cosmic::theme::Container::custom(move |theme| {
        widget::container::Style {
            border: iced::Border {
                color: if selected {
                    design::accent_color(theme)
                } else {
                    design::muted_icon_color(theme)
                },
                width: 1.5,
                radius: 7.0.into(),
            },
            ..Default::default()
        }
    }));
    let mut title = design::body(event.label());
    let mut detail = design::caption(description).wrapping(iced::widget::text::Wrapping::Word);
    if !enabled {
        title = title.class(cosmic::theme::Text::Custom(unavailable_text));
        detail = detail.class(cosmic::theme::Text::Custom(unavailable_text));
    }
    let content = widget::row::with_children(vec![
        widget::container(mark)
            .padding(Padding {
                top: space::XXS,
                ..Padding::ZERO
            })
            .into(),
        widget::column::with_children(vec![title.into(), detail.into()])
            .spacing(1)
            .width(Length::Fill)
            .into(),
    ])
    .spacing(10)
    .align_y(Alignment::Start);
    if !enabled {
        // An unavailable verdict is plain, dimmed text rather than a
        // disabled button, whose own palette would override the dimming.
        return widget::container(content)
            .padding([5.0, space::S])
            .width(Length::Fill)
            .into();
    }
    button::custom(content)
        .class(crate::ui_controls::control_style(false, None))
        .padding([5.0, space::S])
        .width(Length::Fill)
        .on_press(Action::Event(event))
        .into()
}

fn unavailable_text(theme: &cosmic::Theme) -> iced::widget::text::Style {
    let mut color = design::muted_color(theme);
    color.a *= 0.55;
    iced::widget::text::Style {
        color: Some(color),
        ..Default::default()
    }
}

/// A popover's dismissive and committing actions, the commit at the trailing
/// edge where the eye finishes reading.
fn popup_actions<'a>(
    dismiss: impl Into<Element<'a, Action>>,
    commit: impl Into<Element<'a, Action>>,
) -> Element<'a, Action> {
    widget::row::with_children(vec![
        widget::Space::new().width(Length::Fill).into(),
        dismiss.into(),
        commit.into(),
    ])
    .spacing(space::S)
    .align_y(Alignment::Center)
    .into()
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
        self.composer_at(path, old, new, leading, None)
    }

    fn composer_stale(&self, composer: &Composer) -> bool {
        self.context.as_ref().is_some_and(|context| {
            context.head_oid != composer.authority.context.head_oid
                || composer
                    .range
                    .as_ref()
                    .zip(self.range.as_ref())
                    .is_none_or(|(a, b)| a.merge_base != b.merge_base)
        })
    }

    fn composer_at(
        &self,
        path: &str,
        old: Option<u32>,
        new: Option<u32>,
        leading: bool,
        bounds: Option<[usize; 2]>,
    ) -> bool {
        let Some(composer) = &self.discussion.composer else {
            return false;
        };
        match &composer.destination {
            Destination::Line(anchor) => {
                if self.composer_stale(composer) {
                    return leading;
                }
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
            Destination::File(file) => leading && (file == path || self.composer_stale(composer)),
            Destination::Reply(id) => self
                .threads_at(path, old, new, leading, bounds)
                .any(|t| &t.id == id),
            Destination::Edit(id) => self
                .threads_at(path, old, new, leading, bounds)
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
        self.threads_at(path, old, new, leading, None)
    }

    fn threads_at<'a>(
        &'a self,
        path: &'a str,
        old: Option<u32>,
        new: Option<u32>,
        leading: bool,
        bounds: Option<[usize; 2]>,
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
            .chain(
                file.into_iter()
                    .filter(move |_| leading && bounds.is_some())
                    .flat_map(move |file| {
                        file.anchored.iter().filter(move |index| {
                            let Some(thread) = self
                                .discussion
                                .conversation
                                .as_ref()
                                .and_then(|conversation| conversation.threads.get(**index))
                            else {
                                return false;
                            };
                            let counts = bounds.unwrap_or_default();
                            let count = counts[usize::from(thread.side == Right)];
                            thread
                                .line
                                .is_some_and(|line| line == 0 || line > count as u64)
                        })
                    }),
            )
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

    /// How many discussions, counting an open composer, are anchored to a
    /// line in `old` or `new`, so a diff that folds those lines away can say
    /// what it is hiding. Costs one pass over the file's anchored lines.
    pub fn discussions_within(
        &self,
        path: &str,
        old: std::ops::RangeInclusive<u32>,
        new: std::ops::RangeInclusive<u32>,
    ) -> usize {
        use crate::github::ReviewDiffSide::{Left, Right};
        let within = |side, line: u64| {
            let range = match side {
                Left => &old,
                Right => &new,
            };
            u32::try_from(line).is_ok_and(|line| range.contains(&line))
        };
        let threads = self.discussion.threads.get(path).map_or(0, |file| {
            file.lines
                .iter()
                .filter(|((side, line), _)| within(*side, *line))
                .map(|(_, threads)| threads.len())
                .sum()
        });
        let composer = self.discussion.composer.as_ref().is_some_and(|composer| {
            matches!(&composer.destination, Destination::Line(anchor)
                if anchor.path == path && within(anchor.side, anchor.line))
        });
        threads + usize::from(composer)
    }

    /// Natural-height content; the diff measures this before reserving space.
    pub fn annotation<'a>(
        &'a self,
        path: &'a str,
        old: Option<u32>,
        new: Option<u32>,
        leading: bool,
    ) -> Element<'a, super::Action> {
        self.annotation_at(path, old, new, leading, None)
    }

    pub fn has_leading_annotation(&self, path: &str, bounds: [usize; 2]) -> bool {
        self.threads_at(path, None, None, true, Some(bounds))
            .next()
            .is_some()
            || self.composer_at(path, None, None, true, Some(bounds))
    }

    pub fn leading_annotation<'a>(
        &'a self,
        path: &'a str,
        bounds: [usize; 2],
    ) -> Element<'a, super::Action> {
        self.annotation_at(path, None, None, true, Some(bounds))
    }

    fn annotation_at<'a>(
        &'a self,
        path: &'a str,
        old: Option<u32>,
        new: Option<u32>,
        leading: bool,
        bounds: Option<[usize; 2]>,
    ) -> Element<'a, super::Action> {
        let enabled = self.can_mutate_discussion();
        let mut rows: Vec<Element<'a, Action>> = Vec::new();
        for thread in self.threads_at(path, old, new, leading, bounds) {
            let expanded = !thread.resolved || self.discussion.expanded.contains(&thread.id);
            let mut label = format!(
                "{}{} · {} comments",
                if thread.outdated { "Outdated · " } else { "" },
                if thread.resolved {
                    "Resolved"
                } else {
                    "Discussion"
                },
                thread.comments.len()
            );
            if leading && !thread.outdated {
                label = match thread.line {
                    Some(line) => format!("Line {line} · unavailable in this diff · {label}"),
                    // GitHub reports no line, current or original, only for
                    // a discussion of the file as a whole.
                    None => format!("Whole file · {label}"),
                };
            }
            rows.push(
                crate::ui_controls::button::quiet(label)
                    .on_press(Action::ToggleThread(thread.id.clone()))
                    .into(),
            );
            if !expanded {
                continue;
            }
            for comment in &thread.comments {
                let body = if let Some(items) = comment
                    .id
                    .as_ref()
                    .and_then(|id| self.discussion.markdown.get(id))
                {
                    let theme = cosmic::theme::active();
                    markdown::view(items, design::markdown_settings(&theme, design::TEXT_BODY))
                        .map(|uri| Action::OpenLink(uri.to_string()))
                } else {
                    design::body(&comment.body).into()
                };
                let actions = if comment.is_pending() {
                    design::caption("Pending").into()
                } else {
                    cosmic::iced::widget::Space::new().into()
                };
                rows.push(crate::ui_controls::comment_card(
                    comment
                        .author
                        .as_ref()
                        .map_or("Deleted account", |author| author.login.as_str()),
                    &comment.created_at,
                    body,
                    actions,
                ));
                for (index, suggestion) in
                    crate::github::suggested_changes::suggestions(&comment.body)
                        .iter()
                        .enumerate()
                {
                    rows.push(design::caption("Suggested replacement").into());
                    let preview = comment
                        .id
                        .as_ref()
                        .and_then(|id| {
                            self.discussion
                                .suggestion_previews
                                .get(&(id.clone(), index))
                        })
                        .filter(|(body, head, _)| {
                            body == &comment.body
                                && self
                                    .context
                                    .as_ref()
                                    .is_some_and(|context| &context.head_oid == head)
                        });
                    let text = preview.map_or_else(
                        || {
                            suggestion
                                .replacement
                                .lines()
                                .map(|line| format!("+ {line}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        },
                        |(_, _, source)| {
                            crate::github::suggested_changes::suggestion_diff(
                                source,
                                &suggestion.replacement,
                            )
                        },
                    );
                    rows.push(
                        widget::container(design::body(text))
                            .padding(space::S)
                            .into(),
                    );
                    if preview.is_none()
                        && let Some(id) = &comment.id
                        && crate::github::suggested_changes::suggestion_range(thread, suggestion)
                            .is_some()
                    {
                        rows.push(
                            crate::ui_controls::button::quiet("Preview change")
                                .on_press_maybe(enabled.then(|| Action::PreviewSuggestion {
                                    comment: id.clone(),
                                    index,
                                }))
                                .into(),
                        );
                    }
                    if let Some(id) = &comment.id
                        && !comment.is_pending()
                        && !thread.outdated
                        && thread.side == crate::github::ReviewDiffSide::Right
                        && self
                            .context
                            .as_ref()
                            .is_some_and(|context| context.viewer_is_author)
                    {
                        rows.push(
                            crate::ui_controls::button::secondary("Apply suggestion")
                                .on_press_maybe((enabled && preview.is_some()).then(|| {
                                    Action::ApplySuggestion {
                                        comment: id.clone(),
                                        index,
                                    }
                                }))
                                .into(),
                        );
                    }
                }
                if comment.viewer_did_author
                    && let Some(id) = &comment.id
                {
                    rows.push(
                        widget::row::with_children(vec![
                            crate::ui_controls::button::quiet("Edit")
                                .on_press_maybe(
                                    (enabled && comment.viewer_can_update)
                                        .then_some(Action::Edit(id.clone())),
                                )
                                .into(),
                            crate::ui_controls::button::quiet("Delete draft")
                                .on_press_maybe(
                                    (enabled && comment.is_pending() && comment.viewer_can_delete)
                                        .then_some(Action::Delete(id.clone())),
                                )
                                .into(),
                        ])
                        .spacing(space::S)
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
                    crate::ui_controls::button::quiet("Reply")
                        .on_press_maybe(enabled.then_some(Action::Reply(thread.id.clone())))
                        .into(),
                    crate::ui_controls::button::quiet(if thread.resolved {
                        "Reopen"
                    } else {
                        "Resolve"
                    })
                    .on_press_maybe((enabled && can_resolve).then_some(Action::Resolve {
                        thread: thread.id.clone(),
                        resolved: !thread.resolved,
                    }))
                    .into(),
                ])
                .spacing(space::S)
                .into(),
            );
            if thread.truncated {
                rows.push(design::caption("More comments are available on GitHub.").into());
            }
        }
        let has_pending_review = self
            .context
            .as_ref()
            .is_some_and(|context| context.pending.is_some());
        if self.composer_at(path, old, new, leading, bounds)
            && let Some(composer) = &self.discussion.composer
        {
            let label = match &composer.destination {
                Destination::Line(anchor) if anchor.start_line != anchor.line => {
                    format!("New comment · lines {}–{}", anchor.start_line, anchor.line)
                }
                Destination::Line(anchor) => format!("New comment · line {}", anchor.line),
                Destination::File(_) => "New comment · whole file".into(),
                Destination::Reply(_) => "Reply".into(),
                Destination::Edit(_) => "Edit comment".into(),
            };
            rows.push(design::heading(label).into());
            let stale = matches!(
                composer.destination,
                Destination::Line(_) | Destination::File(_)
            ) && self.context.as_ref().is_some_and(|context| {
                context.head_oid != composer.authority.context.head_oid
                    || composer
                        .range
                        .as_ref()
                        .zip(self.range.as_ref())
                        .is_none_or(|(a, b)| a.merge_base != b.merge_base)
            });
            if stale {
                rows.push(design::caption("The draft is kept, but its diff changed. Re-anchor it automatically, or pick a new line in the gutter.").into());
                rows.push(
                    crate::ui_controls::button::secondary("Re-anchor to current head")
                        .on_press_maybe(enabled.then_some(Action::Reanchor))
                        .into(),
                );
            }
            if composer.preview {
                rows.push(
                    markdown::view(
                        &composer.markdown,
                        crate::design::markdown_settings(
                            &cosmic::theme::active(),
                            crate::design::TEXT_BODY,
                        ),
                    )
                    .map(|uri| Action::OpenLink(uri.to_string())),
                );
            } else {
                let mut editor = text_editor::TextEditor::new(&composer.content)
                    .placeholder("Leave a review comment…")
                    .height(140);
                if !self.discussion.busy {
                    editor = editor.on_action(Action::Composer);
                }
                rows.push(editor.into());
            }
            rows.push(
                widget::row::with_children(vec![
                    crate::ui_controls::button::quiet(if self.discussion.suggestion_loading { "Loading selected lines…" } else { "Insert suggestion" })
                        .on_press_maybe((enabled && !self.discussion.suggestion_loading && matches!(&composer.destination, Destination::Line(anchor) if anchor.side == crate::github::ReviewDiffSide::Right)).then_some(Action::InsertSuggestion))
                        .into(),
                    crate::ui_controls::button::primary(
                        if matches!(composer.destination, Destination::Edit(_)) {
                            "Save comment"
                        } else if has_pending_review {
                            "Add to review"
                        } else {
                            "Comment"
                        },
                    )
                    .on_press_maybe(
                        (enabled && !stale && !composer.content.text().trim().is_empty())
                            .then_some(
                                if matches!(composer.destination, Destination::Edit(_))
                                    || has_pending_review
                                {
                                    Action::Save
                                } else {
                                    Action::SaveNow
                                },
                            ),
                    )
                    .into(),
                    crate::ui_controls::button::secondary(if has_pending_review {
                        "Comment now"
                    } else {
                        "Add to review"
                    })
                    .on_press_maybe(
                        (enabled
                            && !stale
                            && !matches!(composer.destination, Destination::Edit(_))
                            && !composer.content.text().trim().is_empty())
                        .then_some(if has_pending_review {
                            Action::SaveNow
                        } else {
                            Action::Save
                        }),
                    )
                    .into(),
                    // Local to the draft, so live through a background
                    // refresh, as typing is; only a write in flight holds them.
                    crate::ui_controls::button::quiet(if composer.preview {
                        "Write"
                    } else {
                        "Preview"
                    })
                    .on_press_maybe((!self.discussion.busy).then_some(Action::Preview))
                    .into(),
                    crate::ui_controls::button::secondary("Cancel")
                        .on_press_maybe(self.can_discard_draft().then_some(Action::Cancel))
                        .into(),
                ])
                .spacing(space::S)
                .into(),
            );
        }
        let element: Element<'_, Action> =
            widget::container(widget::column::with_children(rows).spacing(space::S))
                .padding(space::M)
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
    fn stale_suggestion_lines_cannot_replace_a_new_composer_at_the_same_anchor() {
        let mut state = ready();
        drop(state.update_discussion(Action::Begin(anchor())));
        drop(state.update_discussion(Action::InsertSuggestion));
        let revision = state.discussion.composer_revision;
        let head = state.context.as_ref().unwrap().head_oid.clone();
        drop(state.update_discussion(Action::Cancel));
        drop(state.update_discussion(Action::Begin(anchor())));
        drop(state.update_discussion(Action::InsertSuggestion));
        assert!(state.discussion.suggestion_loading);
        drop(state.update_discussion(Action::SuggestionLines {
            generation: state.generation,
            composer_revision: revision,
            head,
            anchor: anchor(),
            original: String::new(),
            result: Ok("old selected text".into()),
        }));
        assert!(
            state
                .discussion
                .composer
                .as_ref()
                .unwrap()
                .content
                .text()
                .is_empty()
        );
        assert!(
            state.discussion.suggestion_loading,
            "an old receipt cannot clear a newer request"
        );
    }

    #[test]
    fn suggestion_lines_from_an_old_head_leave_the_draft_unchanged() {
        let mut state = ready();
        drop(state.update_discussion(Action::Begin(anchor())));
        drop(state.update_discussion(Action::InsertSuggestion));
        let revision = state.discussion.composer_revision;
        let head = state.context.as_ref().unwrap().head_oid.clone();
        state.context.as_mut().unwrap().head_oid = "d".repeat(40);
        drop(state.update_discussion(Action::SuggestionLines {
            generation: state.generation,
            composer_revision: revision,
            head,
            anchor: anchor(),
            original: String::new(),
            result: Ok("old selected text".into()),
        }));
        assert!(
            state
                .discussion
                .composer
                .as_ref()
                .unwrap()
                .content
                .text()
                .is_empty()
        );
        assert!(!state.discussion.suggestion_loading);
        assert!(state.notice.as_ref().unwrap().contains("head changed"));
    }

    #[test]
    fn a_fenced_reanchor_receipt_can_finish_while_an_unrelated_modal_is_open() {
        let mut state = ready();
        drop(state.update_discussion(Action::Begin(anchor())));
        let mut mapped = anchor();
        mapped.line = 12;
        mapped.start_line = 12;
        let callback = crate::pull_request_review::Action::Discussion(Action::Reanchored {
            generation: state.generation,
            composer_revision: state.discussion.composer_revision,
            context: state.context.clone().unwrap(),
            result: Ok(mapped.clone()),
        });
        assert!(
            callback.allowed_while_modal(),
            "the shell must deliver this worker receipt beneath a modal"
        );
        drop(state.update(callback));
        assert_eq!(
            state.discussion.composer.as_ref().unwrap().destination,
            Destination::Line(mapped)
        );
    }

    #[test]
    fn late_reanchor_cannot_retarget_a_replacement_reply() {
        let mut state = ready();
        drop(state.update_discussion(Action::Begin(anchor())));
        let composer_revision = state.discussion.composer_revision;
        let context = state.context.clone().unwrap();
        drop(state.update_discussion(Action::Cancel));
        drop(state.update_discussion(Action::Reply("other-thread".into())));
        state.discussion.composer.as_mut().unwrap().content =
            text_editor::Content::with_text("Reply body stays a reply");
        drop(state.update_discussion(Action::Reanchored {
            generation: state.generation,
            composer_revision,
            context,
            result: Ok(anchor()),
        }));
        let composer = state.discussion.composer.as_ref().unwrap();
        assert_eq!(
            composer.destination,
            Destination::Reply("other-thread".into())
        );
        assert_eq!(composer.content.text(), "Reply body stays a reply");
    }

    #[test]
    fn selecting_another_gutter_anchor_invalidates_an_inflight_mapping() {
        let mut state = ready();
        drop(state.update_discussion(Action::Begin(anchor())));
        let composer_revision = state.discussion.composer_revision;
        let context = state.context.clone().unwrap();
        let mut selected = anchor();
        selected.line = 12;
        selected.start_line = 12;
        drop(state.update_discussion(Action::Begin(selected.clone())));
        drop(state.update_discussion(Action::Reanchored {
            generation: state.generation,
            composer_revision,
            context,
            result: Ok(anchor()),
        }));
        assert_eq!(
            state.discussion.composer.as_ref().unwrap().destination,
            Destination::Line(selected)
        );
    }

    #[test]
    fn same_draft_mapping_preserves_text_typed_while_git_runs() {
        let mut state = ready();
        drop(state.update_discussion(Action::Begin(anchor())));
        let composer_revision = state.discussion.composer_revision;
        let context = state.context.clone().unwrap();
        drop(
            state.update_discussion(Action::Composer(text_editor::Action::Edit(
                text_editor::Edit::Insert('x'),
            ))),
        );
        let mut mapped = anchor();
        mapped.line = 9;
        mapped.start_line = 9;
        drop(state.update_discussion(Action::Reanchored {
            generation: state.generation,
            composer_revision,
            context,
            result: Ok(mapped.clone()),
        }));
        let composer = state.discussion.composer.as_ref().unwrap();
        assert_eq!(composer.destination, Destination::Line(mapped));
        assert_eq!(composer.content.text().trim(), "x");
    }

    #[test]
    fn successful_latest_cache_write_allows_saved_drafts_to_survive_restart() {
        let mut state = ready();
        // A store is present; this test drives its receipt without background I/O.
        let directory = tempfile::tempdir().unwrap();
        state.cache = Some(Arc::new(cache::JsonStore::new(
            directory.path().join("cache"),
        )));
        drop(state.update_discussion(Action::Begin(anchor())));
        state.discussion.composer.as_mut().unwrap().content =
            text_editor::Content::with_text("Saved draft");
        assert!(state.restart_blocker().is_some());
        state.cache_order = 4;
        drop(state.update_inner(super::super::Action::CacheSaved {
            generation: state.generation,
            order: 4,
            draft_version: None,
            result: Ok(()),
        }));
        assert!(state.restart_blocker().is_none());
    }

    #[test]
    fn failed_or_conflicted_cache_write_keeps_the_local_draft_and_restart_fence() {
        let mut state = ready();
        let directory = tempfile::tempdir().unwrap();
        state.cache = Some(Arc::new(cache::JsonStore::new(
            directory.path().join("cache"),
        )));
        drop(state.update_discussion(Action::Begin(anchor())));
        state.discussion.composer.as_mut().unwrap().content =
            text_editor::Content::with_text("Unsaved text");
        state.cache_order = 4;
        drop(state.update_inner(super::super::Action::CacheSaved {
            generation: state.generation,
            order: 4,
            draft_version: Some("new-version".into()),
            result: Err("Draft changed in another window".into()),
        }));
        assert_eq!(
            state.discussion.composer.as_ref().unwrap().content.text(),
            "Unsaved text"
        );
        assert!(state.restart_blocker().is_some());
        assert!(state.cache_save_error);
        drop(state.update_inner(super::super::Action::RetryCacheSave));
        assert!(state.cache_dirty);
        assert!(!state.cache_save_error);
        assert!(!state.discussion.drafts_saved);
    }

    #[test]
    fn saved_file_draft_reanchor_fences_restart_and_persists_the_new_authority() {
        use cache::Store;

        let mut state = ready();
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(cache::JsonStore::new(directory.path().join("cache")));
        state.cache = Some(store.clone());
        drop(state.update_discussion(Action::BeginFile("file.rs".into())));
        drop(
            state.update_discussion(Action::Composer(text_editor::Action::Edit(
                text_editor::Edit::Insert('x'),
            ))),
        );
        let mut snapshot = cache::fixture();
        snapshot.drafts = state.local_drafts();
        snapshot.drafts_changed = state.drafts_need_save();
        snapshot.drafts_owner = Some(state.cache_owner.clone());
        snapshot.drafts_version = Some("original-draft".into());
        store
            .save(&snapshot, &cache::WriteOrder::fixture(1))
            .unwrap();
        state.drafts_cache_saved(snapshot.drafts_version.clone());
        assert!(state.restart_blocker().is_none());

        let mut next = loaded(vec![entry("file.rs", ChangeKind::Modified)], vec![]);
        next.context.head_oid = "d".repeat(40);
        next.range.head = crate::repository::Oid::parse("d".repeat(40)).unwrap();
        state.adopt(&next);
        // The earlier draft stays on its old authority until the user re-anchors.
        assert_eq!(
            state
                .local_drafts()
                .unwrap()
                .composer
                .unwrap()
                .context
                .head_oid,
            "a".repeat(40)
        );
        state.cache_dirty = false;
        drop(state.update_discussion(Action::Reanchor));
        assert!(
            state.restart_blocker().is_some(),
            "the new anchor is not durable yet"
        );
        assert!(state.drafts_need_save());
        assert!(state.cache_dirty);

        snapshot.range = state.range.clone().unwrap();
        snapshot.pull_request.head_ref_oid = snapshot.range.head.as_str().to_owned();
        snapshot.drafts = state.local_drafts();
        snapshot.drafts_changed = state.drafts_need_save();
        snapshot.drafts_basis = state.draft_cache_basis();
        snapshot.drafts_version = Some("reanchored-draft".into());
        store
            .save(&snapshot, &cache::WriteOrder::fixture(2))
            .unwrap();
        let saved = store.load(&snapshot.key).unwrap().unwrap();
        let mut restored = ready();
        restored.adopt(&next);
        restored.restore_drafts(saved.drafts, saved.drafts_version);
        let composer = restored.discussion.composer.as_ref().unwrap();
        assert_eq!(composer.content.text().trim(), "x");
        assert_eq!(composer.destination, Destination::File("file.rs".into()));
        assert_eq!(composer.authority.context.head_oid, "d".repeat(40));
        assert_eq!(composer.range.as_ref().unwrap().head, next.range.head);
        state.drafts_cache_saved(snapshot.drafts_version);
        assert!(state.restart_blocker().is_none());
    }

    #[test]
    fn local_draft_roundtrip_keeps_text_anchor_and_original_head() {
        let mut state = ready();
        drop(state.update_discussion(Action::Begin(anchor())));
        state.discussion.composer.as_mut().unwrap().content =
            text_editor::Content::with_text("Keep across restart");
        state.discussion.summary = text_editor::Content::with_text("Summary survives");
        let drafts =
            serde_json::from_str(&serde_json::to_string(&state.local_drafts()).unwrap()).unwrap();
        let mut restored = ready();
        restored.restore_drafts(drafts, None);
        let composer = restored.discussion.composer.as_ref().unwrap();
        assert_eq!(composer.content.text(), "Keep across restart");
        assert_eq!(composer.destination, Destination::Line(anchor()));
        assert_eq!(restored.discussion.summary.text(), "Summary survives");
        let mut next = loaded(vec![entry("file.rs", ChangeKind::Modified)], vec![]);
        next.context.head_oid = "c".repeat(40);
        restored.adopt(&next);
        assert_eq!(
            restored
                .discussion
                .composer
                .as_ref()
                .unwrap()
                .authority
                .context
                .head_oid,
            "a".repeat(40)
        );
        assert_eq!(
            restored
                .discussion
                .composer
                .as_ref()
                .unwrap()
                .content
                .text(),
            "Keep across restart"
        );
    }
    #[test]
    fn unresolved_write_receipt_survives_recovery_and_still_fences_mutations() {
        let mut state = ready();
        state.discussion.unknown = Some("The write may have reached GitHub".into());
        let drafts =
            serde_json::from_str(&serde_json::to_string(&state.local_drafts()).unwrap()).unwrap();
        let mut restored = ready();
        restored.restore_drafts(drafts, None);
        assert_eq!(restored.discussion.unknown, state.discussion.unknown);
        assert!(!restored.can_mutate_discussion());
        assert!(restored.restart_blocker().is_some());
    }

    #[test]
    fn published_inline_edit_persists_its_original_text_and_keeps_refused_input() {
        let mut state = ready();
        let comment = serde_json::from_value(serde_json::json!({"id":"comment","body":"Original text","createdAt":"now","url":"https://github.com/o/r/pull/1#comment","viewerDidAuthor":true,"viewerCanUpdate":true,"commentState":"SUBMITTED"})).unwrap();
        state.discussion.conversation = Some(Arc::new(PullRequestConversation {
            threads: vec![PullRequestReviewThread {
                id: "thread".into(),
                path: "file.rs".into(),
                line: Some(1),
                start_line: None,
                side: ReviewDiffSide::Right,
                start_side: None,
                outdated: false,
                resolved: false,
                collapsed: false,
                viewer_can_resolve: true,
                viewer_can_unresolve: false,
                truncated: false,
                comments: vec![comment],
            }],
            ..Default::default()
        }));
        drop(state.update_discussion(Action::Edit("comment".into())));
        state.discussion.composer.as_mut().unwrap().content =
            text_editor::Content::with_text("My retained edit");
        let serialized = serde_json::to_value(state.local_drafts()).unwrap();
        let mut restored = ready();
        restored.restore_drafts(serde_json::from_value(serialized.clone()).unwrap(), None);
        let composer = restored.discussion.composer.as_ref().unwrap();
        assert_eq!(composer.original_body.as_deref(), Some("Original text"));
        assert_eq!(composer.content.text(), "My retained edit");
        drop(restored.update_discussion(Action::Finished {
            generation: restored.generation,
            outcome: Outcome::Refused("The comment changed on GitHub".into()),
        }));
        assert_eq!(
            restored
                .discussion
                .composer
                .as_ref()
                .unwrap()
                .content
                .text(),
            "My retained edit"
        );
        let mut legacy = serialized;
        legacy["composer"]
            .as_object_mut()
            .unwrap()
            .remove("original_body");
        let mut restored = ready();
        restored.restore_drafts(serde_json::from_value(legacy).unwrap(), None);
        assert!(
            restored
                .discussion
                .composer
                .as_ref()
                .unwrap()
                .original_body
                .is_none()
        );
        assert_eq!(
            restored
                .discussion
                .composer
                .as_ref()
                .unwrap()
                .content
                .text(),
            "My retained edit"
        );
    }

    #[test]
    fn empty_comment_submission_explains_refusal_but_empty_approval_is_valid() {
        let mut state = ready();
        drop(state.update_discussion(Action::PrepareSubmit));
        assert!(state.discussion.confirmation.is_none());
        assert!(
            state
                .discussion
                .submit_error
                .as_deref()
                .unwrap()
                .contains("Write a summary")
        );
        drop(state.update_discussion(Action::Event(ReviewEvent::Approve)));
        drop(state.update_discussion(Action::PrepareSubmit));
        assert!(state.discussion.confirmation.is_some());
    }
    #[test]
    fn resolve_and_reply_controls_remain_usable_during_background_refresh() {
        let mut state = ready();
        state.refreshing = true;
        state.discussion.loading = true;
        assert!(state.can_mutate_discussion());
        drop(state.update_discussion(Action::Resolve {
            thread: "thread".into(),
            resolved: true,
        }));
        assert!(
            state.discussion.busy,
            "the action is sent, never dropped by refresh"
        );
        assert!(!state.refreshing);
    }

    #[test]
    fn review_navigation_preserves_drafts_and_blocks_unresolved_writes() {
        let mut state = ready();
        drop(state.update_discussion(Action::Begin(anchor())));
        drop(
            state.update_discussion(Action::Composer(text_editor::Action::Edit(
                text_editor::Edit::Insert('x'),
            ))),
        );
        state.discussion.summary = text_editor::Content::with_text("summary for first PR");
        drop(state.update(super::super::Action::Close));
        assert!(state.target.is_none());
        assert_eq!(state.parked_discussions.len(), 1);
        assert!(
            state.restart_blocker().is_some(),
            "parked drafts still block app exit"
        );
        drop(state.update(super::super::Action::Open(target(2))));
        state.discussion.summary = text_editor::Content::with_text("second PR");
        drop(state.update(super::super::Action::Open(target(1))));
        assert_eq!(state.discussion.summary.text(), "summary for first PR");
        assert_eq!(
            state.discussion.composer.as_ref().unwrap().content.text(),
            "x"
        );
        assert!(
            state.discussion.conversation.is_none(),
            "reload fetched metadata"
        );
        state.discussion.busy = true;
        let generation = state.generation;
        drop(state.update(super::super::Action::Close));
        drop(state.update(super::super::Action::Open(target(2))));
        assert_eq!(state.target.as_ref().unwrap().pull_request.number, 1);
        assert_eq!(
            state.generation, generation,
            "retain ownership of write callbacks"
        );
        state.discussion.busy = false;
        state.discussion.unknown = Some("lost receipt".into());
        drop(state.update(super::super::Action::Close));
        drop(state.update(super::super::Action::Open(target(2))));
        assert_eq!(state.generation, generation);
        assert_eq!(state.discussion.unknown.as_deref(), Some("lost receipt"));
        assert!(!state.can_mutate_discussion());
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
    fn choosing_a_new_line_reanchors_the_kept_draft_to_the_current_head() {
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
        assert_eq!(composer.authority.context.head_oid, "c".repeat(40));
        assert!(matches!(&composer.destination, Destination::Line(a) if a.line == 10));
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
        let fallback = |bounds| {
            state
                .threads_at("file.rs", None, None, true, Some(bounds))
                .map(|thread| thread.id.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(fallback([7, 6]), ["outdated", "right"]);
        assert_eq!(fallback([0, 0]), ["outdated", "left", "right"]);
        assert_eq!(fallback([7, 7]), ["outdated"]);
        drop(state.update_discussion(Action::Reply("right".into())));
        assert!(state.composer_at("file.rs", None, None, true, Some([0, 0])));
        assert!(!state.composer_at("file.rs", None, None, true, Some([7, 7])));
    }

    #[test]
    fn review_popover_is_disclosed_on_demand_and_closing_it_drops_a_confirmation() {
        let mut state = ready();
        state.discussion.summary = text_editor::Content::with_text("Review summary");
        assert!(!state.discussion.review_open, "submission starts hidden");
        drop(state.update_discussion(Action::ToggleReview));
        assert!(state.discussion.review_open);
        drop(state.update_discussion(Action::PrepareSubmit));
        assert!(state.discussion.confirmation.is_some());
        drop(state.update_discussion(Action::CloseReview));
        assert!(!state.discussion.review_open);
        assert!(
            state.discussion.confirmation.is_none(),
            "reopening must not land on a stale confirmation"
        );
    }

    #[test]
    fn a_submitted_review_closes_its_popover_and_resets_the_draft() {
        let mut state = ready();
        drop(state.update_discussion(Action::ToggleReview));
        drop(state.update_discussion(Action::Event(ReviewEvent::Approve)));
        state.discussion.summary = text_editor::Content::with_text("Ship it");
        state.discussion.busy = true;
        state.discussion.submitting = true;
        drop(state.update_discussion(Action::Finished {
            generation: state.generation,
            outcome: Outcome::Applied,
        }));
        assert!(!state.discussion.review_open);
        assert_eq!(state.discussion.event, ReviewEvent::Comment);
        assert!(state.discussion.summary.text().trim().is_empty());
        assert_eq!(state.notice.as_deref(), Some("Review submitted to GitHub."));
    }

    #[test]
    fn a_refused_submission_keeps_the_popover_and_its_draft() {
        let mut state = ready();
        drop(state.update_discussion(Action::ToggleReview));
        state.discussion.summary = text_editor::Content::with_text("Needs work");
        state.discussion.busy = true;
        state.discussion.submitting = true;
        drop(state.update_discussion(Action::Finished {
            generation: state.generation,
            outcome: Outcome::Refused("changed".into()),
        }));
        assert!(state.discussion.review_open);
        assert_eq!(state.discussion.summary.text().trim(), "Needs work");
        assert_eq!(state.discussion.submit_error.as_deref(), Some("changed"));
        assert!(
            state.notice.is_none(),
            "the refusal belongs in the popover, not under it"
        );
    }

    #[test]
    fn a_lost_submission_closes_the_popover_over_its_warning() {
        let mut state = ready();
        drop(state.update_discussion(Action::ToggleReview));
        state.discussion.busy = true;
        state.discussion.submitting = true;
        drop(state.update_discussion(Action::Finished {
            generation: state.generation,
            outcome: Outcome::Unknown("lost".into()),
        }));
        assert!(!state.discussion.review_open);
        assert!(state.discussion.unknown.is_some());
    }

    #[test]
    fn confirming_keeps_the_confirmation_on_screen_while_submitting() {
        let mut state = ready();
        state.discussion.summary = text_editor::Content::with_text("Review summary");
        drop(state.update_discussion(Action::ToggleReview));
        drop(state.update_discussion(Action::PrepareSubmit));
        drop(state.update_discussion(Action::CancelSubmit));
        assert!(state.discussion.confirmation.is_none());
        assert!(state.discussion.review_open);
        drop(state.update_discussion(Action::PrepareSubmit));
        drop(state.update_discussion(Action::ConfirmSubmit));
        assert!(state.discussion.busy && state.discussion.submitting);
        assert!(state.discussion.confirmation.is_some());
        drop(state.update_discussion(Action::CancelSubmit));
        assert!(
            state.discussion.confirmation.is_some(),
            "Back cannot dismiss the confirmation while GitHub is receiving it"
        );
        drop(state.update_discussion(Action::CloseReview));
        assert!(
            state.discussion.review_open,
            "an in-flight submission keeps its progress visible"
        );
    }

    #[test]
    fn drafts_accept_typing_during_a_background_refresh() {
        let mut state = ready();
        drop(state.update_discussion(Action::BeginFile("file.rs".into())));
        state.refreshing = true;
        assert!(state.can_mutate_discussion());
        let typed = text_editor::Action::Edit(text_editor::Edit::Insert('x'));
        drop(state.update_discussion(Action::Composer(typed.clone())));
        drop(state.update_discussion(Action::Summary(typed.clone())));
        assert_eq!(
            state
                .discussion
                .composer
                .as_ref()
                .unwrap()
                .content
                .text()
                .trim(),
            "x"
        );
        assert_eq!(state.discussion.summary.text().trim(), "x");
        // A write in flight replaces the draft when it lands, so it is held.
        state.discussion.busy = true;
        drop(state.update_discussion(Action::Composer(typed)));
        assert_eq!(
            state
                .discussion
                .composer
                .as_ref()
                .unwrap()
                .content
                .text()
                .trim(),
            "x"
        );
        // Preview and Cancel are just as local, but not while a write is in
        // flight.
        drop(state.update_discussion(Action::Preview));
        assert!(!state.discussion.composer.as_ref().unwrap().preview);
        drop(state.update_discussion(Action::Cancel));
        assert!(state.discussion.composer.is_some());
        state.discussion.busy = false;
        drop(state.update_discussion(Action::Preview));
        assert!(state.discussion.composer.as_ref().unwrap().preview);
        drop(state.update_discussion(Action::Cancel));
        assert!(state.discussion.composer.is_none());
    }

    #[test]
    fn a_file_comment_opens_above_the_diff_of_the_selected_file_only() {
        let mut state = ready();
        drop(state.update_discussion(Action::BeginFile("other.rs".into())));
        assert!(state.discussion.composer.is_none());
        drop(state.take_effects());

        drop(state.update_discussion(Action::BeginFile("file.rs".into())));
        assert!(matches!(
            &state.discussion.composer.as_ref().unwrap().destination,
            Destination::File(path) if path == "file.rs"
        ));
        assert_eq!(state.take_effects(), [Effect::RevealFileDiscussion]);
        assert!(state.has_leading_annotation("file.rs", [3, 3]));
        assert!(!state.has_annotation("file.rs", Some(1), Some(1), false));

        // One draft at a time: a line comment cannot replace it.
        drop(state.update_discussion(Action::Begin(anchor())));
        assert!(matches!(
            state.discussion.composer.as_ref().unwrap().destination,
            Destination::File(_)
        ));
        // Asking again leads back to the open draft rather than refusing.
        state.notice = None;
        drop(state.update_discussion(Action::BeginFile("file.rs".into())));
        assert_eq!(state.take_effects(), [Effect::RevealFileDiscussion]);
        assert!(state.notice.is_none());
    }
}
