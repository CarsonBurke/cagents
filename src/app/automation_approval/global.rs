//! Candidate UI-only extraction. Requires A281's separately allocated backend.
//! Discovery never supplies consent or replaces the current modal owner.
use super::*;
use crate::automations::{PROPOSAL_PAGE_SIZE, ProposalDelivery, ProposalSummary};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    identity: uuid::Uuid,
    sequence: u64,
    epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Read {
    Delivery,
    Page {
        key: Key,
        after: Option<uuid::Uuid>,
        start: Option<u64>,
    },
    Open {
        key: Key,
        id: uuid::Uuid,
        revision: u64,
        generation: u64,
        focused: Option<CardId>,
        destination: Option<WorkspaceDestination>,
    },
    Review {
        key: Key,
        id: uuid::Uuid,
        revision: u64,
        generation: u64,
    },
}

#[derive(Debug, Clone)]
pub enum Action {
    Toggle,
    Refresh,
    Page {
        key: Key,
        after: Option<uuid::Uuid>,
        // Capture the position with the rendered cursor. A delayed action may
        // outlive the page which established this rank in the same snapshot.
        start: Option<u64>,
    },
    Open {
        key: Key,
        id: uuid::Uuid,
        revision: u64,
    },
    Loaded {
        nonce: u64,
        epoch: u64,
        read: Read,
        result: Arc<Result<Response, String>>,
    },
}

#[derive(Default)]
pub(super) struct State {
    epoch: u64,
    nonce: u64,
    exhausted: bool,
    connected: bool,
    trigger: Option<(uuid::Uuid, u64)>,
    key: Option<Key>,
    total: Option<u64>,
    pending: Vec<ProposalSummary>,
    after: Option<uuid::Uuid>,
    // Presentation position within the exact delivery snapshot. Arbitrary
    // cursors remain usable without inventing a position for them.
    page_end: Option<u64>,
    expanded: bool,
    error: Option<String>,
    dirty: bool,
    // Retained across reconnect: an invalid result must not free its physical
    // blocking reader early. Matching completion alone retires this slot.
    flight: Option<(u64, u64, Read)>,
    wanted: Option<Read>,
    authorized: Option<(Key, uuid::Uuid, u64)>,
    latest_open: Option<(Key, uuid::Uuid, u64, u64)>,
    reviewed_identity: Option<uuid::Uuid>,
    // One explicit notice activation may precede the first model/delivery.
    // It conveys no registry or consent authority until the scoped full read.
    initial_open: Option<(
        uuid::Uuid,
        u64,
        Option<CardId>,
        Option<WorkspaceDestination>,
    )>,
    pub(super) review_stale: bool,
}

impl State {
    pub(super) fn has_consent_read(&self) -> bool {
        self.latest_open.is_some()
            || matches!(self.flight, Some((_, _, Read::Review { .. })))
            || matches!(self.wanted, Some(Read::Review { .. }))
    }

    pub(super) fn can_decide(&self) -> bool {
        !self.exhausted
            && !self.review_stale
            && self
                .reviewed_identity
                .is_none_or(|identity| self.key.is_some_and(|key| key.identity == identity))
    }

    pub(super) fn snapshot_key(&self) -> Option<Key> {
        self.key
    }

    pub(super) fn has_seen_delivery(&self) -> bool {
        self.total.is_some()
    }

    pub(super) fn defer_initial_open(
        &mut self,
        id: uuid::Uuid,
        revision: u64,
        focused: Option<CardId>,
        destination: Option<WorkspaceDestination>,
    ) {
        self.initial_open = Some((id, revision, focused, destination));
        self.dirty = true;
    }

    pub(super) fn decision_request(
        &self,
        proposal: &Proposal,
        approved: bool,
        authority: crate::agent_execution::Authority,
    ) -> (
        Request,
        Option<uuid::Uuid>,
        crate::agent_execution::Authority,
    ) {
        if let Some(identity) = self.reviewed_identity {
            (
                Request::ResolveReviewedProposal {
                    identity,
                    proposal: Box::new(proposal.clone()),
                    approved,
                },
                Some(identity),
                authority,
            )
        } else {
            // Original per-project request shape, with the same inherited
            // authority. A scoped refusal never manufactures User authority.
            (
                Request::ResolveProposal {
                    id: proposal.id,
                    revision: proposal.revision,
                    approved,
                },
                None,
                authority,
            )
        }
    }

