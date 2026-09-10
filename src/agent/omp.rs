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
    ],
    switches: &[
        "--no-lsp",
        "--no-tools",
        "--no-extensions",
        "--no-skills",
        "--no-rules",
        "--auto-approve",
    ],
    resume_flag: "--resume",
    resume_precedes_options: false,
    forwards_to_native_child: false,
    #[cfg(feature = "host")]
    create_chat: crate::chat::protocol::omp::create,
};

fn session_root(launch: &LaunchRecipe, home: &Path) -> Option<PathBuf> {
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
        .or_else(|| launch.environment.get("OMP_PROFILE").map(String::as_str));
    if let Some(profile) = profile {
        if profile.is_empty() || profile == "." || profile == ".." || profile.contains('/') {
            return None;
        }
        Some(home.join(".omp/profiles").join(profile).join("agent"))
    } else {
        Some(
            launch
                .environment
                .get("PI_CODING_AGENT_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".omp/agent")),
        )
    }
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
