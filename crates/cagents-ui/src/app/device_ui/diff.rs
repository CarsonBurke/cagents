//! A change's diff in a device window, read by the device's Git and drawn in
//! the device dialog. A device has no repository pane to open it in (#688),
//! so the dialog stands in for one: the comparison, the way through the
//! rest of its Changes group, and the stage or unstage that finishes with it.
use super::*;
use crate::repository::{
    ChangeArea, DiffBasis, DiffContent, DiffEntry, DiffRequest, DiffSide, RepositoryReply,
    RepositoryRequest, UnstageRequest,
};
use chat_ui::patch_view;
use std::sync::atomic::{AtomicU64, Ordering};

/// Reads outlive the diff that asked for them, so each belongs to this
/// process rather than to one dialog.
static NEXT_READ: AtomicU64 = AtomicU64::new(1);
/// Past this many bytes of patch, it is drawn in the text colour, as a long
/// patch in a conversation is.
const HIGHLIGHT_LIMIT: usize = 256 * 1024;
/// Past this many bytes of the two texts, they are listed as one hunk of
/// removed and added lines rather than compared. A device sends sides of at
/// most 2 MiB, so this only bounds a future larger limit.
const COMPARED: usize = 8 * 1024 * 1024;
/// The widest the dialog grows: a diff reads best at a code editor's width.
const MAX_WIDTH: f32 = 1040.0;
/// About what the dialog's padding, two-line header and actions take, which
/// the change's scrolled region leaves them.
const CHROME_HEIGHT: f32 = 2.0 * space::L + 2.0 * space::M + 84.0;

pub(in crate::app) struct Diff {
    key: ActiveWorktreeKey,
    request: DiffRequest,
    /// The Changes group it was opened from, which Previous, Next and Stage
    /// act in. A committed comparison has none.
    area: Option<ChangeArea>,
    read: u64,
    reading: bool,
    refresh_owed: bool,
    showing_file: bool,
    content: Content,
    view: patch_view::ViewState,
    /// The source-control read current when this dialog last staged or
    /// unstaged a change. Until a later read lands, the rows it steps through
    /// still list that change where it was, so it doesn't stage again.
    settling: Option<u64>,
}

enum Content {
    Loading,
    Ready(PreparedRead),
    Failed(String),
}

/// The patch and the revision's file, both from the same Git answer.
#[derive(Debug, Clone)]
pub struct PreparedRead {
    patch: Prepared,
    file: Option<CommittedFile>,
}

#[derive(Debug, Clone)]
struct CommittedFile {
    text: Arc<str>,
    label: String,
}

/// A comparison read for drawing, off the update thread.
#[derive(Debug, Clone)]
pub enum Prepared {
    Patch {
        model: Arc<patch_view::PatchModel>,
        /// The patch as text, for one too irregular to draw as rows.
        text: Arc<str>,
    },
    /// Both sides hold the same text.
    Same,
    /// It has no text to compare, as the heading and detail say.
    Note { title: String, detail: String },
}

#[derive(Debug, Clone)]
pub enum Action {
    Loaded {
        read: u64,
        result: Result<PreparedRead, String>,
    },
    View(patch_view::Event),
    /// To the previous or next change in its group.
    Step {
        forward: bool,
    },
    /// Stages the change, or unstages a staged one, then moves on.
    Stage,
    /// Opens the working copy in Device files.
    OpenFile,
    /// Shows the immutable contents in place of their comparison.
    ShowFile(bool),
}

/// A working change's comparison, as the local Changes panel opens it.
pub(in crate::app) fn working_request(
    key: &ActiveWorktreeKey,
    row: &sidebar::ChangeRow,
    area: ChangeArea,
) -> DiffRequest {
    DiffRequest {
        worktree: key.canonical_path.clone(),
        basis: if area == ChangeArea::Staged {
            DiffBasis::HeadToIndex
        } else {
            DiffBasis::IndexToWorktree
        },
        path: row.entry.path.clone(),
        old_path: row.entry.old_path.clone(),
    }
}

/// Reads `content` for drawing, with its syntax coloured in `theme`.
fn prepare(
    content: &DiffContent,
    path: &Path,
    theme: crate::settings::EditorSyntaxTheme,
) -> Prepared {
    if let DiffContent::Comparison {
        old,
        new,
        old_label,
        new_label,
    } = content
        && old.is_diffable()
        && new.is_diffable()
    {
        let (before, after) = (
            old.text().unwrap_or_default(),
            new.text().unwrap_or_default(),
        );
        let text = crate::unified_diff::unified(before, after, "", COMPARED);
        if text.is_empty() {
            if before == after {
                return Prepared::Same;
            }
            // The comparison reads a last line the same whether or not a
            // newline ends it, so that is all that differs.
            let unended = if before.ends_with('\n') {
                new_label
            } else {
                old_label
            };
            return Prepared::Note {
                title: "Only the final newline changed".into(),
                detail: format!("{unended}: no newline at the end of the file"),
            };
        }
        let text: Arc<str> = text.into();
        let theme = (text.len() <= HIGHLIGHT_LIMIT).then_some(theme);
        return Prepared::Patch {
            model: Arc::new(patch_view::PatchModel::new(
                text.clone(),
                path,
                crate::chat::LineOrigin::File,
                theme,
            )),
            text,
        };
    }
    let (title, detail) = crate::repository_widget::no_text_summary(content);
    Prepared::Note { title, detail }
}

