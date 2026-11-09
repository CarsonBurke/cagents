//! The plan `scripts/fixture-workspace` writes, and the records it stands for.
//!
//! The generator describes a workspace at the level of a fixture: which
//! projects and worktrees exist, which cards were opened and closed, which
//! channels and shared views hold them. The daemon and each window load the
//! same plan and build their records here, so a pane in the daemon's shared
//! view and the card in a window's session are the same value. Nothing in the
//! stores is written by hand: the daemon applies `ModelEdit`s through its
//! registry and a window saves through its `StateStore`.
//!
//! Development builds only. A production executable recognises the seeding
//! flags and refuses them; see [`refusal`].
//!
//! See docs/fixture-workspace.md.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::Deserialize;

use crate::model::{
    BrowserId, BrowserRestoreFidelity, BrowserSession, CardId, CardKind, CardState, ChannelId,
    ChannelTitleSource, Existence, LaunchRecipe, ProjectId, PullRequestView, RepositoryView,
    TerminalContinuity, TerminalId, TerminalSession, Tombstone, WorkspaceId, WorktreeId,
};
use crate::workspace_model::views::Scope;

#[cfg(any(debug_assertions, feature = "development"))]
pub mod destination;

/// The plan format these appliers understand. The generator writes its own
/// (`PLAN_FORMAT` in scripts/fixture_workspace/generate.py); the two change
/// together.
pub const PLAN_FORMAT: u32 = 1;

/// The flag both executables take.
pub const SEED_FLAG: &str = "--development-seed-workspace";

