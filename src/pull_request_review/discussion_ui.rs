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
    Cancel,
    Save,
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

#[derive(Debug, Clone)]
enum Destination {
    Line(Anchor),
    File(String),
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
    /// Preserve GitHub's order when anchors must fall back above the file.
    anchored: Vec<usize>,
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
    /// The submission surface is disclosed on demand rather than parked
    /// under every diff.
    review_open: bool,
    /// Why GitHub refused the last submission, shown inside the popover.
    submit_error: Option<String>,
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
            review_open: false,
            submit_error: None,
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

    fn can_discard_draft(&self) -> bool {
        !self.discussion.busy && self.discussion.unknown.is_none()
    }

    fn send_discussion(&mut self, authority: Authority, mutation: Mutation) -> ActionTask {
        if !self.can_mutate_discussion() {
            return ActionTask::none();
        }
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
                    destination: Destination::File(path),
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
            Action::Composer(_) | Action::Preview | Action::Summary(_) | Action::Cancel => {}
            Action::Event(event) => {
                self.discussion.event = event;
                self.discussion.confirmation = None;
                self.discussion.submit_error = None;
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
                    Destination::File(path) => Mutation::FileComment {
                        path: path.clone(),
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
                self.discussion.submit_error = None;
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
        let spacing = cosmic::theme::spacing();
        let enabled = self.can_mutate_discussion();
        let pending = self.pending_count();
        let mut rows: Vec<Element<'_, Action>> = Vec::new();
        if let Some((authority, request, _)) = &self.discussion.confirmation {
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
                    .padding([spacing.space_xxs, spacing.space_xs])
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
                button::text("Back")
                    .on_press_maybe((!self.discussion.busy).then_some(Action::CancelSubmit)),
                button::suggested(if self.discussion.busy {
                    "Submitting…"
                } else {
                    "Submit review"
                })
                .on_press_maybe(enabled.then_some(Action::ConfirmSubmit)),
            ));
        } else {
            rows.push(design::heading("Finish your review").into());
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
                .padding(spacing.space_xs)
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
            rows.push(widget::column::with_children(choices).spacing(2).into());
            let drafting = self.discussion.composer.is_some();
            if drafting {
                rows.push(
                    design::caption("Save or cancel the comment you’re writing before submitting.")
                        .class(cosmic::theme::Text::Custom(design::warning_text))
                        .wrapping(iced::widget::text::Wrapping::Word)
                        .into(),
                );
            }
            rows.push(popup_actions(
                button::text("Cancel").on_press(Action::CloseReview),
                button::suggested(if self.discussion.busy {
                    "Saving…"
                } else {
                    "Submit review"
                })
                .on_press_maybe((enabled && !drafting).then_some(Action::PrepareSubmit)),
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
        widget::container(widget::column::with_children(rows).spacing(spacing.space_xs))
            .padding(spacing.space_xxs)
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
        let control: Element<'_, Action> = crate::ui_controls::icon_control(
            symbolic_icon("comment-symbolic"),
            if drafting_here {
                "Show the comment on this file"
            } else {
                "Comment on this file"
            },
            drafting_here,
            enabled,
            Some(Action::BeginFile(path)),
        )
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
        let spacing = cosmic::theme::spacing();
        let element: Element<'_, Action> = widget::column::with_children(vec![
            design::caption(format!(
                "GitHub may have received the last review change. Check the discussion before continuing. {message}"
            ))
            .class(cosmic::theme::Text::Custom(design::warning_text))
            .wrapping(iced::widget::text::Wrapping::Word)
            .width(Length::Fill)
            .into(),
            button::text("I checked GitHub; continue")
                .on_press_maybe(
                    (!modal_open && !self.discussion.busy).then_some(Action::AcknowledgeUnknown),
                )
                .into(),
        ])
        .spacing(spacing.space_xxxs)
        .padding(Padding::from([spacing.space_xxxs, spacing.space_xs]))
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
                top: 2.0,
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
            .padding([5, 8])
            .width(Length::Fill)
            .into();
    }
    button::custom(content)
        .class(crate::ui_controls::control_style(false, None))
        .padding([5, 8])
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
    .spacing(8)
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
            Destination::File(file) => leading && file == path,
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
                button::text(label)
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
                rows.push(design::comment_card(
                    comment
                        .author
                        .as_ref()
                        .map_or("Deleted account", |author| author.login.as_str()),
                    &comment.created_at,
                    body,
                    actions,
                ));
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
        if self.composer_at(path, old, new, leading, bounds)
            && let Some(composer) = &self.discussion.composer
        {
            let label = match &composer.destination {
                Destination::Line(anchor) if anchor.start_line != anchor.line => {
                    format!("New comment · lines {}–{}", anchor.start_line, anchor.line)
                }
                Destination::Line(anchor) => format!("New comment · line {}", anchor.line),
                Destination::File(_) => "New comment · whole file".into(),
                Destination::Reply(_) => "Reply in pending review".into(),
                Destination::Edit(_) => "Edit pending comment".into(),
            };
            rows.push(design::heading(label).into());
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
                    button::suggested("Save pending comment")
                        .on_press_maybe(
                            (enabled && !composer.content.text().trim().is_empty())
                                .then_some(Action::Save),
                        )
                        .into(),
                    // Local to the draft, so live through a background
                    // refresh, as typing is; only a write in flight holds them.
                    button::text(if composer.preview { "Write" } else { "Preview" })
                        .on_press_maybe((!self.discussion.busy).then_some(Action::Preview))
                        .into(),
                    button::text("Cancel")
                        .on_press_maybe(self.can_discard_draft().then_some(Action::Cancel))
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
        assert!(!state.can_mutate_discussion());
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
