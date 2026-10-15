//! Read-only actor capture for Automatic forwarding of a NEW unaccepted send.
//! This neither stops the source nor authorizes any provider prompt or FD grant.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Options {
    generation: u64,
    controls: AccountSwitchOptions,
}
impl Options {
    pub(crate) fn generation(&self) -> u64 { self.generation }
    pub(crate) fn controls(&self) -> &AccountSwitchOptions { &self.controls }
    /// A successor must itself pass a fresh, complete policy capture after
    /// restoration. A model/mode match without policy coverage is insufficient.
    pub(crate) fn same_controls(&self, successor: &Self) -> bool {
        self.controls == successor.controls
    }
}

impl LocalHandle {
    /// Refresh provider policy using its acknowledged read-only settings query,
    /// then capture current controls at an actor-ordered full-readiness barrier.
    /// None means proved busy before capture, never consent to stop or retry a
    /// prompt. An already-admitted getter may settle after the caller's budget.
    pub(crate) fn capture_continuation_options(&self, deadline: Instant) -> Result<Option<Options>, String> {
        if Instant::now() >= deadline { return Err("Automatic continuation capture budget expired".into()); }
        let readiness = self.account_switch_generation()?;
        if Instant::now() >= deadline { return Err("Automatic continuation capture budget expired before its settings read".into()); }
        if readiness.is_none() { return Ok(None); }
        self.command(Command::RefreshPermissions)?;
        if Instant::now() >= deadline { return Err("Automatic continuation capture budget expired after its read-only settings acknowledgement".into()); }
        let (captured, response) = crossbeam_channel::bounded(1);
        self.ownership_command(ActorCommand::CaptureContinuation { deadline, captured })?;
        let result = response.try_recv().map_err(|error| format!("Automatic continuation capture was not acknowledged: {error}"))?;
        if Instant::now() >= deadline { return Err("Automatic continuation capture budget expired before delivery".into()); }
        result
    }
}

pub(super) fn capture_actor(actor: &Actor, deadline: Instant) -> Result<Option<Options>, String> {
    if Instant::now() >= deadline { return Err("Automatic continuation capture budget expired".into()); }
    let snapshot = actor.state.lock().unwrap_or_else(|p| p.into_inner());
    if Instant::now() >= deadline { return Err("Automatic continuation capture budget expired while observing metadata".into()); }
    if actor.update_readiness(&snapshot) != UpdateReadiness::Idle
        || !actor.adapter.account_switch_ready() || !snapshot.queued_messages.is_empty() {
        return Ok(None);
    }
    let policy = actor.adapter.account_continuation_policy()?;
    let result = capture_known(&snapshot, policy)?;
    if Instant::now() >= deadline { return Err("Automatic continuation capture budget expired while validating controls".into()); }
    Ok(Some(result))
}

