//! A provider identity that can be compared with its saved conversation.
use crate::{
    agent::Provider,
    chat::{Item, ItemContent, Role, Snapshot, TranscriptPart},
};
use serde::{Deserialize, Serialize};

/// Codex names turns directly. Claude names its root user input, which is
/// stable across the live stream and a reload of the provider's transcript.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumePoint {
    provider: Provider,
    session: String,
    turn: String,
}

impl ResumePoint {
    pub(crate) fn capture(provider: Provider, snapshot: &Snapshot) -> Option<Self> {
        let session = snapshot.session_id.as_ref()?;
        let turn = Self::snapshot_turns(provider, snapshot)?.next()?;
        Some(Self {
            provider,
            session: session.clone(),
            turn: turn.to_owned(),
        })
    }

    pub fn matches_launch(&self, launch: &crate::chat::Launch) -> bool {
        self.provider == launch.provider
            && launch.session_id.as_deref() == Some(self.session.as_str())
    }

    /// A confirmation from a current provider publication. Earlier pages and
    /// metadata-only publications may lack both inputs.
    pub fn advanced_in(&self, provider: Provider, snapshot: &Snapshot) -> bool {
        self.provider == provider
            && snapshot.session_id.as_deref() == Some(self.session.as_str())
            && Self::snapshot_turns(provider, snapshot).is_some_and(|turns| self.advanced(turns))
    }

    fn snapshot_turns(
        provider: Provider,
        snapshot: &Snapshot,
    ) -> Option<impl Iterator<Item = &str>> {
        (!snapshot.has_later && !snapshot.history_loading).then(|| {
            snapshot
                .items
                .iter()
                .rev()
                .filter_map(move |item| Self::item_turn(provider, item))
        })
    }

    fn item_turn(provider: Provider, item: &Item) -> Option<&str> {
        if item.role != Role::User
            || !matches!(
                item.content,
                ItemContent::Text {
                    thinking: false,
                    ..
                } | ItemContent::Image { .. }
            )
        {
            return None;
        }
        let id = TranscriptPart::parse(&item.id).map_or(item.id.as_str(), |part| part.item);
        let turn = match provider {
            Provider::Claude => {
                let (input, block) = id.rsplit_once(':')?;
                block.parse::<usize>().ok()?;
                input
            }
            Provider::Codex => {
                // Only the app-server's input item IDs are scoped to a turn.
                let (item, block) = id.rsplit_once("/input/")?;
                block.parse::<usize>().ok()?;
                let (turn, _) = item.split_once('/')?;
                turn
            }
            _ => return None,
        };
        Self::provider_id(turn)
    }

    fn provider_id(id: &str) -> Option<&str> {
        (id.len() == 36 && uuid::Uuid::parse_str(id).is_ok()).then_some(id)
    }

    /// A different ID can be an older publication, or persisted history that
    /// has yet to catch up to the source. Require a later turn in the same
    /// history, with the recorded turn still preceding it.
    fn advanced<'a>(&self, mut newest_first: impl Iterator<Item = &'a str>) -> bool {
        newest_first.next().is_some_and(|id| id != self.turn)
            && newest_first.any(|id| id == self.turn)
    }
}

#[cfg(feature = "ui")]
impl ResumePoint {
    /// Check the provider's own file. A paused pane may still show a frozen
    /// pre-update transcript. Neither this read nor an unavailable file starts
    /// a provider or changes the saved conversation.
    pub fn advanced_on_disk(&self, launch: &crate::chat::Launch) -> bool {
        self.matches_launch(launch) && self.read_history(launch).unwrap_or(false)
    }