    pub(super) fn authorizes(&self, proposal: &Proposal) -> bool {
        self.key
            .is_some_and(|key| self.authorized == Some((key, proposal.id, proposal.revision)))
    }

    pub(super) fn review_opened(&mut self, id: uuid::Uuid, revision: u64) {
        self.reviewed_identity = self
            .authorized
            .filter(|(key, expected_id, expected_revision)| {
                Some(*key) == self.key && *expected_id == id && *expected_revision == revision
            })
            .map(|(key, _, _)| key.identity);
        self.latest_open = None;
        if matches!(self.wanted, Some(Read::Open { .. })) {
            self.wanted = None;
        }
        self.authorized = None;
        self.review_stale = false;
    }

    pub(super) fn review_closed(&mut self) {
        self.reviewed_identity = None;
        self.latest_open = None;
        self.authorized = None;
        self.review_stale = false;
        if matches!(self.wanted, Some(Read::Open { .. } | Read::Review { .. })) {
            self.wanted = None;
        }
    }

    pub(super) fn resolved(&mut self, id: uuid::Uuid) {
        self.pending.retain(|proposal| proposal.id != id);
        // Only a new delivery supplies an exact current count. Do not invent
        // total-1 from a receipt which may follow another window's resolution.
        self.key = None;
        self.dirty = true;
    }
}

fn valid_page(page: &[ProposalSummary], after: Option<uuid::Uuid>) -> bool {
    page.len() <= PROPOSAL_PAGE_SIZE
        && page.windows(2).all(|pair| pair[0].id < pair[1].id)
        && page
            .first()
            .is_none_or(|first| after.is_none_or(|after| first.id > after))
}

fn message(action: Action) -> Message {
    Message::AutomationApproval(super::Action::Global(action))
}

fn resolved_proposal_notice(state: &ProposalState) -> Option<TransientNotice> {
    match state {
        ProposalState::Pending => None,
        ProposalState::Approved { .. } => Some(TransientNotice::success(
            "This request was already approved. No new decision was sent.",
        )),
        ProposalState::Declined => Some(TransientNotice::warning(
            "This request was already declined. No new decision was sent.",
        )),
    }
}

impl Cagents {
    /// Called only by the real model observer's disconnect/Connected path.
    pub(in crate::app) fn retire_global_proposals(&mut self) {
        let state = &mut self.automation_approval;
        let global = &mut state.global;
        global.connected = false;
        global.trigger = None;
        global.key = None;
        global.authorized = None;
        global.latest_open = None;
        global.wanted = None;
        if global.total.is_some() {
            global.initial_open = None;
        }
        global.dirty = false;
        if let Some(next) = global.epoch.checked_add(1) {
            global.epoch = next;
        } else {
            global.exhausted = true;
        }
        state.available = None;
        state.reading = false;
        // Preserve the exact visible consent and neutral dismissal. Reconnect
        // invalidates queued decisions and old ACKs, not the user's saved text.
        if state.proposal.is_some() {
            global.review_stale = true;
            state.deciding = false;
        }
        // This also fences a legacy per-project Read that was already in flight.
        if let Some(next) = state.generation.checked_add(1) {
            state.generation = next;
        } else {
            global.exhausted = true;
        }
    }

    /// A workspace snapshot is a wake, not an atomic proposal snapshot.
    pub(in crate::app) fn refresh_global_proposals(&mut self) -> Task<Message> {
        let trigger = (self.workspace_model.identity, self.workspace_model.sequence);
        if trigger.0 == crate::workspace_model::NO_REGISTRY {
            return Task::none();
        }
        if self
            .automation_approval
            .global
            .trigger
            .is_some_and(|old| old.0 != trigger.0)
        {
            self.retire_global_proposals();
        }
        let global = &mut self.automation_approval.global;
        global.connected = true;
        if global.trigger != Some(trigger) {
            global.trigger = Some(trigger);
            global.dirty = true;
        }
        self.drive_global_proposal_read()
    }

