//! The Linear tickets cards name: explicit links by issue id, and Linear-style
//! branch names by identifier. A branch name is only a guess (`fix/discard-1117`
//! names no ticket), so its identifier reaches a card once Linear confirms it.
//! Lookups use the user's Linear credentials and run only while someone reads
//! the board, so the daemon never reaches for a keyring unattended.
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, hash_map::Entry},
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
/// How long a ticket Linear answered is shown before it is asked again. A
/// branch's identifier names an issue only in the default workspace, so after
/// switching workspaces, or after a ticket is renamed or deleted, a card
/// catches up within this rather than keeping the old answer for good.
const CONFIRMED_FOR: Duration = Duration::from_secs(60 * 60);

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
            id: crate::work::ticket_from_branch(
                &crate::work::branch_name(branch).to_string_lossy(),
            )?,
            workspace: None,
        })
    }
}

#[derive(Debug, Default)]
pub(super) struct Memory {
    /// References Linear answered without an issue.
    missing: HashMap<String, Instant>,
    /// When Linear last answered each shown ticket. A ticket restored from
    /// the store has no entry, so it is confirmed once after a restart.
    confirmed: HashMap<String, Instant>,
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

    /// Whether a ticket already shown is due to be asked again.
    fn reconfirmable(&self, reference: &Reference) -> bool {
        self.confirmed
            .get(&reference.id)
            .is_none_or(|at| at.elapsed() >= CONFIRMED_FOR)
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
        self.confirmed.retain(|_, at| at.elapsed() < CONFIRMED_FOR);
        self.unavailable.retain(|_, (at, _)| at.elapsed() < RETRY);
    }

    fn set_aside(&mut self, workspace: &Option<String>, error: &crate::linear::Error) {
        self.unavailable
            .insert(workspace.clone(), (Instant::now(), error.to_string()));
    }
}

/// What a round learned: tickets Linear answered, and shown tickets whose
/// reference it now answers without an issue.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Round {
    pub resolved: Vec<(String, Ticket)>,
    pub gone: Vec<String>,
}

/// Looks up a bounded share of `wanted`, by reference id: those no card shows
/// a ticket for first, then shown tickets due to be confirmed again. Each
/// workspace's credential is read once per round, and one failure sets the
/// whole workspace aside, so a locked keyring or an offline Linear costs one
/// attempt an hour rather than one per ticket; a shown ticket stands until
/// Linear answers.
pub(super) fn resolve(
    wanted: &BTreeSet<Reference>,
    shown: &BTreeMap<String, Ticket>,
    memory: &mut Memory,
    cancellation: &CancellationToken,
) -> Round {
    resolve_with(wanted, shown, memory, cancellation, |workspace| {
        Client::from_workspace(workspace)
    })
}