/// Why a production executable will not seed: its stores are a person's real
/// workspace.
pub fn refusal() -> String {
    format!(
        "{SEED_FLAG} only runs in a development build: this executable's stores are a real \
         installation's. Build with scripts/run-development, or use a native-evidence side's \
         target-SIDE/dev-runtime executables."
    )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub format: u32,
    pub profile: String,
    pub seed: u64,
    pub epoch: i64,
    pub root: PathBuf,
    pub home: PathBuf,
    pub edge_cases: serde_json::Value,
    pub operation_namespace: uuid::Uuid,
    pub environments: BTreeMap<String, BTreeMap<String, String>>,
    pub projects: Vec<Project>,
    pub cards: Vec<Card>,
    pub channels: Vec<Channel>,
    pub views: Vec<View>,
    pub windows: Vec<Window>,
    pub history: serde_json::Value,
    /// Where the plan was read from, resolved.
    #[serde(skip)]
    pub path: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    pub checkout: PathBuf,
    pub repository: PathBuf,
    pub worktrees: Vec<Worktree>,
    /// Worktrees deleted long ago that still have a shared view.
    pub historical: Vec<PathBuf>,
    pub avatar: Option<Avatar>,
    pub remote: Option<String>,
    pub dirty: Vec<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Worktree {
    pub id: WorktreeId,
    pub path: PathBuf,
    pub branch: String,
    /// Deleted from disk and forgotten by Git.
    pub removed: bool,
    pub main: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Avatar {
    pub owner: String,
    pub color: [u8; 3],
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Card {
    pub id: CardId,
    pub title: String,
    pub width: f32,
    pub closed_at_ms: Option<i64>,
    /// The worktree the card belongs to, when it belongs to one.
    pub scope: Option<PathBuf>,
    pub project: Option<ProjectId>,
    pub kind: Kind,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Kind {
    Chat {
        provider: String,
        terminal_id: TerminalId,
        session_id: Option<String>,
        cwd: PathBuf,
        program: String,
        arguments: Vec<String>,
        environment: String,
    },
    Terminal {
        terminal_id: TerminalId,
        cwd: PathBuf,
        program: String,
        arguments: Vec<String>,
        environment: String,
    },
    Repository {
        project: ProjectId,
        repository: PathBuf,
        worktree: PathBuf,
        selected_file: Option<PathBuf>,
    },
    PullRequests {
        repository: PathBuf,
        selected_number: Option<u64>,
    },
    Browser {
        browser_id: BrowserId,
        url: String,
    },
    Settings,
    Accounts,
    Automations,
    Work,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Channel {
    pub id: ChannelId,
    pub project: ProjectId,
    pub title: String,
    pub title_source: ChannelTitleSource,
    pub cards: Vec<CardId>,
    pub focused: Option<CardId>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct View {
    pub scope: PlanScope,
    pub panes: Vec<CardId>,
    pub retired: Vec<CardId>,
    /// How many operations the view recorded (`shared_view_edits` rows).
    pub edits: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PlanScope {
    Channel(ChannelId),
    Worktree { repository: PathBuf, path: PathBuf },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    pub slot: String,
    pub workspace_id: WorkspaceId,
    pub cards: Vec<CardId>,
    pub active_channel: Option<ChannelId>,
    pub active_worktree: Option<PathBuf>,
    pub focused: Option<CardId>,
    pub card_scopes: Vec<CardId>,
    pub worktree_focus: BTreeMap<PathBuf, CardId>,
    pub unread: Vec<Unread>,
    pub drafts: BTreeMap<CardId, String>,
    pub pending_chat_admissions: Vec<CardId>,
    pub user_titles: BTreeMap<CardId, String>,
    pub collapsed_projects: Vec<ProjectId>,
    pub worktree_offsets: BTreeMap<PathBuf, f32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Unread {
    pub card: CardId,
    pub owner: Option<(ProjectId, WorktreeId)>,
}

impl PlanScope {
    pub fn scope(&self) -> Scope {
        match self {
            Self::Channel(id) => Scope::Channel(*id),
            Self::Worktree { repository, path } => Scope::Worktree {
                repository: repository.clone(),
                path: path.clone(),
            },
        }
    }
}

/// What every seeded recipe points inside the root, as HOME is.
const PROVIDER_HOMES: [&str; 3] = ["HOME", "CLAUDE_CONFIG_DIR", "CODEX_HOME"];

/// The credentials a provider would prefer over its (fixture) login. A seeded
/// launch removes them, so it never runs on the person's keys.
fn unset_credentials() -> std::collections::BTreeSet<String> {
    [
        crate::agent::Provider::Claude,
        crate::agent::Provider::Codex,
    ]
    .into_iter()
    .flat_map(crate::account::credential_overrides)
    .map(|variable| (*variable).to_owned())
    .collect()
}

impl Plan {
    /// Reads and checks a plan. Every failure names what to do about it.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("Cannot read the plan {}: {error}", path.display()))?;
        let header: serde_json::Value = serde_json::from_str(&text)
            .map_err(|error| format!("{} is not a plan: {error}", path.display()))?;
        let format = header["format"].as_u64().unwrap_or(0);
        if format != u64::from(PLAN_FORMAT) {
            return Err(format!(
                "{} is plan format {format}, and this executable reads format {PLAN_FORMAT}. \
                 Generate it again with the scripts/fixture-workspace of the checkout these \
                 executables were built from.",
                path.display()
            ));
        }
        let mut plan: Self = serde_json::from_str(&text).map_err(|error| {
            format!(
                "{} does not match plan format {PLAN_FORMAT}: {error}. Generate it again with \
                 this checkout's scripts/fixture-workspace.",
                path.display()
            )
        })?;
        plan.path = path
            .canonicalize()
            .map_err(|error| format!("Cannot resolve {}: {error}", path.display()))?;
        // Every path in the plan names something under the root it was
        // generated in. A copied or moved plan would name the original's.
        let here = plan.path.parent().map(Path::to_owned);
        if plan.root.canonicalize().ok() != here {
            return Err(format!(
                "{} was generated for {}, and its paths name the repositories and history there. \
                 Generate it again where it is to be applied.",
                path.display(),
                plan.root.display()
            ));
        }
        Ok(plan)
    }

    pub fn card(&self, id: CardId) -> Result<&Card, String> {
        self.cards
            .iter()
            .find(|card| card.id == id)
            .ok_or_else(|| format!("The plan names card {id} but does not describe it"))
    }

    pub fn project(&self, id: ProjectId) -> Result<&Project, String> {
        self.projects
            .iter()
            .find(|project| project.id == id)
            .ok_or_else(|| format!("The plan names project {id} but does not describe it"))
    }

    fn recipe(
        &self,
        program: &str,
        arguments: &[String],
        cwd: &Path,
        environment: &str,
    ) -> Result<LaunchRecipe, String> {
        let name = environment;
        let environment = self
            .environments
            .get(name)
            .cloned()
            .ok_or_else(|| format!("The plan names environment {name} but lacks it"))?;
        // Each layer the app adds sits under the recipe: the daemon's and the
        // window's own environments, which may name the person's provider
        // homes or credentials. A seeded launch must only reach the fixture's.
        for variable in PROVIDER_HOMES {
            if !environment
                .get(variable)
                .is_some_and(|path| Path::new(path).starts_with(&self.root))
            {
                return Err(format!(
                    "The plan's environment {name} does not set {variable} inside {}. Generate it \
                     again with this checkout's scripts/fixture-workspace.",
                    self.root.display()
                ));
            }
        }
        Ok(LaunchRecipe {
            program: program.to_owned(),
            arguments: arguments.to_vec(),
            working_directory: cwd.to_owned(),
            environment,
            unset_environment: unset_credentials(),
        })
    }

    /// The card as a window saves it.
    pub fn card_state(&self, card: &Card) -> Result<CardState, String> {
        let kind = match &card.kind {
            Kind::Chat {
                provider,
                terminal_id,
                session_id,
                cwd,
                program,
                arguments,
                environment,
            } => CardKind::Chat(crate::chat::Session {
                id: *terminal_id,
                launch: crate::chat::Launch {
                    provider: match provider.as_str() {
                        "claude" => crate::agent::Provider::Claude,
                        "codex" => crate::agent::Provider::Codex,
                        other => return Err(format!("Unknown chat provider {other} in the plan")),
                    },
                    recipe: self.recipe(program, arguments, cwd, environment)?,
                    session_id: session_id.clone(),
                },
                proof: None,
                terminal: None,
                owned_codex_listener: None,
                native_continuity: Default::default(),
            }),
            Kind::Terminal {
                terminal_id,
                cwd,
                program,
                arguments,
                environment,
            } => {
                let launch = self.recipe(program, arguments, cwd, environment)?;
                CardKind::Terminal(TerminalSession {
                    id: *terminal_id,
                    owned_codex_listener: None,
                    recovery_origin: None,
                    predecessor: None,
                    // Where the app keeps a terminal's scrollback.
                    transcript_path: crate::instance::project_dirs().map(|dirs| {
                        dirs.data_dir()
                            .join("transcripts")
                            .join(format!("{terminal_id}.log"))
                    }),
                    resume_hint: None,
                    continuity: TerminalContinuity::RelaunchAvailable {
                        recipe: launch.clone(),
                        last_exit: None,
                    },
                    launch,
                })
            }
            Kind::Repository {
                project,
                repository,
                worktree,
                selected_file,
            } => CardKind::Repository(RepositoryView {
                project: *project,
                repository: repository.clone(),
                worktree: worktree.clone(),
                selected_file: selected_file.clone(),
            }),
            Kind::PullRequests {
                repository,
                selected_number,
            } => CardKind::PullRequests(PullRequestView {
                repository: repository.clone(),
                selected_number: *selected_number,
                file_panel_width: None,
                file_panel_collapsed: None,
            }),
            Kind::Browser { browser_id, url } => {
                let url: url::Url = url
                    .parse()
                    .map_err(|error| format!("The plan's browser URL {url} is invalid: {error}"))?;
                CardKind::Browser(BrowserSession {
                    id: *browser_id,
                    history: vec![url.clone()],
                    current_url: url,
                    history_index: 0,
                    scroll_x: 0.0,
                    scroll_y: 0.0,
                    fidelity: BrowserRestoreFidelity::UrlOnly,
                })
            }
            Kind::Settings => CardKind::Settings,
            Kind::Accounts => CardKind::Accounts,
            Kind::Automations => CardKind::Automations,
            Kind::Work => CardKind::Work,
        };
        Ok(CardState {
            id: card.id,
            generation: 0,
            title: card.title.clone(),
            width_fraction: card.width,
            existence: match card.closed_at_ms {
                None => Existence::Open,
                Some(closed_at_unix_ms) => Existence::Closed {
                    tombstone: Tombstone {
                        operation_id: None,
                        closed_at_unix_ms,
                        reason: Some("Closed".into()),
                    },
                },
            },
            kind,
        })
    }
}

/// Generates a workspace with this checkout's scripts/fixture-workspace, for
/// tests that load it through the stores' own readers.
#[cfg(test)]
pub(crate) mod test_support {
    use super::Plan;

    /// The small profile with every edge case, at `directory/workspace`.
    pub(crate) fn small(directory: &std::path::Path) -> Plan {
        let root = directory.join("workspace");
        let generator =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/fixture-workspace");
        let output = std::process::Command::new(&generator)
            .args([
                "generate",
                "--profile",
                "small",
                "--edge-case",
                "all",
                "--root",
            ])
            .arg(&root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap_or_else(|error| panic!("cannot run {}: {error}", generator.display()));
        assert!(
            output.status.success(),
            "the generator failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Plan::load(&root.join("plan.json")).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn another_format_names_the_fix() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("plan.json");
        std::fs::write(&path, r#"{"format": 99}"#).unwrap();
        let error = Plan::load(&path).unwrap_err();
        assert!(error.contains("format 99"), "{error}");
        assert!(error.contains("scripts/fixture-workspace"), "{error}");
    }

    /// The app layers a recipe over its own inherited environment, which may
    /// name the person's provider homes and keys.
    #[test]
    fn seeded_launches_reach_only_the_fixture() {
        let directory = tempfile::tempdir().unwrap();
        let plan = test_support::small(directory.path());
        let mut launches = 0;
        for card in &plan.cards {
            let state = plan.card_state(card).unwrap();
            let recipe = match &state.kind {
                CardKind::Chat(session) => session.launch.recipe.clone(),
                CardKind::Terminal(TerminalSession {
                    continuity: TerminalContinuity::RelaunchAvailable { recipe, .. },
                    ..
                }) => recipe.clone(),
                _ => continue,
            };
            for variable in PROVIDER_HOMES {
                let path = Path::new(&recipe.environment[variable]);
                assert!(
                    path.starts_with(&plan.root),
                    "{variable}={}",
                    path.display()
                );
            }
            assert_eq!(
                recipe.environment["CLAUDE_CONFIG_DIR"],
                format!("{}/.claude", plan.home.display())
            );
            assert_eq!(
                recipe.environment["CODEX_HOME"],
                format!("{}/.codex", plan.home.display())
            );
            for key in [
                "ANTHROPIC_API_KEY",
                "CLAUDE_CODE_OAUTH_TOKEN",
                "OPENAI_API_KEY",
                "CODEX_API_KEY",
            ] {
                assert!(recipe.unset_environment.contains(key), "{key}");
            }
            launches += 1;
        }
        assert!(launches > 10, "{launches}");
    }

    #[test]
    fn a_moved_plan_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let plan = test_support::small(directory.path());
        let moved = directory.path().join("moved");
        std::fs::create_dir(&moved).unwrap();
        std::fs::copy(&plan.path, moved.join("plan.json")).unwrap();
        let error = Plan::load(&moved.join("plan.json")).unwrap_err();
        assert!(
            error.contains("Generate it again where it is to be applied"),
            "{error}"
        );
    }

    #[test]
    fn every_planned_card_builds_its_saved_state() {
        let directory = tempfile::tempdir().unwrap();
        let plan = test_support::small(directory.path());
        for card in &plan.cards {
            let state = plan.card_state(card).unwrap();
            assert_eq!(state.existence.is_open(), card.closed_at_ms.is_none());
        }
    }

    /// The providers' history readers live in the daemon.
    #[cfg(feature = "host")]
    mod history {
        use super::*;
        use crate::chat::{Command, Event, ItemContent, Role};

        /// Opens a conversation the way a restored chat card does, and pages
        /// through all of it.
        fn claude_history(plan: &Plan, session_id: &str, cwd: &str) -> Vec<crate::chat::Item> {
            let launch = crate::chat::Launch {
                provider: crate::agent::Provider::Claude,
                session_id: Some(session_id.to_owned()),
                recipe: LaunchRecipe {
                    program: "claude".into(),
                    arguments: Vec::new(),
                    working_directory: cwd.into(),
                    environment: BTreeMap::from([(
                        "HOME".to_owned(),
                        plan.home.to_string_lossy().into_owned(),
                    )]),
                    unset_environment: Default::default(),
                },
            };
            let (_, mut adapter) = crate::chat::protocol::create_history(&launch)
                .unwrap_or_else(|error| panic!("{session_id}: {error}"));
            let mut pages = Vec::new();
            let mut step = adapter.initialize().unwrap();
            loop {
                let mut earlier = false;
                for event in step.events {
                    if let Event::History { items, has_earlier } = event {
                        pages.push(items);
                        earlier = has_earlier;
                    }
                }
                if !earlier {
                    break;
                }
                step = adapter.command(Command::LoadEarlier).unwrap();
            }
            pages.into_iter().rev().flatten().collect()
        }

        /// Harness records the reader shows as the user's own text, although
        /// Claude Code wrote them: a local command's empty output, and the
        /// /clear command that clearing a conversation records, which the
        /// reader shows as the command typed. The generator writes them as
        /// noise, so they are left out of the count.
        fn shown_as_text(text: &str) -> bool {
            text.starts_with("<local-command-stdout>") || text == "/clear"
        }

        fn messages(items: &[crate::chat::Item]) -> usize {
            items
                .iter()
                .filter(|item| match (&item.role, &item.content) {
                    (
                        Role::User | Role::Assistant,
                        ItemContent::Text {
                            text,
                            thinking: false,
                            ..
                        },
                    ) => !shown_as_text(text),
                    _ => false,
                })
                .count()
        }

        /// Every intact transcript the generator wrote reads back through the
        /// resume reader with exactly the messages it meant to be visible: the
        /// harness noise it adds (reminders, command output, compaction
        /// summaries, task notifications) stays hidden, as it does for real
        /// Claude Code transcripts.
        #[test]
        fn claude_transcripts_read_back_through_the_resume_reader() {
            let directory = tempfile::tempdir().unwrap();
            let plan = test_support::small(directory.path());
            let mut read = 0;
            let mut mismatched = Vec::new();
            for entry in plan.history["claude"].as_array().unwrap() {
                // Damaged files are what the resume reader's own tolerance
                // tests cover; whether they read is that reader's call. A fork
                // shows its origin's messages and then its own.
                if !matches!(
                    entry["edge"].as_str(),
                    None | Some("sidechain" | "forked" | "large-record")
                ) {
                    continue;
                }
                let session_id = entry["session_id"].as_str().unwrap();
                let items = claude_history(&plan, session_id, entry["cwd"].as_str().unwrap());
                let expected = entry["visible"].as_u64().unwrap() as usize;
                if messages(&items) != expected {
                    let shown: Vec<String> = items
                        .iter()
                        .filter_map(|item| match &item.content {
                            ItemContent::Text {
                                text,
                                thinking: false,
                                ..
                            } => Some(format!("{:?}: {:.60}", item.role, text)),
                            _ => None,
                        })
                        .collect();
                    mismatched.push(format!(
                        "{session_id}: expected {expected}, shown {shown:#?}"
                    ));
                }
                read += 1;
            }
            assert!(mismatched.is_empty(), "{}", mismatched.join("\n"));
            assert!(read >= 15, "only {read} transcripts were intact");
        }

        /// Each rollout shows one prompt per turn, and an answer for every turn
        /// that ended normally.
        #[test]
        fn codex_rollouts_read_back_through_the_rollout_reader() {
            let directory = tempfile::tempdir().unwrap();
            let plan = test_support::small(directory.path());
            let edge = &plan.edge_cases;
            let cut_short = [
                edge["codex-interrupted-turn"]["thread_id"].as_str(),
                edge["codex-usage-limit"]["thread_id"].as_str(),
            ];
            for entry in plan.history["codex"].as_array().unwrap() {
                let thread = entry["thread_id"].as_str().unwrap();
                let items = crate::chat::protocol::codex::rollout::read(std::path::Path::new(
                    entry["file"].as_str().unwrap(),
                ))
                .unwrap();
                let count = |role: Role| {
                    items
                        .iter()
                        .filter(|item| {
                            item.role == role
                                && matches!(
                                    &item.content,
                                    ItemContent::Text {
                                        thinking: false,
                                        ..
                                    }
                                )
                        })
                        .count()
                };
                let turns = entry["turns"].as_u64().unwrap() as usize;
                assert_eq!(count(Role::User), turns, "{thread}");
                let answered = turns - usize::from(cut_short.contains(&Some(thread)));
                assert_eq!(count(Role::Assistant), answered, "{thread}");
            }
        }
    }
}
