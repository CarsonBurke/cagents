//! Lightweight cached usage; reads never sweep the registry or hydrate history.
use super::{SharedHost, caller};
use crate::{
    account::{
        Account,
        cache::{Cache, Entry},
        home::Homes,
        info::{AccountInfo, Caller, Query, Report},
        probe,
        registry::Store,
    },
    agent_execution::Authority,
};

pub(super) fn read(
    host: &SharedHost,
    authority: &Authority,
    query: Query,
) -> Result<Report, String> {
    if query.refresh && query.account.is_none() {
        return Err("Refreshing usage requires one explicit --account".into());
    }
    let (_, caller_id) = caller(host, authority)?;
    let registry = Store::installation()
        .ok_or("Account registry is unavailable")?
        .load()
        .map_err(|e| e.to_string())?;
    if registry.is_newer() {
        return Err("Account registry needs a newer cagents version".into());
    }
    let homes = Homes::installation();
    let mut configured = registry.accounts(homes.as_ref());
    let all = registry.all_accounts(homes.as_ref());
    enum Resident {
        Chat(crate::agent::Provider, crate::chat::runtime::Handle),
        Terminal(crate::terminal::TerminalHandle),
        Missing,
    }
    let resident = caller_id.map(|id| {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let resident = if let Some(record) = state.chats.get(&id) {
            Resident::Chat(record.session.launch.provider, record.handle.clone())
        } else if let Some(record) = state.terminals.get(&id) {
            Resident::Terminal(record.handle.clone())
        } else {
            Resident::Missing
        };
        (id, resident)
    });
    // The host only selects exact live handles. Actor metadata locks and all
    // filesystem/probe work happen after releasing HostState.
    let caller = resident.map(|(id, resident)| {
        let terminal = matches!(&resident, Resident::Terminal(_));
        let (provider, environment, usage) = match resident {
            Resident::Chat(provider, handle) => (
                Some(provider),
                Some(handle.execution_environment().0),
                Some(handle.usage()),
            ),
            // Foreground provider activity does not prove its account home.
            // A shell can export different bindings after the PTY was started;
            // even retained handoff environments describe that earlier launch.
            Resident::Terminal(handle) => (
                handle.agent_activity().map(|activity| activity.provider),
                None,
                None,
            ),
            Resident::Missing => (None, None, None),
        };
        let selected = provider.and_then(|provider| {
            environment
                .as_ref()
                .and_then(|environment| select(&all, provider, environment))
        });
        let detail = if terminal {
            Some("Terminal account selection and context usage are unknown: foreground provider activity does not verify its current account environment".into())
        } else if selected.is_none() {
            Some("The session's selected account is not registered or cannot be resolved".into())
        } else {
            None
        };
        Caller::new(
            id,
            provider,
            selected.as_ref().map(Account::id),
            detail,
            usage,
        )
    });
    let now = crate::chat::now_ms().unwrap_or_default();
    let cache = Cache::installation();
    let cached = cache
        .as_ref()
        .map(|cache| cache.load(now))
        .unwrap_or_default();
    let mut diagnostics = Vec::new();
    let mut brokered = std::collections::BTreeMap::new();
    if query
        .provider
        .is_none_or(|provider| provider == crate::agent::Provider::Omp)
    {
        match crate::account::omp::database(&|name| std::env::var(name).ok())
            .map(|path| crate::account::omp::read(&path, now))
        {
            Some(crate::account::omp::History::Read(rows)) => {
                for row in rows {
                    brokered.insert(
                        row.account.id(),
                        Entry {
                            identity: Some(row.identity),
                            snapshot: row.snapshot,
                        },
                    );
                    configured.push(row.account);
                }
            }
            Some(crate::account::omp::History::Unreadable(error)) => {
                diagnostics.push(format!("OMP recorded usage unavailable: {error}"))
            }
            _ => diagnostics.push("OMP has no recorded account usage".into()),
        }
    }
    let current = caller.as_ref().and_then(|caller| caller.account.as_ref());
    let selected = report_accounts(configured, &all, current, &query)
        .into_iter()
        .filter(|account| {
            query
                .provider
                .is_none_or(|provider| provider == account.provider)
                && query.account.as_ref().is_none_or(|id| *id == account.id())
        })
        .collect::<Vec<_>>();
    if query.account.is_some() && selected.is_empty() {
        return Err("No configured account matches --account and --provider".into());
    }
    if query.refresh
        && selected
            .iter()
            .any(|account| account.provider == crate::agent::Provider::Omp)
    {
        return Err(
            "OMP usage comes from its recorded history; --refresh does not start an OMP probe"
                .into(),
        );
    }
    let runtime = query
        .refresh
        .then(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
        })
        .transpose()
        .map_err(|e| e.to_string())?;
    let mut accounts = Vec::new();
    for account in selected {
        let registered = account.registry_key().and_then(|key| {
            registry
                .registered()
                .iter()
                .find(|registered| registered.key == key)
        });
        let mut info = AccountInfo {
            id: account.id(),
            provider: account.provider,
            label: registry.label(&account).map(str::to_owned),
            registered_identity: registered.map(|registered| registered.identity()),
            observed_identity: None,
            identity_conflict: false,
            selected_by_caller: caller
                .as_ref()
                .and_then(|caller| caller.account.as_ref())
                .is_some_and(|id| *id == account.id()),
            default_for_new_chats: registry.is_default(&account),
            archived: account
                .registry_key()
                .is_some_and(|key| registry.is_archived(key)),
            usage: None,
            remaining: Vec::new(),
            credits: None,
            credits_remaining_fraction: None,
            credits_remaining: None,
            age_ms: None,
            stale: true,
            detail: None,
        };
        let entry = brokered.remove(&account.id()).or_else(|| {
            account
                .cache_key()
                .and_then(|key| cached.get(&key))
                .cloned()
        });
        if let Some(entry) = entry {
            apply(&mut info, entry, now);
        }
        if let Some(runtime) = &runtime {
            let outcome = with_refresh_admission(host, &account, || {
                runtime.block_on(probe::run(account.clone(), cache.clone(), true))
            })?;
            match outcome {
                probe::Outcome::Answered {
                    identity, snapshot, ..
                } => apply(
                    &mut info,
                    Entry { identity, snapshot },
                    crate::chat::now_ms().unwrap_or(now),
                ),
                probe::Outcome::Failed(error) => {
                    info.detail = Some(format!(
                        "Refresh failed; retained cached observation: {error}"
                    ))
                }
                probe::Outcome::Busy => {
                    info.detail = Some(
                        "Another process is probing this provider; retained cached observation"
                            .into(),
                    )
                }
            }
        }
        accounts.push(info);
    }
    Ok(Report {
        sampled_at: crate::chat::now_ms().unwrap_or(now),
        caller,
        accounts,
        diagnostics,
    })
}

