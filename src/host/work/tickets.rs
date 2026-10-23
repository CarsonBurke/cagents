//! The Linear tickets cards name: explicit links by issue id, and Linear-style
//! branch names by identifier. A branch name is only a guess (`fix/discard-1117`
//! names no ticket), so its identifier reaches a card once Linear confirms it.
//! Lookups use the user's Linear credentials and run only while someone reads
//! the board, so the daemon never reaches for a keyring unattended.
use std::{
    collections::{BTreeSet, HashMap, hash_map::Entry},
    time::{Duration, Instant},
};

use super::store::Ticket;
use crate::{
    linear::{IssuePreview, api::Client},
    repository::CancellationToken,
};

/// Lookups per round, and the time a round may take, so a slow Linear holds
/// the forge thread for one poll at most.
const PER_ROUND: usize = 8;
const ROUND_BUDGET: Duration = Duration::from_secs(20);
/// When a workspace whose credentials or connection failed is tried again.
const RETRY: Duration = Duration::from_secs(60 * 60);
/// How long a reference Linear answered without an issue stays unasked.
const MISSING_FOR: Duration = Duration::from_secs(24 * 60 * 60);

/// An issue to look up: a linked issue's id or a branch name's identifier,
/// in the workspace whose credentials can read it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Reference {
    pub id: String,
    pub workspace: Option<String>,
}

impl Reference {
    pub(super) fn linked(link: &crate::linear::context::LinkedIssue) -> Self {
        Self {
            id: link.issue_id.clone(),
            workspace: link.workspace.clone(),
        }
    }

    /// The identifier a Linear-style branch name carries, looked up in the
    /// default workspace.
    pub(super) fn from_branch(branch: &str) -> Option<Self> {
        Some(Self {
            id: crate::work::ticket_from_branch(branch)?,
            workspace: None,
        })
    }
}

#[derive(Debug, Default)]
pub(super) struct Memory {
    /// References Linear answered without an issue.
    missing: HashMap<String, Instant>,
    /// Workspaces whose credentials or connection failed, and why.
    unavailable: HashMap<Option<String>, (Instant, String)>,
}

impl Memory {
    /// Whether looking `reference` up now could tell anything new.
    pub(super) fn askable(&self, reference: &Reference) -> bool {
        self.missing
            .get(&reference.id)
            .is_none_or(|at| at.elapsed() >= MISSING_FOR)
            && self
                .unavailable
                .get(&reference.workspace)
                .is_none_or(|(at, _)| at.elapsed() >= RETRY)
    }

    /// Why the workspaces some of `wanted` need cannot be asked, until a
    /// retry reaches them.
    pub(super) fn unavailable_for(&self, wanted: &BTreeSet<Reference>) -> Option<String> {
        let reasons = wanted
            .iter()
            .map(|reference| &reference.workspace)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter_map(|workspace| {
                let (_, reason) = self.unavailable.get(workspace)?;
                Some(match workspace {
                    Some(workspace) => format!("workspace {workspace}: {reason}"),
                    None => reason.clone(),
                })
            })
            .collect::<Vec<_>>();
        (!reasons.is_empty()).then(|| reasons.join("; "))
    }

    fn forget_expired(&mut self) {
        self.missing.retain(|_, at| at.elapsed() < MISSING_FOR);
        self.unavailable.retain(|_, (at, _)| at.elapsed() < RETRY);
    }

    fn set_aside(&mut self, workspace: &Option<String>, error: &crate::linear::Error) {
        self.unavailable
            .insert(workspace.clone(), (Instant::now(), error.to_string()));
    }
}

