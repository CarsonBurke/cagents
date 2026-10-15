//! Pair account provenance and transcript source inside their actual owner.
//! No public metadata, two-object factory, or requested resume UUID can mint it.
use super::{Handle, Store};
use crate::{account::scope::LaunchAccountScope, model::TerminalId,
    transcript_cache::export::{SessionNamespace, StoreSourceReceipt}};
use std::{sync::{Arc, TryLockError}, time::{Duration, Instant}};

#[derive(Default)]
pub(super) enum StoreAccountBinding {
    #[default]
    Unbound,
    Bound { scope: LaunchAccountScope, session: TerminalId },
    Unavailable(String),
}
fn same_home(left: &LaunchAccountScope, right: &LaunchAccountScope) -> bool {
    left.provider == right.provider && left.canonical_home == right.canonical_home
        && left.device == right.device && left.inode == right.inode
}
/// Called by start_inner before any holder/provider is started. A retained
/// foreign Store must never receive new bodies under its old reader grants.
pub(super) fn bind_store(store: &mut Store, scope: Result<&LaunchAccountScope, &str>, session: Option<TerminalId>) -> Result<(), String> {
    match (&store.account_source_binding, scope, session) {
        (StoreAccountBinding::Unbound, Ok(scope), Some(session)) => {
            scope.revalidate()?;
            store.account_source_binding = StoreAccountBinding::Bound { scope: scope.clone(), session };
        }
        (StoreAccountBinding::Unbound, _, _) => {
            store.account_source_binding = StoreAccountBinding::Unavailable("The original Store had no typed actual account provenance".into());
        }
        (StoreAccountBinding::Bound {scope: original, session: original_session}, Ok(current), Some(session))
            if same_home(original, current) && *original_session == session => current.revalidate()?,
        (StoreAccountBinding::Bound {..}, _, _) => return Err("The retained transcript belongs to another or unproved account/source; start a separate conversation instead".into()),
        // Unknown old bodies never acquire account authority from a later actor.
        (StoreAccountBinding::Unavailable(_), _, _) => {}
    }
    Ok(())
}