/// The caller already holds shared control admission through the refresh.
/// This is only a negative lifecycle check, not account selection/read authority.
/// Keep it immediately before provider work; cached reads never invoke it.
fn with_refresh_admission<T>(
    host: &SharedHost,
    account: &Account,
    refresh: impl FnOnce() -> T,
) -> Result<T, String> {
    super::super::account_lifecycle::ensure_paths(host, account.resolved_home())
        .map_err(|error| error.message)?;
    Ok(refresh())
}

/// Preserve a running account after it was archived; explicit reads can inspect
/// preserved metadata without making that account a choice for new work.
fn report_accounts(
    mut configured: Vec<Account>,
    all: &[Account],
    current: Option<&crate::account::AccountId>,
    query: &Query,
) -> Vec<Account> {
    let wanted = query.account.as_ref().or(current);
    if let Some(account) = wanted.and_then(|id| all.iter().find(|account| account.id() == *id))
        && !configured.iter().any(|known| known.id() == account.id())
    {
        configured.push(account.clone());
    }
    configured
}

fn apply(info: &mut AccountInfo, entry: Entry, now: u64) {
    info.identity_conflict = info.registered_identity.as_ref().is_some_and(|registered| {
        entry.identity.as_ref().is_some_and(|observed| {
            registered.email.is_some()
                && observed.email.is_some()
                && !registered.same_login(observed)
        })
    });
    info.observed_identity = entry.identity;
    info.set_usage(entry.snapshot, now);
}

