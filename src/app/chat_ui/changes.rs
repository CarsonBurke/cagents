//! A conversation's reported edits, independent of the visible transcript page.
use super::*;
mod scroll_restore;
use crate::chat::{FileChangeKey, FileChangeSource, FilePatchCursor};

// Tasks can outlive a Runtime replacement on the same card. A request ID
// must therefore belong to this process, rather than restart with each State.
static NEXT_REVIEW_REQUEST: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
pub(super) struct State {
    pub(super) open: bool,
    pub(super) list_scroll: f32,
    pub(super) list_epoch: u64,
    file_open_request: Option<u64>,
    selected: Option<FileChangeKey>,
    pub(super) resolved_path: Option<String>,
    pub(super) list: Option<chat::FileChangesSummary>,
    page: Option<Arc<chat::FileChangePage>>,
    list_request: Option<u64>,
    patch_request: Option<u64>,
    waiting: HashSet<u64>,
    pub(super) patch_index: usize,
    patch: Option<tool_text::ToolText>,
    loaded_patch: Option<(u64, usize)>,
    error: Option<String>,
}

impl State {
    pub(super) fn show(&mut self) {
        self.open = true;
        self.list_epoch = NEXT_REVIEW_REQUEST.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn close(&mut self) {
        self.open = false;
        self.list_epoch = NEXT_REVIEW_REQUEST.fetch_add(1, Ordering::Relaxed);
        self.list_scroll = 0.0;
        self.file_open_request = None;
        self.selected = None;
        self.resolved_path = None;
        self.list = None;
        self.page = None;
        self.patch = None;
        self.loaded_patch = None;
        self.patch_request = None;
        self.list_request = None;
        self.waiting.clear();
        self.error = None;
    }

    pub(super) fn record_scroll(
        &mut self,
        page: u64,
        first_page: u64,
        epoch: u64,
        narrow: bool,
        offset: f32,
    ) {
        let current = self
            .list
            .as_ref()
            .map_or(first_page, |list| list.request_id);
        if self.open
            && page == current
            && epoch == self.list_epoch
            && (!narrow || self.selected.is_none())
        {
            self.list_scroll = offset;
        }
    }

    pub(super) fn back(&mut self) {
        self.list_epoch = NEXT_REVIEW_REQUEST.fetch_add(1, Ordering::Relaxed);
        if let Some(previous) = self.patch_request.take() {
            self.waiting.remove(&previous);
        }
        self.selected = None;
        self.resolved_path = None;
        self.page = None;
        self.patch = None;
        self.loaded_patch = None;
        self.patch_index = 0;
        self.error = None;
    }

    pub(super) fn select(&mut self, key: FileChangeKey) {
        self.list_epoch = NEXT_REVIEW_REQUEST.fetch_add(1, Ordering::Relaxed);
        if let Some(previous) = self.patch_request.take() {
            self.waiting.remove(&previous);
        }
        self.selected = Some(key);
        self.resolved_path = None;
        self.page = None;
        self.loaded_patch = None;
        self.patch = None;
        self.patch_index = 0;
        self.patch_request = None;
        self.error = None;
    }

    fn request(&mut self, patches: bool) -> u64 {
        let id = NEXT_REVIEW_REQUEST.fetch_add(1, Ordering::Relaxed);
        let previous = if patches {
            self.patch_index = 0;
            self.patch = None;
            self.page = None;
            self.loaded_patch = None;
            self.patch_request.replace(id)
        } else {
            self.list_request.replace(id)
        };
        if let Some(previous) = previous {
            self.waiting.remove(&previous);
        }
        self.waiting.insert(id);
        self.error = None;
        id
    }

    pub(super) fn complete(
        &mut self,
        id: u64,
        result: Result<chat::FileChangeReply, String>,
    ) -> bool {
        if !self.waiting.remove(&id) {
            return false;
        }
        match result {
            Ok(chat::FileChangeReply::Files(summary))
                if self.list_request == Some(id) && summary.request_id == id =>
            {
                self.list = Some(summary);
                self.list_scroll = 0.0;
                true
            }
            Ok(chat::FileChangeReply::Patches(page))
                if self.patch_request == Some(id)
                    && page.request_id == id
                    && self.selected.as_ref() == Some(&page.key) =>
            {
                self.page = Some(Arc::new(page));
                self.sync();
                true
            }
            Ok(_) => {
                self.error =
                    Some("The requested edits were not returned. Refresh to try again.".into());
                false
            }
            Err(error) => {
                self.error = Some(error);
                false
            }
        }
    }

    fn page(&self) -> Option<&chat::FileChangePage> {
        self.page.as_deref().filter(|page| {
            self.patch_request == Some(page.request_id) && self.selected.as_ref() == Some(&page.key)
        })
    }

    pub(super) fn sync(&mut self) {
        let Some(page) = self.page.clone() else {
            return;
        };
        let id = page.request_id;
        if self.loaded_patch == Some((id, self.patch_index)) {
            return;
        }
        self.patch = page.patches.get(self.patch_index).map(|patch| {
            tool_text::ToolText::update(self.patch.take(), &patch.diff, tool_text::Language::Diff)
        });
        self.loaded_patch = Some((id, self.patch_index));
    }
}

fn kind(kind: &chat::ChangeKind) -> &'static str {
    match kind {
        chat::ChangeKind::Added => "Added",
        chat::ChangeKind::Deleted => "Deleted",
        chat::ChangeKind::Updated => "Edited",
        chat::ChangeKind::Renamed { .. } => "Renamed",
    }
}


