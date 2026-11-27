//! A change's diff in a device window, read by the device's Git and drawn in
//! the device dialog. A device has no repository pane to open it in (#688),
//! so the dialog stands in for one: the comparison, the way through the
//! rest of its Changes group, and the stage or unstage that finishes with it.
use super::*;
use crate::repository::{
    ChangeArea, DiffBasis, DiffContent, DiffRequest, RepositoryReply, RepositoryRequest,
    UnstageRequest,
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
    content: Content,
    view: patch_view::ViewState,
    /// The source-control read current when this dialog last staged or
    /// unstaged a change. Until a later read lands, the rows it steps through
    /// still list that change where it was, so it doesn't stage again.
    settling: Option<u64>,
}

enum Content {
    Loading,
    Ready(Prepared),
    Failed(String),
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
        result: Result<Prepared, String>,
    },
    View(patch_view::Event),
    /// To the previous or next change in its group.
    Step {
        forward: bool,
    },
    /// Stages the change, or unstages a staged one, then moves on.
    Stage,
    /// Opens the file in Device files.
    OpenFile,
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
    content: DiffContent,
    path: &Path,
    theme: crate::settings::EditorSyntaxTheme,
) -> Prepared {
    if let DiffContent::Comparison {
        old,
        new,
        old_label,
        new_label,
    } = &content
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
    let (title, detail) = crate::repository_widget::no_text_summary(&content);
    Prepared::Note { title, detail }
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
        let read = NEXT_READ.fetch_add(1, Ordering::Relaxed);
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
        self.device_ui.diff = Some(Diff {
            key,
            request: request.clone(),
            area,
            read,
            content: Content::Loading,
            view,
            settling,
        });
        let theme = self.settings.editor_syntax_theme;
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let path = request.path.as_path().to_owned();
                    let content = crate::device_service::answer_repository(
                        RepositoryRequest::Diff(request),
                        &CancellationToken::default(),
                    )
                    .and_then(RepositoryReply::into_diff)?;
                    Ok(prepare(content, &path, theme))
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

    pub(super) fn update_device_diff(&mut self, action: Action) -> Task<Message> {
        if self.device_ui.diff.is_none() {
            return Task::none();
        }
        match action {
            Action::Loaded { read, result } => {
                if let Some(diff) = self.device_ui.diff.as_mut()
                    && read == diff.read
                {
                    diff.content = match result {
                        Ok(prepared) => Content::Ready(prepared),
                        Err(error) => Content::Failed(error),
                    };
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
                let Some((key, area, rows, index)) = self.device_diff_position() else {
                    return Task::none();
                };
                let next = if forward {
                    index.map_or(Some(0), |index| index.checked_add(1))
                } else {
                    index.and_then(|index| index.checked_sub(1))
                };
                let Some(row) = next.and_then(|next| rows.get(next)) else {
                    return Task::none();
                };
                let request = working_request(key, row, area);
                let key = key.clone();
                self.open_device_diff(key, request, Some(area))
            }
            Action::Stage => {
                if !self.device_diff_can_stage() {
                    return Task::none();
                }
                let Some((key, area, rows, Some(index))) = self.device_diff_position() else {
                    return Task::none();
                };
                let key = key.clone();
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
                let Some(diff) = self.device_ui.diff.take() else {
                    return Task::none();
                };
                let path = diff.key.canonical_path.join(diff.request.path.as_path());
                self.update_device(super::Action::OpenFile {
                    path,
                    fragment: None,
                })
            }
        }
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
            && self
                .device_ui
                .diff
                .as_ref()
                .is_some_and(|diff| diff.settling.is_none())
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
        context.push(crate::repository_widget::basis_label(&diff.request.basis));
        if let Content::Ready(Prepared::Patch { model, .. }) = &diff.content
            && let Some((added, removed)) = model.counts()
        {
            context.push(format!("+{added} −{removed}"));
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
        if let Some((_, _, rows, index)) = &position
            && rows.len() > usize::from(index.is_some())
        {
            let index = *index;
            let previous = index.is_some_and(|index| index > 0);
            let next = index.map_or(!rows.is_empty(), |index| index + 1 < rows.len());
            header.push(
                crate::ui_controls::icon_button(Icon::ChevronUp, "Previous change")
                    .on_press_maybe(previous.then(|| action(Action::Step { forward: false })))
                    .into(),
            );
            header.push(
                crate::ui_controls::icon_button(Icon::ChevronDown, "Next change")
                    .on_press_maybe(next.then(|| action(Action::Step { forward: true })))
                    .into(),
            );
        }
        let width = size.width.min(MAX_WIDTH);
        // The patch fits beside the region's scrollbar rail, so only a line
        // longer than that scrolls sideways.
        let body_width = (width - 2.0 * space::L - design::SCROLLBAR_GUTTER).max(1.0);
        let body: Element<'_, Message> = match &diff.content {
            Content::Loading => design::body("Reading the change from the device…").into(),
            Content::Failed(error) => widget::column::with_children(vec![
                design::heading("Couldn't read this change").into(),
                design::body(error.as_str())
                    .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                    .into(),
            ])
            .spacing(space::XS)
            .into(),
            Content::Ready(Prepared::Same) => design::body("Both sides hold the same text.").into(),
            Content::Ready(Prepared::Note { title, detail }) => {
                widget::column::with_children(vec![
                    design::heading(title.as_str()).into(),
                    design::body(detail.as_str())
                        .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                        .into(),
                ])
                .spacing(space::XS)
                .into()
            }
            Content::Ready(Prepared::Patch { model, text }) => model
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
        };
        let mut actions: Vec<Element<'_, Message>> = vec![
            crate::ui_controls::button::secondary("Open file")
                .icon(Icon::OpenFolder)
                .name("Open this file in Device files")
                .on_press(action(Action::OpenFile))
                .into(),
            widget::Space::new().width(Length::Fill).into(),
            crate::ui_controls::button::secondary("Close")
                .on_press(Message::Device(super::Action::Close))
                .into(),
        ];
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
            actions.push(
                crate::ui_controls::button::primary(label)
                    .on_press_maybe(can_stage.then(|| action(Action::Stage)))
                    .into(),
            );
        }
        // The name, the way through the group and the actions stay in
        // place; only the change scrolls.
        let height = (size.height - CHROME_HEIGHT).max(120.0);
        Some(dialog_ui::surface(
            widget::column::with_children(vec![
                widget::row::with_children(header)
                    .spacing(space::XS)
                    .align_y(Alignment::Start)
                    .into(),
                // One height whatever the change, so stepping through a
                // group leaves Previous, Next and Stage where they were.
                widget::container(dialog_ui::scrolled_region(body, height))
                    .height(height)
                    .into(),
                widget::row::with_children(actions)
                    .spacing(space::S)
                    .align_y(Alignment::Center)
                    .into(),
            ])
            .spacing(space::M),
            width,
        ))
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
    use crate::repository::DiffSide;

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
            comparison(text("one\ntwo\n"), text("one\n2\n")),
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
                comparison(text("same\n"), text("same\n")),
                Path::new("same.txt"),
                crate::settings::EditorSyntaxTheme::default(),
            ),
            Prepared::Same
        ));
        let Prepared::Note { title, detail } = prepare(
            comparison(text("small\n"), DiffSide::Oversize { bytes: 3_000_000 }),
            Path::new("big.log"),
            crate::settings::EditorSyntaxTheme::default(),
        ) else {
            panic!("expected a note");
        };
        assert_eq!(title, "No text diff between Index and Working tree");
        assert_eq!(detail, "Working tree: 3000000 bytes, too large to compare");
        let Prepared::Note { title, detail } = prepare(
            comparison(text("last\n"), text("last")),
            Path::new("unended.txt"),
            crate::settings::EditorSyntaxTheme::default(),
        ) else {
            panic!("a dropped final newline is a change");
        };
        assert_eq!(title, "Only the final newline changed");
        assert_eq!(detail, "Working tree: no newline at the end of the file");
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
                result: Ok(Prepared::Same),
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
                result: Ok(Prepared::Same),
            },
        ))));
        assert!(matches!(
            app.device_ui.diff.as_ref().unwrap().content,
            Content::Ready(Prepared::Same)
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