/// Match the effective environment captured by the executing host. Durable
/// recipes omit inherited/transient values and cannot identify a running login.
fn select(
    accounts: &[Account],
    provider: crate::agent::Provider,
    environment: &std::collections::BTreeMap<String, String>,
) -> Option<Account> {
    // API credentials override subscription homes. Never label plan allowance
    // as the current execution budget; no credential values leave this helper.
    if crate::account::credential_overrides(provider)
        .iter()
        .any(|name| {
            environment
                .get(*name)
                .is_some_and(|value| !value.is_empty())
        })
    {
        return None;
    }
    let variable = crate::account::home_variable(provider)?;
    if environment
        .get(variable)
        .is_some_and(|value| value.is_empty())
    {
        return None;
    }
    let home = environment
        .get(variable)
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            let home = environment.get("HOME").filter(|home| !home.is_empty())?;
            Some(std::path::Path::new(home).join(match provider {
                crate::agent::Provider::Claude => ".claude",
                crate::agent::Provider::Codex => ".codex",
                crate::agent::Provider::Antigravity => ".gemini/antigravity-cli",
                crate::agent::Provider::Omp => return None,
            }))
        })?;
    let home = canonical_home(home)?;
    let mut matching = accounts.iter().filter(|account| {
        account.provider == provider
            && account.resolved_home().and_then(canonical_home).as_ref() == Some(&home)
    });
    let first = matching.next()?.clone();
    matching.next().is_none().then_some(first)
}