impl PreparedRead {
    fn new(
        content: DiffContent,
        request: &DiffRequest,
        theme: crate::settings::EditorSyntaxTheme,
    ) -> Self {
        let patch = prepare(&content, request.path.as_path(), theme);
        let file = match content {
            DiffContent::Comparison {
                old,
                new,
                old_label,
                new_label,
            } if request.basis.is_immutable() => {
                // A deletion only has the parent/base version to show.
                let (side, label) = if matches!(new, DiffSide::Absent) {
                    (old, old_label)
                } else {
                    (new, new_label)
                };
                match side {
                    DiffSide::Text { text, .. } => Some(CommittedFile {
                        text: text.into(),
                        label,
                    }),
                    _ => None,
                }
            }
            _ => None,
        };
        Self { patch, file }
    }
}

impl Diff {
    fn read(&mut self, theme: crate::settings::EditorSyntaxTheme) -> Task<Message> {
        let read = NEXT_READ.fetch_add(1, Ordering::Relaxed);
        self.read = read;
        self.reading = true;
        self.refresh_owed = false;
        let request = self.request.clone();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let content = crate::device_service::answer_repository(
                        RepositoryRequest::Diff(request.clone()),
                        &CancellationToken::default(),
                    )
                    .and_then(RepositoryReply::into_diff)?;
                    Ok(PreparedRead::new(content, &request, theme))
                })
                .await
                .unwrap_or_else(|error| Err(error.to_string()))
            },
            move |result| {
                cosmic::Action::App(Message::Device(super::Action::Diff(Action::Loaded {
                    read,
                    result,
                })))
            },
        )
    }
}

impl Cagents {
    /// Opens `request`'s comparison in the device dialog. `area` is the
    /// Changes group it belongs to, for a working change.
    pub(in crate::app) fn open_device_diff(
        &mut self,
        key: ActiveWorktreeKey,
        request: DiffRequest,
        area: Option<ChangeArea>,
    ) -> Task<Message> {
        self.device_ui.open = true;
        self.device_ui.menu_open = false;
        self.device_ui.configure = false;
        self.device_ui.choosing = false;
        self.device_ui.notice = None;
        // A step keeps showing as much of the next change as of the last.
        let (view, settling) = match &self.device_ui.diff {
            Some(diff) if diff.key == key => (diff.view.clone(), diff.settling),
            _ => (opened_view(), None),
        };
        let mut diff = Diff {
            key,
            request,
            area,
            read: 0,
            reading: false,
            refresh_owed: false,
            showing_file: false,
            content: Content::Loading,
            view,
            settling,
        };
        let read = diff.read(self.settings.editor_syntax_theme);
        self.device_ui.diff = Some(diff);
        read
    }

    pub(super) fn update_device_diff(&mut self, action: Action) -> Task<Message> {
        if self.device_ui.diff.is_none() {
            return Task::none();
        }
        match action {
            Action::Loaded { read, result } => {
                if let Some(diff) = self.device_ui.diff.as_mut()
                    && read == diff.read
                {
                    diff.reading = false;
                    diff.content = match result {
                        Ok(prepared) => Content::Ready(prepared),
                        Err(error) => Content::Failed(error),
                    };
                    if diff.refresh_owed {
                        // Keep making visible progress if a checkout is
                        // edited more often than the device can read a diff.
                        return diff.read(self.settings.editor_syntax_theme);
                    }
                }
                Task::none()
            }
            Action::View(event) => {
                if let Some(diff) = self.device_ui.diff.as_mut() {
                    diff.view.apply(event);
                }
                Task::none()
            }
            Action::Step { forward } => {
                self.device_source_activity(false);
                self.step_device_diff(forward)
            }
            Action::Stage => {
                if !self.device_diff_can_stage() {
                    return Task::none();
                }
                let Some((key, area, rows, Some(index))) = self.device_diff_position() else {
                    return Task::none();
                };
                let key = key.clone();
                let path = rows[index].entry.path.clone();
                let selection = vec![rows[index].selection()];
                // What it moves on to once this change leaves the group.
                let after = rows
                    .get(index + 1)
                    .or_else(|| index.checked_sub(1).and_then(|index| rows.get(index)))
                    .map(|row| working_request(&key, row, area));
                let unstage = area == ChangeArea::Staged;
                let kind = if unstage {
                    MutationKind::Unstage
                } else {
                    MutationKind::Stage
                };
                let Some(operation) = self.begin_mutation(&key, kind, None) else {
                    self.device_ui.notice =
                        Some("Another change to this worktree is still running.".into());
                    return Task::none();
                };
                let change = if unstage {
                    let request = UnstageRequest {
                        worktree: key.canonical_path.clone(),
                        selection,
                    };
                    Self::mutation_task(
                        key.clone(),
                        operation,
                        RepositoryRequest::Unstage(request),
                        |reply| {
                            (
                                mutation_report(
                                    "Unstage",
                                    reply.and_then(RepositoryReply::into_mutation),
                                ),
                                false,
                            )
                        },
                    )
                } else {
                    Self::stage_task(key.clone(), operation, selection)
                };
                let change = change.map(move |mut action| {
                    if let cosmic::Action::App(Message::SourceControlMutated {
                        report: Some(report),
                        ..
                    }) = &mut action
                    {
                        *report = format!("{path}\n{report}");
                    }
                    action
                });
                let next = match after {
                    Some(request) => self.open_device_diff(key, request, Some(area)),
                    None => self.update_device(super::Action::Close),
                };
                let generation = self.refresh_ui.source_generation;
                if let Some(diff) = self.device_ui.diff.as_mut() {
                    diff.settling = Some(generation);
                }
                Task::batch([change, next])
            }
            Action::OpenFile => {
                let Some(diff) = self
                    .device_ui
                    .diff
                    .take_if(|diff| !diff.request.basis.is_immutable())
                else {
                    return Task::none();
                };
                let path = diff.key.canonical_path.join(diff.request.path.as_path());
                self.update_device(super::Action::OpenFile {
                    path,
                    fragment: None,
                })
            }
            Action::ShowFile(showing) => {
                if let Some(diff) = self.device_ui.diff.as_mut()
                    && matches!(&diff.content, Content::Ready(read) if read.file.is_some())
                {
                    diff.showing_file = showing;
                }
                Task::none()
            }
        }
    }

