//! Serial UI scheduling for preserved managed-home hints.
//!
//! Cached account data can be shown before this queue runs. A candidate is
//! metadata only: neither that cache nor a queued row establishes a login.
//! The daemon reports the result of each explicit recovery operation.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::{AtomicU64, Ordering},
};

use uuid::Uuid;

use cosmic::{Element, app::Task, iced::Length, widget};

use crate::{
    account::recovery::Candidate,
    agent::Provider,
    design::{self, space},
};

use super::{
    Action, Cagents, Message, Registry, Settled, TransientNotice, now_ms, remote_lifecycle,
};

#[cfg(test)]
mod callback_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
use crate::account::{Identity, UsageSnapshot};

/// Registry callbacks belong to one app/device state and registry generation.
/// This gate orders reads; it does not authorize installing a lower registry
/// revision. That requires the current opaque queue attempt's checked
/// lifecycle result, while ordinary registry updates remain monotonic.
#[derive(Debug)]
pub(crate) struct RegistryCallbacks {
    owner: Uuid,
    issued: AtomicU64,
    accepted: u64,
}

/// Carry unchanged with the registry read that issued it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReadToken {
    owner: Uuid,
    serial: u64,
    minimum_revision: u64,
}

impl ReadToken {
    pub(crate) fn minimum_revision(&self) -> u64 {
        self.minimum_revision
    }
}

impl Default for RegistryCallbacks {
    fn default() -> Self {
        Self {
            owner: Uuid::new_v4(),
            issued: AtomicU64::new(0),
            accepted: 0,
        }
    }
}

impl RegistryCallbacks {
    /// Read issuance also works from the shared reload-registry path.
    pub(crate) fn issue_read_at(&self, minimum_revision: u64) -> ReadToken {
        let serial = self
            .issued
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |serial| {
                serial.checked_add(1)
            })
            .expect("Registry read sequence exhausted")
            + 1;
        ReadToken {
            owner: self.owner,
            serial,
            minimum_revision,
        }
    }

    #[cfg(test)]
    pub(crate) fn issue_read(&self) -> ReadToken {
        self.issue_read_at(0)
    }

    /// Issuing a newer read does not reject an earlier result until the
    /// newer result has actually been accepted.
    pub(crate) fn accept_read(&mut self, token: ReadToken) -> bool {
        if !self.accept_owner(token.owner) || token.serial < self.accepted {
            return false;
        }
        self.accepted = token.serial;
        true
    }

    /// Capture separately for callbacks that may write registry UI state.
    pub(crate) fn capture_owner(&self) -> Uuid {
        self.owner
    }

    pub(crate) fn accept_owner(&self, owner: Uuid) -> bool {
        self.owner == owner
    }

    /// Call only after the current opaque recovery attempt accepts its
    /// checked lifecycle result, before installing its lower registry
    /// revision. Old reads and writes must not undo that accepted recovery.
    pub(crate) fn reset_after_checked_recovery(&mut self) {
        self.owner = Uuid::new_v4();
        self.issued = AtomicU64::new(0);
        self.accepted = 0;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Pending,
    Checking,
    Restored,
    Busy,
    NoLogin,
    Failed(String),
}

impl Status {
    fn retryable(&self) -> bool {
        matches!(self, Self::Busy | Self::NoLogin | Self::Failed(_))
    }
}

/// Terminal results only; accepting a cached identity is not an operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Restored,
    Busy,
    NoLogin,
    Failed(String),
}

impl From<Outcome> for Status {
    fn from(outcome: Outcome) -> Self {
        match outcome {
            Outcome::Restored => Self::Restored,
            Outcome::Busy => Self::Busy,
            Outcome::NoLogin => Self::NoLogin,
            Outcome::Failed(error) => Self::Failed(error),
        }
    }
}

#[derive(Debug)]
pub(crate) struct Entry {
    candidate: Candidate,
    status: Status,
    attempt: Option<u64>,
    details_open: bool,
}

impl Entry {
    pub(crate) fn candidate(&self) -> &Candidate {
        &self.candidate
    }

    pub(crate) fn status(&self) -> &Status {
        &self.status
    }
}

/// Carry this token unchanged through the asynchronous daemon request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Attempt {
    owner: Uuid,
    serial: u64,
    candidate: Candidate,
}

