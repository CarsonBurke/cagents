//! The top of a chat's or terminal's inspector: what its agent is doing,
//! where it works and, for a chat, how its subagents stand.
//!
//! The block has the same rows whatever its pane reports, so a status that
//! arrives, changes or has nothing to say yet never moves the sections
//! beneath it.

use super::*;

/// Between the block's rows, and above and below them.
const ROW_GAP: f32 = space::XXS;

/// A chat's block: its status and place, then its subagents.
const CHAT_HEIGHT: f32 = design::list::TWO_LINE_HEIGHT + design::DENSE_ROW_HEIGHT + 3.0 * ROW_GAP;
/// A terminal's block: its status and place.
const TERMINAL_HEIGHT: f32 = design::list::TWO_LINE_HEIGHT + 2.0 * ROW_GAP;

/// What the block says the pane's agent is doing.
struct Status {
    glyph: Icon,
    label: String,
}

impl Cagents {
    /// `card`'s activity block, ending its first row with the inspector's
    /// `toggle`, and its height; none for a pane with no agent to describe,
    /// or while the settings leave it out.
    pub(super) fn inspector_activity<'a>(
        &'a self,
        card: CardId,
        toggle: Element<'a, Message>,
    ) -> Result<(Element<'a, Message>, f32), Element<'a, Message>> {
        if !self.settings.inspector_show_activity {
            return Err(toggle);
        }
        let Some(state) = self.card_state(card) else {
            return Err(toggle);
        };
        let (status, subagents) = match (&state.kind, self.cards.get(&card)) {
            (CardKind::Chat(_), Some(CardRuntime::Chat(runtime))) => {
                let (glyph, label) = runtime.inspector_status();
                let status = Status {
                    glyph: glyph.unwrap_or(Icon::Chat),
                    label: if label.trim().is_empty() {
                        "Ready".to_owned()
                    } else {
                        label
                    },
                };
                (
                    status,
                    Some(self.subagents_row(
                        runtime.subagent_tally(),
                        Some(runtime.open_subagent_picker()),
                    )),
                )
            }
            // The row stays while the conversation connects, so it is not
            // added under the reader once it has.
            (CardKind::Chat(_), _) => (
                Status {
                    glyph: Icon::Chat,
                    label: "Connecting…".to_owned(),
                },
                Some(self.subagents_row(Default::default(), None)),
            ),
            (CardKind::Terminal(_), _) => (self.terminal_status(card), None),
            _ => return Err(toggle),
        };
        let place = self.pane_place(state);
        let mark: Element<'a, Message> = if status.glyph == Icon::AgentWorking {
            crate::ui_controls::activity_spinner(
                f32::from(design::ROW_ICON_SIZE),
                design::accent_color,
            )
        } else {
            crate::ui_controls::passive_icon(status.glyph, design::ROW_ICON_SIZE)
        };
        let caption = crate::ui_controls::FadeText::caption(place.label)
            .class(cosmic::theme::Text::Custom(design::muted_text));
        let caption: Element<'a, Message> = if place.path.is_empty() {
            caption.into()
        } else {
            crate::interaction::tooltip(caption, place.path)
        };
        let text = widget::column::with_children(vec![
            crate::ui_controls::fade_text(status.label).into(),
            caption,
        ])
        .width(Length::Fill);
        let first = grid_row(
            mark,
            text.into(),
            Some(toggle),
            design::list::TWO_LINE_HEIGHT,
        );
        let mut rows: Vec<Element<'a, Message>> = vec![first];
        let height = if let Some(subagents) = subagents {
            rows.push(subagents);
            CHAT_HEIGHT
        } else {
            TERMINAL_HEIGHT
        };
        let block = widget::column::with_children(rows)
            .spacing(ROW_GAP)
            .padding([ROW_GAP, design::list::GUTTER])
            .width(Length::Fill)
            .height(Length::Fixed(height));
        Ok((block.into(), height))
    }

    /// The subagents row: how many there are and how many need the reader,
    /// opening the pane's list of them. Without any, it says so, so the row
    /// is there before the first one starts.
    fn subagents_row<'a>(
        &self,
        tally: chat_ui::SubagentTally,
        open: Option<Message>,
    ) -> Element<'a, Message> {
        if tally.total == 0 {
            return grid_row(
                crate::ui_controls::passive_icon(Icon::Agent, design::ROW_ICON_SIZE),
                crate::ui_controls::fade_text("No subagents")
                    .class(cosmic::theme::Text::Custom(design::muted_text))
                    .into(),
                None,
                design::DENSE_ROW_HEIGHT,
            );
        }
        let mut parts = vec![format!(
            "{} subagent{}",
            tally.total,
            if tally.total == 1 { "" } else { "s" }
        )];
        if tally.working > 0 {
            parts.push(format!("{} working", tally.working));
        }
        if tally.attention > 0 {
            parts.push(format!(
                "{} {} attention",
                tally.attention,
                if tally.attention == 1 {
                    "needs"
                } else {
                    "need"
                }
            ));
        }
        let label = parts.join(" · ");
        let content = widget::row::with_children(vec![
            widget::container(crate::ui_controls::passive_icon(
                if tally.working > 0 {
                    Icon::AgentWorking
                } else {
                    Icon::Agent
                },
                design::ROW_ICON_SIZE,
            ))
            .center_x(Length::Fixed(design::list::ICON))
            .center_y(Length::Fill)
            .into(),
            widget::container(crate::ui_controls::fade_text(label.clone()))
                .width(Length::Fill)
                .center_y(Length::Fill)
                .into(),
            widget::container(crate::ui_controls::passive_icon(
                Icon::ChevronRight,
                design::COMPACT_ICON_SIZE,
            ))
            .center_y(Length::Fill)
            .into(),
        ])
        .spacing(design::list::GAP)
        .height(Length::Fill);
        crate::interaction::hover(
            widget::button::custom(content)
                .name(format!("Show subagents: {label}"))
                .class(crate::ui_controls::control_style(false, None))
                .padding([0.0, design::list::INSET])
                .height(design::DENSE_ROW_HEIGHT)
                .width(Length::Fill)
                .on_press_maybe(open.filter(|_| !self.modal_is_open())),
        )
    }

    /// What a terminal's agent is doing, as its pane reports it, or that it
    /// is a terminal without one.
    fn terminal_status(&self, card: CardId) -> Status {
        use crate::agent::ActivityState;
        if self.sidebar_state.card_attention(card).state() == Some(sidebar::AttentionState::Limited)
        {
            return Status {
                glyph: Icon::Warning,
                label: "Stopped by a usage limit".to_owned(),
            };
        }
        match self.sidebar_state.card_activity(card) {
            Some(activity) => match activity.state {
                ActivityState::Working => Status {
                    glyph: Icon::AgentWorking,
                    label: format!("{} working", activity.provider.label()),
                },
                ActivityState::Waiting => Status {
                    glyph: Icon::AgentWaiting,
                    label: "Waiting for your input".to_owned(),
                },
                ActivityState::Idle | ActivityState::Unknown => Status {
                    glyph: Icon::Agent,
                    label: format!("{} ready", activity.provider.label()),
                },
            },
            None => Status {
                glyph: Icon::Terminal,
                label: "Terminal".to_owned(),
            },
        }
    }

    /// Where `card` works: its branch and project, as Projects lists them,
    /// else its folder, with the full path for the tooltip.
    fn pane_place(&self, card: &CardState) -> Place {
        let Some(directory) = super::worktree_ui::card_directory(card) else {
            return Place {
                label: "No folder".to_owned(),
                path: String::new(),
            };
        };
        let path = directory.display().to_string();
        let folder = || {
            directory
                .file_name()
                .map_or_else(|| path.clone(), |name| name.to_string_lossy().into_owned())
        };
        let listed = self
            .place_index
            .listed(&self.sidebar_model.projects, directory);
        let label = match listed {
            Some(Listed {
                project,
                branch: Some(branch),
            }) => format!("{branch} · {project}"),
            Some(Listed {
                project,
                branch: None,
            }) => format!("{} · {project}", folder()),
            None => folder(),
        };
        Place { label, path }
    }
}