fn capture_known(snapshot: &Snapshot, policy: super::super::PermissionSettings) -> Result<Options, String> {
    let unknown = || "The provider has not confirmed all current model, effort, speed and permission controls; continue separately with explicit controls".to_string();
    let selected = snapshot.selected_model.as_ref().filter(|value| !value.is_empty()).ok_or_else(unknown)?;
    let model = snapshot.models.iter().find(|model| &model.id == selected).ok_or_else(unknown)?;
    match snapshot.reasoning.as_ref() {
        Some(effort) if model.reasoning.contains(effort) => {},
        _ => return Err(unknown()),
    }
    let mode = snapshot.permission_mode.as_ref().filter(|value| !value.is_empty()).ok_or_else(unknown)?;
    if !snapshot.capabilities.permission_modes.contains(mode)
        // The UI capability means changeable, not known: Claude can report a
        // confirmed Off preference that this account is not allowed to edit.
        || snapshot.fast_mode.is_none()
        || snapshot.permissions.as_ref() != Some(&policy) {
        return Err(unknown());
    }
    let controls = AccountSwitchOptions::capture(snapshot);
    Ok(Options { generation: snapshot.generation, controls })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known() -> Snapshot {
        let mut state = Snapshot::default();
        state.models = vec![super::super::super::Model {id:"current".into(),label:"Current".into(),reasoning:vec!["high".into()]}];
        state.selected_model = Some("current".into()); state.reasoning = Some("high".into());
        state.permission_mode = Some("plan".into());
        state.capabilities.permission_modes = vec!["plan".into()];
        state.capabilities.fast_mode = true; state.fast_mode = Some(super::super::super::FastMode::default());
        state.permissions = Some(super::super::super::PermissionSettings::default());
        state
    }

    #[test]
    fn automatic_continuation_never_treats_missing_supported_controls_as_defaults() {
        let complete = known();
        let policy = complete.permissions.clone().unwrap();
        assert!(capture_known(&complete,policy.clone()).is_ok());
        for missing in ["model","effort","speed","mode","rules"] {
            let mut state = complete.clone();
            match missing {
                "model" => state.selected_model=None,
                "effort" => state.reasoning=None,
                "speed" => state.fast_mode=None,
                "mode" => state.permission_mode=None,
                "rules" => state.permissions=None,
                _ => unreachable!(),
            }
            assert!(capture_known(&state,policy.clone()).is_err(),"{missing}");
        }
        let mut disabled_speed = complete.clone();
        disabled_speed.capabilities.fast_mode=false;
        let controls = capture_known(&disabled_speed,policy.clone()).unwrap();
        assert_eq!(controls.controls().fast_mode,Some(false),"confirmed nonchangeable speed is retained");
        let mut absent_catalog = complete;
        absent_catalog.models[0].reasoning.clear(); absent_catalog.reasoning=None;
        assert!(capture_known(&absent_catalog,policy).is_err(),"a projected empty effort catalog does not attest an unsupported control");
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    struct IdleReader {
        admitted:crossbeam_channel::Sender<()>,
        release:crossbeam_channel::Receiver<()>,
    }
    impl Adapter for IdleReader {
        fn before_account_continuation_readiness(&mut self) {
            self.admitted.send(()).unwrap();
            self.release.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        fn initialize(&mut self) -> Result<Step,String> { Ok(Step::default()) }
        fn command(&mut self, _:Command) -> Result<Step,String> { Err("the busy actor must never read settings".into()) }
        fn receive(&mut self,_:serde_json::Value) -> Result<Step,String> { Ok(Step::default()) }
    }
    #[test]
    fn continuation_busy_metadata_wait_cannot_succeed_after_original_deadline() {
        let directory=tempfile::tempdir().unwrap();
        let recipe=crate::model::LaunchRecipe {program:"/bin/cat".into(),arguments:Vec::new(),working_directory:directory.path().to_path_buf(),environment:Default::default(),unset_environment:Default::default()};
        let spec=ProcessSpec::from(&recipe);
        let mut initial=Step {events:vec![Event::Ready(true),Event::Status {busy:true,detail:String::new()}],..Step::default()};
        let store=super::super::preflight(&spec,&mut initial).unwrap();
        let (admitted,observed)=crossbeam_channel::bounded(1);
        let (release,resume)=crossbeam_channel::bounded(1);
        let handle=LocalHandle::start(spec,Box::new(IdleReader {admitted,release:resume}),initial,store,None,|_|{}).unwrap();
        let capture=handle.clone(); let (entered,started)=crossbeam_channel::bounded(1);
        let worker=thread::spawn(move|| {
            let deadline=Instant::now()+Duration::from_secs(2);
            entered.send(deadline).unwrap();
            capture.capture_continuation_options(deadline)
        });
        let deadline=started.recv_timeout(Duration::from_secs(5)).unwrap();
        observed.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(Instant::now()<deadline,"the actor admitted the query inside its original budget");
        let state=handle.0.state.lock().unwrap();
        release.send(()).unwrap();
        thread::sleep(deadline.saturating_duration_since(Instant::now())+Duration::from_millis(10));
        drop(state);
        assert!(worker.join().unwrap().unwrap_err().contains("budget expired"));
        assert!(!handle.is_completed()); handle.close().unwrap();
    }
}
