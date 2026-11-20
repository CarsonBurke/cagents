//! Reads cagents' own record of a conversation for what points at pull
//! requests and worktrees, for providers whose transcript files are not
//! read: Antigravity and Muse, whose conversations are databases and logs
//! of their own, and OMP. cagents is the client of each of them, so the
//! commands they ran, the files they edited and what was said already sit in
//! the conversation's items, newest last.
//!
//! The record is read through the conversation's snapshot, its newest
//! window of items, as the agent console reads it: whatever owns the
//! conversation, a local actor or a retained one, serves it the same way.
//! Each item is read once it has settled, so a command is read with its
//! output and a message once it is no longer being written. What scrolled
//! out of the window before it was read makes the reading partial, and says
//! so.

use super::transcript::{self, Found};
use crate::{
    chat::{CommandAction, DeliveryKind, Item, ItemContent, Role, Snapshot, ToolStatus},
    pull_request_signals::{self, Signal},
    work::TranscriptCoverage,
};
use std::collections::{HashMap, VecDeque};

/// Items remembered as seen, about thirty windows' worth: enough to tell an
/// item already read from a new one as the window moves, and to notice when
/// it moved past something unread.
const REMEMBERED: usize = 4096;

/// One conversation's reading state between snapshots.
#[derive(Debug, Default)]
pub(super) struct Items {
    /// The actor that serves the record. Another one, after the
    /// conversation restarts, counts its generations afresh.
    owner: Option<uuid::Uuid>,
    /// The snapshot generation last read; a snapshot no newer is not served.
    generation: Option<u64>,
    /// Whether each item seen has been read: one still running or being
    /// written is seen but read later.
    seen: HashMap<String, bool>,
    /// The order items were first seen in, oldest first, to forget them by.
    order: VecDeque<String>,
    /// Something before the window was never read.
    partial: bool,
    /// The window last read was an earlier page someone scrolled back to,
    /// not the newest one.
    behind: bool,
}

impl Items {
    /// Notes which actor serves the record now. What was read stays read:
    /// a restarted conversation keeps its items' IDs.
    pub fn owned_by(&mut self, owner: uuid::Uuid) {
        if self.owner != Some(owner) {
            self.owner = Some(owner);
            self.generation = None;
        }
    }

    /// The generation a newer snapshot must exceed to be worth serving.
    pub fn generation(&self) -> Option<u64> {
        self.generation
    }

    /// Reads what `snapshot` holds that settled since it was last read.
    pub fn read(&mut self, snapshot: &Snapshot, out: &mut Vec<Found>) {
        self.generation = Some(snapshot.generation);
        self.behind = snapshot.has_later;
        // Earlier items exist, and the window does not begin with one seen
        // before: the first window of a long conversation, or a window that
        // moved on further than one pass between reads.
        if snapshot.has_earlier
            && snapshot
                .items
                .first()
                .is_none_or(|item| !self.seen.contains_key(&item.id))
        {
            self.partial = true;
        }
        let newest = snapshot.items.len().checked_sub(1);
        for (index, item) in snapshot.items.iter().enumerate() {
            if self.seen.get(&item.id) == Some(&true) {
                continue;
            }
            // Only the newest message can still be written to, and only
            // while the conversation works.
            let writing = snapshot.busy && Some(index) == newest;
            let settled = match signals(item, writing) {
                Some(signals) => {
                    out.extend(signals.into_iter().map(|signal| Found {
                        signal,
                        by_subagent: false,
                    }));
                    true
                }
                None => false,
            };
            self.remember(&item.id, settled);
        }
    }

    pub fn coverage(&self) -> TranscriptCoverage {
        match self.generation {
            None => TranscriptCoverage::Pending,
            Some(_) if self.behind => TranscriptCoverage::Pending,
            Some(_) if self.partial => TranscriptCoverage::Partial,
            Some(_) => TranscriptCoverage::Complete,
        }
    }

    fn remember(&mut self, id: &str, read: bool) {
        match self.seen.get_mut(id) {
            Some(known) => *known = read,
            None => {
                self.seen.insert(id.to_owned(), read);
                self.order.push_back(id.to_owned());
                while self.order.len() > REMEMBERED {
                    if let Some(oldest) = self.order.pop_front() {
                        self.seen.remove(&oldest);
                    }
                }
            }
        }
    }
}