/// Owned actual source pairing, not yet a registered index entry or read grant.
/// Contains only metadata and independent source FDs/probe. No process owner,
/// Handle/Inner, Store owner lease, or provider-confirmed session is retained.
pub(crate) struct CapturedAccountSource {
    scope: LaunchAccountScope,
    source_session: TerminalId,
    receipt: StoreSourceReceipt,
}
impl CapturedAccountSource {
    pub(crate) fn scope(&self) -> &LaunchAccountScope { &self.scope }
    pub(crate) fn source_session(&self) -> TerminalId { self.source_session }
    pub(crate) fn native_namespace(&self) -> SessionNamespace { SessionNamespace::Unconfirmed }
    /// Only a codec factory taking this opaque pairing may use its parts. The
    /// factory must still admit/revalidate the namespace, source, FD and epoch.
    pub(crate) fn into_parts(self) -> (LaunchAccountScope, TerminalId, SessionNamespace, StoreSourceReceipt) {
        (self.scope, self.source_session, SessionNamespace::Unconfirmed, self.receipt)
    }
}
impl Handle {
    /// Filesystem/SQLite capture: callers must release HostState before entry.
    pub(crate) fn capture_account_source(&self, expires: Instant) -> Result<CapturedAccountSource, String> {
        if Instant::now() >= expires { return Err("Account source capture deadline expired".into()); }
        let scope = self.0.launch_account_scope.as_ref().map_err(Clone::clone)?;
        let session = self.0.account_source_session.ok_or("The actual backend has no original source session")?;
        scope.revalidate()?;
        // Store -> state is the runtime lock order. Avoid an unbounded lock
        // wait, and acquire no namespace/reader flock while this guard exists.
        let store = loop {
            match self.0.store.try_lock() {
                Ok(store) => break store,
                Err(TryLockError::Poisoned(_)) => return Err("The actual transcript Store owner is unavailable".into()),
                Err(TryLockError::WouldBlock) => {
                    let remaining = expires.saturating_duration_since(Instant::now());
                    if remaining.is_zero() { return Err("Account source capture deadline expired".into()); }
                    std::thread::sleep(remaining.min(Duration::from_millis(5)));
                }
            }
        };
        if Instant::now() >= expires { return Err("Account source capture deadline expired".into()); }
        let expected = Arc::downgrade(&self.0);
        let check_owner = |store: &Store| -> Result<(), String> {
            if !store.source_authority_observer().ptr_eq(&expected) {
                return Err("The transcript Store is now owned by a different backend".into());
            }
            match &store.account_source_binding {
                StoreAccountBinding::Bound {scope: original, session: original_session}
                    if same_home(original, scope) && *original_session == session => Ok(()),
                StoreAccountBinding::Unavailable(reason) => Err(reason.clone()),
                _ => Err("The actual account and original transcript source are not paired".into()),
            }
        };
        check_owner(&store)?;
        let receipt = store.capture_export_source(expires)?;
        check_owner(&store)?;
        drop(store); // capture_export_source also explicitly ended its snapshot.
        scope.revalidate()?;
        let proof = CapturedAccountSource { scope: scope.clone(), source_session: session, receipt };
        // Account alias metadata allocation also belongs to the original budget.
        if Instant::now() >= expires { return Err("Account source capture deadline expired".into()); }
        Ok(proof)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{agent::Provider, chat::{self, protocol::{Adapter, ProcessSpec, Step}}, model::LaunchRecipe};
    use std::{collections::BTreeMap, path::Path};
    struct Quiet;
    impl Adapter for Quiet {
        fn initialize(&mut self) -> Result<Step, String> { Ok(Step::default()) }
        fn command(&mut self, _: chat::Command) -> Result<Step, String> { Ok(Step::default()) }
        fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> { Ok(Step::default()) }
    }
    fn launch(root: &Path, actual: &Path, provider: Provider) -> (ProcessSpec, chat::Session) {
        std::fs::create_dir_all(actual).unwrap();
        let recipe = LaunchRecipe { program: "/bin/cat".into(), arguments: Vec::new(),
            working_directory: root.into(), environment: BTreeMap::new(), unset_environment: Default::default() };
        let source = chat::Session { id: TerminalId::new(), proof: None, terminal: None,
            native_continuity: Default::default(), owned_codex_listener: None,
            launch: chat::Launch { provider, recipe: recipe.clone(), session_id: Some("11111111-0000-4000-8000-000000000001".into()) } };
        let mut spec = ProcessSpec::from(&recipe);
        spec.environment.insert("HOME".into(), root.display().to_string());
        if let Some(variable) = crate::account::home_variable(provider) {
            spec.environment.insert(variable.into(), actual.display().to_string());
        }
        (spec, source)
    }
    fn start(spec: ProcessSpec, source: chat::Session, retained: Option<Store>) -> Result<Handle, String> {
        let mut initial = Step::default();
        let store = match retained {Some(store) => store, None => super::super::preflight(&spec, &mut initial)?};
        Handle::start_owned(spec, Box::new(Quiet), initial, store, source, Default::default(), |_| {})
    }
    fn take_completed_store(handle: &Handle) -> Store {
        handle.close().unwrap();
        std::mem::replace(&mut *handle.0.store.lock().unwrap(), Store::new().unwrap())
    }
    fn expires() -> Instant { Instant::now() + Duration::from_secs(2) }

    #[test]
    fn paired_capture_uses_actual_backend_scope_and_never_requested_native_uuid() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let actual = root.path().join("actual-b");
        let (spec, source) = launch(root.path(), &actual, Provider::Claude);
        let source_id = source.id;
        assert!(source.launch.recipe.environment.is_empty(), "issued recipe is not actual account evidence");
        let handle = start(spec, source, None).unwrap();
        let proof = handle.capture_account_source(expires()).unwrap();
        assert_eq!(proof.scope().canonical_home, actual.canonicalize().unwrap());
        assert_eq!(proof.source_session(), source_id);
        assert_eq!(proof.native_namespace(), SessionNamespace::Unconfirmed,
            "a requested resume UUID never becomes provider confirmation");
        let weak = Arc::downgrade(&handle.0);
        handle.close().unwrap();
        drop(handle);
        assert!(weak.upgrade().is_none(), "an owned source proof must not retain a process owner");
        let (_, actual_id, namespace, _receipt) = proof.into_parts();
        assert_eq!(actual_id, source_id);
        assert_eq!(namespace, SessionNamespace::Unconfirmed);
    }