impl Attempt {
    pub(crate) fn candidate(&self) -> &Candidate {
        &self.candidate
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Finish {
    /// The current row accepted this completion. Apply its associated data.
    Applied,
    /// The exact operation ended, but its row was removed or replaced.
    Retired,
    /// A different owner, attempt or candidate owns the current operation.
    Ignored,
}

#[derive(Debug)]
pub(crate) struct Queue {
    owner: Uuid,
    serial: u64,
    entries: Vec<Entry>,
    // Keep a removed row's operation here until its actual callback arrives;
    // replacing metadata must never admit a second concurrent recovery.
    active: Option<Attempt>,
}

impl Default for Queue {
    fn default() -> Self {
        Self {
            owner: Uuid::new_v4(),
            serial: 0,
            entries: Vec::new(),
            active: None,
        }
    }
}

impl Queue {
    /// Capture this with discovery. A fresh app/device state gets a new owner.
    pub(crate) fn owner(&self) -> Uuid {
        self.owner
    }

    pub(crate) fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub(crate) fn status(&self, candidate: &Candidate) -> Option<&Status> {
        self.entries
            .iter()
            .find(|entry| entry.candidate == *candidate)
            .map(Entry::status)
    }

    /// Replace a successful discovery snapshot, preserving unchanged rows.
    /// Failed discovery must leave the last successful snapshot alone.
    /// Foreign-device completions do not mutate this queue.
    pub(crate) fn replace_candidates(&mut self, owner: Uuid, candidates: Vec<Candidate>) -> bool {
        if owner != self.owner {
            return false;
        }
        let mut previous: BTreeMap<_, _> = std::mem::take(&mut self.entries)
            .into_iter()
            .map(|entry| ((entry.candidate.provider, entry.candidate.key), entry))
            .collect();
        let mut seen = BTreeSet::new();
        self.entries = candidates
            .into_iter()
            .filter_map(|candidate| {
                let key = (candidate.provider, candidate.key);
                if !seen.insert(key) {
                    return None;
                }
                Some(previous.remove(&key).unwrap_or(Entry {
                    candidate,
                    status: Status::Pending,
                    attempt: None,
                    details_open: false,
                }))
            })
            .collect();
        true
    }

    /// Start at most one operation. Already-running ordinary quota probes
    /// block their provider; their completion should pump this queue again.
    pub(crate) fn next_pending(
        &mut self,
        blocked_providers: &BTreeSet<Provider>,
    ) -> Option<Attempt> {
        if self.active.is_some() {
            return None;
        }
        let entry = self.entries.iter_mut().find(|entry| {
            entry.status == Status::Pending
                && !blocked_providers.contains(&entry.candidate.provider)
        })?;
        let serial = self.serial.checked_add(1)?;
        self.serial = serial;
        let attempt = Attempt {
            owner: self.owner,
            serial,
            candidate: entry.candidate.clone(),
        };
        entry.status = Status::Checking;
        entry.attempt = Some(serial);
        self.active = Some(attempt.clone());
        Some(attempt)
    }

    /// Check the token before applying registry changes, notices or results.
    /// A matching detached operation releases serialization, but cannot
    /// update a newly discovered row with the same candidate key.
    pub(crate) fn finish(&mut self, attempt: &Attempt, outcome: Outcome) -> Finish {
        if self.active.as_ref() != Some(attempt) {
            return Finish::Ignored;
        }
        self.active = None;
        let Some(entry) = self.entries.iter_mut().find(|entry| {
            entry.candidate == attempt.candidate
                && entry.status == Status::Checking
                && entry.attempt == Some(attempt.serial)
        }) else {
            return Finish::Retired;
        };
        entry.status = outcome.into();
        entry.attempt = None;
        Finish::Applied
    }

    /// Shared by the controls and their action handler, including when a
    /// removed row still owns an operation that has not returned.
    pub(crate) fn can_retry(&self, candidate: &Candidate) -> bool {
        self.active.is_none() && self.status(candidate).is_some_and(Status::retryable)
    }

    /// Only an explicit action retries a terminal failure.
    pub(crate) fn retry(&mut self, candidate: &Candidate) -> bool {
        if !self.can_retry(candidate) {
            return false;
        }
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.candidate == *candidate && entry.status.retryable())
        else {
            return false;
        };
        entry.status = Status::Pending;
        true
    }

    /// Check in the common quota-probe path, including initial focus/ticks.
    pub(crate) fn suppresses(&self, provider: Provider) -> bool {
        self.active
            .as_ref()
            .is_some_and(|attempt| attempt.candidate.provider == provider)
            || self.entries.iter().any(|entry| {
                entry.candidate.provider == provider
                    && matches!(entry.status, Status::Pending | Status::Checking)
            })
    }
}

/// The daemon operation and the UI registry generation that requested it.
/// Neither a candidate key nor a registry read token can replace this attempt.
#[derive(Clone, Debug)]
pub(crate) struct Request {
    attempt: Attempt,
    callback_owner: Uuid,
    reset_floor: Option<u64>,
}

impl Request {
    pub(crate) fn candidate(&self) -> &Candidate {
        self.attempt.candidate()
    }

