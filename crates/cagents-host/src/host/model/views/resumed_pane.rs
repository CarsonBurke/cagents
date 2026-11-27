//! A chat pane whose conversation the daemon reopened without a window.
//!
//! A window that resumes a conversation saves the process it started into
//! the pane itself. When the daemon resumes one for the CLI, no window is
//! there to do that, so the daemon saves it: otherwise the pane would keep
//! naming the stopped process, and the next window to open it would try to
//! start the conversation a second time.

use super::*;
use crate::{chat::Session, model::CardKind};

impl Registry {
    /// The descriptor the open pane presenting chat `id` saves, if any.
    pub(in crate::host) fn chat_pane_descriptor(
        &self,
        id: crate::model::TerminalId,
    ) -> Result<Option<Session>, String> {
        chat_pane_session(&self.db, id)
    }

    /// Shows `started` in the pane that shows `stopped`, when that pane still
    /// shows exactly the stopped process and [`shows_resumed`] holds for the
    /// two. `started` must be a conversation this daemon has just started
    /// from `stopped`. A pane whose open agent names another process is left
    /// as the agent knows it, since moving the agent is the agent's own
    /// adoption. Returns whether the pane changed.
    pub(in crate::host) fn adopt_resumed_chat_pane(
        &mut self,
        stopped: &Session,
        started: &Session,
    ) -> Result<bool, String> {
        if !shows_resumed(stopped, started) {
            return Ok(false);
        }
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let Some(id) = store::chat_pane(&tx, stopped.id)? else {
            return Ok(false);
        };
        let Some(scope) = owner(&tx, id)? else {
            return Ok(false);
        };
        let mut view = read(&tx, &scope)?;
        let before = view.clone();
        if view.retired.contains(&id) || view.panes.iter().filter(|pane| pane.id == id).count() != 1
        {
            return Ok(false);
        }
        let pane = view.panes.iter_mut().find(|pane| pane.id == id).unwrap();
        let CardKind::Chat(current) = &pane.kind else {
            return Ok(false);
        };
        if !crate::agent_execution::same_chat_process(current, stopped)
            || current.terminal != stopped.terminal
            || current
                .launch
                .session_id
                .as_ref()
                .is_some_and(|id| Some(id) != started.launch.session_id.as_ref())
            || current == started
        {
            return Ok(false);
        }
        let actor = crate::agent_execution::HostedSession::Chat(started.clone());
        let previous = crate::agent_execution::HostedSession::Chat(stopped.clone());
        if super::super::execution::agent_for_session_in(&tx, stopped.id)?
            .filter(|agent| !agent.closed)
            .and_then(|agent| agent.session)
            .is_some_and(|session| {
                !session.same_process(&actor) && !session.same_process(&previous)
            })
        {
            return Ok(false);
        }
        let adopted = crate::workspace_model::views::Pane {
            kind: CardKind::Chat(started.clone()),
            ..pane.clone()
        };
        if store::resource_holder(&tx, &adopted)?.is_some() {
            return Ok(false);
        }
        *pane = adopted;
        view.revision = store::next_revision(&tx)?;
        terminal_retention::sync_view_pins(&tx, &view.panes)?;
        store::store(&tx, &view)?;
        bindings::sync(&tx, &before, &view)?;
        tx.execute(
            "UPDATE metadata SET sequence = sequence + 1 WHERE key = 0",
            [],
        )
        .map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(true)
    }
}

/// Whether `started` can stand for `stopped` in its pane: the same provider
/// conversation, provider and directory, started under a complete proof.
/// In-place recovery can keep its proof. A restored terminal must belong
/// to the new attachment and its daemon.
pub(super) fn shows_resumed(stopped: &Session, started: &Session) -> bool {
    let conversation = |session: &Session| {
        session
            .launch
            .session_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
    };
    started
        .proof
        .as_ref()
        .is_some_and(crate::model::HostSessionProof::is_complete)
        && (started.proof != stopped.proof || started.id == stopped.id)
        && started != stopped
        && started.terminal.as_ref().is_none_or(|terminal| {
            terminal.id == started.id && terminal.continuity.live_proof() == started.proof
        })
        && started.launch.provider == stopped.launch.provider
        && started.launch.recipe.working_directory == stopped.launch.recipe.working_directory
        && conversation(started).is_some()
        // A retained Claude card that never learned its conversation resumes
        // the one the daemon proved its actor confirmed.
        && (conversation(stopped).is_none() || conversation(stopped) == conversation(started))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        chat::Launch,
        model::{
            BootId, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe, TerminalId,
        },
    };

    fn proof(boot: &str, incarnation: &str) -> HostSessionProof {
        HostSessionProof {
            host_session_id: HostSessionId::new(format!("{boot}-{incarnation}")),
            boot_id: BootId::new(boot),
            host_incarnation: HostIncarnation::new(incarnation),
        }
    }

    fn session(id: TerminalId, proof: HostSessionProof) -> Session {
        Session {
            id,
            launch: Launch {
                provider: crate::agent::Provider::Codex,
                recipe: LaunchRecipe {
                    program: "codex".into(),
                    arguments: Vec::new(),
                    working_directory: "/project".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: Some("thread".into()),
                draft_model: None,
            },
            proof: Some(proof),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        }
    }

    #[test]
    fn a_pane_shows_only_a_start_of_its_own_conversation_in_its_own_directory() {
        let id = TerminalId::new();
        let stopped = session(id, proof("earlier", "old"));
        let started = session(id, proof("now", "new"));
        assert!(shows_resumed(&stopped, &started));
        // A successor under a new ID stands for it too.
        assert!(shows_resumed(
            &stopped,
            &Session {
                id: TerminalId::new(),
                ..started.clone()
            }
        ));
        // The stopped process itself is no start.
        assert!(!shows_resumed(&stopped, &stopped));
        let mut other = started.clone();
        other.launch.session_id = Some("another thread".into());
        assert!(!shows_resumed(&stopped, &other));
        let mut elsewhere = started.clone();
        elsewhere.launch.recipe.working_directory = "/elsewhere".into();
        assert!(!shows_resumed(&stopped, &elsewhere));
        let mut provider = started.clone();
        provider.launch.provider = crate::agent::Provider::Antigravity;
        assert!(!shows_resumed(&stopped, &provider));
        let unproved = Session {
            proof: None,
            ..started.clone()
        };
        assert!(!shows_resumed(&stopped, &unproved));
        // A card that never learned its conversation takes the one started.
        let mut unnamed = stopped.clone();
        unnamed.launch.session_id = None;
        assert!(shows_resumed(&unnamed, &started));
        let mut nameless = started;
        nameless.launch.session_id = None;
        assert!(!shows_resumed(&unnamed, &nameless));
    }
}
