//! Provider scope is established before adapter preparation, independently of
//! the immutable launch descriptor issued to clients.
use super::{error, unavailable};
use crate::{
    agent::Provider,
    chat::{
        Launch,
        protocol::{Adapter, ExecutionContext, ProcessSpec, TransportSpec},
    },
    terminal_ipc::ProtocolError,
};
use std::sync::Arc;

pub(super) struct Prepared {
    pub(super) launch: Launch,
    pub(super) context: Option<Arc<ExecutionContext>>,
    preparation_error: Option<String>,
}

pub(super) fn prepare(
    issued: &Launch,
    supplied: Option<Arc<ExecutionContext>>,
) -> Result<Prepared, ProtocolError> {
    prepare_at(issued, supplied, None)
}

/// An approved handoff may execute in a relocated inode while preserving the
/// entire issued recipe. The caller retains this same pin before teardown;
/// supplying a path by itself never authorizes reopening another directory.
pub(super) fn prepare_at(
    issued: &Launch,
    supplied: Option<Arc<ExecutionContext>>,
    operational_directory: Option<&std::path::Path>,
) -> Result<Prepared, ProtocolError> {
    let mut launch = issued.clone();
    if operational_directory.is_some() && supplied.is_none() {
        return Err(unavailable(
            "A relocated provider launch requires its original approved directory descriptor",
        ));
    }
    if let Some(path) = operational_directory {
        supplied.as_ref().expect("checked captured handoff").verify_directory(path).map_err(unavailable)?;
        // History classification must use the approved private location too:
        // an issued Claude path becoming absent after Locate does not make
        // the same still-pinned operational directory a history-only reader.
        launch.recipe.working_directory=path.to_path_buf();
    }
    if launch.needs_read_only_history() {
        if operational_directory.is_some() {
            return Err(error("Read-only history cannot consume an execution-directory handoff"));
        }
        return Ok(Prepared {
            launch,
            context: supplied,
            preparation_error: None,
        });
    }
    let physical = match operational_directory
        .unwrap_or(&launch.recipe.working_directory)
        .canonicalize()
    {
        Ok(path) => path,
        Err(error) => {
            return Ok(Prepared {
                launch,
                context: supplied,
                preparation_error: Some(format!(
                    "Cannot resolve the provider execution directory: {error}"
                )),
            });
        }
    };
    let context = match supplied {
        Some(context) => {
            context.verify_directory(&physical).map_err(unavailable)?;
            if context.directory_descriptor().is_none() {
                return Err(unavailable(
                    "Native preparation requires the original approved directory descriptor",
                ));
            }
            context
        }
        None => {
            let context = Arc::new(ExecutionContext::capture(&physical).map_err(unavailable)?);
            context.verify_directory(&physical).map_err(unavailable)?;
            context
        }
    };
    launch.recipe.working_directory = physical;
    Ok(Prepared {
        launch,
        context: Some(context),
        preparation_error: None,
    })
}

