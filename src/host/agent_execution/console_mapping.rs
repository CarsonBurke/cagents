//! Private executable permission for an already issued accepted console.
//! This grants no actor adoption, pair release, or execution-directory authority.
use super::{SharedRegistry,Agent,Authority,HostedSession};
use crate::{chat::Session,model::{TerminalSession,LaunchRecipe}};

/// Minted only from the durable accepted Agent and its retained authority.
/// The old published executable/recipe remains exact; only the private command
/// executable can use a separately verified current matched CLI descriptor.
#[derive(Clone)]
pub(crate) struct ConsoleProgramAuthorization {
    registry:SharedRegistry,
    agent:Agent,
    source:Session,
    console:TerminalSession,
    authority:Authority,
}
impl std::fmt::Debug for ConsoleProgramAuthorization {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {
        f.debug_struct("ConsoleProgramAuthorization").field("agent",&self.agent.id)
            .field("session",&self.source.id).finish_non_exhaustive()
    }
}
impl ConsoleProgramAuthorization {
    /// The caller separately holds the lifecycle transition/admission fence,
    /// proves the exact actor/pair, and captures the approved directory FD
    /// before any teardown. No user-supplied descriptor can mint this token.
    pub(in crate::host) fn capture(registry:SharedRegistry,source:&Session,console:&TerminalSession)->Result<Self,String> {
        let (agent,authority)={
            let registry=registry.lock().unwrap_or_else(|p|p.into_inner());
            let agent=registry.agent_for_session(source.id)?.ok_or("The issued provider console has no accepted Agent")?;
            let authority=registry.retained_agent_authority(source.id)?;
            validate(&registry,&agent,source,console,&authority)?;
            (agent,authority)
        };
        Ok(Self {registry,agent,source:source.clone(),console:console.clone(),authority})
    }
    /// Recheck after the matched-CLI probe and immediately before the caller's
    /// private spawn. SQLite/probe work must remain outside HostState locks.
    pub(crate) fn revalidate(&self)->Result<(),String> {
        let registry=self.registry.lock().unwrap_or_else(|p|p.into_inner());
        validate(&registry,&self.agent,&self.source,&self.console,&self.authority)
    }
    pub(crate) fn validate_spawn(&self,id:crate::model::TerminalId,recipe:&LaunchRecipe,authority:&Authority)->Result<(),String> {
        if id!=self.console.id || recipe!=&self.console.launch || authority!=&self.authority {
            return Err("The private console executable permission belongs to another exact issued view or authority".into());
        }
        Ok(())
    }
    pub(crate) fn acquire_spawn(&self,id:crate::model::TerminalId,recipe:&LaunchRecipe,authority:&Authority)->Result<crate::daemon::console_transition::Lease,String> {
        let lease=crate::daemon::console_transition::acquire(self.source.id)?;
        self.validate_spawn(id,recipe,authority)?;
        self.revalidate()?;
        Ok(lease)
    }
}
fn validate(registry:&super::Registry,expected:&Agent,source:&Session,console:&TerminalSession,authority:&Authority)->Result<(),String> {
    let current=registry.agent_for_session(source.id)?.ok_or("The accepted console owner was retired")?;
    if &current!=expected || current.closed || current.console.as_ref()!=Some(console)
        || current.session.as_ref()!=Some(&HostedSession::Chat(source.clone()))
        || source.terminal.as_ref()!=Some(console)
        || crate::agent_execution::console_agent(&console.launch)!=Some(current.id)
        || registry.retained_agent_authority_source(source.id)?!=Some(HostedSession::Chat(source.clone()))
        || &registry.retained_agent_authority(source.id)?!=authority || authority.token().is_none() {
        return Err("The exact accepted provider/console/authority binding changed; no executable was remapped".into());
    }
    Ok(())
}