fn canonical_home(path: std::path::PathBuf) -> Option<std::path::PathBuf> {
    path.is_absolute()
        .then(|| std::fs::canonicalize(&path).unwrap_or(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Provider;
    #[test]
    fn launch_selection_is_not_default_or_email_and_ambiguity_is_unknown() {
        let system = Account::system(Provider::Codex);
        let managed =
            Account::managed(Provider::Codex, uuid::Uuid::new_v4(), "/account-one".into());
        let linked = Account::linked(Provider::Codex, uuid::Uuid::new_v4(), "/account-one".into());
        let environment = [("CODEX_HOME".into(), "/account-one".into())].into();
        assert_eq!(
            select(
                &[system.clone(), managed.clone()],
                Provider::Codex,
                &environment
            ),
            Some(managed.clone())
        );
        assert_eq!(
            select(
                &[system.clone(), managed, linked],
                Provider::Codex,
                &environment
            ),
            None
        );
        assert_eq!(
            select(&[system.clone()], Provider::Codex, &Default::default()),
            None
        );
        assert_eq!(select(&[], Provider::Codex, &environment), None);
    }
    #[test]
    fn preserved_current_account_is_reported_without_listing_unrelated_archives() {
        let system = Account::system(Provider::Codex);
        let archived = Account::managed(Provider::Codex, uuid::Uuid::new_v4(), "/archived".into());
        let unrelated =
            Account::managed(Provider::Codex, uuid::Uuid::new_v4(), "/unrelated".into());
        let all = [system.clone(), archived.clone(), unrelated];
        assert_eq!(
            report_accounts(
                vec![system.clone()],
                &all,
                Some(&archived.id()),
                &Query::default()
            ),
            vec![system.clone(), archived.clone()]
        );
        let explicit = Query {
            account: Some(archived.id()),
            ..Default::default()
        };
        assert_eq!(
            report_accounts(vec![system.clone()], &all, None, &explicit),
            vec![system, archived]
        );
    }
    #[test]
    fn effective_launch_home_and_api_overrides_determine_caller_binding() {
        let system = Account {
            provider: Provider::Codex,
            home: crate::account::AccountHome::SystemAt {
                path: "/daemon/.codex".into(),
            },
        };
        let selected = Account::linked(
            Provider::Codex,
            uuid::Uuid::new_v4(),
            "/actual/.codex".into(),
        );
        let transient = Account::linked(Provider::Codex, uuid::Uuid::new_v4(), "/transient".into());
        let accounts = [system.clone(), selected.clone(), transient.clone()];
        // Frozen HOME differs from the daemon HOME; an unset provider override
        // falls back to that executing HOME, not the daemon's system account.
        let mut actual = [("HOME".into(), "/actual".into())].into();
        assert_eq!(select(&accounts, Provider::Codex, &actual), Some(selected));
        // Inherited or transient CODEX_HOME survived into the effective map.
        actual.insert("CODEX_HOME".into(), "/transient".into());
        assert_eq!(select(&accounts, Provider::Codex, &actual), Some(transient));
        // Neither presence of a home nor a same-email plan defeats API auth.
        actual.insert("OPENAI_API_KEY".into(), "fixture-nonsecret".into());
        assert_eq!(select(&accounts, Provider::Codex, &actual), None);
        actual.remove("OPENAI_API_KEY");
        actual.remove("CODEX_HOME");
        actual.insert("HOME".into(), "/unknown".into());
        assert_eq!(select(&accounts, Provider::Codex, &actual), None);
        actual.insert("HOME".into(), "/daemon".into());
        actual.insert("CODEX_HOME".into(), String::new());
        assert_eq!(
            select(&accounts, Provider::Codex, &actual),
            None,
            "explicit empty provider home is not proof of the system login"
        );
        actual.remove("CODEX_HOME");
        assert_eq!(select(&accounts, Provider::Codex, &actual), Some(system));
    }

    #[test]
    fn effective_home_aliases_resolve_once_but_duplicate_bindings_stay_unknown() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("home");
        std::fs::create_dir(&target).unwrap();
        let alias = directory.path().join("alias");
        std::os::unix::fs::symlink(&target, &alias).unwrap();
        let linked = Account::linked(Provider::Claude, uuid::Uuid::new_v4(), target);
        let environment = [(
            "CLAUDE_CONFIG_DIR".into(),
            alias.to_string_lossy().into_owned(),
        )]
        .into();
        assert_eq!(
            select(&[linked.clone()], Provider::Claude, &environment),
            Some(linked.clone())
        );
        let duplicate = Account::linked(Provider::Claude, uuid::Uuid::new_v4(), alias);
        assert_eq!(
            select(&[linked, duplicate], Provider::Claude, &environment),
            None
        );
    }
    #[test]
    fn refresh_admission_refuses_related_alias_before_provider_work_and_preserves_other_accounts() {
        use crate::account::{home::Homes, registry::Store};
        use std::cell::Cell;
        let root = tempfile::tempdir().unwrap();
        let homes = Homes::at(root.path().join("homes"));
        let store = Store::at(root.path().join("registry"));
        let selected = root.path().join("selected");
        let unrelated = root.path().join("unrelated");
        std::fs::create_dir(&selected).unwrap();
        std::fs::create_dir(&unrelated).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&selected, &alias).unwrap();
        let key = uuid::Uuid::new_v4();
        store
            .update(|registry| {
                registry.link(
                    Provider::Codex,
                    key,
                    selected.clone(),
                    None,
                    Some(&homes),
                    1,
                )
            })
            .unwrap()
            .1
            .unwrap();
        let account = Account::linked(Provider::Codex, key, selected);
        let host = crate::host::AccountReservation::test_host();
        let reservation =
            crate::host::AccountReservation::test_install(&host, &[account.clone()], store, &homes)
                .unwrap();
        let invoked = Cell::new(0);
        let aliased = Account::linked(Provider::Codex, key, alias);
        let refused =
            with_refresh_admission(&host, &aliased, || invoked.set(invoked.get() + 1)).unwrap_err();
        assert!(refused.contains("account home is being changed"));
        assert_eq!(
            invoked.get(),
            0,
            "a refused explicit refresh must not invoke its provider"
        );
        let other = Account::linked(Provider::Codex, uuid::Uuid::new_v4(), unrelated);
        with_refresh_admission(&host, &other, || invoked.set(invoked.get() + 1)).unwrap();
        assert_eq!(invoked.get(), 1, "an unrelated refresh is still admitted");
        drop(reservation);
        with_refresh_admission(&host, &account, || invoked.set(invoked.get() + 1)).unwrap();
        assert_eq!(invoked.get(), 2);
    }
}