fn resolve_with<C: Lookup>(
    wanted: &BTreeSet<Reference>,
    shown: &BTreeMap<String, Ticket>,
    memory: &mut Memory,
    cancellation: &CancellationToken,
    connect: impl Fn(Option<&str>) -> Result<C, crate::linear::Error>,
) -> Round {
    memory.forget_expired();
    let (known, unknown): (Vec<_>, Vec<_>) = wanted
        .iter()
        .partition(|reference| shown.contains_key(&reference.id));
    let due = unknown
        .into_iter()
        .filter(|reference| memory.askable(reference))
        .chain(
            known
                .into_iter()
                .filter(|reference| memory.reconfirmable(reference)),
        )
        .take(PER_ROUND)
        .collect::<Vec<_>>();
    let started = Instant::now();
    let mut clients = HashMap::new();
    let mut round = Round::default();
    for reference in due {
        if cancellation.is_cancelled() || started.elapsed() >= ROUND_BUDGET {
            break;
        }
        if memory.unavailable.contains_key(&reference.workspace) {
            continue;
        }
        let client = match clients.entry(reference.workspace.clone()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => match connect(reference.workspace.as_deref()) {
                Ok(client) => entry.insert(client),
                Err(error) => {
                    tracing::debug!(%error, "Linear credentials for the Work board are unavailable");
                    memory.set_aside(&reference.workspace, &error);
                    continue;
                }
            },
        };
        match client.issue(&reference.id) {
            Ok(preview) => {
                memory
                    .confirmed
                    .insert(reference.id.clone(), Instant::now());
                round.resolved.push((
                    reference.id.clone(),
                    Ticket {
                        identifier: preview.identifier,
                        title: preview.title,
                        url: preview.url,
                    },
                ));
            }
            // A shown ticket goes only when Linear says it has no such
            // issue, not on any GraphQL error; it is asked again in an hour.
            Err(error)
                if shown.contains_key(&reference.id)
                    && answered_without_issue(&error)
                    && !names_no_issue(&error) =>
            {
                tracing::debug!(%error, "a shown Linear ticket could not be confirmed");
                memory
                    .confirmed
                    .insert(reference.id.clone(), Instant::now());
            }
            Err(error) if answered_without_issue(&error) => {
                memory.missing.insert(reference.id.clone(), Instant::now());
                memory.confirmed.remove(&reference.id);
                if shown.contains_key(&reference.id) {
                    round.gone.push(reference.id.clone());
                }
            }
            Err(error) => {
                tracing::debug!(%error, "a Linear ticket could not be resolved");
                memory.set_aside(&reference.workspace, &error);
            }
        }
    }
    round
}

/// One workspace's issue lookup.
trait Lookup {
    fn issue(&self, reference: &str) -> Result<IssuePreview, crate::linear::Error>;
}

impl Lookup for Client {
    fn issue(&self, reference: &str) -> Result<IssuePreview, crate::linear::Error> {
        IssuePreview::fetch_with(self, reference)
    }
}

/// Linear answered, and the answer was not an issue: an identifier no team
/// has, a deleted issue, or a reference that is not one at all.
fn answered_without_issue(error: &crate::linear::Error) -> bool {
    matches!(
        error.kind,
        "linear_graphql" | "linear_issue_invalid" | "linear_invalid_issue"
    )
}