    pub(in crate::app) fn device_diff_key(
        &mut self,
        event: &keyboard::Event,
        captured: bool,
    ) -> Option<Task<Message>> {
        if !self.device_ui.open
            || self.device_ui.diff.is_none()
            || self.device_ui.worktree_source.is_some()
            || self.transient_work_close.is_some()
            || self.automation_ui.draft_dialog_open()
        {
            return None;
        }
        let keyboard::Event::KeyPressed {
            key,
            physical_key,
            modifiers,
            repeat: false,
            ..
        } = event
        else {
            return None;
        };
        // An arrow can be captured by a widget beneath the modal that
        // still has keyboard focus. The modal owns its navigation chord.
        let action =
            if modifiers.alt() && !modifiers.control() && !modifiers.logo() && !modifiers.shift() {
                use keyboard::key::{Code, Named, Physical};
                match (key, physical_key) {
                    (keyboard::Key::Named(Named::ArrowUp), _)
                    | (_, Physical::Code(Code::ArrowUp)) => Action::Step { forward: false },
                    (keyboard::Key::Named(Named::ArrowDown), _)
                    | (_, Physical::Code(Code::ArrowDown)) => Action::Step { forward: true },
                    _ => return None,
                }
            } else if !captured
                && settings::primary_alone(modifiers.control(), modifiers.alt(), modifiers.logo())
                && modifiers.shift()
                && crate::interaction::latin_key(key, *physical_key) == Some('s')
            {
                Action::Stage
            } else {
                return None;
            };
        Some(self.update_device_diff(action))
    }

    fn step_device_diff(&mut self, forward: bool) -> Task<Message> {
        let Some(diff) = self.device_ui.diff.as_ref() else {
            return Task::none();
        };
        let (request, area) = if let Some((key, area, rows, index)) = self.device_diff_position() {
            let Some(row) = step_index(index, forward).and_then(|index| rows.get(index)) else {
                return Task::none();
            };
            (working_request(key, row, area), Some(area))
        } else if let Some((rows, index)) = self.device_diff_commit_position() {
            let Some(row) = step_index(Some(index), forward).and_then(|index| rows.get(index))
            else {
                return Task::none();
            };
            (
                DiffRequest {
                    path: row.path.clone(),
                    old_path: row.old_path.clone(),
                    ..diff.request.clone()
                },
                None,
            )
        } else {
            return Task::none();
        };
        let key = diff.key.clone();
        self.open_device_diff(key, request, area)
    }

    /// Notes that the source-control read `request` landed. One that began
    /// after the dialog's last stage lists the change where it now is.
    pub(in crate::app) fn device_diff_rows_read(&mut self, request: u64) {
        if let Some(diff) = self.device_ui.diff.as_mut()
            && diff.settling.is_some_and(|staged| staged != request)
        {
            diff.settling = None;
        }
    }

    /// Whether the shown change can be staged or unstaged now: no change to
    /// its worktree is running, and the rows show the last one's result.
    fn device_diff_can_stage(&self) -> bool {
        let busy = self
            .sidebar_model
            .source_control
            .as_ref()
            .is_some_and(sidebar::SourceControlModel::busy);
        !busy
            && self.device_ui.diff.as_ref().is_some_and(|diff| {
                diff.settling.is_none()
                    && diff.area.is_some_and(|area| area != ChangeArea::Conflict)
            })
    }

    /// The shown change's group, its rows, and where the change is among
    /// them: `None` once it has left the group, as a staged change leaves
    /// Unstaged.
    fn device_diff_position(
        &self,
    ) -> Option<(
        &ActiveWorktreeKey,
        ChangeArea,
        Vec<&sidebar::ChangeRow>,
        Option<usize>,
    )> {
        let diff = self.device_ui.diff.as_ref()?;
        let area = diff.area?;
        if !self.source_control_is_bound(&diff.key) {
            return None;
        }
        // Previous and Next follow the panel's own order.
        let rows: Vec<_> = self
            .sidebar_model
            .source_control
            .as_ref()?
            .group(area)?
            .listed()
            .collect();
        let index = rows
            .iter()
            .position(|row| row.entry.path == diff.request.path);
        Some((&diff.key, area, rows, index))
    }