    #[test]
    fn paired_capture_refuses_foreign_current_store_owner_replaced_home_and_expired_deadline() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let (spec, source) = launch(root.path(), &root.path().join("a"), Provider::Claude);
        let handle = start(spec, source, None).unwrap();
        assert!(handle.capture_account_source(Instant::now()).err().unwrap().contains("deadline"));
        let (spec, source) = launch(root.path(), &root.path().join("b"), Provider::Claude);
        let other = start(spec, source, None).unwrap();
        handle.0.store.lock().unwrap().register_source_authority_observer(Arc::downgrade(&other.0));
        assert!(handle.capture_account_source(expires()).err().unwrap().contains("different backend"));
        handle.0.store.lock().unwrap().register_source_authority_observer(Arc::downgrade(&handle.0));
        assert!(handle.capture_account_source(expires()).is_ok());
        std::fs::rename(root.path().join("a"), root.path().join("old-a")).unwrap();
        std::fs::create_dir(root.path().join("a")).unwrap();
        assert!(handle.capture_account_source(expires()).is_err());
        handle.close().unwrap();other.close().unwrap();
    }

    #[test]
    fn foreign_or_different_source_retained_store_refuses_before_new_provider_effects() {
        for different_session in [false, true] {
            let root = tempfile::tempdir_in("/var/tmp").unwrap();
            let (spec, source) = launch(root.path(), &root.path().join("a"), Provider::Claude);
            let handle = start(spec, source.clone(), None).unwrap();
            let retained = take_completed_store(&handle);
            let (mut spec, mut next) = launch(root.path(), &root.path().join(if different_session {"a"} else {"b"}), Provider::Claude);
            if !different_session {next.id = source.id;}
            spec.program = "/bin/sh".into();
            spec.arguments = vec!["-c".into(), "touch forbidden-start; exec /bin/cat".into()];
            assert!(start(spec, next, Some(retained)).err().unwrap().contains("retained transcript"));
            assert!(!root.path().join("forbidden-start").exists());
        }
    }

    #[test]
    fn same_actual_account_and_source_can_retain_store_but_unknown_bodies_never_upgrade() {
        for originally_unknown in [false, true] {
            let root = tempfile::tempdir_in("/var/tmp").unwrap();
            let (spec, source) = launch(root.path(), &root.path().join("a"), Provider::Claude);
            let handle = if originally_unknown {
                let mut initial = Step::default();
                let store = super::super::preflight(&spec, &mut initial).unwrap();
                Handle::start(spec.clone(), Box::new(Quiet), initial, store, None, |_| {}).unwrap()
            } else {start(spec.clone(), source.clone(), None).unwrap()};
            let retained = take_completed_store(&handle);
            let replacement = start(spec, source, Some(retained)).unwrap();
            let answer = replacement.capture_account_source(expires());
            if originally_unknown {assert!(answer.err().unwrap().contains("original Store"));}
            else {assert!(answer.is_ok());}
            replacement.close().unwrap();
        }
    }

    #[test]
    fn actual_store_lock_wait_is_bounded_without_publishing_a_receipt() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let (spec, source) = launch(root.path(), &root.path().join("a"), Provider::Claude);
        let handle = start(spec, source, None).unwrap();
        let lock = handle.0.store.lock().unwrap();
        let begin = Instant::now();
        assert!(handle.capture_account_source(begin + Duration::from_millis(20)).err().unwrap().contains("deadline"));
        assert!(begin.elapsed() < Duration::from_secs(1));
        drop(lock);
        handle.close().unwrap();
    }
}
