//! Explicit cleanup progresses only original window-owned receipts. Timers may
//! start the first unowned retirement; uncertain cleanup requires an explicit retry.
use super::*;
use crate::edited_root_admission::{AdmissionPhase, PlacementRetirementWork, RetirementWork};

#[derive(Debug, Clone)]
pub(super) enum Action {
    Poll,
    Advanced(Option<String>),
}

enum Work { Root(RetirementWork), Placement(PlacementRetirementWork) }

impl Cagents {
    pub(super) fn root_admission_subscription(&self) -> Subscription<Message> {
        let admissions = self.edited_root_admissions.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if admissions.is_empty() { return Subscription::none(); }
        cosmic::iced::time::every(Duration::from_secs(1)).map(|_| Message::RootAdmissions(Action::Poll))
    }

    pub(super) fn update_root_admissions(&mut self, action: Action) -> Task<Message> {
        if let Action::Advanced(Some(error)) = action {
            self.status = Some(TransientNotice::warning(format!("Original root cleanup remains retained: {error}")));
        }
        let work = {
            // The actual client records ownership and settlement in this SAME
            // ledger before Message delivery. No RPC or filesystem call holds it.
            let mut admissions = self.edited_root_admissions.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            admissions.collect_settled();
            admissions.request_unowned_retirements();
            if admissions.tickets().any(|ticket| ticket.phase() == AdmissionPhase::Retiring)
                || admissions.placement_tickets().any(|ticket| ticket.phase() == AdmissionPhase::Retiring) {
                return Task::none();
            }
            match admissions.next_retirement() {
                Ok(Some(work)) => Some(Work::Root(work)),
                Ok(None) | Err(_) => admissions.next_placement_retirement().map(Work::Placement),
            }
        };
        let Some(work) = work else { return Task::none(); };
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                let deadline = Instant::now() + Duration::from_secs(30);
                match work {
                    Work::Root(work) => work.run(deadline).map_err(|error| error.to_string()),
                    Work::Placement(work) => work.run(deadline).map(|_| ()).map_err(|error| error.to_string()),
                }
            }).await.map_err(|error| format!("Cleanup worker interrupted: {error}"))
                .and_then(|result| result);
            // Typed original custody stays in the ledger on error/panic/lost
            // delivery. This notice contains no authority which could be lost.
            Message::RootAdmissions(Action::Advanced(result.err()))
        })
    }
}
