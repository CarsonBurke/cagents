//! Immutable credential-directory identity for account-scoped annotations.
//! This records filesystem identity, never credentials or a claimed provider login.

use std::{ffi::OsString, os::unix::fs::MetadataExt, path::{Path, PathBuf}};

use crate::agent::Provider;
use super::{AccountId, home::Homes, registry::{Registry, Store}};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct LaunchAccountScope {
    pub provider: Provider,
    pub canonical_home: PathBuf,
    pub device: u64,
    pub inode: u64,
    /// Optional registry metadata for purge, never the annotation authority.
    pub registered_id: Option<AccountId>,
}

impl LaunchAccountScope {
    /// `environment` must describe the actual child environment after explicit
    /// removals and captured inheritance. Do not consult a later UI selection.
    pub(crate) fn resolve(
        provider: Provider,
        cwd: &Path,
        environment: impl Fn(&str) -> Option<OsString>,
    ) -> Result<Self, String> {
        let registry = Store::installation().and_then(|store| store.load().ok());
        let homes = Homes::installation();
        Self::resolve_with_registry(provider, cwd, environment, registry.as_ref(), homes.as_ref())
    }

    fn resolve_with_registry(
        provider: Provider,
        cwd: &Path,
        environment: impl Fn(&str) -> Option<OsString>,
        registry: Option<&Registry>,
        homes: Option<&Homes>,
    ) -> Result<Self, String> {
        if !cwd.is_absolute() {
            return Err("The provider launch needs an absolute working directory for account scope".into());
        }
        let variable = super::home_variable(provider)
            .ok_or("This provider has no attested credential-directory scope")?;
        let selected = environment(variable).filter(|value| !value.is_empty());
        let path = match selected {
            Some(path) => cwd.join(path),
            None => {
                let home = environment("HOME").filter(|value| !value.is_empty())
                    .ok_or("The provider launch has no effective credential home")?;
                cwd.join(home).join(if provider == Provider::Claude { ".claude" } else { ".codex" })
            }
        };
        let canonical_home = path.canonicalize()
            .map_err(|error| format!("Cannot identify the selected provider home: {error}"))?;
        let metadata = std::fs::metadata(&canonical_home)
            .map_err(|error| format!("Cannot identify the selected provider home: {error}"))?;
        if !metadata.is_dir() {
            return Err("The selected provider home is not a directory".into());
        }
        let mut scope = Self { provider, canonical_home, device: metadata.dev(), inode: metadata.ino(), registered_id: None };
        if let Some(registry) = registry.filter(|registry| !registry.is_newer()) {
            let mut matching = registry.registered().iter().filter(|entry| entry.provider == provider)
                .filter_map(|entry| {
                    let path = entry.linked_home.clone().or_else(|| homes.map(|homes| homes.path(provider, entry.key)))?;
                    let canonical = path.canonicalize().ok()?;
                    let metadata = std::fs::metadata(&canonical).ok()?;
                    (canonical == scope.canonical_home && metadata.dev() == scope.device && metadata.ino() == scope.inode)
                        .then(|| super::Account::linked(provider, entry.key, path).id())
                });
            let first = matching.next();
            // Aliases share physical authority. Never arbitrarily name one as
            // the sole registered owner when several IDs resolve to this home.
            if matching.next().is_none() {
                scope.registered_id = first;
            }
        }
        scope.revalidate()?;
        Ok(scope)
    }

    /// Refuse annotation use after this pathname was replaced by another home.
    pub(crate) fn revalidate(&self) -> Result<(), String> {
        let metadata = std::fs::metadata(&self.canonical_home)
            .map_err(|error| format!("The captured provider home is unavailable: {error}"))?;
        if metadata.is_dir() && metadata.dev() == self.device && metadata.ino() == self.inode {
            Ok(())
        } else {
            Err("The captured provider home was replaced; its annotations are unavailable".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn account_scope_uses_actual_home_aliases_and_refuses_missing_selected_home() {
        let root = tempfile::tempdir().unwrap();
        let actual = root.path().join("selected");
        let alias = root.path().join("alias");
        let system = root.path().join(".claude");
        std::fs::create_dir_all(&actual).unwrap();
        std::fs::create_dir_all(&system).unwrap();
        std::os::unix::fs::symlink(&actual, &alias).unwrap();
        let mut environment = BTreeMap::from([
            ("HOME", root.path().as_os_str().to_owned()),
            ("CLAUDE_CONFIG_DIR", OsString::from("selected")),
            (super::super::SELECTION_MARKER, OsString::from("CLAUDE_CONFIG_DIR=selected")),
        ]);
        let resolve = |environment: &BTreeMap<&str, OsString>| LaunchAccountScope::resolve_with_registry(Provider::Claude, root.path(), |name| environment.get(name).cloned(), None, None);
        let selected = resolve(&environment).unwrap();
        assert_eq!(selected.canonical_home, actual.canonicalize().unwrap());
        environment.insert("CLAUDE_CONFIG_DIR", alias.into_os_string());
        assert_eq!(resolve(&environment).unwrap(), selected);
        environment.insert("CLAUDE_CONFIG_DIR", OsString::from("missing-selected"));
        assert!(resolve(&environment).is_err(), "an unavailable selected home cannot fall back to the available system home");
        environment.remove("CLAUDE_CONFIG_DIR");
        assert_eq!(resolve(&environment).unwrap().canonical_home, system.canonicalize().unwrap());
        assert!(LaunchAccountScope::resolve_with_registry(Provider::Omp, root.path(), |_| None, None, None).is_err());
        assert!(LaunchAccountScope::resolve_with_registry(Provider::Claude, Path::new("relative-cwd"), |name| environment.get(name).cloned(), None, None).is_err());
    }

    #[test]
    fn account_scope_rejects_replacement_and_registry_metadata_cannot_split_aliases() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let mut registry = Registry::default();
        let first = uuid::Uuid::new_v4();
        let register = |key| super::super::registry::Registered { key, provider: Provider::Codex, linked_home: Some(home.clone()), label: None, email: Some(format!("{key}@fixture.invalid")), organization: None, added_at: 0 };
        registry.register(register(first), &[]).unwrap();
        let resolve = |registry: &Registry| LaunchAccountScope::resolve_with_registry(Provider::Codex, root.path(), |name| (name == "CODEX_HOME").then(|| home.as_os_str().to_owned()), Some(registry), None).unwrap();
        let scope = resolve(&registry);
        assert_eq!(scope.registered_id, Some(super::super::Account::linked(Provider::Codex, first, home.clone()).id()));
        registry.register(register(uuid::Uuid::new_v4()), &[]).unwrap();
        let alias_scope = resolve(&registry);
        assert!(alias_scope.registered_id.is_none());
        assert_eq!((alias_scope.device, alias_scope.inode), (scope.device, scope.inode));
        std::fs::rename(&home, root.path().join("old-home")).unwrap();
        std::fs::create_dir(&home).unwrap();
        assert!(scope.revalidate().is_err());
    }
}