    pub(super) fn drive_global_proposal_read(&mut self) -> Task<Message> {
        let global = &mut self.automation_approval.global;
        if global.exhausted || !global.connected || global.flight.is_some() {
            return Task::none();
        }
        let read = if let Some(read) = global.wanted.take() {
            read
        } else if std::mem::take(&mut global.dirty) {
            Read::Delivery
        } else {
            return Task::none();
        };
        let Some(nonce) = global.nonce.checked_add(1) else {
            global.exhausted = true;
            global.review_stale = true;
            return Task::none();
        };
        global.nonce = nonce;
        let epoch = global.epoch;
        global.flight = Some((nonce, epoch, read.clone()));
        let authority = crate::agent_execution::Authority::from_environment();
        let scoped_remote = authority.token().is_some() && crate::device::is_remote();
        cosmic::task::future(async move {
            let request = match &read {
                Read::Delivery => Request::ProposalDelivery,
                Read::Page { key, after, .. } => Request::AllPendingProposals {
                    identity: Some(key.identity),
                    sequence: Some(key.sequence),
                    after: *after,
                    limit: PROPOSAL_PAGE_SIZE,
                },
                Read::Open { key, id, .. } | Read::Review { key, id, .. } => {
                    Request::ReviewProposal {
                        identity: key.identity,
                        id: *id,
                    }
                }
            };
            let result = tokio::task::spawn_blocking(move || {
                if scoped_remote {
                    return Err(
                        "A local session capability cannot use remote proposal APIs".to_owned()
                    );
                }
                // Reuse current UI User authority/read transport. Never start
                // a daemon, manufacture a Child token or approve in this leaf.
                let socket = crate::terminal::ipc::daemon_socket_path()
                    .map_err(|error| error.to_string())?;
                let mut client = crate::control::ControlClient::connect(&socket)
                    .map_err(|error| error.to_string())?;
                client
                    .automation(authority, request)
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            message(Action::Loaded {
                nonce,
                epoch,
                read,
                result: Arc::new(result),
            })
        })
    }

    /// Notification activation has no cached/legacy full-consent shortcut.
    pub(in crate::app) fn proposal_notice_read_state(&self, registry: uuid::Uuid) -> (bool, bool) {
        let state = &self.automation_approval.global;
        (
            !state.exhausted
                && state.connected
                && state.key.is_some_and(|key| {
                    key.identity == registry && registry == self.workspace_model.identity
                }),
            state.flight.is_some(),
        )
    }

    pub(in crate::app) fn refresh_proposal_notice_delivery(&mut self) -> Task<Message> {
        self.update_global_proposals(Action::Refresh)
    }

    pub(in crate::app) fn open_proposal_notice(
        &mut self,
        registry: uuid::Uuid,
        id: uuid::Uuid,
        revision: u64,
    ) -> Task<Message> {
        if !self.proposal_notice_read_state(registry).0
            || id.is_nil()
            || revision == 0
            || i64::try_from(revision).is_err()
        {
            return Task::none();
        }
        let Some(key) =
            self.automation_approval.global.key.filter(|key| {
                key.identity == registry && registry == self.workspace_model.identity
            })
        else {
            return Task::none();
        };
        self.update_global_proposals(Action::Open { key, id, revision })
    }

    fn queue_global_review_refresh(&mut self) {
        let state = &mut self.automation_approval;
        if let (Some(key), Some(proposal)) = (state.global.key, state.proposal.as_ref()) {
            state.global.review_stale = true;
            if state
                .global
                .reviewed_identity
                .is_some_and(|identity| identity != key.identity)
            {
                state.error = Some(
                    "The registry changed. Dismiss and reopen this request before deciding.".into(),
                );
                return;
            }
            state.global.wanted = Some(Read::Review {
                key,
                id: proposal.id,
                revision: proposal.revision,
                generation: state.generation,
            });
        }
    }

    pub(super) fn update_global_proposals(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Toggle => {
                self.automation_approval.global.expanded = !self.automation_approval.global.expanded
            }
            Action::Refresh => self.automation_approval.global.dirty = true,
            Action::Page { key, after, start } => {
                if self.update_prepared || self.modal_is_open() {
                    return Task::none();
                }
                let state = &mut self.automation_approval.global;
                if state.key != Some(key) || key.identity != self.workspace_model.identity {
                    return Task::none();
                }
                state.wanted = Some(Read::Page { key, after, start });
            }
            Action::Open { key, id, revision } => {
                if self.automation_approval.global.key != Some(key)
                    || key.identity != self.workspace_model.identity
                    || self.update_prepared
                    || self.modal_is_open()
                    || self.chrome_ui.workspace_preview_since.is_some()
                {
                    return Task::none();
                }
                self.automation_approval.global.latest_open =
                    Some((key, id, revision, self.automation_approval.generation));
                self.automation_approval.global.wanted = Some(Read::Open {
                    key,
                    id,
                    revision,
                    generation: self.automation_approval.generation,
                    focused: self.workspace.focused(),
                    destination: self.active_workspace_destination(),
                });
            }
            Action::Loaded {
                nonce,
                epoch,
                read,
                result,
            } => {
                let global = &mut self.automation_approval.global;
                if global.flight.as_ref() != Some(&(nonce, epoch, read.clone())) {
                    return Task::none();
                }
                global.flight = None;
                let completed_open = match &read {
                    Read::Open {
                        key,
                        id,
                        revision,
                        generation,
                        ..
                    } => Some((*key, *id, *revision, *generation)),
                    _ => None,
                };
                if epoch != global.epoch || !global.connected {
                    return self.drive_global_proposal_read();
                }
                match (read, result.as_ref()) {
                    (Read::Delivery, Ok(Response::ProposalDelivery(delivery))) => {
                        self.install_proposal_delivery(delivery)
                    }
                    (Read::Page { key, after, start }, Ok(Response::ProposalPage(page)))
                        if self.automation_approval.global.key == Some(key)
                            && page.identity == key.identity
                            && page.automation_proposal_sequence == key.sequence
                            && page.after == after
                            && valid_page(&page.proposals, after) =>
                    {
                        self.automation_approval.global.pending = page.proposals.clone();
                        self.automation_approval.global.after = after;
                        self.automation_approval.global.page_end =
                            start.and_then(|start| start.checked_add(page.proposals.len() as u64));
                    }
                    (
                        Read::Open {
                            key,
                            id,
                            revision,
                            generation,
                            focused,
                            destination,
                        },
                        Ok(Response::ProposalReview(review)),
                    ) if self.automation_approval.global.key == Some(key)
                        && self.automation_approval.global.latest_open
                            == Some((key, id, revision, generation))
                        && generation == self.automation_approval.generation
                        && review.identity == key.identity
                        && review.proposal.id == id
                        && review.proposal.revision == revision
                        && self.workspace.focused() == focused
                        && self.active_workspace_destination() == destination
                        && self.refresh_ui.focused
                        && !self.update_prepared
                        && !self.modal_is_open()
                        && self.chrome_ui.workspace_preview_since.is_none() =>
                    {
                        if review.proposal.state != ProposalState::Pending {
                            self.status = resolved_proposal_notice(&review.proposal.state);
                            self.automation_approval.global.latest_open = None;
                            self.automation_approval.global.authorized = None;
                            self.automation_approval.global.resolved(id);
                            return self.drive_global_proposal_read();
                        }
                        self.automation_approval.global.authorized = Some((key, id, revision));
                        self.automation_approval.available = Some((*review.proposal).clone());
                        let opened =
                            self.update_automation_approval(super::Action::Open { id, revision });
                        return Task::batch([opened, self.drive_global_proposal_read()]);
                    }
                    (
                        Read::Review {
                            key,
                            id,
                            revision,
                            generation,
                        },
                        Ok(Response::ProposalReview(review)),
                    ) if self.automation_approval.global.key == Some(key)
                        && generation == self.automation_approval.generation
                        && review.identity == key.identity
                        && self
                            .automation_approval
                            .global
                            .reviewed_identity
                            .is_none_or(|identity| identity == key.identity)
                        && !self.automation_approval.deciding =>
                    {
                        if let Some(expected) = self.automation_approval.proposal.as_ref()
                            && expected.id == id
                            && expected.revision == revision
                        {
                            let mut resolved = expected.clone();
                            resolved.state = review.proposal.state.clone();
                            if &resolved == review.proposal.as_ref() {
                                if review.proposal.state != ProposalState::Pending {
                                    self.status = resolved_proposal_notice(&review.proposal.state);
                                    self.automation_approval.global.resolved(id);
                                    let closed = self.close_automation_approval();
                                    return Task::batch([
                                        closed,
                                        self.drive_global_proposal_read(),
                                    ]);
                                }
                                self.automation_approval.global.reviewed_identity =
                                    Some(key.identity);
                                self.automation_approval.global.review_stale = false;
                            } else {
                                self.automation_approval.error = Some(
                                    "The request changed. Dismiss and reopen it before deciding."
                                        .into(),
                                );
                            }
                        }
                    }
                    (Read::Page { .. }, _) => {
                        self.automation_approval.global.dirty = true;
                        self.automation_approval.global.error = Some(
                            "Requests changed or could not be read. Refreshing the first page."
                                .into(),
                        );
                    }
                    (Read::Review { .. }, Err(_)) => {
                        self.automation_approval.error = Some("The request could not be refreshed. Choose Not now, then reopen it before deciding.".into());
                    }
                    (_, Err(_)) => {
                        if self
                            .automation_approval
                            .global
                            .initial_open
                            .take()
                            .is_some()
                        {
                            self.status = Some(TransientNotice::warning(
                                "The request could not be loaded. It remains unresolved; reopen it when connected.",
                            ));
                        }
                        self.automation_approval.global.error = Some(
                            "Approval requests could not be refreshed. Retry when connected."
                                .into(),
                        );
                    }
                    _ => {} // A stale/foreign reply never replaces a current page or consent.
                }
                if completed_open.is_some()
                    && self.automation_approval.global.latest_open == completed_open
                {
                    self.automation_approval.global.latest_open = None;
                }
            }
        }
        self.drive_global_proposal_read()
    }

    fn install_proposal_delivery(&mut self, delivery: &ProposalDelivery) {
        let reviewing = self.automation_approval.proposal.is_some();
        // Read whole-App guards before borrowing the delivery state mutably.
        let initial_open_is_current = self.automation_approval.global.initial_open.is_some_and(
            |(_, _, focused, destination)| {
                self.workspace.focused() == focused
                    && self.active_workspace_destination() == destination
                    && self.refresh_ui.focused
                    && !self.update_prepared
                    && !self.modal_is_open()
                    && self.chrome_ui.workspace_preview_since.is_none()
            },
        );
        let state = &mut self.automation_approval.global;
        if delivery.identity != self.workspace_model.identity
            || delivery.automation_proposal_sequence == 0
            || !valid_page(&delivery.pending_automation_proposals, None)
            || delivery.pending_automation_proposals.len() as u64
                != delivery
                    .pending_automation_proposal_count
                    .min(PROPOSAL_PAGE_SIZE as u64)
            || state.key.is_some_and(|key| {
                key.identity == delivery.identity
                    && key.sequence > delivery.automation_proposal_sequence
            })
        {
            state.key = None;
            state.authorized = None;
            state.review_stale |= reviewing;
            state.error = Some("The global approval snapshot is unavailable. Local approval discovery remains enabled.".into());
            return;
        }
        let key = Key {
            identity: delivery.identity,
            sequence: delivery.automation_proposal_sequence,
            epoch: state.epoch,
        };
        state.error = None;
        if state.key == Some(key) {
            return;
        }
        if let Some(Read::Open {
            key: queued,
            id,
            revision,
            generation,
            ..
        }) = state.wanted.as_mut()
            && Some(*queued) == state.key
            && queued.identity == key.identity
            && queued.epoch == key.epoch
            && queued.sequence < key.sequence
            && state.latest_open == Some((*queued, *id, *revision, *generation))
        {
            // A delivery may finish after an explicit notice was queued behind
            // it. Keep that exact intent; its full consent still has to be read.
            *queued = key;
            state.latest_open = Some((key, *id, *revision, *generation));
        }
        state.key = Some(key);
        state.total = Some(delivery.pending_automation_proposal_count);
        state.pending = delivery.pending_automation_proposals.clone();
        state.after = None;
        state.page_end = Some(state.pending.len() as u64);
        state.authorized = None;
        state.error = None;
        let initial_open = state.initial_open.take();
        if let Some((id, revision, focused, destination)) = initial_open {
            if initial_open_is_current {
                self.automation_approval.global.latest_open =
                    Some((key, id, revision, self.automation_approval.generation));
                self.automation_approval.global.wanted = Some(Read::Open {
                    key,
                    id,
                    revision,
                    generation: self.automation_approval.generation,
                    focused,
                    destination,
                });
                return;
            }
        }
        if self
            .automation_approval
            .proposal
            .as_ref()
            .is_some_and(|proposal| {
                state.reviewed_identity.is_none()
                    || state.review_stale
                    || !state
                        .pending
                        .iter()
                        .any(|row| row.id == proposal.id && row.revision == proposal.revision)
            })
            && !self.automation_approval.deciding
        {
            // First-page absence is not a decision receipt. The exact full
            // request must confirm Pending or an immutable terminal state.
            self.queue_global_review_refresh();
        }
    }

    pub(super) fn global_proposal_banner(&self) -> Option<Element<'_, Message>> {
        let state = &self.automation_approval.global;
        let Some(total) = state.total else {
            let error = state.error.as_ref()?;
            // A failed first read has no count. Keep its explicit retry visible
            // without turning a scoped refusal into a polling loop or zero.
            return Some(
                widget::container(
                    widget::column::with_children(vec![
                        design::body("Approval requests unavailable").into(),
                        design::body(error).into(),
                        crate::ui_controls::button::secondary("Refresh")
                            .on_press_maybe(
                                (state.connected
                                    && !state.exhausted
                                    && state.flight.is_none()
                                    && !self.modal_is_open())
                                .then_some(message(Action::Refresh)),
                            )
                            .into(),
                    ])
                    .spacing(space::S),
                )
                .padding(space::M)
                .width(Length::Fill)
                .into(),
            );
        };
        if total == 0 && state.key.is_some() && state.error.is_none() {
            return Some(iced::widget::Space::new().height(Length::Shrink).into());
        }
        let current = state.key.is_some() && !state.exhausted;
        let busy = state.flight.is_some();
        let title = if current {
            format!("Pending approvals ({total})")
        } else {
            format!("Approvals updating · {total} last seen")
        };
        let mut body = widget::column::with_children(vec![
            widget::row::with_children(vec![
                design::body(title).width(Length::Fill).into(),
                crate::ui_controls::button::secondary(if state.expanded {
                    "Hide requests"
                } else {
                    "Review requests"
                })
                .on_press(message(Action::Toggle))
                .into(),
                crate::ui_controls::button::secondary("Refresh")
                    .on_press_maybe(
                        (state.connected && !busy && !self.modal_is_open())
                            .then_some(message(Action::Refresh)),
                    )
                    .into(),
            ])
            .spacing(space::S)
            .align_y(Alignment::Center)
            .into(),
        ])
        .spacing(space::S);
        if let Some(error) = &state.error {
            body = body.push(design::body(error));
        }
        if state.expanded {
            let rows = state.pending.iter().map(|proposal| {
                let row: Element<'_, Message> = widget::row::with_children(vec![
                    design::body(format!(
                        "{} · {}",
                        proposal.name,
                        proposal.repository.display()
                    ))
                    .width(Length::Fill)
                    .into(),
                    crate::ui_controls::button::secondary("Review request")
                        .on_press_maybe(
                            state
                                .key
                                .filter(|_| !busy && !self.modal_is_open() && !self.update_prepared)
                                .map(|key| {
                                    message(Action::Open {
                                        key,
                                        id: proposal.id,
                                        revision: proposal.revision,
                                    })
                                }),
                        )
                        .into(),
                ])
                .spacing(space::S)
                .align_y(Alignment::Center)
                .into();
                ((proposal.id, proposal.revision), row)
            });
            body = body.push(
                widget::container(widget::scrollable(
                    iced::widget::keyed::Column::with_children(rows).spacing(space::S),
                ))
                .max_height(160),
            );
            let next = state.pending.last().map(|row| row.id);
            let can_next = state.pending.len() == PROPOSAL_PAGE_SIZE
                && state.page_end.is_none_or(|end| end < total);
            let controls: Element<'_, Message> = widget::row::with_children(vec![
                crate::ui_controls::button::secondary("First requests")
                    .on_press_maybe(state.key.filter(|_| !busy && !self.modal_is_open()).map(
                        |key| {
                            message(Action::Page {
                                key,
                                after: None,
                                start: Some(0),
                            })
                        },
                    ))
                    .into(),
                crate::ui_controls::button::secondary("Next requests")
                    .on_press_maybe(
                        state
                            .key
                            .filter(|_| !busy && can_next && !self.modal_is_open())
                            .map(|key| {
                                message(Action::Page {
                                    key,
                                    after: next,
                                    start: state.page_end,
                                })
                            }),
                    )
                    .into(),
            ])
            .spacing(space::S)
            .into();
            body = body.push(iced::widget::keyed::Column::with_children([(
                (state.key, state.after, next),
                controls,
            )]));
        }
        let content: Element<'_, Message> = widget::container(body)
            .padding(space::M)
            .width(Length::Fill)
            .into();
        Some(
            iced::widget::keyed::Column::with_children([(
                state.key.map(|key| (key.identity, key.epoch)),
                content,
            )])
            .width(Length::Fill)
            .into(),
        )
    }
}

#[cfg(test)]
mod tests;
