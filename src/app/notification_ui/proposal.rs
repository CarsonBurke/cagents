//! One explicit desktop/Attention activation, not discovery or consent. The
//! existing App postlude retries it after model/focus messages; no new worker.
use super::*;

#[derive(Clone)]
pub(super) struct Activation {
    registry: uuid::Uuid,
    id: uuid::Uuid,
    revision: u64,
    generation: u64,
    host: HostSessionProof,
    backend: Option<(String, String, String)>,
    scope: Option<(
        Weak<WorkspaceState>,
        Option<CardId>,
        Option<WorkspaceDestination>,
    )>,
    requested_delivery: bool,
}

impl Cagents {
    pub(super) fn activate_proposal_notice(
        &mut self,
        registry: uuid::Uuid,
        id: uuid::Uuid,
        revision: u64,
    ) -> Task<Message> {
        if registry.is_nil()
            || id.is_nil()
            || revision == 0
            || i64::try_from(revision).is_err()
            || !self.can_open_lifecycle_notice()
        {
            self.status = Some(TransientNotice::warning(
                "Return to the workspace and finish any open edit or request, then choose this approval notice again.",
            ));
            return self.activate_main_window(None);
        }
        let Some(host) = self
            .notification_ui
            .host
            .clone()
            .filter(|proof| proof.is_complete())
        else {
            self.status = Some(TransientNotice::warning(
                "The notification backend is not connected. The request remains available in Attention.",
            ));
            return self.activate_main_window(None);
        };
        let known = self.workspace_model.identity != crate::workspace_model::NO_REGISTRY;
        self.notification_ui.proposal_activation = Some(Activation {
            registry,
            id,
            revision,
            generation: self.notification_ui.activation_generation,
            host,
            backend: lifecycle_backend(),
            scope: known.then(|| {
                (
                    Arc::downgrade(&self.durable),
                    self.workspace.focused(),
                    self.active_workspace_destination(),
                )
            }),
            requested_delivery: false,
        });
        self.activate_main_window(None)
    }

    pub(super) fn retry_proposal_notice_activation(&mut self) -> Task<Message> {
        let Some(mut wanted) = self.notification_ui.proposal_activation.clone() else {
            return Task::none();
        };
        if wanted.generation != self.notification_ui.activation_generation
            || self.notification_ui.host.as_ref() != Some(&wanted.host)
            || lifecycle_backend() != wanted.backend
        {
            self.notification_ui.proposal_activation = None;
            return Task::none();
        }
        if self.workspace_model.identity == crate::workspace_model::NO_REGISTRY {
            return Task::none();
        }
        if self.workspace_model.identity != wanted.registry {
            self.notification_ui.proposal_activation = None;
            self.status = Some(TransientNotice::warning(
                "This request belongs to another registry. No approval was sent; the original notice remains in Attention.",
            ));
            return Task::none();
        }
        // Startup model and notification streams are independent. Arm the
        // presentation scope only once the exact registry is actually known.
        if wanted.scope.is_none() {
            wanted.scope = Some((
                Arc::downgrade(&self.durable),
                self.workspace.focused(),
                self.active_workspace_destination(),
            ));
            self.notification_ui.proposal_activation = Some(wanted.clone());
        }
        let scope_current =
            wanted
                .scope
                .as_ref()
                .is_some_and(|(workspace, focused, destination)| {
                    workspace.as_ptr() == Arc::as_ptr(&self.durable)
                        && *focused == self.workspace.focused()
                        && *destination == self.active_workspace_destination()
                });
        if !scope_current
            || !self.can_open_lifecycle_notice()
            || self.chrome_ui.workspace_preview_since.is_some()
        {
            self.notification_ui.proposal_activation = None;
            self.status = Some(TransientNotice::warning(
                "The workspace changed. Choose the request again from Attention when ready to review it.",
            ));
            return Task::none();
        }
        // The activate-window task is not proof of actual focus. Wait for the
        // real window event before capturing the consent read's focus guards.
        if !self.refresh_ui.focused {
            return Task::none();
        }
        let (ready, reading) = self.proposal_notice_read_state(wanted.registry);
        if ready {
            self.notification_ui.proposal_activation = None;
            return self.open_proposal_notice(wanted.registry, wanted.id, wanted.revision);
        }
        if !wanted.requested_delivery {
            if let Some(current) = &mut self.notification_ui.proposal_activation {
                current.requested_delivery = true;
            }
            return self.refresh_proposal_notice_delivery();
        }
        if !reading {
            self.notification_ui.proposal_activation = None;
            self.status = Some(TransientNotice::warning(
                "The request could not be loaded. It remains unresolved; choose it again when connected.",
            ));
        }
        Task::none()
    }
}