impl Prepared {
    pub(super) fn authorize(
        &self,
        spec: &mut ProcessSpec,
        adapter: &mut dyn Adapter,
    ) -> Result<(), ProtocolError> {
        if matches!(
            spec.transport,
            TransportSpec::History | TransportSpec::UnixWebSocket { .. }
        ) {
            return Ok(());
        }
        if let Some(error) = &self.preparation_error {
            return Err(unavailable(error.clone()));
        }
        let path = &self.launch.recipe.working_directory;
        let context = self
            .context
            .as_ref()
            .ok_or_else(|| unavailable("Provider startup has no pinned execution directory"))?;
        context.verify_directory(path).map_err(unavailable)?;
        if &spec.working_directory != path {
            return Err(error(
                "The provider adapter changed its prepared execution directory",
            ));
        }
        adapter.relocate_directory(path).map_err(unavailable)?;
        context.verify_directory(path).map_err(unavailable)?;
        // Codex's hook overrides the next turn cwd; Claude's process cwd and
        // cached operational paths are prepared together. OMP has no verified
        // resumed-tool cwd override here, so its scoped close stays unknown.
        // A legacy resumed thread also needs authoritative current-tool scope;
        // overriding a future turn cannot prove where existing work executes.
        // Proven retained actors keep their original resident receipt.
        if self.launch.session_id.is_none()
            && matches!(self.launch.provider, Provider::Codex | Provider::Claude)
            && !matches!(spec.transport, TransportSpec::UnixWebSocket { .. })
        {
            context
                .observe_provider_directory(path)
                .map_err(unavailable)?;
        }
        spec.execution = Some(context.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn launch(path: std::path::PathBuf) -> Launch {
        Launch {
            provider: Provider::Codex,
            session_id: None,
            recipe: crate::model::LaunchRecipe {
                program: "codex".into(),
                arguments: Vec::new(),
                working_directory: path,
                environment: Default::default(),
                unset_environment: Default::default(),
            },
        }
    }
    #[test]
    fn private_launch_is_physical_and_issued_alias_remains_unchanged() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let first = root.path().join("first");
        let other = root.path().join("other");
        let alias = root.path().join("alias");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&other).unwrap();
        std::os::unix::fs::symlink(&first, &alias).unwrap();
        let issued = launch(alias.clone());
        let prepared = prepare(&issued, None).unwrap();
        assert_eq!(issued.recipe.working_directory, alias);
        assert_eq!(prepared.launch.recipe.working_directory, first);
        let context = prepared.context.unwrap();
        assert!(
            !context.provider_directory_observed(&first).unwrap(),
            "pinning alone cannot certify a prebuilt adapter"
        );
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&other, &alias).unwrap();
        assert!(context.verify_directory(&first).is_ok());
        assert!(
            prepare(&issued, Some(context)).is_err(),
            "the original pin must reject the retargeted alias"
        );
    }
    #[test]
    fn preparation_always_updates_adapter_but_legacy_resume_has_no_close_receipt() {
        struct Hook(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl Adapter for Hook {
            fn relocate_directory(&mut self, _: &std::path::Path) -> Result<(), String> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
            fn initialize(&mut self) -> Result<crate::chat::protocol::Step, String> {
                Ok(Default::default())
            }
            fn command(
                &mut self,
                _: crate::chat::Command,
            ) -> Result<crate::chat::protocol::Step, String> {
                Ok(Default::default())
            }
            fn receive(
                &mut self,
                _: serde_json::Value,
            ) -> Result<crate::chat::protocol::Step, String> {
                Ok(Default::default())
            }
        }
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for resumed in [false, true] {
            let mut issued = launch(root.path().into());
            issued.session_id = resumed.then(|| uuid::Uuid::new_v4().to_string());
            let prepared = prepare(&issued, None).unwrap();
            let mut spec = ProcessSpec::from(&prepared.launch.recipe);
            prepared
                .authorize(&mut spec, &mut Hook(calls.clone()))
                .unwrap();
            assert_eq!(
                prepared
                    .context
                    .as_ref()
                    .unwrap()
                    .provider_directory_observed(root.path())
                    .unwrap(),
                !resumed
            );
        }
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "unchanged path spellings still require adapter preparation"
        );
    }
    #[test]
    fn approved_directory_receipt_rejects_replaced_inode() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let path = root.path().join("project");
        std::fs::create_dir(&path).unwrap();
        let prepared = prepare(&launch(path.clone()), None).unwrap();
        let context = prepared.context.unwrap();
        context.observe_provider_directory(&path).unwrap();
        assert!(context.provider_directory_observed(&path).unwrap());
        std::fs::rename(&path, root.path().join("old")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(context.provider_directory_observed(&path).is_err());
    }

    #[test]
    fn relocated_preparation_reuses_the_pre_teardown_pin_without_reopening_issued_path() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let issued_path = root.path().join("issued");
        let moved = root.path().join("located");
        let other = root.path().join("unrelated");
        std::fs::create_dir(&issued_path).unwrap();
        std::fs::create_dir(&other).unwrap();
        let mut issued = launch(issued_path.clone());
        issued.session_id = Some(uuid::Uuid::new_v4().to_string());
        let before = issued.clone();
        let context = Arc::new(ExecutionContext::capture(&issued_path).unwrap());
        std::fs::rename(&issued_path, &moved).unwrap();
        let prepared = prepare_at(&issued, Some(context.clone()), Some(&moved)).unwrap();
        assert_eq!(issued, before, "private execution does not rewrite the issued identity");
        assert_eq!(prepared.launch.recipe.working_directory, moved);
        assert!(Arc::ptr_eq(prepared.context.as_ref().unwrap(), &context));
        assert!(!issued_path.exists());
        assert!(!context.provider_directory_observed(&moved).unwrap(), "a physical pin alone does not claim resumed-tool scope");
        let mut claude=issued.clone();claude.provider=Provider::Claude;claude.recipe.program="claude".into();
        assert!(claude.needs_read_only_history(),"the immutable original spelling is now missing");
        let prepared_claude=prepare_at(&claude,Some(context.clone()),Some(&moved)).unwrap();
        assert!(!prepared_claude.launch.needs_read_only_history(),"the verified same-inode operational handoff remains runnable");
        assert_eq!(claude.recipe.working_directory,issued_path);
        assert!(prepare_at(&issued, Some(context.clone()), Some(&other)).is_err());
        assert!(prepare_at(&issued, None, Some(&moved)).is_err());
        std::fs::rename(&moved, root.path().join("moved-again")).unwrap();
        std::fs::create_dir(&moved).unwrap();
        assert!(prepare_at(&issued, Some(context), Some(&moved)).is_err(), "a replacement clone must not consume the captured handoff");
    }
}