struct Place {
    label: String,
    path: String,
}

/// The project, and the branch when it is one of its worktrees, of each
/// folder Projects lists, built again only when Projects changes: every
/// inspector draws its pane's place on every frame, so scanning each
/// project's worktrees there would cost the whole list per pane per frame.
#[derive(Debug, Default)]
pub(super) struct PlaceIndex {
    places: std::cell::RefCell<Option<Places>>,
    #[cfg(test)]
    builds: std::cell::Cell<usize>,
}

#[derive(Debug)]
struct Places {
    /// The [`sidebar::Revisioned`] revision of the projects it was built from.
    revision: u64,
    folders: HashMap<PathBuf, Listed>,
}

#[derive(Debug, Clone)]
struct Listed {
    project: String,
    branch: Option<String>,
}

impl PlaceIndex {
    /// Where `directory` is listed in `projects`: the first project, in
    /// Projects' order, that has it as a worktree or as its repository, a
    /// worktree ahead of a repository within each project.
    fn listed(
        &self,
        projects: &sidebar::Revisioned<Vec<sidebar::SidebarProject>>,
        directory: &Path,
    ) -> Option<Listed> {
        let revision = sidebar::Revisioned::revision(projects);
        let mut places = self.places.borrow_mut();
        if places
            .as_ref()
            .is_none_or(|places| places.revision != revision)
        {
            *places = Some(Places {
                revision,
                folders: folders(projects),
            });
            #[cfg(test)]
            self.builds.set(self.builds.get() + 1);
        }
        places.as_ref()?.folders.get(directory).cloned()
    }

