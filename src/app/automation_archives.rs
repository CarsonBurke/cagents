//! Bounded discovery of older retained scheduled results in the selected view.
use super::*;
use crate::{automations::{ArchiveAgentPage,Request,Response},workspace_model::views::Scope};

#[derive(Clone,Debug,PartialEq,Eq)]
struct Key {scope:Scope,identity:uuid::Uuid,sequence:u64,epoch:u64}
struct Pending {generation:u64,key:Key,proof:Option<crate::model::HostSessionProof>}
struct Probe {generation:u64,epoch:u64,kind:CardKind}
#[derive(Default)]
pub(super) struct State {epoch:u64,generation:u64,pending:Option<Pending>,completed:Option<Key>,probes:HashMap<CardId,Probe>,checked:HashMap<CardId,crate::model::TerminalId>}
impl State {
    pub(super) fn reconnected(&mut self) {self.epoch=self.epoch.wrapping_add(1);self.completed=None;self.checked.clear();}
}

impl Cagents {
    pub(super) fn startup_archive_probe_pending(&self,card:CardId)->bool {
        self.card_state(card).is_some_and(|state|self.startup_archive_probe_for_state(state))
    }

    fn startup_archive_probe_for_state(&self,state:&CardState)->bool {
        let card=state.id;
        if !state.existence.is_open() || self.durable.automation_archives.contains_key(&card) {return false;}
        if self.chat_ui.archive_discovery.probes.contains_key(&card) {return true;}
        let session_id=match &state.kind {CardKind::Chat(session)=>session.id,CardKind::Terminal(session)=>session.id,_=>return false,};
        // Negative discovery is only a cheap retry suppression, never a Ready
        // receipt. A distinct native identity or daemon reconnect retries it;
        // ordinary live-pane qualification remains mandatory after a miss.
        if self.chat_ui.archive_discovery.checked.get(&card)==Some(&session_id) {return false;}
        match &state.kind {
            CardKind::Chat(session)=> !session.is_unstarted()
                && !self.chat_has_live_handle(card),
            CardKind::Terminal(session)=>session.launch.arguments==["agent".to_owned(),"console".to_owned(),crate::model::AgentId::from_uuid(card.as_uuid()).to_string()],
            _=>false,
        }
    }

    pub(super) fn pending_startup_archive_probes(&self)->usize {self.chat_ui.archive_discovery.probes.len()}

    pub(super) fn probe_startup_automation_archives(&mut self,slots:usize)->Vec<Task<Message>> {
        let open=self.durable.cards.iter().filter(|card|card.existence.is_open()).map(|card|card.id).collect::<HashSet<_>>();
        self.chat_ui.archive_discovery.checked.retain(|card,_|open.contains(card));
        self.chat_ui.archive_discovery.probes.retain(|card,_|open.contains(card));
        let candidates=self.durable.cards.iter().filter(|card|card.existence.is_open()
            && !self.chat_start_pending(card.id)
            && !self.chat_ui.archive_discovery.probes.contains_key(&card.id)
            && self.startup_archive_probe_for_state(card))
            .take(slots).map(|card|(card.id,card.kind.clone())).collect::<Vec<_>>();
        candidates.into_iter().map(|(card,kind)| {
            let state=&mut self.chat_ui.archive_discovery;
            state.generation=state.generation.wrapping_add(1).max(1);let generation=state.generation;
            state.probes.insert(card,Probe {generation,epoch:state.epoch,kind});
            cosmic::task::future(async move {
                let result=tokio::task::spawn_blocking(move || {
                    // This bounded probe never starts a service or a provider.
                    let socket=crate::terminal::ipc::daemon_socket_path().map_err(|error|error.to_string())?;
                    let mut client=crate::control::ControlClient::connect(&socket).map_err(|error|error.to_string())?;
                    match client.automation(crate::agent_execution::Authority::default(),Request::ArchivedConversation {agent:crate::model::AgentId::from_uuid(card.as_uuid())}).map_err(|error|error.to_string())? {
                        Response::ArchivedConversation(archive)=>Ok(*archive),
                        _=>Err("No exact saved automation result was returned".into()),
                    }
                }).await.map_err(|error|error.to_string()).and_then(|result|result);
                Message::ChatUi(chat_ui::Action::ArchiveProbe(card,generation,Arc::new(result)))
            })
        }).collect()
    }

