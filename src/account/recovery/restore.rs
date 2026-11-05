//! Orphan registration uses the ordinary daemon lifecycle frontdoor, while
//! retaining the original key and refusing unconfirmed provider identities.

use std::{ffi::OsString, path::PathBuf, sync::Arc};

use crate::{
    account::{
        home::Homes,
        probe::{
            self,
            status::{self, LoginStatus},
        },
        remote::Settled,
    },
    host::{AccountRecovery, AccountReservation},
};

use super::Candidate;

pub(crate) fn restore_reserved(
    reservation: Arc<AccountReservation>,
    homes: Homes,
    candidate: Candidate,
) -> Result<Settled, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "Account restoration could not start its provider status task".to_owned())?;
    // Resolve the ordinary provider executable before minting a status permit.
    // Keep this owned PATH for both children, even if another discovery
    // refreshes the shell path while the first status request is running.
    // This is the application's explicit restore operation; test entrypoints
    // below take an absolute private executable and never search PATH.
    let search = runtime.block_on(probe::search_path());
    let program = probe::find_executable(candidate.provider.executable(), search.as_deref())
        .ok_or(
            "The provider CLI is not installed on this device; the preserved account was kept",
        )?;
    let recovery = AccountRecovery::new(reservation, homes, candidate)?;
    runtime.block_on(restore_with_program(recovery, program, search))
}

async fn restore_with_program(
    recovery: AccountRecovery,
    program: PathBuf,
    search_path: Option<OsString>,
) -> Result<Settled, String> {
    let account = recovery.account();
    let permit = recovery.status_permit(account.clone(), program.clone())?;
    let identity = match status::run(account, program.clone(), search_path.clone(), permit).await? {
        LoginStatus::SignedIn(identity) => identity,
        LoginStatus::SignedOut => return Err("The provider did not confirm a login in this preserved home. Its files were kept and no account was restored.".into()),
    };
    let system = match recovery.system() {
        Some(account) => {
            let permit = recovery.status_permit(account.clone(), program.clone())?;
            match status::run(account, program, search_path, permit).await? {
                LoginStatus::SignedIn(identity) => Some(identity),
                LoginStatus::SignedOut => None,
            }
        }
        None => None,
    };
    // An unanswered System status is an error above, never a guessed absence.
    // The checked Store transaction revalidates the exact reserved paths and
    // affected key while preserving unrelated edits made during provider I/O.
    let now = crate::chat::now_ms().unwrap_or_default();
    let (registry, _) = recovery
        .store()
        .try_restore_managed_reserved(&recovery, &identity, system.as_ref(), now)
        .map_err(|error| error.to_string())?;
    Ok(Settled::Registered {
        registry,
        identity: Some(identity),
        snapshot: None,
    })
}

#[cfg(test)]
mod tests;