    /// How many times it has been built.
    #[cfg(test)]
    pub(super) fn builds(&self) -> usize {
        self.builds.get()
    }
}

fn folders(projects: &[sidebar::SidebarProject]) -> HashMap<PathBuf, Listed> {
    let mut folders = HashMap::new();
    for project in projects {
        let rows = project
            .worktrees
            .iter()
            .map(|worktree| (&worktree.path, worktree.branch.clone()))
            .chain([(&project.repository, None)]);
        for (path, branch) in rows {
            folders.entry(path.clone()).or_insert_with(|| Listed {
                project: project.name.clone(),
                branch,
            });
        }
    }
    folders
}

/// A row on the list grid: `mark` in the icon column, `text` on the label
/// column, then `trailing`.
fn grid_row<'a>(
    mark: Element<'a, Message>,
    text: Element<'a, Message>,
    trailing: Option<Element<'a, Message>>,
    height: f32,
) -> Element<'a, Message> {
    // A trailing control sits where a section header puts the inspector
    // toggle, at the gutter, so the toggle stays put whichever leads.
    let right = if trailing.is_some() {
        0.0
    } else {
        design::list::INSET
    };
    let mut children = vec![
        widget::container(mark)
            .center_x(Length::Fixed(design::list::ICON))
            .center_y(Length::Fill)
            .into(),
        widget::container(text)
            .width(Length::Fill)
            .center_y(Length::Fill)
            .into(),
    ];
    if let Some(trailing) = trailing {
        children.push(
            widget::container(trailing)
                .align_y(Alignment::Start)
                .height(Length::Fill)
                .into(),
        );
    }
    widget::container(
        widget::row::with_children(children)
            .spacing(design::list::GAP)
            .height(Length::Fill),
    )
    .padding(Padding::ZERO.left(design::list::INSET).right(right))
    .height(Length::Fixed(height))
    .width(Length::Fill)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The place `app`'s focused pane, a terminal, shows after it moves to
    /// `directory`.
    fn place_in(app: &mut Cagents, directory: &str) -> String {
        let terminal = app.workspace.focused().unwrap();
        let CardKind::Terminal(session) = &mut app.card_state_mut(terminal).unwrap().kind else {
            unreachable!("the test app's pane is a terminal")
        };
        session.launch.working_directory = directory.into();
        app.pane_place(app.card_state(terminal).unwrap()).label
    }

    #[test]
    fn a_panes_place_is_looked_up_in_an_index_built_again_only_when_projects_change() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        app.sidebar_model = crate::sidebar::tests::panel_model();

        assert_eq!(
            place_in(&mut app, "/repo/fix-sidebar"),
            "fix-sidebar · cagents"
        );
        // Every frame draws it again; none of them scans Projects.
        for _ in 0..3 {
            assert_eq!(
                place_in(&mut app, "/repo/fix-sidebar"),
                "fix-sidebar · cagents"
            );
        }
        // The repository itself, and a folder Projects doesn't list.
        assert_eq!(place_in(&mut app, "/repo/cagents"), "cagents · cagents");
        assert_eq!(place_in(&mut app, "/elsewhere/scratch"), "scratch");
        assert_eq!(app.place_index.builds(), 1);

        // A change to Projects is seen at the next draw.
        app.sidebar_model.projects[0].name = "renamed".into();
        assert_eq!(
            place_in(&mut app, "/repo/fix-sidebar"),
            "fix-sidebar · renamed"
        );
        assert_eq!(place_in(&mut app, "/repo/main"), "main · renamed");
        assert_eq!(app.place_index.builds(), 2);
    }

    #[test]
    fn a_folder_listed_twice_is_placed_as_the_scan_placed_it() {
        let project = |name: &str, repository: &str, worktrees: &[(&str, &str)]| {
            let mut project = crate::sidebar::tests::panel_model().projects[0].clone();
            project.name = name.into();
            project.repository = repository.into();
            let template = project.worktrees[0].clone();
            project.worktrees = worktrees
                .iter()
                .map(|(path, branch)| {
                    let mut worktree = template.clone();
                    worktree.path = (*path).into();
                    worktree.branch = Some((*branch).into());
                    worktree
                })
                .collect();
            project
        };
        let folders = folders(&[
            project("first", "/shared", &[("/first/wt", "one")]),
            project(
                "second",
                "/second",
                &[("/shared", "two"), ("/first/wt", "three")],
            ),
            project("third", "/third", &[("/third", "main")]),
        ]);
        let at = |path: &str| {
            let listed = &folders[Path::new(path)];
            (listed.project.as_str(), listed.branch.as_deref())
        };
        // The first project to list a folder keeps it.
        assert_eq!(at("/shared"), ("first", None));
        assert_eq!(at("/first/wt"), ("first", Some("one")));
        // Within a project, its worktree comes ahead of its repository.
        assert_eq!(at("/third"), ("third", Some("main")));
    }
}