    fn capture(attempt: Attempt, callbacks: &RegistryCallbacks, reset_floor: Option<u64>) -> Self {
        Self {
            attempt,
            callback_owner: callbacks.capture_owner(),
            reset_floor,
        }
    }
}

struct CheckedRestoration {
    registry: Registry,
}

fn contains_original_home(registry: &Registry, candidate: &Candidate) -> bool {
    !registry.is_newer()
        && registry
            .find(candidate.key)
            .is_some_and(|row| row.provider == candidate.provider && row.linked_home.is_none())
}

fn checked_restoration(
    candidate: &Candidate,
    result: Result<Settled, String>,
) -> Result<CheckedRestoration, String> {
    match result? {
        Settled::Registered { registry, .. } if contains_original_home(&registry, candidate) => {
            Ok(CheckedRestoration { registry })
        }
        _ => Err(
            "The device did not confirm the original preserved account. Its files were kept; try restoring it again."
                .into(),
        ),
    }
}

struct RegistryPlan {
    registry: Registry,
    reset: bool,
    /// Discovery may have removed this hint after observing registration.
    accepts_retired: bool,
    problem: Option<String>,
}

/// A scan at or beyond the checked response owns later registry choices.
/// It can confirm a retired hint, but a removal or relink must not be undone.
fn plan_registry(
    candidate: &Candidate,
    checked: Registry,
    cached: &Registry,
    latest_scan: Option<&Registry>,
    reset_floor: Option<u64>,
) -> Result<RegistryPlan, String> {
    let confirming_scan = latest_scan.filter(|scan| scan.revision() >= checked.revision());
    let proof_current = reset_floor.is_some_and(|floor| floor >= cached.revision());
    if confirming_scan.is_some_and(|scan| !contains_original_home(scan, candidate)) {
        let problem = "The account list changed after restoration was checked. The device's later account choices were kept; reload Accounts before trying again.".to_owned();
        if proof_current {
            let registry = confirming_scan.unwrap().clone();
            return Ok(RegistryPlan {
                reset: registry.revision() < cached.revision(),
                registry,
                accepts_retired: true,
                problem: Some(problem),
            });
        }
        return Err(problem);
    }
    let registry = confirming_scan.cloned().unwrap_or(checked);
    let reset = registry.revision() < cached.revision();
    if reset && reset_floor.is_none_or(|floor| floor < cached.revision()) {
        return Err("The account list changed while restoration was checked. Reload Accounts and try again; its newer state was kept.".into());
    }
    Ok(RegistryPlan {
        registry,
        reset,
        accepts_retired: confirming_scan.is_some(),
        problem: None,
    })
}

fn preserved_name(entries: &[Entry], index: usize) -> String {
    let provider = entries[index].candidate.provider;
    let count = entries
        .iter()
        .filter(|entry| entry.candidate.provider == provider)
        .count();
    if count > 1 {
        let ordinal = entries[..=index]
            .iter()
            .filter(|entry| entry.candidate.provider == provider)
            .count();
        format!("Preserved {} account {ordinal}", provider.label())
    } else {
        format!("Preserved {} account", provider.label())
    }
}

impl Cagents {
    /// Called only with a discovery and registry scan already accepted by
    /// the registry callback gate. Cached rows stay visible while work runs.
    pub(super) fn install_preserved_discovery(
        &mut self,
        discovery: crate::account::recovery::Discovery,
        registry: Registry,
    ) -> Task<Message> {
        self.account_ui.last_registry_scan = Some(registry);
        let changed_problem = self.account_ui.recovery_problem != discovery.problem;
        self.account_ui.recovery_problem = discovery.problem;
        if let Some(problem) = &self.account_ui.recovery_problem {
            if changed_problem {
                self.status = Some(
                    TransientNotice::warning("Preserved accounts could not be checked.")
                        .with_details(problem.clone()),
                );
            }
            // A failed scan is not evidence that previously found homes went
            // away. Keep their results and let daemon admission revalidate.
        } else {
            let owner = self.account_ui.recovery.owner();
            self.account_ui
                .recovery
                .replace_candidates(owner, discovery.candidates);
            if self
                .account_ui
                .registry_reset_floor
                .is_some_and(|floor| floor >= self.account_ui.registry.revision())
            {
                // A completed hint can outlive a failed refresh. A later
                // proven registry loss makes it explicitly restorable again,
                // without repeating automatic recovery or trusting an old read.
                for entry in &mut self.account_ui.recovery.entries {
                    if entry.status == Status::Restored {
                        entry.status = Status::Failed(
                            "The registration is missing again. Your saved files were kept. Choose Restore to check this account again.".into(),
                        );
                    }
                }
            }
        }
        self.pump_account_recovery()
    }

