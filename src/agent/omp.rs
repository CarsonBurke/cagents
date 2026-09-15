use super::{ActivityState, Provider, ProviderAdapter};
use crate::model::LaunchRecipe;
use std::path::{Path, PathBuf};

pub(super) const ADAPTER: ProviderAdapter = ProviderAdapter {
    provider: Provider::Omp,
    executable: "omp",
    label: "Oh My Pi",
    recognizes_script: |path| {
        matches!(
            path.file_name().and_then(|name| name.to_str()),
            Some("omp" | "omp.js")
        )
    },
    session_file_id: |stem| Some(stem.rsplit_once('_')?.1),
    session_root,
    #[cfg(feature = "host")]
    session_directory: "sessions",
    activity_from_title,
    identity_options: &["--resume", "-r"],
    cwd_options: &["--cwd"],
    value_options: &[
        "--profile",
        "--session-dir",
        "--model",
        "--provider",
        "--thinking",
        "--config",
        "--add-dir",
        "--approval-mode",
        "--smol",
        "--slow",
        "--plan",
        "--models",
        "--service-tier",
        "--tools",
        "--skills",
        "--extension",
        "-e",
        "--hook",
        "--plugin-dir",
        "--append-system-prompt",
        "--system-prompt",
    ],
    switches: &[
        "--no-lsp",
        "--no-tools",
        "--no-extensions",
        "--no-skills",
        "--no-rules",
        "--auto-approve",
        "--no-pty",
        "--no-title",
        "--hide-thinking",
        "--advisor",
    ],
    resume_flag: "--resume",
    resume_precedes_options: false,
    #[cfg(feature = "host")]
    forwards_to_native_child: false,
    #[cfg(feature = "host")]
    create_chat: crate::chat::protocol::omp::create,
};

fn session_root(launch: &LaunchRecipe, home: &Path) -> Option<PathBuf> {
    storage_root(launch, home, "XDG_DATA_HOME")
}

#[cfg(feature = "host")]
pub(super) fn state_root(launch: &LaunchRecipe, home: &Path) -> Option<PathBuf> {
    storage_root(launch, home, "XDG_STATE_HOME")
}

fn storage_root(launch: &LaunchRecipe, home: &Path, category: &str) -> Option<PathBuf> {
    let profile = launch
        .arguments
        .iter()
        .enumerate()
        .find_map(|(index, arg)| {
            arg.strip_prefix("--profile=").or_else(|| {
                (arg == "--profile")
                    .then(|| launch.arguments.get(index + 1).map(String::as_str))
                    .flatten()
            })
        })
        .or_else(|| {
            launch
                .environment
                .get("OMP_PROFILE")
                .or_else(|| launch.environment.get("PI_PROFILE"))
                .map(String::as_str)
        })
        .map(str::trim)
        .filter(|profile| !profile.is_empty() && *profile != "default");
    let config = home.join(
        launch
            .environment
            .get("PI_CONFIG_DIR")
            .map(String::as_str)
            .unwrap_or(".omp"),
    );
    let default = if let Some(profile) = profile {
        if profile == "." || profile == ".." || profile.contains(['/', '\\']) {
            return None;
        }
        config.join("profiles").join(profile).join("agent")
    } else {
        config.join("agent")
    };
    let root = if profile.is_none() {
        launch
            .environment
            .get("PI_CODING_AGENT_DIR")
            .filter(|value| !value.is_empty())
            .map(|value| launch.working_directory.join(value))
            .unwrap_or_else(|| default.clone())
    } else {
        default.clone()
    };
    if root == default
        && let Some(xdg) = launch
            .environment
            .get(category)
            .filter(|value| !value.is_empty())
    {
        let mut path = launch.working_directory.join(xdg).join("omp");
        if let Some(profile) = profile {
            path = path.join("profiles").join(profile);
        }
        if path.is_dir() {
            return Some(path);
        }
    }
    Some(root)
}

fn activity_from_title(title: &str) -> Option<ActivityState> {
    // title-generator.ts buildTerminalTitleWithState: exact Linux lifecycle
    // separator, optionally followed by a space and label. Disabled/custom
    // titles have no recognized separator. Never search the session label.
    let mut suffix = title.strip_prefix("π ")?.chars();
    let state = match suffix.next()? {
        '>' => ActivityState::Idle,
        '!' => ActivityState::Waiting,
        '⠋' | '⠙' | '⠹' | '⠸' | '⠼' | '⠴' | '⠦' | '⠧' | '⠇' | '⠏' => {
            ActivityState::Working
        }
        _ => return None,
    };
    match suffix.next() {
        None | Some(' ') => Some(state),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_uses_the_observed_xdg_profile_and_respects_explicit_agent_override() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path();
        let data = home.join("data/omp");
        let state = home.join("state/omp");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let mut recipe = LaunchRecipe {
            program: "omp".into(),
            arguments: Vec::new(),
            working_directory: home.into(),
            environment: std::collections::BTreeMap::from([
                ("HOME".into(), home.to_string_lossy().into_owned()),
                (
                    "XDG_DATA_HOME".into(),
                    home.join("data").to_string_lossy().into_owned(),
                ),
                (
                    "XDG_STATE_HOME".into(),
                    home.join("state").to_string_lossy().into_owned(),
                ),
            ]),
        };
        assert_eq!(session_root(&recipe, home), Some(data.clone()));
        // Only the host resolves a state root; the UI lane compiles it out.
        #[cfg(feature = "host")]
        assert_eq!(state_root(&recipe, home), Some(state));
        recipe.arguments = vec!["--profile".into(), "work".into()];
        assert_eq!(
            session_root(&recipe, home),
            Some(home.join(".omp/profiles/work/agent"))
        );
        std::fs::create_dir_all(data.join("profiles/work")).unwrap();
        assert_eq!(
            session_root(&recipe, home),
            Some(data.join("profiles/work"))
        );
        recipe.arguments.clear();
        recipe
            .environment
            .insert("PI_CODING_AGENT_DIR".into(), "custom-agent".into());
        assert_eq!(session_root(&recipe, home), Some(home.join("custom-agent")));
    }

    #[test]
    fn lifecycle_separator_owns_state_not_label_content() {
        assert_eq!(
            Provider::Omp.activity_from_title("π > ⠋ processing !"),
            Some(ActivityState::Idle)
        );
        assert_eq!(
            Provider::Omp.activity_from_title("π !"),
            Some(ActivityState::Waiting)
        );
        for frame in ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'] {
            assert_eq!(
                Provider::Omp.activity_from_title(&format!("π {frame} waiting >")),
                Some(ActivityState::Working)
            );
        }
        for title in [
            "π",
            "π: ⠋ task",
            "custom π ⠋ task",
            "π >task",
            "π ⠁ task",
            "π  ⠋ task",
            "working",
        ] {
            assert_eq!(Provider::Omp.activity_from_title(title), None);
        }
    }
}
