//! Exact live-source routing; no recipe/account DTO can select a replacement.
use super::state::SharedHost;
use crate::{account::reset::{controller, request::{Report, Request}}, agent::Provider};
use std::time::{Duration, Instant};

pub(super) fn execute(host: &SharedHost, request: Request) -> Result<Report, String> {
    let expires = Instant::now()+Duration::from_secs(45);
    request.validate()?;
    let session = request.session;
    let (handle, instance) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down { return Err("Daemon is shutting down; no reset operation was admitted".into()); }
        let record = state.chats.get(&session).ok_or("Target is not an existing live native conversation. No replacement process or usage command was started")?;
        if record.session.launch.provider != Provider::Codex {
            return Err(match record.session.launch.provider {
                Provider::Claude => "Claude's documented saved resets are redeemed manually on web/Desktop; this live terminal/IDE transport has no supported programmatic redemption path",
                Provider::Omp => "Omp has upstream saved-reset commands, but this live transport exposes no authenticated credential-ID reset path. No potentially auto-redeeming usage command was run",
                Provider::Codex => unreachable!(),
            }.into());
        }
        if record.returning_terminal || record.handle.is_completed() {
            return Err("The exact target actor is completed or changing surface; no restart or reset was attempted".into());
        }
        (record.handle.clone(), record.execution_instance)
    };
    let current = || {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down || !state.chats.get(&session).is_some_and(|record|
            record.execution_instance == instance && record.handle.same_actor(&handle)
                && !record.returning_terminal && !record.handle.is_completed()) {
            return Err("The exact target owner changed; preserve and reconcile any existing reset attempt instead of selecting a replacement".into());
        }
        Ok(())
    };
    let root = crate::instance::project_dirs().map(|dirs| dirs.data_dir().join("account-reset-attempts"));
    controller::execute(&handle, request, root.as_deref(), expires, current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{account::reset::{ProviderAccount, request::Action}, model::{BootId, HostIncarnation, HostSessionId, HostSessionProof, TerminalId}};
    use std::sync::{Arc,Mutex};
    #[test]
    fn missing_target_unauthorized_reset_and_shutdown_never_create_a_replacement_source() {
        let host=Arc::new(Mutex::new(super::super::state::HostState::new(HostSessionProof {
            host_session_id:HostSessionId::new("private-allowance-test"),
            boot_id:BootId::new("private-boot"),host_incarnation:HostIncarnation::new("private-incarnation")})));
        let session=TerminalId::new();
        let error=execute(&host,Request {session,action:Action::Inspect}).unwrap_err();
        assert!(error.contains("not an existing live native conversation"));
        let error=execute(&host,Request {session,action:Action::Redeem {
            expected:ProviderAccount {account_id:"private-account".into(),backend_origin:"https://chatgpt.com".into()},
            credit_id:"private-credit".into(),authorized:false,condition:None}}).unwrap_err();
        assert!(error.contains("explicit --authorize"));
        host.lock().unwrap().shutting_down=true;
        let error=execute(&host,Request {session,action:Action::Inspect}).unwrap_err();
        assert!(error.contains("shutting down"));
        let state=host.lock().unwrap();
        assert!(state.chats.is_empty() && state.chat_spawning.is_empty() && state.terminals.is_empty() && state.spawning.is_empty());
        assert!(state.chat_handoffs.is_empty());
    }
}