fn totals(additions: Option<u64>, deletions: Option<u64>) -> String {
    match (additions, deletions) {
        (Some(added), Some(deleted)) => format!("+{added} −{deleted}"),
        _ => "Line totals unavailable".into(),
    }
}

fn patch_label(patch: &chat::FileChangePatch, index: usize) -> String {
    let state = match patch.status {
        chat::ToolStatus::Running => " · Pending",
        chat::ToolStatus::Failed => " · Failed",
        chat::ToolStatus::Succeeded => "",
        chat::ToolStatus::Denied => " · Denied",
        chat::ToolStatus::Stopped => " · Stopped",
    };
    let history = if patch.imported {
        " · Earlier history"
    } else {
        ""
    };
    let ownership = if patch.ownership_unverified && !patch.imported {
        " · Reported edit"
    } else {
        ""
    };
    let continued = if patch.offset > 0 {
        " · Continued"
    } else {
        ""
    };
    format!(
        "{} {}{state}{history}{ownership}{continued}",
        kind(&patch.kind),
        index + 1
    )
}

impl Runtime {
    fn change_source(&self, key: &FileChangeKey) -> String {
        match &key.source {
            FileChangeSource::Conversation => "Main chat".into(),
            FileChangeSource::Subagent { id } => self
                .snapshot
                .subagents
                .iter()
                .find(|agent| &agent.id == id)
                .map(|agent| format!("Subagent: {}", Self::native_child_name(agent)))
                .unwrap_or_else(|| format!("Subagent: {}", id.chars().take(8).collect::<String>())),
        }
    }

