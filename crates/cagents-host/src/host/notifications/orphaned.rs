//! Reclaim exact attention left by an already-closed background session.
use crate::{
    daemon::holder_registry::{self, Kind},
    model::TerminalId,
    notifications::Target,
    terminal_ipc::StartupState,
};

use super::{super::model::Registry, SharedHost};

pub(in crate::host) fn collect(host: &SharedHost, model: &Registry) -> Result<usize, String> {
    let (notifications, generation, evidence) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.startup_state() != StartupState::Ready || state.shutting_down {
            return Ok(0);
        }
        let (Some(notifications), Some(evidence)) = (&state.notifications, &state.upgrade_evidence)
        else {
            return Ok(0);
        };
        (
            notifications.clone(),
            state.inventory_generation,
            evidence.clone(),
        )
    };
    // Choose exact IDs before the slower inventory reads. A replacement
    // notice that arrives during the pass has separate acknowledgement.
    let sequence = model.saved_session_inventory_sequence()?;
    let mut candidates = Vec::new();
    for notice in notifications.snapshot().notices {
        if !notice.acknowledged
            && let Target::Session(session) = notice.event.target
            && model.has_closed_session_for_attention(session)?
        {
            candidates.push((session, notice.id));
        }
    }
    if candidates.is_empty() {
        return Ok(0);
    }

    if evidence.load()?.is_some() {
        return Ok(0);
    }
    let mut saved = model.saved_session_inventory()?;
    saved.extend(evidence.unattached()?.into_iter().map(|entry| entry.id));
    include_abandoned(&evidence, &mut saved)?;
    let floor = crate::upgrade::floor::load()?;
    for record in &floor.records {
        saved.extend(record.terminals.iter().copied());
        for session in record.chats.iter().chain(&record.aliases).chain(
            record
                .presentations
                .iter()
                .filter(|pane| !pane.restored)
                .map(|pane| &pane.session),
        ) {
            saved.insert(session.id);
            saved.extend(session.terminal.iter().map(|terminal| terminal.id));
        }
    }

    let terminals = holder_registry::directory(Kind::Terminal)?;
    let native = holder_registry::directory(Kind::Native)?;
    let released = without_recovery(candidates, &saved, &terminals, &native)?;

    if model.saved_session_inventory_sequence()? != sequence {
        return Ok(0);
    }
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    if state.inventory_generation != generation
        || state.startup_state() != StartupState::Ready
        || state.shutting_down
    {
        return Ok(0);
    }
    let mut acknowledged = 0;
    for (session, id) in released {
        if !state.may_restore_session_notice(session) {
            notifications.acknowledge(id);
            acknowledged += 1;
        }
    }
    Ok(acknowledged)
}

fn include_abandoned(
    evidence: &crate::upgrade::handoff::Store,
    saved: &mut std::collections::BTreeSet<TerminalId>,
) -> Result<(), String> {
    let Some(checkpoint) = evidence.load_abandoned()? else {
        return Ok(());
    };
    // The archive is retained even after identity reconciliation, and is
    // never replayed. Conservatively keep only the sessions it can name.
    for (current, previous) in checkpoint
        .terminals
        .iter()
        .map(|entry| (&entry.source, &entry.previous_sources))
        .chain(
            checkpoint
                .carried
                .iter()
                .map(|entry| (&entry.source, &entry.previous_sources)),
        )
    {
        saved.insert(current.id);
        saved.extend(previous.iter().map(|entry| entry.id));
    }
    for entry in &checkpoint.chats {
        for source in std::iter::once(&entry.source)
            .chain(entry.previous_sources.iter().map(|entry| &entry.source))
        {
            saved.insert(source.id);
            saved.extend(source.terminal.iter().map(|terminal| terminal.id));
        }
    }
    Ok(())
}

fn without_recovery(
    candidates: Vec<(TerminalId, uuid::Uuid)>,
    saved: &std::collections::BTreeSet<TerminalId>,
    terminals: &std::path::Path,
    native: &std::path::Path,
) -> Result<Vec<(TerminalId, uuid::Uuid)>, String> {
    let mut released = Vec::new();
    for (session, id) in candidates {
        if !saved.contains(&session)
            && no_holder(terminals, session)?
            && no_holder(native, session)?
        {
            released.push((session, id));
        }
    }
    Ok(released)
}

fn no_holder(directory: &std::path::Path, id: TerminalId) -> Result<bool, String> {
    let socket = holder_registry::socket(directory, id);
    if holder_registry::execution_record(&socket)
        .map_err(|e| e.to_string())?
        .is_some()
    {
        return Ok(false);
    }
    match std::fs::symlink_metadata(socket) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_abandoned_checkpoint_protects_only_the_sessions_it_names() {
        use crate::model::{
            BootId, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe,
            TerminalContinuity, TerminalSession,
        };
        use crate::upgrade::handoff::{Checkpoint, Store, Terminal};

        let root = tempfile::tempdir().unwrap();
        let upgrade = Store::open(&root.path().join("upgrade")).unwrap();
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("upgrade-source"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        };
        let recipe = LaunchRecipe::shell(root.path().to_owned()).unwrap();
        let archived = TerminalId::new();
        let unrelated = TerminalId::new();
        upgrade
            .save(&Checkpoint::new(
                proof.clone(),
                vec![Terminal {
                    source: TerminalSession {
                        id: archived,
                        owned_codex_listener: None,
                        recovery_origin: None,
                        predecessor: None,
                        launch: recipe.clone(),
                        transcript_path: None,
                        resume_hint: None,
                        continuity: TerminalContinuity::DetachedSameBoot {
                            host_session_id: proof.host_session_id,
                            boot_id: proof.boot_id,
                            host_incarnation: proof.host_incarnation,
                        },
                    },
                    previous_sources: Vec::new(),
                    relaunches: Vec::new(),
                    recipe,
                    options: Default::default(),
                }],
                Vec::new(),
                Vec::new(),
            ))
            .unwrap();
        upgrade.archive().unwrap();

        let mut saved = std::collections::BTreeSet::new();
        include_abandoned(&upgrade, &mut saved).unwrap();
        let release = uuid::Uuid::new_v4();
        let candidates = vec![(archived, uuid::Uuid::new_v4()), (unrelated, release)];
        assert_eq!(
            without_recovery(candidates, &saved, root.path(), root.path()).unwrap(),
            [(unrelated, release)]
        );
    }

    #[test]
    fn absent_holder_differs_from_an_unreadable_or_unsettled_holder() {
        let directory = tempfile::tempdir().unwrap();
        let id = TerminalId::new();
        assert!(no_holder(directory.path(), id).unwrap());
        let socket = holder_registry::socket(directory.path(), id);
        std::fs::write(&socket, b"present").unwrap();
        assert!(!no_holder(directory.path(), id).unwrap());
        std::fs::write(socket.with_extension("json"), b"{").unwrap();
        assert!(no_holder(directory.path(), id).is_err());
    }
}