    fn device_diff_commit_position(&self) -> Option<(&[DiffEntry], usize)> {
        let diff = self.device_ui.diff.as_ref()?;
        let DiffBasis::CommitToParent { oid, .. } = &diff.request.basis else {
            return None;
        };
        let changes = self
            .sidebar_model
            .source_control
            .as_ref()
            .filter(|source| source.key == diff.key)?
            .commit_changes
            .as_ref()
            .filter(|changes| changes.commit.oid == *oid)?;
        let rows = changes.result.as_ref()?.as_ref().ok()?;
        let index = rows.iter().position(|row| row.path == diff.request.path)?;
        Some((rows, index))
    }

    /// A status read saw a new digest. Re-read the current comparison, but
    /// keep its patch in view and let an outstanding read finish first.
    pub(in crate::app) fn refresh_device_working_diff(
        &mut self,
        key: &ActiveWorktreeKey,
    ) -> Task<Message> {
        let Some(diff) = self.device_ui.diff.as_mut().filter(|diff| &diff.key == key) else {
            return Task::none();
        };
        let Some(area) = diff.area else {
            return Task::none();
        };
        diff.request.old_path = self
            .sidebar_model
            .source_control
            .as_ref()
            .and_then(|source| source.group(area))
            .and_then(|group| {
                group
                    .rows()
                    .iter()
                    .find(|row| row.entry.path == diff.request.path)
            })
            .and_then(|row| row.entry.old_path.clone());
        if diff.reading {
            diff.refresh_owed = true;
            Task::none()
        } else {
            diff.read(self.settings.editor_syntax_theme)
        }
    }

    pub(in crate::app) fn device_working_diff_unavailable(
        &mut self,
        key: &ActiveWorktreeKey,
        error: &str,
    ) {
        if let Some(diff) = self.device_ui.diff.as_mut()
            && &diff.key == key
            && diff.area.is_some()
        {
            // An older answer can no longer replace the reported failure.
            diff.read = NEXT_READ.fetch_add(1, Ordering::Relaxed);
            diff.reading = false;
            diff.refresh_owed = false;
            diff.content = Content::Failed(error.to_owned());
        }
    }

    fn device_diff_notice(&self, key: &ActiveWorktreeKey) -> Option<&str> {
        self.device_ui.notice.as_deref().or_else(|| {
            self.sidebar_model
                .source_control
                .as_ref()
                .filter(|source| source.key == *key)
                .and_then(|source| source.error.as_deref())
        })
    }