    /// Ordinary probe completions and accepted discovery/completion events
    /// advance this queue. Focus/ticks never turn a failure back into Pending.
    pub(super) fn pump_account_recovery(&mut self) -> Task<Message> {
        let now = now_ms();
        self.account_ui.now_ms = now;
        let mut blocked: BTreeSet<_> = self
            .account_ui
            .accounts
            .iter()
            .filter(|account| account.schedule.in_flight(now))
            .map(|account| account.account.provider)
            .collect();
        // A provisional desktop PATH is not an authoritative absence. Wait
        // until shell-path discovery supplies either true or false.
        for entry in self.account_ui.recovery.entries() {
            if self.account_ui.recovery_cli_pending
                || !self
                    .account_ui
                    .installed
                    .contains_key(&entry.candidate.provider)
            {
                blocked.insert(entry.candidate.provider);
            }
        }
        loop {
            let Some(attempt) = self.account_ui.recovery.next_pending(&blocked) else {
                return Task::none();
            };
            let candidate = attempt.candidate().clone();
            let unavailable = if self
                .account_ui
                .last_registry_scan
                .as_ref()
                .is_some_and(Registry::is_newer)
            {
                Some("The accounts were saved by a newer cagents. Update cagents before restoring this preserved account.".to_owned())
            } else if self.account_ui.installed.get(&candidate.provider) == Some(&false) {
                Some(format!(
                    "The {} CLI is not installed on this device. Install it, then try restoring this preserved account again. Its files were kept.",
                    candidate.provider.label()
                ))
            } else {
                None
            };
            if let Some(error) = unavailable {
                if self
                    .account_ui
                    .recovery
                    .finish(&attempt, Outcome::Failed(error.clone()))
                    == Finish::Applied
                {
                    self.status = Some(
                        TransientNotice::warning(format!(
                            "The preserved {} account could not be restored.",
                            candidate.provider.label()
                        ))
                        .with_details(error),
                    );
                }
                continue;
            }
            let request = Request::capture(
                attempt,
                &self.account_ui.registry_callbacks,
                self.account_ui.registry_reset_floor,
            );
            return cosmic::task::future(async move {
                // The same daemon-owned User lifecycle frontdoor is used for
                // local and remote devices. Never probe or write Store here.
                let result = tokio::task::spawn_blocking(move || {
                    remote_lifecycle::<Settled>(crate::account::remote::Operation::Register {
                        provider: candidate.provider,
                        key: candidate.key,
                    })
                })
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
                Message::AccountUi(Action::PreservedRestored(request, result))
            });
        }
    }

    pub(super) fn restore_preserved(&mut self, candidate: Candidate) -> Task<Message> {
        match self.account_ui.recovery.status(&candidate) {
            Some(Status::Pending) => self.pump_account_recovery(),
            Some(Status::Busy | Status::NoLogin | Status::Failed(_)) => {
                if self.account_ui.recovery.retry(&candidate) {
                    self.pump_account_recovery()
                } else {
                    Task::none()
                }
            }
            _ => Task::none(),
        }
    }