    fn read_history(&self, launch: &crate::chat::Launch) -> Option<bool> {
        use std::io::{BufRead, BufReader, Read};
        use std::os::unix::fs::{FileExt, OpenOptionsExt};

        const TAIL_BYTES: u64 = 4 * 1024 * 1024;
        let path = crate::agent::native_history_path(launch)?;
        crate::secure_file::reject_symlink_components(&path).ok()?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() {
            return None;
        }
        if self.provider == Provider::Codex {
            let mut header = Vec::new();
            BufReader::new((&file).take(64 * 1024))
                .read_until(b'\n', &mut header)
                .ok()?;
            let header = serde_json::from_slice::<serde_json::Value>(&header).ok()?;
            if header["type"] != "session_meta" || header["payload"]["id"] != self.session {
                return None;
            }
        }
        let start = metadata.len().saturating_sub(TAIL_BYTES);
        let mut bytes = vec![0; (metadata.len() - start) as usize];
        file.read_exact_at(&mut bytes, start).ok()?;

        // A writer can be in the middle of appending its next record.
        let end = bytes.iter().rposition(|byte| *byte == b'\n')?;
        let first = if start == 0 {
            0
        } else {
            bytes[..end].iter().position(|byte| *byte == b'\n')? + 1
        };
        let mut seen_later = false;
        for line in bytes[first..end].rsplit(|byte| *byte == b'\n') {
            let Some(turn) = self.saved_turn(line) else {
                continue;
            };
            if turn == self.turn {
                return Some(seen_later);
            }
            seen_later = true;
        }
        None
    }

    fn saved_turn(&self, line: &[u8]) -> Option<String> {
        let candidate = match self.provider {
            Provider::Claude => b"\"user\"".as_slice(),
            Provider::Codex => b"\"turn_id\"".as_slice(),
            _ => return None,
        };
        if !line.windows(candidate.len()).any(|part| part == candidate) {
            return None;
        }
        let row = serde_json::from_slice::<serde_json::Value>(line).ok()?;
        let turn = match self.provider {
            Provider::Claude
                if row["type"] == "user"
                    && row["sessionId"] == self.session
                    && crate::agent::claude::root_input(&row) =>
            {
                row["message"]["id"]
                    .as_str()
                    .or_else(|| row["uuid"].as_str())?
            }
            Provider::Codex if row["type"] == "event_msg" => {
                let payload = &row["payload"];
                if payload["type"] != "task_started" {
                    return None;
                }
                let turn = payload["turn_id"].as_str()?;
                if payload["root_turn_id"]
                    .as_str()
                    .is_some_and(|root| root != turn)
                {
                    return None;
                }
                turn
            }
            _ => return None,
        };
        Self::provider_id(turn).map(str::to_owned)
    }
}

#[cfg(all(test, any(feature = "ui", not(feature = "host"))))]
mod tests {
    use super::*;
    use std::sync::Arc;

    const SESSION: &str = "d1ffb0e0-d588-478a-94e4-be7d954d3c5b";
    const BEFORE: &str = "d1ffb0e0-d588-478a-94e4-be7d954d3c5c";
    const AFTER: &str = "d1ffb0e0-d588-478a-94e4-be7d954d3c5d";

    fn snapshot(ids: &[&str]) -> Snapshot {
        Snapshot {
            session_id: Some(SESSION.into()),
            items: ids
                .iter()
                .map(|id| {
                    Arc::new(Item {
                        id: (*id).into(),
                        role: Role::User,
                        content: ItemContent::Text {
                            text: "same words".into(),
                            thinking: false,
                            phase: Default::default(),
                        },
                        started_at: None,
                        at: None,
                    })
                })
                .collect(),
            ..Snapshot::default()
        }
    }

    #[test]
    fn codex_uses_turn_ids_and_ignores_other_parts_of_the_same_turn() {
        let input = |turn: &str, part| format!("{turn}/input-item/input/{part}");
        let initial = snapshot(&[&input(BEFORE, 0)]);
        let point = ResumePoint::capture(Provider::Codex, &initial).unwrap();
        let same = snapshot(&[&input(BEFORE, 1)]);
        let mut newer = snapshot(&[&input(BEFORE, 1), &input(AFTER, 0)]);
        assert!(!point.advanced_in(Provider::Codex, &same));
        assert!(point.advanced_in(Provider::Codex, &newer));
        newer.has_later = true;
        assert!(!point.advanced_in(Provider::Codex, &newer));
        assert!(ResumePoint::capture(Provider::Codex, &snapshot(&["line-45"])).is_none());
    }