    pub(super) fn device_diff_layout(&self, size: iced::Size) -> Option<Element<'_, Message>> {
        let diff = self.device_ui.diff.as_ref()?;
        let action = |action| Message::Device(super::Action::Diff(action));
        let path = diff.request.path.as_path();
        let name = path.file_name().map_or_else(
            || diff.request.path.to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        let mut context = Vec::with_capacity(3);
        if let Some(directory) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            context.push(directory.display().to_string());
        }
        if let Some(old) = &diff.request.old_path {
            context.push(format!("from {old}"));
        }
        let prepared = match &diff.content {
            Content::Ready(read) => Some(read),
            _ => None,
        };
        let file = prepared.and_then(|read| read.file.as_ref());
        let showing_file = file.filter(|_| diff.showing_file);
        if let Some(file) = showing_file {
            context.push(format!("{} · Read only", file.label));
        } else {
            context.push(crate::repository_widget::basis_label(&diff.request.basis));
            if let Some(Prepared::Patch { model, .. }) = prepared.map(|read| &read.patch)
                && let Some((added, removed)) = model.counts()
            {
                context.push(format!("+{added} −{removed}"));
            }
        }
        let position = self.device_diff_position();
        let can_stage = self.device_diff_can_stage();
        let mut header: Vec<Element<'_, Message>> = vec![
            widget::column::with_children(vec![
                design::heading(name)
                    .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                    .into(),
                design::caption(context.join(" · "))
                    .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                    .into(),
            ])
            .spacing(space::XXS)
            .width(Length::Fill)
            .into(),
        ];
        let neighbors = if let Some((_, _, rows, index)) = &position {
            Some((rows.len(), *index))
        } else {
            self.device_diff_commit_position()
                .map(|(rows, index)| (rows.len(), Some(index)))
        };
        if let Some((count, index)) = neighbors
            && count > usize::from(index.is_some())
        {
            let previous = step_index(index, false).is_some();
            let next = step_index(index, true).is_some_and(|index| index < count);
            for (icon, label, chord, enabled, forward) in [
                (
                    Icon::ChevronUp,
                    "Previous change",
                    "Alt+Up",
                    previous,
                    false,
                ),
                (Icon::ChevronDown, "Next change", "Alt+Down", next, true),
            ] {
                let button = crate::ui_controls::icon_button(
                    icon,
                    crate::interaction::RichTip::new(label).shortcut(chord),
                )
                .on_press_maybe(enabled.then(|| action(Action::Step { forward })));
                header.push(button.into());
            }
        }
        let width = size.width.min(MAX_WIDTH);
        // The patch fits beside the region's scrollbar rail, so only a line
        // longer than that scrolls sideways.
        let body_width = (width - 2.0 * space::L - design::SCROLLBAR_GUTTER).max(1.0);
        let body: Element<'_, Message> = if let Some(file) = showing_file {
            widget::text(file.text.as_ref())
                .font(iced::Font::MONOSPACE)
                .size(design::TEXT_CAPTION)
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                .into()
        } else {
            match &diff.content {
                Content::Loading => design::body("Reading the change from the device…").into(),
                Content::Failed(error) => widget::column::with_children(vec![
                    design::heading("Couldn't read this change").into(),
                    design::body(error.as_str())
                        .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                        .into(),
                ])
                .spacing(space::XS)
                .into(),
                Content::Ready(read) => match &read.patch {
                    Prepared::Same => design::body("Both sides hold the same text.").into(),
                    Prepared::Note { title, detail } => widget::column::with_children(vec![
                        design::heading(title.as_str()).into(),
                        design::body(detail.as_str())
                            .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                            .into(),
                    ])
                    .spacing(space::XS)
                    .into(),
                    Prepared::Patch { model, text } => model
                        .view(
                            &diff.view,
                            patch_view::Options {
                                width: body_width,
                                theme: self.settings.editor_syntax_theme,
                            },
                            move |event| action(Action::View(event)),
                            patch_view::plain,
                        )
                        .unwrap_or_else(|| {
                            widget::text(text.as_ref())
                                .font(iced::Font::MONOSPACE)
                                .size(design::TEXT_CAPTION)
                                .into()
                        }),
                },
            }
        };
        let mut actions: Vec<Element<'_, Message>> = Vec::new();
        if file.is_some() {
            let label = if diff.showing_file {
                "Back to diff"
            } else {
                "View committed file"
            };
            actions.push(
                crate::ui_controls::button::secondary(label)
                    .on_press(action(Action::ShowFile(!diff.showing_file)))
                    .into(),
            );
        } else if !diff.request.basis.is_immutable() {
            actions.push(
                crate::ui_controls::button::secondary("Open file")
                    .icon(Icon::OpenFolder)
                    .name("Open this file in Device files")
                    .on_press(action(Action::OpenFile))
                    .into(),
            );
        }
        actions.extend([
            widget::Space::new().width(Length::Fill).into(),
            crate::ui_controls::button::secondary("Close")
                .on_press(Message::Device(super::Action::Close))
                .into(),
        ]);
        // A conflict is marked resolved from Changes, which first checks it
        // for conflict markers.
        if let Some((_, area, _, Some(_))) = position
            && area != ChangeArea::Conflict
        {
            let label = if area == ChangeArea::Staged {
                "Unstage"
            } else {
                "Stage"
            };
            let binding = if cfg!(target_os = "macos") {
                "Cmd+Shift+S"
            } else {
                "Ctrl+Shift+S"
            };
            let hint = format!("{label} ({binding})");
            let button = crate::ui_controls::button::primary(label)
                .name(hint.clone())
                .on_press_maybe(can_stage.then(|| action(Action::Stage)));
            actions.push(crate::interaction::tooltip(button, hint));
        }
        // A stage can finish after the dialog advances. Keep its result
        // above the patch so it remains visible while reading the next file.
        let mut rows = vec![
            widget::row::with_children(header)
                .spacing(space::XS)
                .align_y(Alignment::Start)
                .into(),
        ];
        let mut chrome_height = CHROME_HEIGHT;
        if let Some(notice) = self.device_diff_notice(&diff.key) {
            const NOTICE_HEIGHT: f32 = 64.0;
            rows.push(dialog_ui::scrolled_region(
                design::body(notice)
                    .class(cosmic::theme::Text::Custom(design::destructive_text))
                    .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
                NOTICE_HEIGHT,
            ));
            chrome_height += NOTICE_HEIGHT + space::M;
        }
        let height = (size.height - chrome_height).max(120.0);
        rows.extend([
            dialog_ui::scrolled_region(body, height),
            widget::row::with_children(actions)
                .spacing(space::S)
                .align_y(Alignment::Center)
                .into(),
        ]);
        Some(dialog_ui::surface(
            widget::column::with_children(rows).spacing(space::M),
            width,
        ))
    }
}

fn step_index(index: Option<usize>, forward: bool) -> Option<usize> {
    if forward {
        index.map_or(Some(0), |index| index.checked_add(1))
    } else {
        index.and_then(|index| index.checked_sub(1))
    }
}