    pub(super) fn startup_archive_probe_received(&mut self,card:CardId,generation:u64,result:Arc<Result<crate::automations::ArchivedConversation,String>>)->Task<Message> {
        let state=&mut self.chat_ui.archive_discovery;
        if !state.probes.get(&card).is_some_and(|probe|probe.generation==generation) {return Task::none();}
        let probe=state.probes.remove(&card).unwrap();
        if probe.epoch!=state.epoch {return Task::none();}
        if self.card_state(card).is_none_or(|state|!state.existence.is_open() || state.kind!=probe.kind) {return Task::none();}
        let native=match &probe.kind {CardKind::Chat(session)=>session.id,CardKind::Terminal(session)=>session.id,_=>return Task::none(),};
        self.chat_ui.archive_discovery.checked.insert(card,native);
        let Ok(archive)=result.as_ref() else {return Task::none();};
        let id=crate::model::AgentId::from_uuid(card.as_uuid());
        if crate::automations::archive::ArchivedPaneReadiness::qualify_kind(id,&probe.kind,archive).is_err()
            || !self.can_present_automation_archive(card,&archive.agent)
            || self.workspace_model.identity!=crate::workspace_model::NO_REGISTRY && crate::workspace_model::views::managed_display(&self.workspace_model,&archive.agent).is_none() {return Task::none();}
        // A private exact closed-owner receipt permits presentation conversion
        // of this existing pane, including older hidden panes absent from the
        // bounded model push. No new pane or provider identity is adopted.
        Arc::make_mut(&mut self.durable).automation_archives.insert(card,id);
        self.prepare_automation_archive(card,&archive.agent);
        self.persist();
        self.qualify_startup_automation_archives()
    }

    fn archive_discovery_key(&self)->Option<Key> {
        if self.workspace_model.identity==crate::workspace_model::NO_REGISTRY {return None;}
        let scope=self.durable.active_channel.map(Scope::Channel).or_else(||
            self.active_workspace.as_deref().and_then(|path|self.worktree_view_scope(path)))?;
        Some(Key {scope,identity:self.workspace_model.identity,sequence:self.workspace_model.sequence,epoch:self.chat_ui.archive_discovery.epoch})
    }

    pub(super) fn discover_scope_automation_archives(&mut self)->Task<Message> {
        let Some(key)=self.archive_discovery_key() else {return Task::none();};
        let state=&self.chat_ui.archive_discovery;
        if state.pending.is_some() || state.completed.as_ref()==Some(&key) {return Task::none();}
        self.read_scope_automation_archives(key,None,None)
    }

    fn read_scope_automation_archives(&mut self,key:Key,after:Option<crate::model::AgentId>,proof:Option<crate::model::HostSessionProof>)->Task<Message> {
        let state=&mut self.chat_ui.archive_discovery;
        state.generation=state.generation.wrapping_add(1).max(1);
        let generation=state.generation;
        state.pending=Some(Pending {generation,key:key.clone(),proof});
        cosmic::task::future(async move {
            let result=tokio::task::spawn_blocking(move || {
                // Discovery never starts a daemon or an agent.
                let socket=crate::terminal::ipc::daemon_socket_path().map_err(|error|error.to_string())?;
                let mut client=crate::control::ControlClient::connect(&socket).map_err(|error|error.to_string())?;
                let proof=client.proof().clone();
                let response=client.automation(crate::agent_execution::Authority::default(),Request::ArchivedAgents {scope:key.scope,identity:key.identity,sequence:key.sequence,after,limit:64}).map_err(|error|error.to_string())?;
                match response {
                    Response::ArchivedAgents(page) if page.proof==proof=>Ok(page),
                    _=>Err("The saved-result discovery response has changed ownership".into()),
                }
            }).await.map_err(|error|error.to_string()).and_then(|result|result);
            Message::ChatUi(chat_ui::Action::ArchiveAgents(generation,Arc::new(result)))
        })
    }

