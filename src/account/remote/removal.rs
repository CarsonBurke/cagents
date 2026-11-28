use super::{Archived, LifecycleError, RemovalPlan, Reservation};
use crate::account::{
    Account, AccountId,
    handoff::Inventory,
    home::Homes,
    registry::{LIST_PATIENCE, Registry, Store, patiently},
};

pub(super) fn source(
    registry: &Registry,
    homes: &Homes,
    id: &AccountId,
) -> Result<Account, LifecycleError> {
    if registry.is_newer() {
        return Err("The accounts were saved by a newer cagents".into());
    }
    let account = registry
        .all_accounts(Some(homes))
        .into_iter()
        .find(|account| account.id() == *id)
        .ok_or("The source account is no longer registered")?;
    if !Account::PROVIDERS.contains(&account.provider)
        || !matches!(account.home, crate::account::AccountHome::Managed { .. })
    {
        return Err("Only a managed Claude or Codex account can be archived".into());
    }
    Ok(account)
}

pub(super) fn target(
    registry: &Registry,
    homes: &Homes,
    source: &Account,
    id: Option<&AccountId>,
) -> Result<Option<Account>, LifecycleError> {
    id.map(|id| {
        registry
            .accounts(Some(homes))
            .into_iter()
            .find(|account| {
                account.id() == *id && account.provider == source.provider && account != source
            })
            .ok_or_else(|| "Choose another active account for the same provider".into())
    })
    .transpose()
}

pub fn removal_plan(id: &AccountId) -> Result<serde_json::Value, LifecycleError> {
    let homes = Homes::installation().ok_or("The device has no account data directory")?;
    let store = Store::installation().ok_or("The device has no account registry")?;
    let registry = patiently(LIST_PATIENCE, || store.try_load())?;
    let account = source(&registry, &homes, id)?;
    let conversations = inventory(&account)?.count();
    serde_json::to_value(RemovalPlan { conversations }).map_err(|error| error.to_string().into())
}

fn inventory(account: &Account) -> Result<Inventory, LifecycleError> {
    let home = account
        .resolved_home()
        .ok_or("This account has no credential directory")?;
    Inventory::read(account.provider, &home).map_err(Into::into)
}

pub(super) fn archive(
    reservation: &impl Reservation,
    homes: &Homes,
    store: &Store,
    id: &AccountId,
    move_to: Option<&AccountId>,
) -> Result<serde_json::Value, LifecycleError> {
    let registry = reservation.patiently(|| store.try_load())?;
    reservation.verify_registry(&registry)?;
    let source = source(&registry, homes, id)?;
    let key = source.registry_key().expect("managed account has a key");
    let target = target(&registry, homes, &source, move_to)?;
    let home = source
        .resolved_home()
        .ok_or("This account has no credential directory")?;
    crate::secure_file::reject_symlink_components(&home)?;
    crate::secure_file::validate_owned_directory(&home)?;
    let moved = if let Some(target) = target {
        let original = inventory(&source)?;
        let staged = original.stage_to(&inventory(&target)?)?;
        let count = staged.len();
        reservation.verify()?;
        for transcript in staged {
            transcript.commit().map_err(|error| format!(
                "{error}. All source transcripts were kept and the account was not signed out. Some copies may already be on the target."
            ))?;
        }
        count
    } else {
        0
    };

    if !registry.is_signed_out(key) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        reservation.hide_before_sign_out(store, key)?;
        // A crash after logout leaves the account hidden. A retry reuses any
        // published copies and repeats sign-out until it can be confirmed.
        runtime.block_on(crate::account::probe::logout(source)).map_err(|error| format!(
            "{error}. The account is hidden from routing and its history is still on disk. Retry archive to finish signing out."
        ))?;
    }
    let (registry, ()) = reservation
        .commit(|| store.try_update_reserved(reservation, |registry| registry.archive_signed_out(key)))
        .map_err(|error| format!("The account is hidden from routing and the provider signed out. Saving the sign-out confirmation failed: {error}. Retry archive to record the confirmation."))?;
    serde_json::to_value(Archived {
        revision: registry.revision(),
        moved,
    })
    .map_err(|error| error.to_string().into())
}