/// A newly opened diff shows the whole change, not a preview of it.
fn opened_view() -> patch_view::ViewState {
    let mut view = patch_view::ViewState::default();
    view.apply(patch_view::Event::ShowMore);
    view
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WorktreeId;

    #[test]
    fn the_next_device_diff_shows_the_worktrees_stage_failure() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let key = ActiveWorktreeKey {
            project_id: ProjectId::new(),
            worktree_id: WorktreeId::new(),
            canonical_path: "/repo".into(),
        };
        let mut source = sidebar::SourceControlModel::loading(key.clone());
        source.set_error(Some("Stage could not update the Git index.".into()));
        app.sidebar_model.source_control = Some(source);
        assert_eq!(
            app.device_diff_notice(&key),
            Some("Stage could not update the Git index.")
        );
        let another = ActiveWorktreeKey {
            worktree_id: WorktreeId::new(),
            ..key.clone()
        };
        assert_eq!(app.device_diff_notice(&another), None);
        app.device_ui.notice = Some("Another change to this worktree is still running.".into());
        assert_eq!(
            app.device_diff_notice(&key),
            Some("Another change to this worktree is still running.")
        );
    }

    fn text(text: &str) -> DiffSide {
        DiffSide::Text {
            text: text.into(),
            bytes: text.len() as u64,
        }
    }

    fn comparison(old: DiffSide, new: DiffSide) -> DiffContent {
        DiffContent::Comparison {
            old,
            new,
            old_label: "Index".into(),
            new_label: "Working tree".into(),
        }
    }

    #[test]
    fn a_text_change_is_drawn_as_its_patch() {
        let prepared = prepare(
            &comparison(text("one\ntwo\n"), text("one\n2\n")),
            Path::new("notes.txt"),
            crate::settings::EditorSyntaxTheme::default(),
        );
        let Prepared::Patch { model, text } = prepared else {
            panic!("expected a patch, got {prepared:?}");
        };
        assert_eq!(&*text, "@@ -1,2 +1,2 @@\n one\n-two\n+2\n");
        assert!(!model.is_raw());
        assert_eq!(model.counts(), Some((1, 1)));
    }

    #[test]
    fn a_change_with_no_text_says_what_its_sides_are() {
        assert!(matches!(
            prepare(
                &comparison(text("same\n"), text("same\n")),
                Path::new("same.txt"),
                crate::settings::EditorSyntaxTheme::default(),
            ),
            Prepared::Same
        ));
        let Prepared::Note { title, detail } = prepare(
            &comparison(text("small\n"), DiffSide::Oversize { bytes: 3_000_000 }),
            Path::new("big.log"),
            crate::settings::EditorSyntaxTheme::default(),
        ) else {
            panic!("expected a note");
        };
        assert_eq!(title, "No text diff between Index and Working tree");
        assert_eq!(detail, "Working tree: 3000000 bytes, too large to compare");
        let Prepared::Note { title, detail } = prepare(
            &comparison(text("last\n"), text("last")),
            Path::new("unended.txt"),
            crate::settings::EditorSyntaxTheme::default(),
        ) else {
            panic!("a dropped final newline is a change");
        };
        assert_eq!(title, "Only the final newline changed");
        assert_eq!(detail, "Working tree: no newline at the end of the file");
    }

    #[test]
    fn device_commit_diff_steps_through_the_commits_paths_and_shows_the_committed_text() {
        use crate::repository::{ChangeKind, CommitIdentity, Oid, RepoPath};

        let mut app = crate::app::tests::context_inspector_test_app(false);
        let key = ActiveWorktreeKey {
            project_id: ProjectId::new(),
            worktree_id: WorktreeId::new(),
            canonical_path: "/repo".into(),
        };
        let oid = Oid::parse_object("11".repeat(20)).unwrap();
        let parent = Oid::parse_object("22".repeat(20)).unwrap();
        let identity = CommitIdentity {
            name: "Fixture".into(),
            email: "fixture@example.invalid".into(),
            timestamp: 0,
            offset_minutes: 0,
        };
        let mut source = SourceControlModel::loading(key.clone());
        source.append_history(&[CommitSummary {
            oid: oid.clone(),
            parents: vec![parent.clone()],
            author: identity.clone(),
            committer: identity,
            subject: "Update two files".into(),
            body: String::new(),
        }]);
        let a = RepoPath::parse("a.txt").unwrap();
        let b = RepoPath::parse("b.txt").unwrap();
        let prior = RepoPath::parse("old-b.txt").unwrap();
        source.commit_changes = Some(sidebar::CommitChanges {
            commit: source.history[0].clone(),
            visible_files: 1,
            result: Some(Ok(vec![
                DiffEntry {
                    path: a.clone(),
                    old_path: None,
                    kind: ChangeKind::Modified,
                    added_lines: Some(1),
                    removed_lines: Some(1),
                },
                DiffEntry {
                    path: b.clone(),
                    old_path: Some(prior.clone()),
                    kind: ChangeKind::Renamed,
                    added_lines: None,
                    removed_lines: None,
                },
            ])),
        });
        app.sidebar_model.source_control = Some(source);
        let request = DiffRequest {
            worktree: key.canonical_path.clone(),
            basis: DiffBasis::CommitToParent {
                oid,
                parent: Some(parent),
            },
            path: a.clone(),
            old_path: None,
        };
        drop(app.open_device_diff(key, request.clone(), None));
        assert!(!app.device_diff_can_stage());
        let mut next = dialog_ui::tests::key_press(keyboard::key::Named::ArrowDown);
        if let keyboard::Event::KeyPressed { modifiers, .. } = &mut next {
            *modifiers = keyboard::Modifiers::ALT | keyboard::Modifiers::CAPS_LOCK;
        }
        drop(
            app.device_diff_key(&next, true)
                .expect("Alt+Down belongs to the open dialog"),
        );
        let diff = app.device_ui.diff.as_ref().unwrap();
        assert_eq!(diff.request.path, b);
        assert_eq!(diff.request.old_path, Some(prior));
        assert_eq!(diff.request.basis, request.basis);
        drop(app.update_device_diff(Action::Step { forward: false }));
        let diff = app.device_ui.diff.as_ref().unwrap();
        assert_eq!(diff.request.path, a);

        let content = DiffContent::Comparison {
            old: text("parent bytes\n"),
            new: text("committed bytes\n"),
            old_label: "Parent".into(),
            new_label: "Commit".into(),
        };
        let result = PreparedRead::new(content, &request, app.settings.editor_syntax_theme);
        assert_eq!(
            result.file.as_ref().unwrap().text.as_ref(),
            "committed bytes\n"
        );
        drop(app.update_device_diff(Action::Loaded {
            read: diff.read,
            result: Ok(result),
        }));
        drop(app.update_device_diff(Action::ShowFile(true)));
        assert!(app.device_ui.diff.as_ref().unwrap().showing_file);
        drop(app.update_device_diff(Action::OpenFile));
        assert!(
            app.device_ui.diff.is_some(),
            "a commit never opens the current file"
        );
        drop(app.update_device_diff(Action::ShowFile(false)));
        assert!(!app.device_ui.diff.as_ref().unwrap().showing_file);
    }

    #[test]
    fn a_device_diff_refresh_keeps_the_patch_and_combines_overlapping_updates() {
        use crate::repository::RepoPath;
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let key = ActiveWorktreeKey {
            project_id: ProjectId::new(),
            worktree_id: WorktreeId::new(),
            canonical_path: "/repo".into(),
        };
        let request = DiffRequest {
            worktree: key.canonical_path.clone(),
            basis: DiffBasis::IndexToWorktree,
            path: RepoPath::parse("a.txt").unwrap(),
            old_path: None,
        };
        let same = || {
            Ok(PreparedRead {
                patch: Prepared::Same,
                file: None,
            })
        };
        drop(app.open_device_diff(key.clone(), request, Some(ChangeArea::Unstaged)));
        let first = app.device_ui.diff.as_ref().unwrap().read;
        drop(app.update_device_diff(Action::Loaded {
            read: first,
            result: same(),
        }));
        drop(app.refresh_device_working_diff(&key));
        let refreshing = app.device_ui.diff.as_ref().unwrap().read;
        assert_ne!(first, refreshing);
        assert!(matches!(
            app.device_ui.diff.as_ref().unwrap().content,
            Content::Ready(_)
        ));

        drop(app.refresh_device_working_diff(&key));
        assert_eq!(app.device_ui.diff.as_ref().unwrap().read, refreshing);
        drop(app.update_device_diff(Action::Loaded {
            read: refreshing,
            result: Ok(PreparedRead {
                patch: Prepared::Note {
                    title: "Second answer".into(),
                    detail: "A newer read is still owed.".into(),
                },
                file: None,
            }),
        }));
        assert!(matches!(
            &app.device_ui.diff.as_ref().unwrap().content,
            Content::Ready(PreparedRead { patch: Prepared::Note { title, .. }, .. })
                if title == "Second answer"
        ));
        let latest = app.device_ui.diff.as_ref().unwrap().read;
        assert_ne!(latest, refreshing);
        app.device_working_diff_unavailable(&key, "Checkout moved");
        drop(app.update_device_diff(Action::Loaded {
            read: latest,
            result: same(),
        }));
        assert!(matches!(
            &app.device_ui.diff.as_ref().unwrap().content,
            Content::Failed(error) if error == "Checkout moved"
        ));
    }

    fn git(root: &Path, arguments: &[&str]) {
        let status = std::process::Command::new("git")
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .current_dir(root)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
            ])
            .args(arguments)
            .status()
            .unwrap();
        assert!(status.success(), "git {arguments:?}");
    }

    #[test]
    fn a_device_change_opens_in_the_dialog_and_stages_on_to_the_next() {
        const CHILD: &str = "CAGENTS_TEST_DEVICE_DIFF";
        const TEST: &str = "app::device_ui::diff::tests::a_device_change_opens_in_the_dialog_and_stages_on_to_the_next";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env(CHILD, "1")
                .env("XDG_CONFIG_HOME", root.path().join("config"))
                .env("XDG_DATA_HOME", root.path().join("data"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        for name in ["a.txt", "b.txt"] {
            std::fs::write(root.join(name), "before\n").unwrap();
        }
        git(&root, &["init", "-q", "--template="]);
        git(&root, &["add", "a.txt", "b.txt"]);
        git(&root, &["commit", "-qm", "fixture"]);
        for name in ["a.txt", "b.txt"] {
            std::fs::write(root.join(name), "after\n").unwrap();
        }
        // Read as the device's daemon would, before this window is one on it.
        let snapshot = crate::repository::RepositoryService::system("git")
            .snapshot_with_options(
                &root,
                crate::repository::SnapshotOptions::default(),
                &CancellationToken::default(),
            )
            .unwrap();
        crate::device::save(crate::device::Device {
            name: "diff-test".into(),
            host: "build-host".into(),
            executable: "cagents".into(),
        })
        .unwrap();
        crate::device::select(Some("diff-test")).unwrap();
        let mut app = super::super::super::tests::context_inspector_test_app(true);
        let project_id = ProjectId::new();
        let worktree_id = crate::model::WorktreeId::new();
        *app.sidebar_model.projects = vec![SidebarProject::new(
            project_id,
            RepositoryId(project_id.as_uuid()),
            "Remote".into(),
            root.join(".git"),
            vec![sidebar::SidebarWorktree::from_durable(
                worktree_id,
                root.clone(),
                Some("main".into()),
            )],
            Vec::new(),
        )];
        drop(app.update(Message::Sidebar(SidebarAction::OpenWorktree {
            project_id,
            worktree_id,
            path: root.clone(),
        })));
        let key = app.sidebar_state.active_worktree().cloned().unwrap();
        app.sidebar_model.source_control =
            Some(sidebar::SourceControlModel::ready(key.clone(), &snapshot));
        let shown = |app: &Cagents| {
            app.device_ui
                .diff
                .as_ref()
                .map(|diff| diff.request.path.to_string())
        };

        drop(app.update(Message::Sidebar(SidebarAction::OpenChange {
            key: key.clone(),
            path: crate::repository::RepoPath::parse("a.txt").unwrap(),
            area: ChangeArea::Unstaged,
        })));
        assert!(app.device_ui.open, "the change opens in the device dialog");
        assert_eq!(shown(&app).as_deref(), Some("a.txt"));
        let diff = app.device_ui.diff.as_ref().unwrap();
        assert_eq!(diff.request.basis, DiffBasis::IndexToWorktree);
        assert!(matches!(diff.content, Content::Loading));
        let read = diff.read;
        drop(app.update(Message::Device(super::super::Action::Diff(
            Action::Loaded {
                read: read + 1,
                result: Ok(PreparedRead {
                    patch: Prepared::Same,
                    file: None,
                }),
            },
        ))));
        assert!(
            matches!(
                app.device_ui.diff.as_ref().unwrap().content,
                Content::Loading
            ),
            "another read's answer is not this change's"
        );
        drop(app.update(Message::Device(super::super::Action::Diff(
            Action::Loaded {
                read,
                result: Ok(PreparedRead {
                    patch: Prepared::Same,
                    file: None,
                }),
            },
        ))));
        assert!(matches!(
            app.device_ui.diff.as_ref().unwrap().content,
            Content::Ready(PreparedRead {
                patch: Prepared::Same,
                ..
            })
        ));

        let step = |forward| Message::Device(super::super::Action::Diff(Action::Step { forward }));
        drop(app.update(step(true)));
        assert_eq!(shown(&app).as_deref(), Some("b.txt"));
        drop(app.update(step(true)));
        assert_eq!(
            shown(&app).as_deref(),
            Some("b.txt"),
            "b.txt is the last change"
        );
        drop(app.update(step(false)));
        assert_eq!(shown(&app).as_deref(), Some("a.txt"));

        drop(app.update(Message::Device(super::super::Action::Diff(Action::Stage))));
        assert!(
            app.sidebar_model.source_control.as_ref().unwrap().busy(),
            "staging runs as the Changes panel's own stage does"
        );
        assert_eq!(
            shown(&app).as_deref(),
            Some("b.txt"),
            "staging moves on to the next change"
        );

        // The stage finishes, but until the device is read again the rows
        // still list a.txt as unstaged, where staging b.txt would move on to.
        let staged_at = app.refresh_ui.source_generation;
        app.source_control_mutations.clear();
        app.sidebar_model
            .source_control
            .as_mut()
            .unwrap()
            .set_mutation(None);
        assert!(!app.device_diff_can_stage(), "the rows predate the stage");
        drop(app.update(Message::Device(super::super::Action::Diff(Action::Stage))));
        assert!(
            !app.sidebar_model.source_control.as_ref().unwrap().busy(),
            "nothing is staged from rows that predate the last stage"
        );
        assert_eq!(shown(&app).as_deref(), Some("b.txt"));
        app.device_diff_rows_read(staged_at);
        assert!(!app.device_diff_can_stage(), "a read from before the stage");
        app.device_diff_rows_read(staged_at.wrapping_add(1));
        assert!(app.device_diff_can_stage(), "a read since the stage");

        drop(app.update(Message::Device(super::super::Action::Close)));
        assert!(app.device_ui.diff.is_none());
        assert!(!app.device_ui.open);

        // A device that could not be read is read again later, not never.
        {
            use super::super::super::context_sections::{Section, WorkspaceView};
            app.context_sections.workspace_view = WorkspaceView::SourceControl;
            app.context_sections.set_open(Section::Workspace, true);
            app.settle_pane_inspectors();
            if app.pane_inspectors.source_control().is_none() {
                let card = app.workspace.focused().expect("a focused card");
                drop(app.update(Message::TogglePaneInspector(card)));
                app.settle_pane_inspectors();
            }
            assert_eq!(
                app.workspace_view_shown(),
                Some(WorkspaceView::SourceControl)
            );
        }
        app.cancel_source_read();
        app.refresh_ui.source_reload = Default::default();
        let started = Instant::now();
        let request = app.refresh_ui.source_generation;
        drop(app.source_control_loaded(key.clone(), request, Err("unreachable".into())));
        let due = app
            .refresh_ui
            .source_reload
            .due()
            .expect("a failed device read is tried again");
        assert!(due >= started + Duration::from_secs(20), "after a while");
    }
}