    pub(super) fn changes_view(&self, width: f32) -> Element<'_, Message> {
        let target = self.target;
        let summary = self
            .changes
            .list
            .as_ref()
            .unwrap_or(self.snapshot.file_changes.as_ref());
        let readable = self.handle().is_some() && !self.disconnected;
        let narrow = width < 560.0;
        let mut heading: Vec<Element<'_, Message>> = Vec::new();
        if narrow && self.changes.selected.is_some() {
            heading.push(
                crate::ui_controls::button::quiet("Back to files")
                    .on_press(action(Action::ChangeBack(target)))
                    .into(),
            );
        }
        heading.push(design::heading("Files changed").width(Length::Fill).into());
        heading.push(
            crate::ui_controls::button::quiet("Close")
                .id(card_control_id(target.card, "chat-changes-close"))
                .on_press(action(Action::ChangesPicker(target, false)))
                .into(),
        );
        let mut rows: Vec<Element<'_, Message>> = vec![
            widget::row::with_children(heading)
                .align_y(Alignment::Center)
                .spacing(space::XS)
                .into(),
            crate::interaction::tooltip(
                design::caption("Reported edits · line totals include repeated edits")
                    .wrapping(iced::widget::text::Wrapping::None)
                    .ellipsize(iced::widget::text::Ellipsize::End(
                        iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                    )),
                "Changes reported in available conversation history. Line totals count reported edits, including repeated edits; they are not a net worktree diff.",
            ),
        ];
        let mut coverage = Vec::new();
        let mut explanation = Vec::new();
        if summary.coverage.has_unloaded_history {
            coverage.push("Partial history");
            explanation
                .push("Earlier chat history is not fully available; this list may be incomplete.");
        }
        if summary.coverage.has_imported_history {
            coverage.push("Includes earlier edits");
            explanation.push(
                "Includes earlier conversation history, which may predate this chat or a fork.",
            );
        }
        if summary.coverage.has_incomplete_delegated_history {
            coverage.push("Partial subagent history");
            explanation.push("Some earlier subagent edits may be unavailable.");
        }
        if summary.coverage.has_uncached_changes {
            coverage.push("Some edits unavailable");
            explanation.push("Some reported edits could not be included in this list.");
        }
        if summary.revision != self.snapshot.file_changes.revision {
            coverage.push("New edits available");
            explanation.push("New edits were reported. Refresh files to include them.");
        }
        if !coverage.is_empty() {
            rows.push(crate::interaction::tooltip(
                design::caption(coverage.join(" · "))
                    .wrapping(iced::widget::text::Wrapping::None)
                    .ellipsize(iced::widget::text::Ellipsize::End(
                        iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                    )),
                explanation.join(" "),
            ));
        }
        let mut files: Vec<Element<'_, Message>> = Vec::new();
        if self
            .changes
            .list_request
            .is_some_and(|id| self.changes.waiting.contains(&id))
        {
            files.push(design::caption("Loading files…").into());
        } else {
            for file in &summary.files {
                let selected = self.changes.selected.as_ref() == Some(&file.key);
                let applied = if file.succeeded > 0 {
                    kind(&file.kind)
                } else {
                    "Proposed edit"
                };
                let mut detail = format!(
                    "{} · {} · {}",
                    applied,
                    self.change_source(&file.key),
                    totals(file.additions, file.deletions)
                );
                if file.succeeded > 0 {
                    detail.push_str(&format!(
                        " · {} applied {}",
                        file.succeeded,
                        if file.succeeded == 1 { "edit" } else { "edits" }
                    ));
                }
                if file.pending > 0 {
                    detail.push_str(&format!(" · {} pending", file.pending));
                }
                if file.failed > 0 {
                    detail.push_str(&format!(" · {} unsuccessful", file.failed));
                }
                if file.ownership_unverified && file.imported == 0 {
                    detail.push_str(" · Reported by subagent");
                }
                if file.imported > 0 {
                    detail.push_str(" · Earlier history");
                }
                let mut file_rows: Vec<Element<'_, Message>> = vec![
                    design::body(&file.key.path)
                        .font(iced::Font::MONOSPACE)
                        .width(Length::Fill)
                        .wrapping(iced::widget::text::Wrapping::None)
                        .ellipsize(iced::widget::text::Ellipsize::End(
                            iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                        ))
                        .into(),
                    design::caption(detail.clone())
                        .width(Length::Fill)
                        .ellipsize(iced::widget::text::Ellipsize::End(
                            iced::advanced::text::EllipsizeHeightLimit::Lines(2),
                        ))
                        .into(),
                ];
                if let chat::ChangeKind::Renamed { from } = &file.kind {
                    file_rows.push(
                        design::caption(format!("from {from}"))
                            .wrapping(iced::widget::text::Wrapping::None)
                            .ellipsize(iced::widget::text::Ellipsize::End(
                                iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                            ))
                            .into(),
                    );
                }
                files.push(crate::interaction::tooltip(
                    widget::button::custom(widget::column::with_children(file_rows).spacing(2))
                        .name(format!(
                            "View reported edits for {}. {detail}{}",
                            file.key.path,
                            match &file.kind {
                                chat::ChangeKind::Renamed { from } => format!(". from {from}"),
                                _ => String::new(),
                            }
                        ))
                        .selected(selected)
                        .class(crate::ui_controls::control_style(selected, None))
                        .padding([4, 8])
                        .width(Length::Fill)
                        .on_press_maybe(
                            readable
                                .then(|| action(Action::ChangeSelect(target, file.key.clone()))),
                        ),
                    format!("{}\n{detail}", file.key.path),
                ));
            }
            if summary.files.is_empty() {
                files.push(design::caption("No file edits have been reported.").into());
            }
            let after = summary.files.last().map(|file| file.key.clone());
            files.push(
                widget::row::with_children(vec![
                    crate::ui_controls::button::quiet("Refresh files")
                        .on_press_maybe(readable.then(|| action(Action::ChangeFiles(target, None))))
                        .into(),
                    crate::ui_controls::button::quiet("More files")
                        .on_press_maybe(
                            (readable && summary.has_more)
                                .then(|| action(Action::ChangeFiles(target, after))),
                        )
                        .into(),
                ])
                .spacing(space::XS)
                .into(),
            );
        }
        let list_epoch = self.changes.list_epoch;
        let list: Element<'_, Message> =
            widget::scrollable(widget::column::with_children(files).spacing(2))
                .id(card_control_id(target.card, "chat-changes-files"))
                .on_scroll(move |viewport| {
                    action(Action::ChangeListScrolled(
                        target,
                        summary.request_id,
                        list_epoch,
                        narrow,
                        viewport.absolute_offset().y,
                    ))
                })
                .height(Length::Fill)
                .into();
        let list = scroll_restore::on_mount(
            list,
            card_control_id(target.card, "chat-changes-files"),
            self.changes.list_scroll,
        );
        let body: Element<'_, Message> = if narrow {
            if self.changes.selected.is_some() {
                self.change_detail(readable)
            } else {
                list
            }
        } else {
            widget::row::with_children(vec![
                widget::container(list)
                    .width(Length::FillPortion(2))
                    .height(Length::Fill)
                    .into(),
                widget::container(self.change_detail(readable))
                    .width(Length::FillPortion(3))
                    .height(Length::Fill)
                    .into(),
            ])
            .spacing(space::S)
            .height(Length::Fill)
            .into()
        };
        rows.push(body);
        if let Some(error) = &self.changes.error {
            rows.push(crate::interaction::tooltip(
                design::caption(error.as_str())
                    .wrapping(iced::widget::text::Wrapping::None)
                    .ellipsize(iced::widget::text::Ellipsize::End(
                        iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                    )),
                error.clone(),
            ));
        }
        crate::interaction::focused_escape(
            widget::container(
                widget::column::with_children(rows)
                    .spacing(space::XS)
                    .height(Length::Fixed(260.0)),
            )
            .padding(space::S)
            .width(Length::Fill)
            .class(cosmic::theme::Container::Card),
            move || action(Action::ChangesPicker(target, false)),
        )
    }

    fn change_detail(&self, readable: bool) -> Element<'_, Message> {
        let target = self.target;
        let Some(key) = &self.changes.selected else {
            return design::caption("Choose a file to review its reported edits.").into();
        };
        let mut rows: Vec<Element<'_, Message>> = vec![
            design::body(&key.path).font(iced::Font::MONOSPACE).into(),
            design::caption(self.change_source(key)).into(),
        ];
        if let Some(path) = self.changes.resolved_path.as_deref() {
            rows.push(
                crate::ui_controls::button::quiet("Open file")
                    .on_press(action(Action::OpenChangedFile(target, PathBuf::from(path))))
                    .into(),
            );
        } else {
            rows.push(design::caption("The file location was not reported.").into());
        }
        let page = self.changes.page();
        if self
            .changes
            .patch_request
            .is_some_and(|id| self.changes.waiting.contains(&id))
        {
            rows.push(design::caption("Loading reported edits…").into());
        } else if let Some(page) = page {
            if let Some(error) = &page.error {
                rows.push(design::caption(error.as_str()).into());
            } else {
                if self
                    .snapshot
                    .file_changes
                    .files
                    .iter()
                    .find(|file| &file.key == key)
                    .is_some_and(|file| file.revision != page.revision)
                {
                    rows.push(
                        design::caption(
                            "New changes were reported. Refresh edits to include them.",
                        )
                        .into(),
                    );
                }
                for (index, patch) in page.patches.iter().enumerate() {
                    rows.push(
                        crate::ui_controls::button::quiet(patch_label(patch, index))
                            .on_press(action(Action::ChangePatch(target, index)))
                            .into(),
                    );
                    if index == self.changes.patch_index {
                        if let chat::ChangeKind::Renamed { from } = &patch.kind {
                            rows.push(design::caption(format!("from {from}")).into());
                        }
                        if let Some(block) = &self.changes.patch {
                            if block.text.is_empty() {
                                rows.push(
                                    design::caption(
                                        "The provider reported this edit without a text patch.",
                                    )
                                    .into(),
                                );
                            } else {
                                rows.push(block.view_from_start(
                                    self.syntax_theme,
                                    120.0,
                                    target,
                                    None,
                                ));
                                let excerpt = patch.offset > 0
                                    || patch.offset.saturating_add(patch.diff.len())
                                        < patch.total_bytes;
                                rows.push(
                                    crate::ui_controls::button::quiet(if excerpt {
                                        "Copy patch excerpt"
                                    } else {
                                        "Copy patch"
                                    })
                                    .on_press(action(Action::CopyLiteral(block.text.to_string())))
                                    .into(),
                                );
                                if excerpt {
                                    rows.push(design::caption("This is part of a larger patch. More edits continues it.").into());
                                }
                            }
                        }
                    }
                }
                if page.patches.is_empty() {
                    rows.push(design::caption("No patch details are available.").into());
                }
            }
        } else {
            rows.push(design::caption("Refresh edits to load the file's patch history.").into());
        }
        rows.push(
            widget::row::with_children(vec![
                crate::ui_controls::button::quiet("First edits")
                    .on_press_maybe(readable.then(|| action(Action::ChangePatches(target, None))))
                    .into(),
                crate::ui_controls::button::quiet("More edits")
                    .on_press_maybe(
                        page.and_then(|page| page.next.clone())
                            .filter(|_| readable)
                            .map(|cursor| action(Action::ChangePatches(target, Some(cursor)))),
                    )
                    .into(),
            ])
            .spacing(space::XS)
            .into(),
        );
        widget::scrollable(widget::column::with_children(rows).spacing(space::XS))
            .id(card_control_id(target.card, "chat-changes-detail"))
            .height(Length::Fill)
            .into()
    }
}

