//! Per-window presentation only. A missing resident never grants teardown or
//! authority to remove the daemon's shared pane or its saved history.
use super::*;
use crate::workspace_model::views::{Pane,Scope};

#[derive(Clone)]
pub struct Target {
    card:CardId, source:TerminalSession, registry:uuid::Uuid,
    scope:Scope,revision:u64,pane:Pane,token:uuid::Uuid,
    captured:Option<CapturedTerminal>,inventory:u64,presentation_generation:uuid::Uuid,
}
impl std::fmt::Debug for Target {
    fn fmt(&self,formatter:&mut std::fmt::Formatter<'_>)->std::fmt::Result {
        formatter.debug_struct("LocalDismissalTarget").field("card",&self.card).field("registry",&self.registry).field("revision",&self.revision).field("token",&self.token).finish_non_exhaustive()
    }
}
#[derive(Default)]
pub(super) struct State {
    unavailable:HashMap<CardId,Target>,hidden:HashMap<CardId,Target>,
    inventory:u64,last_seen:HashMap<TerminalId,u64>,generations:HashMap<CardId,uuid::Uuid>,
}
impl State {
    pub(super) fn forget(&mut self,card:CardId){self.unavailable.remove(&card);self.hidden.remove(&card);self.generations.insert(card,uuid::Uuid::new_v4());}
}
impl Cagents {
    pub(in crate::app) fn local_terminal_dismissal_target(&self,card:CardId)->Option<Target> {
        if !self.card_state(card).is_some_and(|state|state.existence.is_open()){return None;}
        let source=ended_terminal(self.card_state(card))?.clone();
        if self.workspace_model.identity==crate::workspace_model::NO_REGISTRY || self.terminal_start_in_flight(card) || self.chat_start_pending(card) {return None;}
        let mut panes=self.workspace_model.views.iter().flat_map(|view|view.panes.iter().filter(move|pane|pane.id==card).map(move|pane|(view,pane)));
        let (view,pane)=panes.next()?;
        if panes.next().is_some() || pane.pending_terminal_start || pane.kind!=CardKind::Terminal(source.clone()){return None;}
        Some(Target{card,source,registry:self.workspace_model.identity,scope:view.scope.clone(),revision:view.revision,pane:pane.clone(),token:uuid::Uuid::new_v4(),captured:self.terminal_close_bindings.bound.get(&card).cloned(),inventory:self.terminal_close_bindings.local.inventory,presentation_generation:self.terminal_close_bindings.local.generations.get(&card).copied().unwrap_or_else(uuid::Uuid::nil)})
    }
    fn local_terminal_dismissal_current(&self,target:&Target)->bool {
        if !self.card_state(target.card).is_some_and(|state|state.existence.is_open()){return false;}
        if self.workspace_model.identity!=target.registry || self.terminal_start_in_flight(target.card) || self.chat_start_pending(target.card) || ended_terminal(self.card_state(target.card))!=Some(&target.source) {return false;}
        if !same_captured(target.captured.as_ref(),self.terminal_close_bindings.bound.get(&target.card)){return false;}
        let state=&self.terminal_close_bindings.local;
        if state.generations.get(&target.card).copied().unwrap_or_else(uuid::Uuid::nil)!=target.presentation_generation {return false;}
        if state.last_seen.get(&target.source.id).is_some_and(|seen|*seen>target.inventory){return false;}
        if let Some(CardRuntime::Terminal(surface))=self.cards.get(&target.card) {
            if let Some(binding)=surface.worker_binding(){
                if target.captured.as_ref().is_none_or(|captured|binding.execution_instance!=captured.execution_instance || binding.source.id!=captured.source.id || binding.source.launch!=captured.source.launch || surface.host_proof().as_ref()!=Some(&captured.proof)){return false;}
            }
        }
        let mut owned=self.workspace_model.views.iter().flat_map(|view|view.panes.iter().filter(move|pane|pane.id==target.card).map(move|pane|(view,pane)));
        owned.next().is_some_and(|(view,pane)|view.scope==target.scope&&view.revision==target.revision&&pane==&target.pane) && owned.next().is_none()
    }
    pub(in crate::app) fn offer_local_terminal_dismissal(&mut self,target:Option<Target>)->bool {
        if let Some(target)=target.filter(|target|self.local_terminal_dismissal_current(target)) {
            self.terminal_close_bindings.local.unavailable.insert(target.card,target);
            true
        } else {false}
    }
    pub(in crate::app) fn locally_dismissed_terminal(&self,card:CardId)->bool {
        self.terminal_close_bindings.local.hidden.get(&card).is_some_and(|target|self.local_terminal_dismissal_current(target))
    }
    pub(in crate::app) fn locally_dismissed_terminal_ids(&self)->HashSet<CardId> {
        self.terminal_close_bindings.local.hidden.keys().copied().filter(|card|self.locally_dismissed_terminal(*card)).collect()
    }
    pub(in crate::app) fn prune_local_terminal_dismissals(&mut self) {
        let valid=self.terminal_close_bindings.local.hidden.iter().chain(self.terminal_close_bindings.local.unavailable.iter()).filter(|(_,target)|self.local_terminal_dismissal_current(target)).map(|(_,target)|target.token).collect::<HashSet<_>>();
        let state=&mut self.terminal_close_bindings.local;
        state.hidden.retain(|_,target|valid.contains(&target.token));state.unavailable.retain(|_,target|valid.contains(&target.token));
        let ids=self.durable.cards.iter().filter(|card|card.existence.is_open()).filter_map(|card|match &card.kind{CardKind::Terminal(source)=>Some(source.id),_=>None}).collect::<HashSet<_>>();
        state.last_seen.retain(|id,_|ids.contains(id));
        let cards=self.durable.cards.iter().filter(|card|card.existence.is_open()).map(|card|card.id).collect::<HashSet<_>>();
        state.generations.retain(|card,_|cards.contains(card));
    }
    pub(in crate::app) fn observe_local_terminal_residents(&mut self,terminals:&[crate::terminal::ipc::TerminalDescriptor])->Task<Message> {
        let state=&mut self.terminal_close_bindings.local;
        state.inventory=state.inventory.saturating_add(1);
        let present=terminals.iter().map(|terminal|terminal.session.id).collect::<HashSet<_>>();
        let hidden_before=state.hidden.len();
        let reveal=state.hidden.values().any(|target|present.contains(&target.source.id));
        for id in present {state.last_seen.insert(id,state.inventory);}
        self.prune_local_terminal_dismissals();
        if reveal || self.terminal_close_bindings.local.hidden.len()<hidden_before {self.reconcile_shared_views()} else {Task::none()}
    }
    pub(in crate::app) fn dismiss_terminal_locally(&mut self,target:Target)->Task<Message> {
        if self.modal_is_open() || !self.local_terminal_dismissal_current(&target) || self.terminal_close_bindings.local.unavailable.get(&target.card).is_none_or(|current|current.token!=target.token){return Task::none();}
        self.terminal_close_bindings.local.unavailable.remove(&target.card);
        self.terminal_close_bindings.local.hidden.insert(target.card,target);
        self.status=Some(TransientNotice::warning("Pane dismissed in this window. Its sessions, shared pane and saved history were preserved."));
        self.reconcile_shared_views()
    }
    pub(in crate::app) fn terminal_local_presentation_started(&mut self,card:CardId)->Task<Message> {
        let reveal=self.terminal_close_bindings.local.hidden.contains_key(&card);
        self.terminal_close_bindings.local.forget(card);
        if reveal {self.reconcile_shared_views()} else {Task::none()}
    }
    pub(in crate::app) fn restore_locally_dismissed_terminals(&mut self)->Task<Message> {
        self.terminal_close_bindings.local.hidden.clear();self.reconcile_shared_views()
    }
    pub(in crate::app) fn local_terminal_unavailable_notice(&self,card:CardId)->Option<Element<'_,Message>> {
        let target=self.terminal_close_bindings.local.unavailable.get(&card).filter(|target|self.local_terminal_dismissal_current(target))?;
        Some(notice_surface(NoticeSeverity::Warning,widget::column::with_children(vec![
            notice_body("This saved terminal is unavailable on this connection."),
            design::caption("Dismiss hides this pane only in this window. Sessions, other windows and saved history stay available.").into(),
            crate::ui_controls::button::quiet("Dismiss locally").id(card_control_id(card,"terminal.dismiss-locally")).on_press_maybe((!self.modal_is_open()).then_some(Message::DismissTerminalLocally(target.clone()))).into(),
        ]).spacing(space::S).into(),None,false))
    }
    pub(in crate::app) fn locally_dismissed_terminals_notice(&self)->Option<Element<'_,Message>> {
        let count=self.terminal_close_bindings.local.hidden.keys().filter(|card|self.locally_dismissed_terminal(**card)).count();
        (count>0).then(||notice_surface(NoticeSeverity::Warning,widget::row::with_children(vec![
            design::caption(format!("{count} saved panes hidden in this window")).into(),
            crate::ui_controls::button::quiet("Show dismissed panes").on_press_maybe((!self.modal_is_open()).then_some(Message::RestoreLocallyDismissedTerminals)).into(),
        ]).spacing(space::S).into(),None,false))
    }
    pub(in crate::app) fn preserve_locally_dismissed_order(&self,original:&[CardId],visible:Vec<CardId>)->Vec<CardId> {
        let mut visible=VecDeque::from(visible);let mut merged=Vec::new();
        for id in original {if self.locally_dismissed_terminal(*id){merged.push(*id);}else if let Some(next)=visible.pop_front(){merged.push(next);}}
        merged.extend(visible);merged
    }
}