/// Linear said outright that it has no such issue.
fn names_no_issue(error: &crate::linear::Error) -> bool {
    error.kind == "linear_graphql"
        && error
            .details
            .as_ref()
            .and_then(|details| details["errors"].as_array())
            .is_some_and(|errors| {
                errors.iter().any(|error| {
                    error["message"]
                        .as_str()
                        .is_some_and(|message| message.starts_with("Entity not found"))
                })
            })
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

    /// A workspace's issues by identifier, counting each lookup.
    struct Workspace {
        issues: BTreeMap<String, &'static str>,
        asked: std::cell::RefCell<Vec<String>>,
    }

    impl Lookup for &Workspace {
        fn issue(&self, reference: &str) -> Result<IssuePreview, crate::linear::Error> {
            self.asked.borrow_mut().push(reference.to_owned());
            let organization = self.issues.get(reference).ok_or_else(|| {
                crate::linear::Error::new("linear_graphql", "Linear returned GraphQL errors")
                    .details(
                        serde_json::json!({"errors": [{"message": "Entity not found: Issue"}]}),
                    )
            })?;
            Ok(IssuePreview {
                id: format!("{organization}-{reference}"),
                identifier: reference.to_owned(),
                title: format!("{organization} title"),
                branch_name: String::new(),
                url: format!("https://linear.app/{organization}/issue/{reference}"),
                state: "Todo".into(),
            })
        }
    }

    fn workspace(issues: &[(&str, &'static str)]) -> Workspace {
        Workspace {
            issues: issues
                .iter()
                .map(|(reference, organization)| ((*reference).to_owned(), *organization))
                .collect(),
            asked: Default::default(),
        }
    }

    #[test]
    fn a_shown_ticket_follows_the_default_workspace_it_is_asked_in() {
        let branch = Reference::from_branch("fix/team-123-display").unwrap();
        let wanted = BTreeSet::from([branch.clone()]);
        let mut memory = Memory::default();
        let cancellation = CancellationToken::new();

        let first = workspace(&[("TEAM-123", "first")]);
        let round = resolve_with(
            &wanted,
            &BTreeMap::new(),
            &mut memory,
            &cancellation,
            |_| Ok(&first),
        );
        assert!(round.gone.is_empty());
        let mut shown = round.resolved.into_iter().collect::<BTreeMap<_, _>>();
        assert_eq!(
            shown["TEAM-123"].url,
            "https://linear.app/first/issue/TEAM-123"
        );

        // Just confirmed: shown without asking again.
        let second = workspace(&[("TEAM-123", "second")]);
        let round = resolve_with(&wanted, &shown, &mut memory, &cancellation, |_| Ok(&second));
        assert_eq!(round, Round::default());
        assert!(second.asked.borrow().is_empty());

        // After switching the default workspace, the identifier is asked
        // again once the confirmation lapses, and names the new issue.
        memory
            .confirmed
            .insert("TEAM-123".into(), Instant::now() - CONFIRMED_FOR);
        let round = resolve_with(&wanted, &shown, &mut memory, &cancellation, |_| Ok(&second));
        assert_eq!(round.resolved.len(), 1);
        shown.extend(round.resolved);
        assert_eq!(
            shown["TEAM-123"].url,
            "https://linear.app/second/issue/TEAM-123"
        );

        // A workspace without the issue takes the card's ticket away.
        memory
            .confirmed
            .insert("TEAM-123".into(), Instant::now() - CONFIRMED_FOR);
        struct Failing;
        impl Lookup for Failing {
            fn issue(&self, _: &str) -> Result<IssuePreview, crate::linear::Error> {
                Err(crate::linear::Error::new(
                    "linear_graphql",
                    "internal error",
                ))
            }
        }
        let round = resolve_with(&wanted, &shown, &mut memory, &cancellation, |_| Ok(Failing));
        assert_eq!(
            round,
            Round::default(),
            "an unclear answer keeps the ticket"
        );
        memory
            .confirmed
            .insert("TEAM-123".into(), Instant::now() - CONFIRMED_FOR);
        let empty = workspace(&[]);
        let round = resolve_with(&wanted, &shown, &mut memory, &cancellation, |_| Ok(&empty));
        assert_eq!(
            round,
            Round {
                resolved: Vec::new(),
                gone: vec!["TEAM-123".into()],
            }
        );
    }

    #[test]
    fn restored_tickets_are_confirmed_after_new_ones_and_stand_while_linear_is_away() {
        let mut memory = Memory::default();
        let cancellation = CancellationToken::new();
        let ticket = Ticket {
            identifier: "OLD-0".into(),
            title: "Restored".into(),
            url: "https://linear.app/old/issue/OLD-0".into(),
        };
        // A store restored more tickets than one round asks about.
        let shown = (0..PER_ROUND)
            .map(|number| (format!("OLD-{number}"), ticket.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut wanted = shown
            .keys()
            .map(|id| Reference {
                id: id.clone(),
                workspace: None,
            })
            .collect::<BTreeSet<_>>();
        wanted.insert(Reference {
            id: "ZED-1".into(),
            workspace: None,
        });
        let linear = workspace(&[("ZED-1", "new")]);
        resolve_with(&wanted, &shown, &mut memory, &cancellation, |_| Ok(&linear));
        let asked = linear.asked.borrow();
        assert_eq!(asked.len(), PER_ROUND);
        assert_eq!(asked[0], "ZED-1", "an unshown ticket is asked first");

        let mut memory = Memory::default();
        let round = resolve_with(&wanted, &shown, &mut memory, &cancellation, |_| {
            Err::<&Workspace, _>(crate::linear::Error::new("linear_auth_missing", "no key"))
        });
        assert_eq!(
            round,
            Round::default(),
            "a shown ticket stands without Linear"
        );
    }
}