impl Cagents {
    pub(super) fn chat_open_changed_file(
        &mut self,
        target: Target,
        path: PathBuf,
    ) -> Task<Message> {
        if self.chat_runtime_mut(target).is_none() || self.workspace_inputs_blocked() {
            return Task::none();
        }
        if !path.is_absolute() || path.as_os_str().as_encoded_bytes().contains(&0) {
            self.status = Some(TransientNotice::error("The reported file path is invalid."));
            return Task::none();
        }
        if self.refuse_in_deleting_worktree(&path) {
            return Task::none();
        }
        if crate::device::is_remote() {
            // The owning daemon canonicalizes the directory before selecting
            // and reading this file; never resolve a device path on the UI host.
            return self.open_file_path(target.card, path);
        }
        let id = NEXT_REVIEW_REQUEST.fetch_add(1, Ordering::Relaxed);
        self.chat_runtime_mut(target)
            .unwrap()
            .changes
            .file_open_request = Some(id);
        cosmic::task::future(async move {
            let reported = path.clone();
            let result = tokio::task::spawn_blocking(move || std::fs::canonicalize(path))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()));
            Message::ChatUi(Action::ChangedFileResolved(target, id, reported, result))
        })
    }

    pub(super) fn chat_changed_file_resolved(
        &mut self,
        target: Target,
        id: u64,
        reported: PathBuf,
        result: Result<PathBuf, String>,
    ) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        if runtime.changes.file_open_request != Some(id) {
            return Task::none();
        }
        runtime.changes.file_open_request = None;
        if crate::device::is_remote()
            || self.workspace_inputs_blocked()
            || self.refuse_in_deleting_worktree(&reported)
        {
            return Task::none();
        }
        match result {
            Ok(path) => self.open_file_path(target.card, path),
            Err(error) => {
                self.status = Some(TransientNotice::error(format!(
                    "Could not open the reported file: {error}"
                )));
                Task::none()
            }
        }
    }

    pub(super) fn chat_change_files(
        &mut self,
        target: Target,
        after: Option<FileChangeKey>,
    ) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        let Some(handle) = runtime.handle().cloned() else {
            return Task::none();
        };
        if !runtime.changes.open || runtime.disconnected {
            return Task::none();
        }
        let id = runtime.changes.request(false);
        send(
            target,
            handle,
            id,
            Command::LoadFileChanges {
                request_id: id,
                after,
            },
        )
    }

    pub(super) fn chat_change_patches(
        &mut self,
        target: Target,
        cursor: Option<FilePatchCursor>,
    ) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        let Some(handle) = runtime.handle().cloned() else {
            return Task::none();
        };
        let Some(key) = runtime.changes.selected.clone() else {
            return Task::none();
        };
        if !runtime.changes.open || runtime.disconnected {
            return Task::none();
        }
        let revision = if cursor.is_some() {
            runtime.changes.page().map_or(0, |page| page.revision)
        } else {
            runtime
                .changes
                .list
                .as_ref()
                .unwrap_or(runtime.snapshot.file_changes.as_ref())
                .files
                .iter()
                .find(|file| file.key == key)
                .map_or(0, |file| file.revision)
        };
        let id = runtime.changes.request(true);
        send(
            target,
            handle,
            id,
            Command::LoadFileChangePatches {
                request_id: id,
                key,
                cursor,
                revision,
            },
        )
    }
}

