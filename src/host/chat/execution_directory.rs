//! Provider scope is established before adapter preparation, independently of
//! the immutable launch descriptor issued to clients.
use super::{error, unavailable};
use crate::{agent::Provider, chat::{Launch, protocol::{Adapter, ExecutionContext, ProcessSpec, TransportSpec}}, terminal_ipc::ProtocolError};
use std::sync::Arc;

pub(super) struct Prepared {
    pub(super) launch: Launch,
    pub(super) context: Option<Arc<ExecutionContext>>,
    preparation_error: Option<String>,
}

pub(super) fn prepare(issued: &Launch, supplied: Option<Arc<ExecutionContext>>) -> Result<Prepared, ProtocolError> {
    let mut launch = issued.clone();
    if launch.needs_read_only_history() {
        return Ok(Prepared { launch, context: supplied, preparation_error: None });
    }
    let physical = match launch.recipe.working_directory.canonicalize() {
        Ok(path) => path,
        Err(error) => return Ok(Prepared { launch, context: supplied, preparation_error: Some(format!("Cannot resolve the provider execution directory: {error}")) }),
    };
    let context = match supplied {
        Some(context) => {
            context.verify_directory(&physical).map_err(unavailable)?;
            if context.directory.is_none() {
                return Err(unavailable("Native preparation requires the original approved directory descriptor"));
            }
            context
        }
        None => {
            let directory = std::fs::File::open(&physical).map_err(|error| unavailable(error.to_string()))?;
            let context = Arc::new(ExecutionContext::new(Some(Arc::new(directory))));
            context.verify_directory(&physical).map_err(unavailable)?;
            context
        }
    };
    launch.recipe.working_directory = physical;
    Ok(Prepared { launch, context: Some(context), preparation_error: None })
}

impl Prepared {
    pub(super) fn authorize(&self, spec: &mut ProcessSpec, adapter: &mut dyn Adapter) -> Result<(), ProtocolError> {
        if matches!(spec.transport, TransportSpec::History | TransportSpec::UnixWebSocket { .. }) { return Ok(()); }
        if let Some(error) = &self.preparation_error { return Err(unavailable(error.clone())); }
        let path = &self.launch.recipe.working_directory;
        let context = self.context.as_ref().ok_or_else(|| unavailable("Provider startup has no pinned execution directory"))?;
        context.verify_directory(path).map_err(unavailable)?;
        if &spec.working_directory != path {
            return Err(error("The provider adapter changed its prepared execution directory"));
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
            && !matches!(spec.transport, TransportSpec::UnixWebSocket { .. }) {
            context.observe_provider_directory(path).map_err(unavailable)?;
        }
        spec.execution = Some(context.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn launch(path: std::path::PathBuf) -> Launch {
        Launch { provider: Provider::Codex, session_id: None, recipe: crate::model::LaunchRecipe {
            program: "codex".into(), arguments: Vec::new(), working_directory: path,
            environment: Default::default(), unset_environment: Default::default(),
        } }
    }
    #[test]
    fn private_launch_is_physical_and_issued_alias_remains_unchanged() {
        let root=tempfile::tempdir_in("/var/tmp").unwrap();
        let first=root.path().join("first"); let other=root.path().join("other"); let alias=root.path().join("alias");
        std::fs::create_dir(&first).unwrap(); std::fs::create_dir(&other).unwrap();
        std::os::unix::fs::symlink(&first,&alias).unwrap();
        let issued=launch(alias.clone());
        let prepared=prepare(&issued,None).unwrap();
        assert_eq!(issued.recipe.working_directory,alias);
        assert_eq!(prepared.launch.recipe.working_directory,first);
        let context=prepared.context.unwrap();
        assert!(!context.provider_directory_observed(&first).unwrap(),"pinning alone cannot certify a prebuilt adapter");
        std::fs::remove_file(&alias).unwrap(); std::os::unix::fs::symlink(&other,&alias).unwrap();
        assert!(context.verify_directory(&first).is_ok());
        assert!(prepare(&issued,Some(context)).is_err(),"the original pin must reject the retargeted alias");
    }
    #[test]
    fn preparation_always_updates_adapter_but_legacy_resume_has_no_close_receipt() {
        struct Hook(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl Adapter for Hook {
            fn relocate_directory(&mut self,_:&std::path::Path)->Result<(),String> {self.0.fetch_add(1,std::sync::atomic::Ordering::SeqCst);Ok(())}
            fn initialize(&mut self)->Result<crate::chat::protocol::Step,String> {Ok(Default::default())}
            fn command(&mut self,_:crate::chat::Command)->Result<crate::chat::protocol::Step,String> {Ok(Default::default())}
            fn receive(&mut self,_:serde_json::Value)->Result<crate::chat::protocol::Step,String> {Ok(Default::default())}
        }
        let root=tempfile::tempdir_in("/var/tmp").unwrap();
        let calls=Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for resumed in [false,true] {
            let mut issued=launch(root.path().into());
            issued.session_id=resumed.then(||uuid::Uuid::new_v4().to_string());
            let prepared=prepare(&issued,None).unwrap();
            let mut spec=ProcessSpec::from(&prepared.launch.recipe);
            prepared.authorize(&mut spec,&mut Hook(calls.clone())).unwrap();
            assert_eq!(prepared.context.as_ref().unwrap().provider_directory_observed(root.path()).unwrap(),!resumed);
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst),2,"unchanged path spellings still require adapter preparation");
    }
    #[test]
    fn approved_directory_receipt_rejects_replaced_inode() {
        let root=tempfile::tempdir_in("/var/tmp").unwrap(); let path=root.path().join("project");
        std::fs::create_dir(&path).unwrap();
        let prepared=prepare(&launch(path.clone()),None).unwrap(); let context=prepared.context.unwrap();
        context.observe_provider_directory(&path).unwrap();
        assert!(context.provider_directory_observed(&path).unwrap());
        std::fs::rename(&path,root.path().join("old")).unwrap(); std::fs::create_dir(&path).unwrap();
        assert!(context.provider_directory_observed(&path).is_err());
    }
}