/// Looks up a bounded share of `wanted`, by reference id. Each workspace's
/// credential is read once per round, and one failure sets the whole
/// workspace aside, so a locked keyring or an offline Linear costs one attempt
/// an hour rather than one per ticket.
pub(super) fn resolve(
    wanted: &BTreeSet<Reference>,
    memory: &mut Memory,
    cancellation: &CancellationToken,
) -> Vec<(String, Ticket)> {
    memory.forget_expired();
    let due = wanted
        .iter()
        .filter(|reference| memory.askable(reference))
        .take(PER_ROUND)
        .collect::<Vec<_>>();
    let started = Instant::now();
    let mut clients = HashMap::new();
    let mut resolved = Vec::new();
    for reference in due {
        if cancellation.is_cancelled() || started.elapsed() >= ROUND_BUDGET {
            break;
        }
        if memory.unavailable.contains_key(&reference.workspace) {
            continue;
        }
        let client = match clients.entry(reference.workspace.clone()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => match Client::from_workspace(reference.workspace.as_deref()) {
                Ok(client) => entry.insert(client),
                Err(error) => {
                    tracing::debug!(%error, "Linear credentials for the Work board are unavailable");
                    memory.set_aside(&reference.workspace, &error);
                    continue;
                }
            },
        };
        match IssuePreview::fetch_with(client, &reference.id) {
            Ok(preview) => resolved.push((
                reference.id.clone(),
                Ticket {
                    identifier: preview.identifier,
                    title: preview.title,
                    url: preview.url,
                },
            )),
            Err(error) if answered_without_issue(&error) => {
                memory.missing.insert(reference.id.clone(), Instant::now());
            }
            Err(error) => {
                tracing::debug!(%error, "a Linear ticket could not be resolved");
                memory.set_aside(&reference.workspace, &error);
            }
        }
    }
    resolved
}

/// Linear answered, and the answer was not an issue: an identifier no team
/// has, a deleted issue, or a reference that is not one at all.
fn answered_without_issue(error: &crate::linear::Error) -> bool {
    matches!(
        error.kind,
        "linear_graphql" | "linear_issue_invalid" | "linear_invalid_issue"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_references_carry_the_identifier_in_the_default_workspace() {
        assert_eq!(
            Reference::from_branch("fix/team-123-display"),
            Some(Reference {
                id: "TEAM-123".into(),
                workspace: None,
            })
        );
        assert_eq!(Reference::from_branch("browser-20261019"), None);
    }

    #[test]
    fn missing_references_and_unavailable_workspaces_are_not_asked_again() {
        let mut memory = Memory::default();
        let guess = Reference::from_branch("test/discard-1117-x").unwrap();
        let linked = Reference {
            id: "uuid".into(),
            workspace: Some("WORK".into()),
        };
        assert!(memory.askable(&guess) && memory.askable(&linked));
        memory.missing.insert(guess.id.clone(), Instant::now());
        memory.set_aside(
            &Some("WORK".into()),
            &crate::linear::Error::new("linear_auth_missing", "no key"),
        );
        assert!(!memory.askable(&guess));
        assert!(!memory.askable(&linked));
        let other = Reference {
            id: "other".into(),
            workspace: None,
        };
        assert!(memory.askable(&other));
        // Only the workspaces still wanted explain missing tickets.
        assert_eq!(
            memory.unavailable_for(&BTreeSet::from([other.clone(), linked.clone()])),
            Some("workspace WORK: no key".into())
        );
        assert_eq!(memory.unavailable_for(&BTreeSet::from([other])), None);
        // Expired entries are asked again without waiting for a pruning
        // round, which runs only once something is due.
        let long_ago = Instant::now() - MISSING_FOR;
        memory.missing.insert(guess.id.clone(), long_ago);
        memory.unavailable.get_mut(&Some("WORK".into())).unwrap().0 = long_ago;
        assert!(memory.askable(&guess) && memory.askable(&linked));
        assert!(answered_without_issue(&crate::linear::Error::new(
            "linear_graphql",
            "Entity not found"
        )));
        assert!(!answered_without_issue(&crate::linear::Error::new(
            "linear_transport",
            "offline"
        )));
    }
}