    pub(super) fn scope_automation_archives_received(&mut self,generation:u64,result:Arc<Result<ArchiveAgentPage,String>>)->Task<Message> {
        if !self.chat_ui.archive_discovery.pending.as_ref().is_some_and(|pending|pending.generation==generation) {return Task::none();}
        let pending=self.chat_ui.archive_discovery.pending.take().unwrap();
        if self.archive_discovery_key().as_ref()!=Some(&pending.key) {
            return self.discover_scope_automation_archives();
        }
        let page=match result.as_ref() {
            Ok(page) if page.scope==pending.key.scope && page.identity==pending.key.identity && page.sequence==pending.key.sequence
                && page.proof.is_complete() && pending.proof.as_ref().is_none_or(|proof|proof==&page.proof)=>page,
            _=>{
                self.chat_ui.archive_discovery.completed=Some(pending.key);
                self.status=Some(TransientNotice::warning("Saved scheduled results could not be discovered. Reopen this view after the daemon reconnects, or inspect run history."));
                return Task::none();
            }
        };
        let index=crate::workspace_model::views::ManagedDisplayIndex::new(&self.workspace_model);
        let candidates=page.agents.iter().filter_map(|agent|index.resolve(agent).filter(|display|display.scope==page.scope).map(|display|(agent.clone(),display))).collect::<Vec<_>>();
        let mut tasks=Vec::new();let mut changed=false;
        for (agent,display) in candidates {
            let card=CardId::from_uuid(agent.id.as_uuid());
            if !self.can_present_automation_archive(card,&agent) {continue;}
            if self.durable.automation_archives.get(&card)!=Some(&agent.id) {
                Arc::make_mut(&mut self.durable).automation_archives.insert(card,agent.id);changed=true;
            }
            changed|=self.prepare_automation_archive(card,&agent);
            if let Some(task)=self.adopt_execution_agent_display(&agent,display) {changed=true;tasks.push(task);}
        }
        if changed {self.persist();tasks.push(self.sync_chrome());}
        let visible=self.workspace.mounted_visibility().filter_map(|(card,class)|(class==VisibilityClass::Visible).then_some(card)).collect();
        tasks.push(self.hydrate_visible_automation_archives(&visible));
        if let Some(after)=page.next {
            tasks.push(self.read_scope_automation_archives(pending.key,Some(after),Some(page.proof.clone())));
        } else {self.chat_ui.archive_discovery.completed=Some(pending.key);}
        Task::batch(tasks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_hidden_console_archive_qualifies_without_a_saved_map_or_global_push() {
        use crate::agent_execution::{Agent,HostedSession,Origin,Placement,Spec,Surface};
        let mut app=crate::app::tests::context_inspector_test_app(true);
        let card=app.workspace.focused().unwrap();
        let CardKind::Terminal(mut console)=app.card_state(card).unwrap().kind.clone() else {panic!("console fixture")};
        let id=crate::model::AgentId::from_uuid(card.as_uuid());
        let proof=crate::model::HostSessionProof {host_session_id:crate::model::HostSessionId::new("hidden-archive"),boot_id:crate::model::BootId::new("hidden-boot"),host_incarnation:crate::model::HostIncarnation::new("hidden-incarnation")};
        console.launch.program="cagents".into();console.launch.arguments=vec!["agent".into(),"console".into(),id.to_string()];
        console.continuity=crate::model::TerminalContinuity::Live {host_session_id:proof.host_session_id.clone(),boot_id:proof.boot_id.clone(),host_incarnation:proof.host_incarnation.clone()};
        let path=console.launch.working_directory.clone();
        let mut session=crate::chat::fresh_session(crate::agent::Provider::Claude,console.id,path.clone(),&crate::account::Account::system(crate::agent::Provider::Claude)).unwrap();
        session.proof=Some(proof);session.launch.session_id=Some("hidden-confirmed-conversation".into());
        let agent=Agent {name:None,id,origin:Origin::Automation {id:uuid::Uuid::new_v4()},spec:Spec {provider:session.launch.provider,account:None,target:crate::agent_execution::Target {repository:path.clone(),directory:path.clone(),placement:None},surface:Surface::Terminal,model:None,permission_mode:None,reasoning_effort:None},placement:Placement::Worktree {repository:path.clone(),path},session:Some(HostedSession::Chat(session.clone())),console:Some(console.clone()),created_ms:1,closed:true};
        let archive=crate::automations::ArchivedConversation {agent,snapshot:crate::chat::Snapshot {session_id:session.launch.session_id.clone(),exited:true,read_only_reason:Some("Saved scheduled result".into()),..Default::default()}};
        app.card_state_mut(card).unwrap().kind=CardKind::Terminal(console);
        app.card_state_mut(card).unwrap().exposure=Exposure::Offscreen;
        app.cards.insert(card,CardRuntime::Failed("old daemon no longer owns this console".into()));
        assert!(app.durable.automation_archives.is_empty());
        assert!(app.workspace_model.archived_automation_agents.is_empty());
        assert_eq!(app.archived_pane_readiness(card),Ok(Some(false)),"an unqualified possible archive waits; it never counts as Ready");
        drop(app.qualify_startup_automation_archives());
        let generation=app.chat_ui.archive_discovery.probes[&card].generation;
        app.device_ui.open=true;assert!(app.modal_is_open());
        drop(app.update(Message::ChatUi(chat_ui::Action::ArchiveProbe(card,generation,Arc::new(Ok(archive))))));
        assert!(app.modal_is_open(),"background qualification never dismisses the user's dialog");
        assert_eq!(app.pending_startup_archive_probes(),0,"completion under a modal must release its bounded probe slot");
        assert_eq!(app.durable.automation_archives.get(&card),Some(&id));
        assert!(matches!(&app.card_state(card).unwrap().kind,CardKind::Chat(saved) if saved.id==session.id && saved.terminal.is_some()));
        assert!(app.chat_start_pending(card),"the typed closed-owner probe transitions only into read-only payload hydration");
        assert_eq!(app.archived_pane_readiness(card),Ok(Some(false)),"probe conversion alone is not a hydrated readiness receipt");
        assert!(!app.chat_has_live_handle(card));
    }

    #[test]
    fn startup_archive_probes_are_bounded_and_never_qualify_a_miss_or_replacement() {
        let mut app=crate::app::tests::context_inspector_test_app(true);
        let original=app.card_state(app.workspace.focused().unwrap()).unwrap().clone();
        for _ in 0..1025 {
            let mut card=original.clone();card.id=CardId::new();
            let CardKind::Terminal(terminal)=&mut card.kind else {unreachable!()};
            terminal.id=crate::model::TerminalId::new();
            terminal.launch.arguments=vec!["agent".into(),"console".into(),crate::model::AgentId::from_uuid(card.id.as_uuid()).to_string()];
            card.exposure=Exposure::Offscreen;Arc::make_mut(&mut app.durable).cards.push(card);
        }
        drop(app.qualify_startup_automation_archives());
        assert_eq!(app.pending_startup_archive_probes(),4);
        let (&card,probe)=app.chat_ui.archive_discovery.probes.iter().next().unwrap();let generation=probe.generation;
        app.device_ui.open=true;assert!(app.modal_is_open());
        drop(app.update(Message::ChatUi(chat_ui::Action::ArchiveProbe(card,generation,Arc::new(Err("ordinary live pane has no saved archive".into()))))));
        assert_eq!(app.pending_startup_archive_probes(),3);
        assert_eq!(app.archived_pane_readiness(card),Ok(None),"a miss keeps ordinary live-pane qualification mandatory");
        assert!(app.durable.automation_archives.is_empty());
        drop(app.qualify_startup_automation_archives());
        assert_eq!(app.pending_startup_archive_probes(),4);
        let (&card,probe)=app.chat_ui.archive_discovery.probes.iter().next().unwrap();let generation=probe.generation;
        let CardKind::Terminal(current)=&mut app.card_state_mut(card).unwrap().kind else {unreachable!()};
        current.id=crate::model::TerminalId::new();
        drop(app.startup_archive_probe_received(card,generation,Arc::new(Err("stale reply".into()))));
        assert!(app.durable.automation_archives.is_empty());
        assert!(app.startup_archive_probe_pending(card),"a distinct replacement retains its own qualification path");
    }
}