    pub(super) fn preserved_restored(
        &mut self,
        request: Request,
        result: Result<Settled, String>,
    ) -> Task<Message> {
        if !self
            .account_ui
            .registry_callbacks
            .accept_owner(request.callback_owner)
        {
            // Retire only the operation that actually ended. No registry,
            // identity, snapshot or notice from the old generation applies.
            self.account_ui.recovery.finish(
                &request.attempt,
                Outcome::Failed(
                    "The account list changed while restoration was checked. Try again.".into(),
                ),
            );
            return self.pump_account_recovery();
        }
        let candidate = request.candidate().clone();
        let (planned, reload_on_failure) = match checked_restoration(&candidate, result) {
            Ok(checked) => {
                let plan = plan_registry(
                    &candidate,
                    checked.registry,
                    &self.account_ui.registry,
                    self.account_ui.last_registry_scan.as_ref(),
                    request
                        .reset_floor
                        .max(self.account_ui.registry_reset_floor),
                );
                let reload = plan.is_err();
                (plan, reload)
            }
            Err(error) => (Err(error), false),
        };
        let outcome = match &planned {
            Ok(plan) => plan.problem.as_ref().map_or(Outcome::Restored, |problem| {
                Outcome::Failed(problem.clone())
            }),
            Err(error) => Outcome::Failed(error.clone()),
        };
        let finish = self.account_ui.recovery.finish(&request.attempt, outcome);
        match finish {
            Finish::Ignored => return Task::none(),
            Finish::Retired if !planned.as_ref().is_ok_and(|plan| plan.accepts_retired) => {
                let reload = if reload_on_failure {
                    self.reload_registry()
                } else {
                    Task::none()
                };
                return Task::batch([reload, self.pump_account_recovery()]);
            }
            Finish::Retired => {}
            Finish::Applied => {}
        }
        let plan = match planned {
            Ok(plan) => plan,
            Err(error) => {
                self.status = Some(
                    TransientNotice::warning(format!(
                        "The preserved {} account could not be restored. Its files were kept.",
                        candidate.provider.label()
                    ))
                    .with_details(error),
                );
                let reload = if reload_on_failure {
                    self.reload_registry()
                } else {
                    Task::none()
                };
                return Task::batch([reload, self.pump_account_recovery()]);
            }
        };
        self.account_ui.last_registry_scan = Some(plan.registry.clone());
        if plan.reset {
            // Only the current attempt's checked lifecycle response can
            // authorize this replacement. Ordinary reads remain monotonic.
            self.account_ui
                .registry_callbacks
                .reset_after_checked_recovery();
            self.account_ui.registry_reset_floor = None;
            self.account_ui.registry = plan.registry;
            self.account_ui.sync_accounts();
        } else {
            self.account_ui.set_registry(plan.registry);
        }
        // Registry confirmation never overrides merged discovery authority or
        // newer login/usage observations. Fresh metadata and probes own those.
        self.refresh_chat_drafts();
        self.status = Some(if let Some(problem) = plan.problem {
            TransientNotice::warning("The preserved account changed before restoration finished.")
                .with_details(problem)
        } else {
            TransientNotice::success(if self.account_ui.registry.is_archived(candidate.key) {
                format!(
                    "The preserved {} account was restored for saved conversations. Its matching login remains listed. Open a saved conversation and choose Resume.",
                    candidate.provider.label()
                )
            } else {
                format!(
                    "The preserved {} account was restored. Open a saved conversation and choose Resume.",
                    candidate.provider.label()
                )
            })
        });
        // Restoration updates account readiness only. It never resumes a
        // chat, changes a recipe or starts a conversation on its callback.
        let remote = crate::device::is_remote();
        let metadata = if remote {
            Task::none()
        } else {
            self.locate_accounts()
        };
        let reload = self.reload_registry();
        Task::batch([metadata, reload, self.pump_account_recovery()])
    }

    pub(super) fn toggle_preserved_details(&mut self, candidate: Candidate) -> Task<Message> {
        if let Some(entry) = self
            .account_ui
            .recovery
            .entries
            .iter_mut()
            .find(|entry| entry.candidate == candidate)
        {
            entry.details_open = !entry.details_open;
        }
        Task::none()
    }

