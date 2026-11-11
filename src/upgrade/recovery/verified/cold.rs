//! Current-incarnation evidence after authenticated orphan adoption. The
//! immutable original outcome remains the only classification of Prepared.
use super::super::*;
use crate::terminal_ipc::TerminalDescriptor;
use std::collections::BTreeMap;

type Aliases = BTreeMap<TerminalId, Vec<super::super::super::handoff::ChatSource>>;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Completeness {
    Verified,
    Unverified { reason: Incomplete },
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Incomplete {
    RosterOwnership,
    OriginalDescriptors,
    PausedPartitionChanged,
    AcceptedChat { id: TerminalId },
    AcceptedTerminal { id: TerminalId },
}

#[derive(Serialize, Deserialize)]
struct Publication {
    version: u16,
    witness: SettledWitness,
    serving: HostSessionProof,
    completeness: Completeness,
    terminals: Vec<TerminalDescriptor>,
    chats: Vec<Session>,
    aliases: Aliases,
}

pub(super) struct ColdView(Publication);

fn name(hash: &str) -> String {
    format!("daemon-upgrade.cold-outcome.{hash}.json")
}

fn chat_alias(issued: &super::super::super::handoff::ChatSource, source: &Session) -> bool {
    if issued.source == *source {
        return true;
    }
    let mut confirmed = issued.source.clone();
    confirmed.launch.session_id = issued.launch.session_id.clone();
    confirmed == *source
}

fn terminal_alias(source: &TerminalSession, issued: &TerminalSession) -> bool {
    let mut normalized = issued.clone();
    if source.continuity.live_proof().is_some()
        && source.continuity.live_proof() == issued.continuity.live_proof()
    {
        normalized.continuity = source.continuity.clone();
    }
    source == &normalized
}

fn complete(
    settled: &AlreadyRecovered,
    serving: &HostSessionProof,
    terminals: &[TerminalDescriptor],
    chats: &[Session],
    aliases: &Aliases,
) -> Result<(), Incomplete> {
    let terminal_ids: BTreeSet<_> = terminals.iter().map(|item| item.session.id).collect();
    let chat_ids: BTreeSet<_> = chats.iter().map(|item| item.id).collect();
    if !serving.is_complete()
        || serving == &settled.source
        || serving.boot_id != settled.source.boot_id
        || terminal_ids.len() != terminals.len()
        || chat_ids.len() != chats.len()
        || terminals
            .iter()
            .any(|item| item.session.continuity.live_proof().as_ref() != Some(serving))
        || chats
            .iter()
            .any(|item| item.proof.as_ref() != Some(serving))
    {
        return Err(Incomplete::RosterOwnership);
    }
    if settled.paused_chats.iter().any(|id| chat_ids.contains(id))
        || settled
            .paused_terminals
            .iter()
            .any(|id| terminal_ids.contains(id))
    {
        return Err(Incomplete::PausedPartitionChanged);
    }
    let expected_chats: BTreeSet<_> = settled
        .census
        .chats
        .iter()
        .map(|item| item.id)
        .filter(|id| !settled.paused_chats.contains(id))
        .collect();
    let expected_terminals: BTreeSet<_> = settled
        .census
        .terminal_ids()
        .into_iter()
        .filter(|id| !settled.paused_terminals.contains(id))
        .collect();
    if settled
        .witness
        .adopted_chats
        .iter()
        .map(|item| item.id)
        .collect::<BTreeSet<_>>()
        != expected_chats
        || settled.witness.adopted_chats.len() != expected_chats.len()
        || settled
            .witness
            .adopted_terminals
            .iter()
            .map(|item| item.session.id)
            .collect::<BTreeSet<_>>()
            != expected_terminals
        || settled.witness.adopted_terminals.len() != expected_terminals.len()
    {
        return Err(Incomplete::OriginalDescriptors);
    }
    let historical = Census {
        chats: settled.witness.adopted_chats.clone(),
        terminals: settled
            .witness
            .adopted_terminals
            .iter()
            .map(|item| item.session.clone())
            .collect(),
        ..Default::default()
    };
    for source in settled
        .census
        .chats
        .iter()
        .filter(|item| expected_chats.contains(&item.id))
    {
        let current = chats
            .iter()
            .find(|item| item.id == source.id)
            .ok_or(Incomplete::AcceptedChat { id: source.id })?;
        let original = settled
            .witness
            .adopted_chats
            .iter()
            .find(|item| item.id == source.id)
            .ok_or(Incomplete::OriginalDescriptors)?;
        let issued = aliases
            .get(&source.id)
            .ok_or(Incomplete::AcceptedChat { id: source.id })?;
        // Both the pre-update source and the original settled adopter must be
        // exact aliases authenticated by the holder which was actually adopted.
        if original.proof.as_ref() != Some(&settled.witness.successor)
            || !issued.iter().any(|item| chat_alias(item, source))
            || !issued.iter().any(|item| chat_alias(item, original))
            || !verified_chat(
                &settled.census,
                current,
                serving,
                terminals,
                &settled.witness.target,
            )
            || !verified_chat(
                &historical,
                current,
                serving,
                terminals,
                &settled.witness.target,
            )
        {
            return Err(Incomplete::AcceptedChat { id: source.id });
        }
    }
    for original in &settled.witness.adopted_terminals {
        let current = terminals
            .iter()
            .find(|item| item.session.id == original.session.id)
            .ok_or(Incomplete::AcceptedTerminal {
                id: original.session.id,
            })?;
        if original.session.continuity.live_proof().as_ref() != Some(&settled.witness.successor)
            || !current
                .upgrade_sources
                .iter()
                .any(|source| terminal_alias(&original.session, source))
            || !verified_terminal(&settled.census, current, serving, &settled.witness.target)
        {
            return Err(Incomplete::AcceptedTerminal {
                id: original.session.id,
            });
        }
    }
    Ok(())
}

impl ColdView {
    pub(super) fn load(
        checkpoint: &Path,
        settled: &AlreadyRecovered,
        serving: &HostSessionProof,
        target: &Path,
        terminals: &[TerminalDescriptor],
        chats: &[Session],
    ) -> Result<Self, String> {
        let bytes =
            read_checkpoint(&checkpoint.with_file_name(name(&settled.witness.checkpoint_hash)))?
                .ok_or("The current daemon has no exact settled-recovery observation")?;
        let publication: Publication = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if publication.version != 1
            || publication.witness != settled.witness
            || publication.serving != *serving
            || publication.witness.target != target
            || publication.witness.target_hash != executable_hash(target)?
            || publication.completeness != Completeness::Verified
        {
            return Err(
                "The current daemon has not verified the exact original recovery inventory".into(),
            );
        }
        complete(
            settled,
            serving,
            &publication.terminals,
            &publication.chats,
            &publication.aliases,
        )
        .map_err(|reason| format!("Current settled-recovery lineage is unverified: {reason:?}"))?;
        if publication.chats.len() != chats.len()
            || publication.terminals.len() != terminals.len()
            || chats.iter().any(|item| !publication.chats.contains(item))
            || terminals.iter().any(|item| {
                !publication
                    .terminals
                    .iter()
                    .any(|saved| same_adopted_terminal(saved, item, serving))
            })
        {
            return Err("Current inventory changed after settled-recovery observation".into());
        }
        Ok(Self(publication))
    }

    pub(super) fn proves_chat(&self, source: &Session, current: &Session) -> bool {
        source.id == current.id
            && source
                .proof
                .as_ref()
                .is_some_and(HostSessionProof::is_complete)
            && current.proof.as_ref() == Some(&self.0.serving)
            && self.0.chats.contains(current)
            && self
                .0
                .aliases
                .get(&source.id)
                .is_some_and(|aliases| aliases.iter().any(|issued| chat_alias(issued, source)))
    }
}

/// Only called after the host committed authenticated orphan adoption. Missing
/// historical work is an honest negative observation, not a new paused owner
/// and not a reason to prevent the remaining real service from becoming Ready.
#[cfg(any(feature = "host", test))]
pub(crate) fn publish(
    checkpoint: &Path,
    target: &Path,
    settled: &AlreadyRecovered,
    serving: &HostSessionProof,
    terminals: &[TerminalDescriptor],
    chats: &[Session],
    aliases: &Aliases,
) -> Result<(), String> {
    let current = classify_retained_prepared(checkpoint, target)?
        .ok_or("The exact settled Prepared outcome disappeared before publication")?;
    if current.witness != settled.witness {
        return Err("The original settled recovery changed before publication".into());
    }
    let completeness = match complete(settled, serving, terminals, chats, aliases) {
        Ok(()) => Completeness::Verified,
        Err(reason) => Completeness::Unverified { reason },
    };
    let publication = Publication {
        version: 1,
        witness: settled.witness.clone(),
        serving: serving.clone(),
        completeness,
        terminals: terminals.to_vec(),
        chats: chats.to_vec(),
        aliases: aliases.clone(),
    };
    let bytes = serde_json::to_vec(&publication).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > LIMIT {
        return Err("Current settled-recovery observation exceeds its size limit".into());
    }
    let path = checkpoint.with_file_name(name(&settled.witness.checkpoint_hash));
    let temporary = path.with_file_name(format!(".cold-outcome-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|e| e.to_string())?;
        fs::rename(&temporary, &path).map_err(|e| e.to_string())?;
        File::open(
            path.parent()
                .ok_or("Current recovery observation has no directory")?,
        )
        .and_then(|directory| directory.sync_all())
        .map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Fixture, proof};
    use super::*;
    use crate::{
        agent::Provider,
        model::TerminalContinuity,
        update::recovery::{Census as RootCensus, Outcome, compare_with_recovery},
    };

    fn adopted(
        fixture: &Fixture,
        incarnation: &str,
        prior_chats: &[Session],
        prior_terminals: &[TerminalDescriptor],
    ) -> (
        HostSessionProof,
        Vec<TerminalDescriptor>,
        Vec<Session>,
        Aliases,
    ) {
        let serving = proof(incarnation);
        let mut chat = fixture.current.clone();
        chat.proof = Some(serving.clone());
        let mut terminal = fixture.record.adopted_terminal_descriptors[0].clone();
        terminal.session.continuity = TerminalContinuity::Live {
            host_session_id: serving.host_session_id.clone(),
            boot_id: serving.boot_id.clone(),
            host_incarnation: serving.host_incarnation.clone(),
        };
        terminal
            .upgrade_sources
            .extend(prior_terminals.iter().map(|item| item.session.clone()));
        let aliases = [fixture.source.clone(), fixture.current.clone()]
            .into_iter()
            .chain(prior_chats.iter().cloned())
            .map(|source| super::super::super::super::handoff::ChatSource {
                source,
                launch: chat.launch.clone(),
            })
            .collect();
        (
            serving,
            vec![terminal],
            vec![chat],
            [(fixture.source.id, aliases)].into(),
        )
    }

    fn census(fixture: &Fixture) -> RootCensus {
        serde_json::from_value(serde_json::json!({
            "version":1, "transaction":fixture.record.job, "source":fixture.record.source,
            "terminals":fixture.record.census.terminals, "chats":fixture.record.census.chats,
            "checkpoint_before":null,
        }))
        .unwrap()
    }

    #[test]
    fn cold_recovery_current_proof_c_and_d_verify_live_terminal_native_and_original_paused_peer() {
        for provider in [Provider::Claude, Provider::Codex] {
            let fixture = Fixture::new(provider, true);
            let original_checkpoint = fs::read(&fixture.checkpoint).unwrap();
            let original_path = fixture
                .checkpoint
                .with_file_name(recovery_name(&fixture.record.checkpoint_hash));
            let original_record = fs::read(&original_path).unwrap();
            let settled = classify_retained_prepared(&fixture.checkpoint, &fixture.target)
                .unwrap()
                .unwrap();
            let expected = census(&fixture);
            let mut prior_chats = Vec::new();
            let mut prior_terminals = fixture.record.adopted_terminal_descriptors.clone();
            for incarnation in ["cold-c", "cold-d"] {
                let (serving, terminals, chats, aliases) =
                    adopted(&fixture, incarnation, &prior_chats, &prior_terminals);
                if incarnation == "cold-d" {
                    assert!(
                        super::super::VerifiedView::load_at(
                            &fixture.checkpoint,
                            &serving,
                            &fixture.target,
                            &terminals,
                            &chats
                        )
                        .is_err(),
                        "a positive C observation cannot stand in for a fresh D publication"
                    );
                }
                publish(
                    &fixture.checkpoint,
                    &fixture.target,
                    &settled,
                    &serving,
                    &terminals,
                    &chats,
                    &aliases,
                )
                .unwrap();
                let view = super::super::VerifiedView::load_at(
                    &fixture.checkpoint,
                    &serving,
                    &fixture.target,
                    &terminals,
                    &chats,
                )
                .unwrap()
                .unwrap();
                assert!(view.proves_chat(&fixture.source, &chats[0]));
                assert!(
                    view.proves_chat(&fixture.current, &chats[0]),
                    "the original B descriptor is an exact authenticated alias"
                );
                assert!(
                    matches!(compare_with_recovery(&expected, &serving, &terminals, &chats, &Default::default(), Some(&view), None),
                    Outcome::Usable { terminals:1, chats:1, paused_chats, paused_terminals, .. }
                        if paused_chats == fixture.record.paused_chats && paused_terminals.is_empty())
                );
                assert_eq!(fs::read(&fixture.checkpoint).unwrap(), original_checkpoint);
                assert_eq!(fs::read(&original_path).unwrap(), original_record);
                prior_chats.extend(chats);
                prior_terminals.extend(terminals);
            }
        }
    }

    #[test]
    fn cold_recovery_foreign_or_changed_same_id_cannot_replace_exact_holder_ancestry() {
        let fixture = Fixture::new(Provider::Claude, true);
        let settled = classify_retained_prepared(&fixture.checkpoint, &fixture.target)
            .unwrap()
            .unwrap();
        for mutation in 0..5 {
            let (serving, terminals, mut chats, mut aliases) = adopted(
                &fixture,
                "cold-c",
                &[],
                &fixture.record.adopted_terminal_descriptors,
            );
            match mutation {
                0 => aliases
                    .get_mut(&fixture.source.id)
                    .unwrap()
                    .retain(|item| item.source != fixture.current),
                1 => aliases.get_mut(&fixture.source.id).unwrap()[0]
                    .source
                    .launch
                    .recipe
                    .arguments
                    .push("--foreign".into()),
                2 => chats[0].proof = Some(proof("foreign")),
                3 => chats[0].launch.session_id = Some(uuid::Uuid::new_v4().to_string()),
                _ => {
                    chats[0]
                        .launch
                        .recipe
                        .environment
                        .insert("ACCOUNT".into(), "foreign".into());
                }
            }
            publish(
                &fixture.checkpoint,
                &fixture.target,
                &settled,
                &serving,
                &terminals,
                &chats,
                &aliases,
            )
            .unwrap();
            assert!(
                super::super::VerifiedView::load_at(
                    &fixture.checkpoint,
                    &serving,
                    &fixture.target,
                    &terminals,
                    &chats
                )
                .is_err(),
                "mutation {mutation}"
            );
        }
    }

    #[test]
    fn cold_recovery_closed_original_native_leaves_other_work_ready_but_root_census_unverified() {
        let fixture = Fixture::new(Provider::Claude, true);
        let settled = classify_retained_prepared(&fixture.checkpoint, &fixture.target)
            .unwrap()
            .unwrap();
        let (serving, terminals, chats, aliases) = adopted(
            &fixture,
            "cold-c",
            &[],
            &fixture.record.adopted_terminal_descriptors,
        );
        publish(
            &fixture.checkpoint,
            &fixture.target,
            &settled,
            &serving,
            &terminals,
            &chats,
            &aliases,
        )
        .unwrap();
        // Closing the old native owner is legitimate. Publishing the remaining
        // authenticated service succeeds without reopening or changing history.
        publish(
            &fixture.checkpoint,
            &fixture.target,
            &settled,
            &serving,
            &terminals,
            &[],
            &aliases,
        )
        .unwrap();
        let bytes = read_checkpoint(
            &fixture
                .checkpoint
                .with_file_name(name(&settled.witness.checkpoint_hash)),
        )
        .unwrap()
        .unwrap();
        let observation: Publication = serde_json::from_slice(&bytes).unwrap();
        assert!(
            matches!(observation.completeness, Completeness::Unverified { reason: Incomplete::AcceptedChat { id } } if id == fixture.source.id)
        );
        assert_eq!(observation.terminals, terminals);
        assert!(observation.chats.is_empty());
        assert!(
            super::super::VerifiedView::load_at(
                &fixture.checkpoint,
                &serving,
                &fixture.target,
                &terminals,
                &[]
            )
            .is_err()
        );
        assert!(matches!(
            compare_with_recovery(
                &census(&fixture),
                &serving,
                &terminals,
                &[],
                &Default::default(),
                None,
                None
            ),
            Outcome::Unverified {
                surviving_terminals: 1,
                ..
            }
        ));
        assert_eq!(
            settled.paused_chats, fixture.record.paused_chats,
            "missing accepted native was not relabeled paused"
        );
    }
}
