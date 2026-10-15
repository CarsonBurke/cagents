//! Conservative selector partition for saved UI metadata, not account authority.
//! Live account access still requires the daemon's captured HomeIdentity.
use super::Session;
use std::os::unix::ffi::OsStrExt;
use sha2::{Digest, Sha256};

pub(crate) fn metadata_key(session: &Session) -> Option<String> {
    let id = session.launch.session_id.as_deref()?.trim();
    if id.is_empty() { return None; }
    let recipe = &session.launch.recipe;
    if !recipe.working_directory.is_absolute() { return None; }
    // Unknown/inherited selectors and unsupported provider profiles cannot
    // safely reuse provider:UUID metadata from another account's history.
    let variable = crate::account::home_variable(session.launch.provider)?;
    let recorded = |name: &str| (!recipe.unset_environment.contains(name))
        .then(|| recipe.environment.get(name)).flatten().filter(|value| !value.is_empty());
    let root = if let Some(selected) = recorded(variable) {
        recipe.working_directory.join(selected)
    } else {
        if !recipe.unset_environment.contains(variable) { return None; }
        recipe.working_directory.join(recorded("HOME")?).join(match session.launch.provider {
            crate::agent::Provider::Claude => ".claude",
            crate::agent::Provider::Codex => ".codex",
            crate::agent::Provider::Omp => return None,
        })
    };
    // These explicit selectors can override the account stored in that home.
    // Never hash credentials or pretend a recipe proves the effective login.
    if crate::account::credential_overrides(session.launch.provider).iter()
        .any(|name| !recipe.unset_environment.contains(*name)) { return None; }
    let mut digest = Sha256::new();
    for part in [session.launch.provider.label().as_bytes(), id.as_bytes(),
        root.as_os_str().as_bytes(), recipe.working_directory.as_os_str().as_bytes()] {
        digest.update((part.len() as u64).to_le_bytes());
        digest.update(part);
    }
    Some(format!("conversation-v2:{:x}", digest.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn session(provider: crate::agent::Provider) -> Session {
        Session { id: crate::model::TerminalId::new(), proof: None, terminal: None,
            owned_codex_listener: None, native_continuity: Default::default(),
            launch: super::super::Launch { provider, session_id: Some("copied-provider-uuid".into()),
                recipe: crate::model::LaunchRecipe { program: provider.executable().into(), arguments: Vec::new(),
                    working_directory: "/project".into(), environment: Default::default(),
                    unset_environment: crate::account::credential_overrides(provider).iter().map(|name| (*name).to_owned()).collect() } } }
    }
    #[test]
    fn copied_provider_ids_partition_by_recorded_home_and_project_without_reading_ambient_state() {
        for provider in [crate::agent::Provider::Claude, crate::agent::Provider::Codex] {
            let mut a = session(provider);
            let variable = crate::account::home_variable(provider).unwrap();
            a.launch.recipe.environment.insert(variable.into(), "/accounts/A".into());
            let a_key = metadata_key(&a).unwrap();
            let mut inherited_credentials = a.clone();
            inherited_credentials.launch.recipe.unset_environment.remove(crate::account::credential_overrides(provider)[0]);
            assert!(metadata_key(&inherited_credentials).is_none(), "omitted credential values can still be inherited");
            let mut b = a.clone(); b.launch.recipe.environment.insert(variable.into(), "/accounts/B".into());
            assert_ne!(metadata_key(&b).unwrap(), a_key);
            b = a.clone(); b.launch.recipe.working_directory = "/another-project".into();
            assert_ne!(metadata_key(&b).unwrap(), a_key);
            b = a.clone(); b.id = crate::model::TerminalId::new(); b.launch.recipe.arguments = vec!["--resume".into(), "copied-provider-uuid".into(), "--model".into(), "different-model".into()];
            assert_eq!(metadata_key(&b).unwrap(), a_key);
            b = a.clone(); b.launch.recipe.environment.insert(variable.into(), "../accounts/A".into());
            let relative_key = metadata_key(&b).unwrap();
            b.launch.recipe.environment.insert(variable.into(), "/project/../accounts/A".into());
            assert_eq!(metadata_key(&b).unwrap(), relative_key);
            a.launch.recipe.environment.clear(); assert!(metadata_key(&a).is_none());
            a.launch.recipe.environment.insert("HOME".into(), "/recorded/home".into());
            assert!(metadata_key(&a).is_none(), "HOME does not rule out an inherited provider selector");
            a.launch.recipe.unset_environment.insert(variable.into());
            assert!(metadata_key(&a).is_some(), "explicit selector removal permits recorded HOME default");
            a.launch.recipe.unset_environment.remove(variable);
            a.launch.recipe.environment.remove("HOME");
            a.launch.recipe.environment.insert(variable.into(), "/accounts/A".into());
            a.launch.recipe.unset_environment.insert(variable.into()); assert!(metadata_key(&a).is_none());
        }
        assert!(metadata_key(&session(crate::agent::Provider::Omp)).is_none());
    }
}
