//! Read-only checks of prepare refusals that the frozen source impact omits.
//!
//! A source can still race or fail while preparing its PTYs or writing the
//! checkpoint. Such a failure is reported with the completed floor actions;
//! it must never expand destructive consent after conversations were closed.
use super::{UpdateError, impact};
use crate::{chat::Session, terminal_ipc::TerminalDescriptor};
use std::collections::BTreeSet;

pub(super) fn needs_service_stop(
    preview: &impact::Preview,
    chats: &[Session],
    terminals: &[TerminalDescriptor],
) -> Result<bool, UpdateError> {
    let Some(daemon) = &preview.daemon else {
        return Ok(false);
    };
    if inventory_needs_service_stop(daemon, chats, terminals) {
        return Ok(true);
    }
    match crate::upgrade::handoff::Store::existing().and_then(|store| {
        store.map_or(Ok(false), |store| {
            store.conflicts_with_source(&daemon.capability.proof)
        })
    }) {
        Ok(conflict) => Ok(conflict),
        Err(error) => {
            // The source cannot prepare over unreadable recovery evidence. A
            // confirmed service stop archives it instead of replaying it.
            eprintln!("The source checkpoint cannot be reused: {error}");
            Ok(true)
        }
    }
}

fn inventory_needs_service_stop(
    daemon: &impact::DaemonImpact,
    chats: &[Session],
    terminals: &[TerminalDescriptor],
) -> bool {
    use impact::{Disposition, SessionKind};
    if !daemon.blockers.is_empty()
        || daemon.sessions.iter().any(|item| {
            item.kind == SessionKind::Terminal && item.disposition == Disposition::Blocked
        })
    {
        return true;
    }
    let mut conversations = BTreeSet::new();
    for chat in chats {
        let Some(item) = daemon
            .sessions
            .iter()
            .find(|item| item.kind == SessionKind::NativeChat && item.id == chat.id)
        else {
            // Pending launches are listed before they acquire an impact row.
            return true;
        };
        if item.disposition == Disposition::Blocked {
            // The floor closes this exact owner before prepare.
            continue;
        }
        if chat.proof.as_ref() != Some(&daemon.capability.proof)
            || crate::upgrade::handoff::validate_chat_source(chat, &chat.launch).is_err()
        {
            return true;
        }
        if let Some(id) = &chat.launch.session_id
            && !conversations.insert((chat.launch.provider.executable(), id))
        {
            return true;
        }
        if let Some(pair) = &chat.terminal {
            if pair.id != chat.id
                || pair.continuity.live_proof().as_ref() != Some(&daemon.capability.proof)
            {
                return true;
            }
            // List reports a live paired console independently. Its issued
            // allocation and proof must agree with the native source, even
            // though mutable hints and presentation may have advanced.
            if let Some(current) = terminals.iter().find(|row| row.session.id == pair.id)
                && current
                    .session
                    .continuity
                    .can_resume_live(&daemon.capability.proof)
                && (current.session.launch != pair.launch
                    || current.session.owned_codex_listener != pair.owned_codex_listener
                    || current.session.recovery_origin != pair.recovery_origin
                    || current.session.predecessor != pair.predecessor)
            {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::Provider,
        chat::{Launch, NativeContinuity},
        model::{
            BootId, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe, TerminalId,
        },
        terminal_ipc::ShutdownCapability,
    };

    fn fixture() -> (impact::DaemonImpact, Session) {
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("host"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("source"),
        };
        let chat = Session {
            id: TerminalId::new(),
            launch: Launch {
                provider: Provider::Claude,
                recipe: LaunchRecipe {
                    program: "claude".into(),
                    arguments: Vec::new(),
                    working_directory: "/work".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: Some(uuid::Uuid::new_v4().to_string()),
            },
            proof: Some(proof.clone()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: NativeContinuity::ReconnectOnly,
        };
        let daemon = impact::DaemonImpact {
            capability: ShutdownCapability {
                proof,
                token: "token".into(),
                inventory_generation: 1,
                live_terminals: 1,
                background_subagents: 0,
                background_identity: String::new(),
                expires_at_unix_ms: i64::MAX,
            },
            policy: impact::Policy::WhenIdle,
            sessions: vec![impact::SessionImpact {
                id: chat.id,
                kind: impact::SessionKind::NativeChat,
                title: "chat".into(),
                identity: "owner".into(),
                disposition: impact::Disposition::Resumes,
                reason: String::new(),
                interrupted_subagents: Vec::new(),
            }],
            blockers: Vec::new(),
        };
        (daemon, chat)
    }

    #[test]
    fn blocked_chats_do_not_expand_the_stop_to_carried_terminals() {
        let (mut daemon, mut chat) = fixture();
        chat.launch.session_id = None;
        daemon.sessions[0].disposition = impact::Disposition::Blocked;
        assert!(!inventory_needs_service_stop(&daemon, &[chat], &[]));
    }

    #[test]
    fn remaining_chat_ownership_and_source_failures_are_predicted_before_closes() {
        let (daemon, chat) = fixture();
        assert!(!inventory_needs_service_stop(&daemon, &[chat.clone()], &[]));
        let mut changed = chat.clone();
        changed.proof = None;
        assert!(inventory_needs_service_stop(&daemon, &[changed], &[]));
        let mut changed = chat.clone();
        changed.launch.session_id = None;
        assert!(inventory_needs_service_stop(&daemon, &[changed], &[]));
        let mut changed = chat.clone();
        changed.id = TerminalId::new();
        assert!(inventory_needs_service_stop(&daemon, &[changed], &[]));
        let mut other = chat.clone();
        other.id = TerminalId::new();
        let mut duplicate = daemon.clone();
        let mut item = duplicate.sessions[0].clone();
        item.id = other.id;
        duplicate.sessions.push(item);
        assert!(inventory_needs_service_stop(
            &duplicate,
            &[chat, other],
            &[]
        ));
    }

    #[test]
    fn paired_console_allocation_changes_require_service_scope() {
        let (daemon, mut chat) = fixture();
        let mut terminal =
            TerminalDescriptor::relaunchable(chat.id, chat.launch.recipe.clone(), None);
        terminal.session.continuity = crate::model::TerminalContinuity::Live {
            host_session_id: daemon.capability.proof.host_session_id.clone(),
            boot_id: daemon.capability.proof.boot_id.clone(),
            host_incarnation: daemon.capability.proof.host_incarnation.clone(),
        };
        chat.terminal = Some(terminal.session.clone());
        assert!(!inventory_needs_service_stop(
            &daemon,
            &[chat.clone()],
            &[terminal.clone()]
        ));
        terminal.session.launch.arguments.push("changed".into());
        assert!(inventory_needs_service_stop(&daemon, &[chat], &[terminal]));
    }

    #[test]
    fn checkpoint_collision_is_read_without_rewriting_or_replaying_it() {
        use std::os::unix::fs::PermissionsExt;
        let (daemon, _) = fixture();
        let directory = tempfile::tempdir().unwrap();
        let store = crate::upgrade::handoff::Store::open(directory.path()).unwrap();
        assert!(
            !store
                .conflicts_with_source(&daemon.capability.proof)
                .unwrap()
        );
        for (phase, proof, expected) in [
            ("Prepared", daemon.capability.proof.clone(), false),
            ("Ready", daemon.capability.proof.clone(), true),
            ("Restoring", daemon.capability.proof.clone(), true),
            (
                "Prepared",
                HostSessionProof {
                    host_incarnation: HostIncarnation::new("another"),
                    ..daemon.capability.proof.clone()
                },
                true,
            ),
        ] {
            let bytes = serde_json::to_vec(
                &serde_json::json!({"version": 5, "proof": proof, "phase": phase}),
            )
            .unwrap();
            std::fs::write(&store.path, &bytes).unwrap();
            std::fs::set_permissions(&store.path, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(
                store
                    .conflicts_with_source(&daemon.capability.proof)
                    .unwrap(),
                expected
            );
            assert_eq!(std::fs::read(&store.path).unwrap(), bytes);
        }
    }
}