    #[test]
    fn claude_requires_a_new_input_after_the_recorded_input_in_its_own_session() {
        let baseline = format!("{BEFORE}:0");
        let after = format!("{AFTER}:0");
        let point = ResumePoint::capture(Provider::Claude, &snapshot(&[&baseline])).unwrap();
        assert!(!point.advanced_in(Provider::Claude, &snapshot(&[&after])));
        let mut newer = snapshot(&[&baseline, &after]);
        assert!(point.advanced_in(Provider::Claude, &newer));
        newer.session_id = Some(AFTER.into());
        assert!(!point.advanced_in(Provider::Claude, &newer));
    }

    #[cfg(feature = "ui")]
    fn launch(provider: Provider, home: &std::path::Path) -> crate::chat::Launch {
        crate::chat::Launch {
            provider,
            session_id: Some(SESSION.into()),
            recipe: crate::model::LaunchRecipe {
                program: provider.executable().into(),
                arguments: Vec::new(),
                working_directory: home.to_owned(),
                environment: [("HOME".into(), home.to_string_lossy().into_owned())].into(),
                unset_environment: Default::default(),
            },
            draft_model: None,
        }
    }

    #[cfg(feature = "ui")]
    #[test]
    fn a_paused_claude_chat_uses_the_newest_complete_root_input_from_its_private_history() {
        use serde_json::json;
        use std::io::Write;

        let home = crate::test_fixture::tempdir().unwrap();
        let home = home.path().canonicalize().unwrap();
        let launch = launch(Provider::Claude, &home);
        let key = home
            .to_str()
            .unwrap()
            .chars()
            .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
            .collect::<String>();
        let project = home.join(".claude/projects").join(key);
        std::fs::create_dir_all(&project).unwrap();
        let mut file = std::fs::File::create(project.join(format!("{SESSION}.jsonl"))).unwrap();
        let user = |turn, session, text| {
            json!({
                "type": "user", "sessionId": session, "uuid": turn,
                "message": {"role": "user", "content": text}
            })
        };
        let point =
            ResumePoint::capture(Provider::Claude, &snapshot(&[&format!("{BEFORE}:0")])).unwrap();
        let copied = user(AFTER, AFTER, "copied from a different conversation");
        let mut child = user(AFTER, SESSION, "from a sidechain");
        child["isSidechain"] = true.into();
        let initial = user(BEFORE, SESSION, "the interrupted question");
        let interruption = user(AFTER, SESSION, "[Request interrupted by user]");
        let peer = user(
            AFTER,
            SESSION,
            "Another Claude session sent a message:\n<teammate-message teammate_id=\"peer\">\nReport\n</teammate-message>",
        );
        for row in [initial, copied, child, interruption, peer] {
            writeln!(file, "{row}").unwrap();
        }
        assert!(!point.advanced_on_disk(&launch));
        let next = user(AFTER, SESSION, "continued from the CLI");
        write!(file, "{next}").unwrap();
        assert!(!point.advanced_on_disk(&launch));
        writeln!(file).unwrap();
        assert!(point.advanced_on_disk(&launch));
    }

    #[cfg(feature = "ui")]
    #[test]
    fn a_codex_rollout_completion_for_the_same_turn_still_needs_resume() {
        use serde_json::json;
        use std::io::Write;

        let home = crate::test_fixture::tempdir().unwrap();
        let launch = launch(Provider::Codex, home.path());
        let day = home.path().join(".codex/sessions/2026/11/28");
        std::fs::create_dir_all(&day).unwrap();
        let path = day.join(format!("rollout-2026-11-28T00-00-00-{SESSION}.jsonl"));
        let mut file = std::fs::File::create(path).unwrap();
        let event =
            |kind, turn| json!({"type":"event_msg", "payload":{"type":kind, "turn_id":turn}});
        let point = ResumePoint::capture(
            Provider::Codex,
            &snapshot(&[&format!("{BEFORE}/item/input/0")]),
        )
        .unwrap();
        for row in [
            json!({"type":"session_meta", "payload":{"id":SESSION, "source":"cli"}}),
            event("task_started", BEFORE),
            event("turn_aborted", BEFORE),
        ] {
            writeln!(file, "{row}").unwrap();
        }
        assert!(!point.advanced_on_disk(&launch));
        writeln!(file, "{}", event("task_started", AFTER)).unwrap();
        assert!(point.advanced_on_disk(&launch));
    }
}
