//! Reads cagents' own record of a conversation for what points at pull
//! requests and worktrees, for providers whose transcript files are not
//! read: Antigravity and Muse, whose conversations are databases and logs
//! of their own, and OMP. cagents is the client of each of them, so the
//! commands they ran, the files they edited and what was said already sit in
//! the conversation's items.
//!
//! The record is followed through its store, which serves each item whose
//! content changed since the last read, whole, without touching what any
//! window presents. An item is read once it has settled, so a command is
//! read with its output and a message once it is no longer being written,
//! and read again if it changes after that. A first read, or one after more
//! changed between reads than the store remembers, starts from the newest
//! items; what came before them and was never read makes the reading
//! partial, and says so.

use super::transcript::{self, Found};
use crate::{
    chat::{CommandAction, DeliveryKind, Item, ItemContent, Role, ToolStatus, runtime::Unread},
    pull_request_signals::{self, Signal},
    work::TranscriptCoverage,
};
use std::collections::{BTreeSet, HashSet, VecDeque};

/// Items remembered as read, so a restarted read can tell whether it
/// reached back to what was read before.
const REMEMBERED: usize = 4096;
/// Unsettled items asked for again on each read.
const HELD: usize = 1024;

/// One conversation's reading state between reads.
#[derive(Debug, Default)]
pub(super) struct Items {
    started: bool,
    /// Items read, in the order they were first read, to forget them by.
    seen: HashSet<String>,
    order: VecDeque<String>,
    /// Items served before they settled. Settling changes most of them, but
    /// not a message whose turn ended, so each read asks for them again.
    held: BTreeSet<String>,
    /// Something before what was read was never read.
    partial: bool,
    /// Why the last read failed.
    error: Option<String>,
}

impl Items {
    /// The items to serve again with the next read.
    pub fn again(&self) -> Vec<String> {
        self.held.iter().cloned().collect()
    }

    /// Reads what settled in `unread`, served while the conversation was
    /// `busy` or not.
    pub fn read(&mut self, unread: Unread, busy: bool, out: &mut Vec<Found>) {
        self.error = None;
        // Earlier items exist, and the read does not reach back to one read
        // before: the first read of a long conversation, or one that missed
        // more changes than were remembered.
        if unread.restarted
            && unread.has_earlier
            && unread
                .items
                .first()
                .is_none_or(|item| !self.seen.contains(&item.id))
        {
            self.partial = true;
        }
        for item in &unread.items {
            // Only the newest message can still be written to, and only
            // while the conversation works.
            let writing = busy && unread.newest.as_ref() == Some(&item.id);
            match signals(item, writing) {
                Some(signals) => {
                    self.held.remove(&item.id);
                    out.extend(signals.into_iter().map(|signal| Found {
                        signal,
                        by_subagent: false,
                    }));
                }
                None => {
                    self.held.insert(item.id.clone());
                    while self.held.len() > HELD {
                        self.held.pop_first();
                    }
                }
            }
            self.remember(&item.id);
        }
        self.started = true;
    }

    /// Records that the record could not be read.
    pub fn failed(&mut self, error: String) {
        self.error = Some(error);
    }

    pub fn coverage(&self) -> TranscriptCoverage {
        match &self.error {
            Some(reason) => TranscriptCoverage::Unavailable {
                reason: format!("This chat's record cannot be read: {reason}"),
            },
            None if !self.started => TranscriptCoverage::Pending,
            None if self.partial => TranscriptCoverage::Partial,
            None => TranscriptCoverage::Complete,
        }
    }

