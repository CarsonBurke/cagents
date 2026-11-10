//! Read-only saved-pane outcomes carried with the update review.
use crate::model::TerminalId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type PaneLabel = String;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoppedKind {
    Chat,
    Terminal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoppedPane {
    pub pane: TerminalId,
    pub kind: StoppedKind,
    pub label: PaneLabel,
    pub reason: String,
    pub recovery: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoppedWindow {
    pub window: usize,
    pub reason: String,
    pub recovery: String,
}

/// Flattened into Preview to keep its `labels` JSON map and Rust constructors
/// intact. Older readers ignore the separate `review_details` field.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewInventory {
    #[serde(default, rename = "labels")]
    pub names: BTreeMap<TerminalId, PaneLabel>,
    #[serde(default, rename = "review_details")]
    pub details: ReviewDetails,
}

/// Saved panes absent from the live service still belong in the review.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SavedOutcome {
    Unchanged,
    Resumes,
    KeepsRunning,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectedPane {
    pub kind: super::impact::SessionKind,
    pub outcome: SavedOutcome,
    #[serde(default)]
    pub detail: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewDetails {
    #[serde(default)]
    pub panes: BTreeMap<TerminalId, InspectedPane>,
    #[serde(default)]
    pub notices: Vec<String>,
    /// Versions belong to the matched cagents companions, not the editor.
    #[serde(default)]
    pub editor_companions: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceInspection {
    #[serde(default)]
    pub details: ReviewDetails,
    #[serde(default)]
    pub stopped: Vec<StoppedPane>,
    #[serde(default)]
    pub stopped_windows: Vec<StoppedWindow>,
    #[serde(default)]
    pub labels: BTreeMap<TerminalId, PaneLabel>,
}

impl SourceInspection {
    pub fn preserves_consent(&self, observed: &Self) -> bool {
        Self::same_outcomes(
            &self.stopped,
            &self.stopped_windows,
            &observed.stopped,
            &observed.stopped_windows,
        )
    }

    pub fn same_outcomes(
        previous_panes: &[StoppedPane],
        previous_windows: &[StoppedWindow],
        current_panes: &[StoppedPane],
        current_windows: &[StoppedWindow],
    ) -> bool {
        fn panes(panes: &[StoppedPane]) -> BTreeMap<(TerminalId, StoppedKind), &str> {
            panes
                .iter()
                .map(|pane| ((pane.pane, pane.kind), pane.reason.as_str()))
                .collect()
        }
        fn windows(windows: &[StoppedWindow]) -> BTreeMap<usize, &str> {
            windows
                .iter()
                .map(|window| (window.window, window.reason.as_str()))
                .collect()
        }
        let previous = panes(previous_panes);
        let current = panes(current_panes);
        let before_windows = windows(previous_windows);
        let after_windows = windows(current_windows);
        previous.len() == previous_panes.len()
            && current.len() == current_panes.len()
            && previous == current
            && before_windows.len() == previous_windows.len()
            && after_windows.len() == current_windows.len()
            && before_windows == after_windows
    }

    #[cfg(feature = "host")]
    pub(crate) fn stop(
        &mut self,
        pane: TerminalId,
        kind: StoppedKind,
        reason: &str,
        recovery: &str,
    ) {
        if self
            .stopped
            .iter()
            .any(|stopped| stopped.pane == pane && stopped.kind == kind)
        {
            return;
        }
        self.stopped.push(StoppedPane {
            pane,
            kind,
            label: self
                .labels
                .get(&pane)
                .cloned()
                .unwrap_or_else(|| match kind {
                    StoppedKind::Chat => "Conversation".into(),
                    StoppedKind::Terminal => "Terminal".into(),
                }),
            reason: reason.into(),
            recovery: recovery.into(),
        });
    }
}

#[cfg(any(feature = "host", test))]
pub(crate) fn is_untitled(title: &str, chat: bool) -> bool {
    title.is_empty()
        || matches!(title, "Claude Code" | "Codex" | "OMP")
        || (chat
            && matches!(
                title,
                "OMP chat" | "Claude chat" | "Codex chat" | "Native chat"
            ))
}

/// Match the sidebar's persisted pane/channel/project names without parsing
/// punctuation inside a user title. Old stores may lack a session start time.
#[cfg(any(feature = "host", test))]
pub(crate) fn pane_label(
    title: &str,
    channel: Option<&str>,
    project: &str,
    chat: bool,
    started: Option<i64>,
) -> PaneLabel {
    let untitled = is_untitled(title, chat);
    if untitled && (chat || matches!(title, "Claude Code" | "Codex" | "OMP")) {
        let started = started.and_then(crate::chat::local_time).map(|time| {
            let month = [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
            ][time.month];
            format!("{} {month} {}", time.day, time.clock())
        });
        return match started {
            Some(started) => format!("Untitled conversation in {project}, started {started}"),
            None => format!("Untitled conversation in {project}, start time unavailable"),
        };
    }
    let title = if title.is_empty() { "Terminal" } else { title };
    match channel {
        Some(channel) => format!("{title} · {channel} · {project}"),
        None => format!("{title} · {project}"),
    }
}

/// Read bounded provider metadata, never a transcript's tail or filesystem
/// modification time. A missing timestamp remains explicitly unavailable.
#[cfg(feature = "host")]
pub(crate) fn conversation_started(source: &crate::chat::Session) -> Option<i64> {
    launch_started(&source.launch)
}

#[cfg(feature = "host")]
pub(crate) fn terminal_started(source: &crate::model::TerminalSession) -> Option<i64> {
    let recipe = source.agent_resume_recipe()?;
    let provider = crate::agent::Provider::from_program(&recipe.program)?;
    let session_id = provider.resume_id(&recipe.arguments)?.to_owned();
    launch_started(&crate::chat::Launch {
        provider,
        recipe,
        session_id: Some(session_id),
    })
}

#[cfg(feature = "host")]
fn launch_started(launch: &crate::chat::Launch) -> Option<i64> {
    let id = launch.session_id.as_deref()?;
    if launch.provider == crate::agent::Provider::Codex {
        let id = uuid::Uuid::parse_str(id).ok()?;
        return id
            .get_timestamp()
            .filter(|_| id.get_version_num() == 7)
            .map(|at| at.to_unix().0 as i64);
    }
    if launch.provider != crate::agent::Provider::Claude {
        return None;
    }
    // Claude's long-path resolver scans project directories. Label collection
    // must stay bounded independently of how much provider history exists.
    if launch.recipe.working_directory.to_str()?.chars().count() > 200 {
        return None;
    }
    let root = launch.provider.session_root(&launch.recipe)?;
    let path = crate::agent::claude_transcript(&root, &launch.recipe.working_directory, id).ok()?;
    let file = std::fs::File::open(path).ok()?;
    transcript_started(file)
}

#[cfg(feature = "host")]
fn transcript_started(reader: impl std::io::Read) -> Option<i64> {
    use std::io::BufRead;
    let mut input = std::io::BufReader::new(reader.take(64 * 1024));
    let mut line = String::new();
    for _ in 0..64 {
        line.clear();
        input.read_line(&mut line).ok()?;
        if !line.ends_with('\n') {
            return None;
        }
        let Ok(metadata) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let timestamp = metadata.get("timestamp").or_else(|| {
            metadata
                .get("snapshot")
                .and_then(|snapshot| snapshot.get("timestamp"))
        });
        if let Some(timestamp) = timestamp
            .and_then(serde_json::Value::as_str)
            .and_then(|at| at.parse::<jiff::Timestamp>().ok())
        {
            return Some(timestamp.as_second());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_labels_preserve_punctuation_and_missing_times() {
        assert_eq!(
            pane_label(
                "Release Details: rollout",
                Some("Release Details: alpha"),
                "alpha",
                true,
                None
            ),
            "Release Details: rollout · Release Details: alpha · alpha"
        );
        assert_eq!(
            pane_label("Claude chat", None, "beta", true, None),
            "Untitled conversation in beta, start time unavailable"
        );
        let started = 1_794_258_120;
        let time = crate::chat::local_time(started).unwrap();
        let month = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ][time.month];
        assert_eq!(
            pane_label("", None, "beta", true, Some(started)),
            format!(
                "Untitled conversation in beta, started {} {month} {:02}:{:02}",
                time.day, time.hour, time.minute
            )
        );
    }

    #[test]
    fn old_flat_label_maps_and_typed_review_details_round_trip() {
        let id = TerminalId::new();
        let old = serde_json::json!({"labels": {id.to_string(): "kept title"}});
        let mut inventory: ReviewInventory = serde_json::from_value(old).unwrap();
        assert_eq!(inventory.names[&id], "kept title");
        assert_eq!(inventory.details, ReviewDetails::default());
        inventory
            .details
            .notices
            .push("The running terminal service could not be checked".into());
        let json = serde_json::to_value(&inventory).unwrap();
        let legacy: BTreeMap<TerminalId, String> =
            serde_json::from_value(json["labels"].clone()).unwrap();
        assert_eq!(legacy[&id], "kept title");
        assert_eq!(
            serde_json::from_value::<ReviewInventory>(serde_json::to_value(&inventory).unwrap())
                .unwrap(),
            inventory
        );
    }

    #[test]
    fn provider_default_console_titles_use_recorded_start_times() {
        for title in ["Claude Code", "Codex", "OMP"] {
            let first = pane_label(title, None, "alpha", true, Some(1_794_232_920));
            let second = pane_label(title, None, "alpha", true, Some(1_794_233_820));
            assert!(first.starts_with("Untitled conversation in alpha, started "));
            assert_ne!(first, second);
            assert_eq!(
                pane_label(title, None, "alpha", false, Some(1_794_232_920)),
                first
            );
        }
    }

    #[cfg(feature = "host")]
    #[test]
    fn provider_start_metadata_is_bounded_and_skips_records_without_times() {
        assert_eq!(
            transcript_started(
                b"{\"type\":\"title\"}\n{\"timestamp\":\"2026-11-09T14:02:00Z\"}\n".as_slice()
            ),
            Some(1_794_232_920)
        );
        assert_eq!(transcript_started(b"{\"type\":\"file-history-snapshot\",\"snapshot\":{\"timestamp\":\"2026-11-09T14:02:00Z\"}}\n".as_slice()), Some(1_794_232_920));
        assert_eq!(transcript_started(std::io::repeat(b'x')), None);
    }
}