/// What `item` points at, or `None` while it has not settled. `writing` says
/// whether a message may still grow.
fn signals(item: &Item, writing: bool) -> Option<Vec<Signal>> {
    let settled = |status: &ToolStatus| !status.is_live();
    Some(match &item.content {
        ItemContent::Text { text, thinking, .. } => match item.role {
            // A half-written URL would name another pull request.
            Role::User | Role::Assistant if writing => return None,
            Role::User | Role::Assistant if !thinking => transcript::prose(text),
            _ => Vec::new(),
        },
        ItemContent::Command {
            command,
            working_directory,
            action,
            status,
            exit_code,
            output,
            ..
        } => {
            if !settled(status) {
                return None;
            }
            // Reads, searches and listings name files, not work.
            if *action != CommandAction::Run {
                return Some(Vec::new());
            }
            let script = if working_directory.is_empty() {
                command.clone()
            } else {
                format!(
                    "cd {} && {command}",
                    transcript::shell_quote(working_directory)
                )
            };
            let succeeded = match status {
                ToolStatus::Succeeded => Some(exit_code.is_none_or(|code| code == 0)),
                ToolStatus::Failed => Some(false),
                _ => exit_code.map(|code| code == 0),
            };
            pull_request_signals::command(&script, output, succeeded)
        }
        ItemContent::Changes { changes, status } => {
            if !settled(status) {
                return None;
            }
            // A refused or failed edit changed nothing.
            if *status != ToolStatus::Succeeded {
                return Some(Vec::new());
            }
            let mut signals = changes
                .iter()
                .filter_map(|change| transcript::edited(&change.path))
                .collect::<Vec<_>>();
            signals.sort_by(|a, b| a.reference.cmp(&b.reference));
            signals.dedup();
            signals
        }
        ItemContent::Tool {
            name,
            input,
            output,
            status,
        } => {
            if !settled(status) {
                return None;
            }
            pull_request_signals::tool(name, input, output)
        }
        // What another agent reported or wrote to this conversation names
        // the pull requests it worked on; a harness notice does not.
        ItemContent::Delivery { kind, body, .. } => match kind {
            DeliveryKind::Report | DeliveryKind::Message => transcript::prose(body),
            DeliveryKind::Notice => Vec::new(),
        },
        ItemContent::WebSearch { .. } | ItemContent::Image { .. } => Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        chat::{ChangeKind, FileChange, Phase},
        pull_request_signals::{PullRequestKeyOrd, Reference, RepoKeyOrd},
        work::Relation,
    };
    use std::sync::Arc;

    fn item(id: &str, role: Role, content: ItemContent) -> Arc<Item> {
        Arc::new(Item {
            started_at: None,
            id: id.into(),
            role,
            content,
            at: None,
        })
    }

    fn text(id: &str, role: Role, text: &str) -> Arc<Item> {
        item(
            id,
            role,
            ItemContent::Text {
                text: text.into(),
                thinking: false,
                phase: Phase::Final,
            },
        )
    }

    fn command(id: &str, script: &str, status: ToolStatus, output: &str) -> Arc<Item> {
        item(
            id,
            Role::Assistant,
            ItemContent::Command {
                label: String::new(),
                command: script.into(),
                working_directory: "/work/repo".into(),
                action: CommandAction::Run,
                status,
                exit_code: None,
                duration_ms: None,
                output: output.into(),
            },
        )
    }

    fn snapshot(generation: u64, items: Vec<Arc<Item>>, busy: bool, earlier: bool) -> Snapshot {
        Snapshot {
            generation,
            items,
            busy,
            has_earlier: earlier,
            ..Snapshot::default()
        }
    }

    fn read(items: &mut Items, snapshot: &Snapshot) -> Vec<(Reference, Relation)> {
        let mut found = Vec::new();
        items.read(snapshot, &mut found);
        found
            .into_iter()
            .map(|found| (found.signal.reference, found.signal.relation))
            .collect()
    }

    fn created(number: u64) -> Reference {
        Reference::PullRequest(PullRequestKeyOrd {
            repository: RepoKeyOrd {
                host: "github.com".into(),
                port: None,
                owner: "owner".into(),
                name: "repo".into(),
            },
            number,
        })
    }

    #[test]
    fn a_command_is_read_once_it_finishes_with_what_it_printed() {
        let mut items = Items::default();
        assert_eq!(items.coverage(), TranscriptCoverage::Pending);
        let running = command("c", "gh pr create --fill", ToolStatus::Running, "");
        assert!(read(&mut items, &snapshot(1, vec![running], true, false)).is_empty());
        let done = command(
            "c",
            "gh pr create --fill",
            ToolStatus::Succeeded,
            "https://github.com/owner/repo/pull/7\n",
        );
        let found = read(&mut items, &snapshot(2, vec![done.clone()], false, false));
        assert!(
            found.contains(&(created(7), Relation::Created)),
            "{found:?}"
        );
        // Read once, however often it is served again.
        assert!(read(&mut items, &snapshot(3, vec![done], false, false)).is_empty());
        assert_eq!(items.coverage(), TranscriptCoverage::Complete);
    }

    #[test]
    fn a_message_is_read_once_it_is_no_longer_being_written() {
        let mut items = Items::default();
        let partial = text(
            "m",
            Role::Assistant,
            "Opened https://github.com/owner/repo/pull/1",
        );
        assert!(read(&mut items, &snapshot(1, vec![partial], true, false)).is_empty());
        let whole = text(
            "m",
            Role::Assistant,
            "Opened https://github.com/owner/repo/pull/12",
        );
        // Once the turn ends, or once something follows it.
        let found = read(
            &mut items,
            &snapshot(
                2,
                vec![whole, command("n", "ls", ToolStatus::Running, "")],
                true,
                false,
            ),
        );
        assert_eq!(found, vec![(created(12), Relation::Mentioned)]);
    }

    #[test]
    fn a_succeeded_edit_points_at_its_directory_s_branch_and_a_refused_one_does_not() {
        let mut items = Items::default();
        let edit = |id: &str, status| {
            item(
                id,
                Role::Assistant,
                ItemContent::Changes {
                    changes: vec![FileChange {
                        path: format!("/work/tree-{id}/src/lib.rs"),
                        kind: ChangeKind::Updated,
                        diff: String::new(),
                    }],
                    status,
                },
            )
        };
        let found = read(
            &mut items,
            &snapshot(
                1,
                vec![
                    edit("a", ToolStatus::Succeeded),
                    edit("b", ToolStatus::Denied),
                    edit("c", ToolStatus::Running),
                ],
                true,
                false,
            ),
        );
        assert_eq!(
            found,
            vec![(
                Reference::CurrentBranch {
                    directory: Some("/work/tree-a/src".into())
                },
                Relation::Edited
            )]
        );
    }

    #[test]
    fn thinking_and_harness_notices_name_nothing() {
        let mut items = Items::default();
        let found = read(
            &mut items,
            &snapshot(
                1,
                vec![
                    item(
                        "t",
                        Role::Assistant,
                        ItemContent::Text {
                            text: "maybe https://github.com/owner/repo/pull/3".into(),
                            thinking: true,
                            phase: Phase::Final,
                        },
                    ),
                    item(
                        "n",
                        Role::System,
                        ItemContent::Delivery {
                            kind: DeliveryKind::Notice,
                            from: String::new(),
                            sender: String::new(),
                            summary: String::new(),
                            body: "https://github.com/owner/repo/pull/4".into(),
                        },
                    ),
                ],
                false,
                false,
            ),
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_long_conversation_or_a_window_that_moved_past_unread_items_is_partial() {
        let mut items = Items::default();
        read(
            &mut items,
            &snapshot(1, vec![text("a", Role::User, "hi")], false, false),
        );
        assert_eq!(items.coverage(), TranscriptCoverage::Complete);
        // The window moved on by one: its first item was seen.
        read(
            &mut items,
            &snapshot(
                2,
                vec![text("a", Role::User, "hi"), text("b", Role::User, "more")],
                false,
                true,
            ),
        );
        assert_eq!(items.coverage(), TranscriptCoverage::Complete);
        // It moved past items never read.
        read(
            &mut items,
            &snapshot(3, vec![text("z", Role::User, "later")], false, true),
        );
        assert_eq!(items.coverage(), TranscriptCoverage::Partial);

        let mut resumed = Items::default();
        read(
            &mut resumed,
            &snapshot(1, vec![text("y", Role::User, "newest")], false, true),
        );
        assert_eq!(resumed.coverage(), TranscriptCoverage::Partial);
    }

    #[test]
    fn what_is_remembered_is_bounded() {
        let mut items = Items::default();
        for index in 0..REMEMBERED + 10 {
            items.remember(&index.to_string(), true);
        }
        assert_eq!(items.seen.len(), REMEMBERED);
        assert!(!items.seen.contains_key("0"));
    }
}