fn send(target: Target, handle: Arc<ClientHandle>, id: u64, command: Command) -> Task<Message> {
    cosmic::task::future(async move {
        let result = tokio::task::spawn_blocking(move || handle.file_changes(command))
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
        Message::ChatUi(Action::ChangesLoaded(target, id, result))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(path: &str) -> FileChangeKey {
        FileChangeKey {
            source: FileChangeSource::Conversation,
            path: path.into(),
        }
    }
    fn page(id: u64, key: FileChangeKey, diff: &str, offset: usize) -> chat::FileChangeReply {
        chat::FileChangeReply::Patches(chat::FileChangePage {
            request_id: id,
            key,
            revision: 9,
            patches: vec![chat::FileChangePatch {
                item_id: "edit-1".into(),
                kind: chat::ChangeKind::Updated,
                status: chat::ToolStatus::Succeeded,
                imported: false,
                ownership_unverified: false,
                offset,
                total_bytes: offset + diff.len(),
                diff: diff.into(),
            }],
            next: None,
            error: None,
        })
    }

    #[test]
    fn unknown_totals_are_not_presented_as_zero() {
        assert_eq!(totals(Some(7), Some(3)), "+7 −3");
        assert_eq!(totals(None, Some(3)), "Line totals unavailable");
    }

    #[test]
    fn stale_reply_errors_do_not_replace_the_selected_file() {
        let mut state = State::default();
        let old = state.request(true);
        let current = state.request(true);
        state.complete(old, Err("stale".into()));
        assert!(state.error.is_none());
        assert!(state.waiting.contains(&current));
        state.close();
        state.complete(current, Err("closed".into()));
        assert!(state.error.is_none());
        assert!(state.patch.is_none());
    }

    #[test]
    fn file_selection_discards_old_requests_and_unrelated_streaming_preserves_patch_cache() {
        let mut state = State::default();
        state.open = true;
        state.select(key("first.rs"));
        let old = state.request(true);
        for index in 0..200 {
            state.select(key(&format!("file-{index}.rs")));
            state.request(true);
            assert_eq!(state.waiting.len(), 1);
        }
        let current = state.patch_request.unwrap();
        state.complete(old, Ok(page(old, key("first.rs"), "-old\n+stale\n", 0)));
        assert!(
            state.patch.is_none(),
            "a late old selection must not become visible"
        );
        state.complete(
            current,
            Ok(page(
                current,
                state.selected.clone().unwrap(),
                "-old\n+selected\n",
                0,
            )),
        );
        let text = state.patch.as_ref().unwrap().text.clone();
        assert_eq!(text.as_ref(), "-old\n+selected\n");
        assert!(state.waiting.is_empty());
        state.sync();
        assert!(
            Arc::ptr_eq(&text, &state.patch.as_ref().unwrap().text),
            "unchanged review data must preserve the patch presentation"
        );
        state.close();
        state.sync();
        assert!(state.patch.is_none());
        assert!(state.page.is_none());
        assert!(state.list.is_none());
    }

    #[test]
    fn two_windows_keep_their_own_file_list_and_same_file_patch_cursor_replies() {
        let mut first = State::default();
        let mut second = State::default();
        let a = first.request(false);
        let b = second.request(false);
        // The query transport routes pages to their requesters; neither
        // query can replace a shared actor-wide response slot.
        assert_ne!(a, b);
        let mut first_list = chat::FileChangesSummary::default();
        first_list.request_id = a;
        first_list.revision = 10;
        let mut second_list = chat::FileChangesSummary::default();
        second_list.request_id = b;
        second_list.revision = 20;
        second.complete(b, Ok(chat::FileChangeReply::Files(second_list)));
        first.complete(a, Ok(chat::FileChangeReply::Files(first_list)));
        first.select(key("same.rs"));
        second.select(key("same.rs"));
        let a = first.request(true);
        let b = second.request(true);
        second.complete(b, Ok(page(b, key("same.rs"), "+continued\n", 8192)));
        first.complete(a, Ok(page(a, key("same.rs"), "+first\n", 0)));
        assert_eq!(first.page().unwrap().patches[0].offset, 0);
        assert_eq!(second.page().unwrap().patches[0].offset, 8192);
        assert_eq!(first.patch.as_ref().unwrap().text.as_ref(), "+first\n");
        assert_eq!(second.patch.as_ref().unwrap().text.as_ref(), "+continued\n");
        assert_eq!(first.list.as_ref().unwrap().revision, 10);
        assert_eq!(second.list.as_ref().unwrap().revision, 20);
    }

    #[test]
    fn delayed_retired_review_reply_is_rejected_by_a_replacement_on_the_same_card() {
        let mut retired = State::default();
        retired.select(key("same.rs"));
        let old = retired.request(true);
        let mut replacement = State::default();
        replacement.select(key("same.rs"));
        let current = replacement.request(true);
        assert_ne!(old, current);
        replacement.complete(old, Ok(page(old, key("same.rs"), "+retired\n", 0)));
        assert!(replacement.page.is_none());
        assert!(replacement.waiting.contains(&current));
        replacement.complete(current, Ok(page(current, key("same.rs"), "+current\n", 0)));
        assert_eq!(
            replacement.patch.as_ref().unwrap().text.as_ref(),
            "+current\n"
        );
    }
    #[test]
    fn only_current_accepted_pages_reset_scroll_and_back_retains_the_file_list() {
        let mut state = State::default();
        let old = state.request(false);
        let current = state.request(false);
        state.list_scroll = 800.0;
        let old_reply = chat::FileChangesSummary {
            request_id: old,
            ..Default::default()
        };
        assert!(!state.complete(old, Ok(chat::FileChangeReply::Files(old_reply))));
        assert_eq!(
            state.list_scroll, 800.0,
            "a late page cannot scroll the current list"
        );
        let current_reply = chat::FileChangesSummary {
            request_id: current,
            ..Default::default()
        };
        assert!(state.complete(current, Ok(chat::FileChangeReply::Files(current_reply))));
        assert_eq!(
            state.list_scroll, 0.0,
            "the accepted next page starts at its first file"
        );
        state.list_scroll = 80.0;
        state.select(key("file.rs"));
        let id = state.request(true);
        assert!(state.complete(id, Ok(page(id, key("file.rs"), "+new\n", 0))));
        state.back();
        assert!(state.selected.is_none());
        assert!(state.patch.is_none());
        assert!(state.list.is_some());
        assert_eq!(state.list_scroll, 80.0);
    }
}