    fn remember(&mut self, id: &str) {
        if self.seen.insert(id.to_owned()) {
            self.order.push_back(id.to_owned());
            while self.order.len() > REMEMBERED {
                if let Some(oldest) = self.order.pop_front() {
                    self.seen.remove(&oldest);
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

    fn item(id: &str, role: Role, content: ItemContent) -> Item {
        Item {
            started_at: None,
            id: id.into(),
            role,
            content,
            at: None,
        }
    }

    fn text(id: &str, role: Role, text: &str) -> Item {
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

    fn command(id: &str, script: &str, status: ToolStatus, output: &str) -> Item {
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

    /// What changed, the last of it the newest item.
    fn changed(items: Vec<Item>) -> Unread {
        Unread {
            newest: items.last().map(|item| item.id.clone()),
            items,
            ..Unread::default()
        }
    }

    /// A read that started again from the newest items.
    fn restarted(items: Vec<Item>, earlier: bool) -> Unread {
        Unread {
            restarted: true,
            has_earlier: earlier,
            ..changed(items)
        }
    }

    fn read(items: &mut Items, unread: Unread, busy: bool) -> Vec<(Reference, Relation)> {
        let mut found = Vec::new();
        items.read(unread, busy, &mut found);
        found
            .into_iter()
            .map(|found| (found.signal.reference, found.signal.relation))
            .collect()
    }

    fn pull(number: u64) -> Reference {
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
    fn a_command_is_read_when_it_finishes_and_again_when_it_changes_after() {
        let mut items = Items::default();
        assert_eq!(items.coverage(), TranscriptCoverage::Pending);
        let running = command("c", "gh pr create --fill", ToolStatus::Running, "");
        assert!(read(&mut items, restarted(vec![running], false), true).is_empty());
        assert_eq!(items.again(), ["c"]);
        // Finished before its output arrived: what it printed is read when
        // the output changes it.
        let finished = command("c", "gh pr create --fill", ToolStatus::Succeeded, "");
        assert!(read(&mut items, changed(vec![finished]), false).is_empty());
        assert!(items.again().is_empty());
        let printed = command(
            "c",
            "gh pr create --fill",
            ToolStatus::Succeeded,
            "https://github.com/owner/repo/pull/7\n",
        );
        let found = read(&mut items, changed(vec![printed]), false);
        assert!(found.contains(&(pull(7), Relation::Created)), "{found:?}");
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
        assert!(read(&mut items, restarted(vec![partial], false), true).is_empty());
        assert_eq!(items.again(), ["m"]);
        let whole = text(
            "m",
            Role::Assistant,
            "Opened https://github.com/owner/repo/pull/12",
        );
        // The turn ended without changing it: it is served again, as held.
        let found = read(&mut items, changed(vec![whole.clone()]), false);
        assert_eq!(found, vec![(pull(12), Relation::Mentioned)]);
        assert!(items.again().is_empty());

        // Or something came after it while the turn went on.
        let mut items = Items::default();
        let found = read(
            &mut items,
            restarted(
                vec![whole, command("n", "ls", ToolStatus::Running, "")],
                false,
            ),
            true,
        );
        assert_eq!(found, vec![(pull(12), Relation::Mentioned)]);
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
            restarted(
                vec![
                    edit("a", ToolStatus::Succeeded),
                    edit("b", ToolStatus::Denied),
                    edit("c", ToolStatus::Running),
                ],
                false,
            ),
            true,
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
        assert_eq!(items.again(), ["c"]);
    }

    #[test]
    fn thinking_and_harness_notices_name_nothing() {
        let mut items = Items::default();
        let found = read(
            &mut items,
            restarted(
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
            ),
            false,
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_read_that_does_not_reach_back_to_what_was_read_is_partial() {
        let mut items = Items::default();
        read(
            &mut items,
            restarted(vec![text("a", Role::User, "hi")], false),
            false,
        );
        assert_eq!(items.coverage(), TranscriptCoverage::Complete);
        // Restarted, after more changed than the store remembers, but
        // reaching back to an item read before.
        read(
            &mut items,
            restarted(
                vec![text("a", Role::User, "hi"), text("b", Role::User, "more")],
                true,
            ),
            false,
        );
        assert_eq!(items.coverage(), TranscriptCoverage::Complete);
        // Restarted past items never read.
        read(
            &mut items,
            restarted(vec![text("z", Role::User, "later")], true),
            false,
        );
        assert_eq!(items.coverage(), TranscriptCoverage::Partial);

        let mut resumed = Items::default();
        read(
            &mut resumed,
            restarted(vec![text("y", Role::User, "newest")], true),
            false,
        );
        assert_eq!(resumed.coverage(), TranscriptCoverage::Partial);
    }

    #[test]
    fn a_record_that_cannot_be_read_says_so_until_it_can() {
        let mut items = Items::default();
        items.failed("disk I/O error".into());
        assert!(matches!(
            items.coverage(),
            TranscriptCoverage::Unavailable { reason } if reason.contains("disk I/O error")
        ));
        read(&mut items, restarted(Vec::new(), false), false);
        assert_eq!(items.coverage(), TranscriptCoverage::Complete);
    }

    #[test]
    fn what_is_remembered_is_bounded() {
        let mut items = Items::default();
        for index in 0..REMEMBERED + 10 {
            items.remember(&index.to_string());
        }
        assert_eq!(items.seen.len(), REMEMBERED);
        assert!(!items.seen.contains("0"));
    }
}