    pub(super) fn preserved_accounts_panel(&self) -> Option<Element<'_, Message>> {
        let recovery = &self.account_ui.recovery;
        if recovery.entries().is_empty() && self.account_ui.recovery_problem.is_none() {
            return None;
        }
        let mut rows: Vec<Element<'_, Message>> = vec![
            design::heading("Preserved accounts").into(),
            design::caption("Restore saved account homes, then resume their conversations.").into(),
        ];
        if self.account_ui.recovery_problem.is_some() {
            rows.push(
                widget::row::with_children(vec![
                    design::caption("Some preserved accounts could not be checked.")
                        .width(Length::Fill)
                        .into(),
                    crate::ui_controls::button::quiet("Notices")
                        .name("Show application notices about preserved accounts")
                        .on_press(Message::ChromeUi(
                            super::super::chrome_ui::Action::ToggleNotices,
                        ))
                        .into(),
                ])
                .spacing(space::S)
                .into(),
            );
        }
        for (index, entry) in recovery.entries().iter().enumerate() {
            let candidate = entry.candidate();
            let name = preserved_name(recovery.entries(), index);
            let caption = match entry.status() {
                Status::Pending
                    if self.account_ui.recovery_cli_pending
                        || !self.account_ui.installed.contains_key(&candidate.provider) =>
                {
                    "Locating the provider CLI…"
                }
                Status::Pending => "Waiting for the current account check…",
                Status::Checking => "Checking the saved login…",
                Status::Restored if self.account_ui.registry.is_archived(candidate.key) => {
                    "Restored for saved conversations. Its matching login remains listed."
                }
                Status::Restored => "Restored. Open a saved conversation and choose Resume.",
                Status::Busy => "This account is in use. Try again when it is free.",
                Status::NoLogin => "No login was confirmed. Your saved files were kept.",
                Status::Failed(_) => "Could not restore this account. Your saved files were kept.",
            };
            let mut line: Vec<Element<'_, Message>> = vec![
                widget::column::with_children(vec![
                    design::body(name.clone()).into(),
                    design::caption(caption).into(),
                ])
                .spacing(space::XXS)
                .width(Length::Fill)
                .into(),
            ];
            if entry.status() == &Status::Checking {
                line.push(crate::ui_controls::activity_spinner(
                    f32::from(design::ROW_ICON_SIZE),
                    design::accent_color,
                ));
            } else if entry.status() == &Status::Pending || entry.status().retryable() {
                let label = if entry.status().retryable() {
                    "Retry"
                } else {
                    "Restore"
                };
                let ready = if entry.status().retryable() {
                    recovery.can_retry(candidate)
                } else {
                    recovery.active.is_none()
                        && self.account_ui.installed.contains_key(&candidate.provider)
                        && !self.account_ui.recovery_cli_pending
                        && !self.account_ui.accounts.iter().any(|account| {
                            account.account.provider == candidate.provider
                                && account.schedule.in_flight(self.account_ui.now_ms)
                        })
                };
                line.push(
                    crate::ui_controls::button::secondary(label)
                        .name(format!("{label} {name}"))
                        .on_press_maybe(ready.then(|| {
                            Message::AccountUi(Action::RestorePreserved(candidate.clone()))
                        }))
                        .into(),
                );
            }
            let disclosure = if entry.details_open { "Hide" } else { "Show" };
            line.push(
                crate::ui_controls::button::quiet(if entry.details_open {
                    "Hide details"
                } else {
                    "Details"
                })
                .name(format!("{disclosure} technical details for {name}"))
                .on_press(Message::AccountUi(Action::PreservedDetails(
                    candidate.clone(),
                )))
                .into(),
            );
            rows.push(widget::row::with_children(line).spacing(space::S).into());
            if entry.details_open {
                rows.push(
                    widget::selectable_text(format!("Original account key: {}", candidate.key))
                        .size(design::TEXT_CAPTION)
                        .width(Length::Fill)
                        .wrapping(cosmic::iced::widget::text::Wrapping::WordOrGlyph)
                        .into(),
                );
                if let Status::Failed(error) = entry.status() {
                    rows.push(
                        widget::selectable_text(error.as_str())
                            .size(design::TEXT_CAPTION)
                            .width(Length::Fill)
                            .wrapping(cosmic::iced::widget::text::Wrapping::WordOrGlyph)
                            .into(),
                    );
                }
            }
        }
        Some(
            widget::container(
                widget::scrollable(widget::column::with_children(rows).spacing(space::M))
                    .id(widget::Id::new("preserved-accounts"))
                    .direction(cosmic::iced::widget::scrollable::Direction::Vertical(
                        design::scrollbar(),
                    ))
                    .height(Length::Shrink),
            )
            .max_height((self.window_height * 0.4).clamp(120.0, 280.0))
            .padding(space::M)
            .class(design::surface(
                Some(design::raised),
                [design::RADIUS_M; 4],
                true,
            ))
            .into(),
        )
    }
}
