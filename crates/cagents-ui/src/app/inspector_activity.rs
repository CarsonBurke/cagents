//! The top of a chat's or terminal's inspector: what its agent is doing and
//! where it works. A chat's subagents are not summed up here: the strip of
//! agent tabs above its composer lists each one and opens its chooser.
//!
//! The block has the same rows whatever its pane reports, so a status that
//! arrives, changes or has nothing to say yet never moves the sections
//! beneath it.

use super::*;

/// Above and below the block's row.
const ROW_GAP: f32 = space::XXS;

/// The block: its pane's status and place.
const HEIGHT: f32 = design::list::TWO_LINE_HEIGHT + 2.0 * ROW_GAP;

/// What the block, and the rail an inspector collapses to, say the pane's
/// agent is doing.
pub(super) struct Status {
    pub(super) glyph: Icon,
    pub(super) label: String,
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
        let Some(status) = self.activity_status(card, state) else {
            return Err(toggle);
        };
        let place = self.pane_place(state);
        let mark = status_mark(status.glyph);
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
        // The mark sits in the list's icon column and the text on its label
        // column. The toggle ends the row at the gutter, where a section
        // header puts it, so it stays put whichever section leads.
        let row = widget::row::with_children(vec![
            widget::container(mark)
                .center_x(Length::Fixed(design::list::ICON))
                .center_y(Length::Fill)
                .into(),
            widget::container(text)
                .width(Length::Fill)
                .center_y(Length::Fill)
                .into(),
            widget::container(toggle)
                .align_y(Alignment::Start)
                .height(Length::Fill)
                .into(),
        ])
        .spacing(design::list::GAP)
        .height(Length::Fill);
        let block = widget::container(row)
            .padding(
                Padding::from([ROW_GAP, design::list::GUTTER])
                    .left(design::list::GUTTER + design::list::INSET),
            )
            .width(Length::Fill)
            .height(Length::Fixed(HEIGHT));
        Ok((block.into(), HEIGHT))
    }

    /// What `card`'s agent is doing, as its chat or terminal reports it;
    /// none for a pane with no agent to describe. It reads only what the
    /// pane already holds, so drawing it on every frame costs nothing more.
    pub(super) fn activity_status(&self, card: CardId, state: &CardState) -> Option<Status> {
        match (&state.kind, self.cards.get(&card)) {
            (CardKind::Chat(_), Some(CardRuntime::Chat(runtime))) => {
                let (glyph, label) = runtime.inspector_status();
                Some(Status {
                    glyph: glyph.unwrap_or(Icon::Chat),
                    label: if label.trim().is_empty() {
                        "Ready".to_owned()
                    } else {
                        label
                    },
                })
            }
            (CardKind::Chat(_), _) => Some(Status {
                glyph: Icon::Chat,
                label: "Connecting…".to_owned(),
            }),
            (CardKind::Terminal(_), _) => Some(self.terminal_status(card)),
            _ => None,
        }
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

/// A status's glyph: a spinner while the agent works.
pub(super) fn status_mark<'a>(glyph: Icon) -> Element<'a, Message> {
    if glyph == Icon::AgentWorking {
        crate::ui_controls::activity_spinner(f32::from(design::ROW_ICON_SIZE), design::accent_color)
    } else {
        crate::ui_controls::passive_icon(glyph, design::ROW_ICON_SIZE)
    }
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
