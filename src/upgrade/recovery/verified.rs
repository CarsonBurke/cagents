//! Read-only evidence for one settled Prepared recovery. This module never
//! reconstructs a receipt or adopts, attaches, resumes or starts a provider.
pub(crate) mod cold;
pub(crate) mod ready;
use super::*;
use crate::terminal_ipc::TerminalDescriptor;

pub(crate) struct VerifiedView {
    record: RecoveryRecord,
    issued_chats: Vec<super::super::handoff::Chat>,
    cold: Option<cold::ColdView>,
    retained: Retention,
}

#[derive(Clone, Default)]
struct Retention(Option<super::super::floor::Record>);

impl Retention {
    fn load(source: &HostSessionProof) -> Result<Self, String> {
        super::super::floor::record_for_source(source).map(Self)
    }

    fn chat(&self, original: &Session) -> bool {
        let Some(record) = &self.0 else {
            return false;
        };
        original.proof.as_ref() == Some(&record.source)
            && record.chats.iter().any(|source| {
                (source == original
                    || (record.aliases.contains(original)
                        && super::super::floor::same_allocation(source, original)))
                    && record
                        .snapshots
                        .iter()
                        .filter(|saved| saved.source == *source)
                        .count()
                        == 1
            })
    }

    fn terminal(&self, original: &TerminalSession) -> bool {
        self.0.as_ref().is_some_and(|record| {
            original.continuity.live_proof().as_ref() == Some(&record.source)
                && record.terminals.contains(&original.id)
        })
    }
}

#[derive(Deserialize)]
struct IssuedCheckpoint {
    proof: HostSessionProof,
    phase: super::super::handoff::Phase,
    chats: Vec<super::super::handoff::Chat>,
}

/// Metadata may precede the archive rename. Verify the actual private bytes,
/// retaining the original outcome identity through every retirement generation.
fn archive_bytes(
    checkpoint: &Path,
    target: &Path,
    settled: &AlreadyRecovered,
) -> Result<Vec<u8>, String> {
    let proof = archived_proof(checkpoint, target)?
        .ok_or("The original recovery has no verified checkpoint archive metadata")?;
    if proof.checkpoint_sha256 != settled.witness.checkpoint_hash
        || proof.outcome_sha256 != settled.witness.record_hash
        || proof.outcome_path
            != checkpoint.with_file_name(recovery_name(&settled.witness.checkpoint_hash))
    {
        return Err("The checkpoint archive names a different original outcome".into());
    }
    let retirement = read_checkpoint(&proof.retirement_path)?
        .ok_or("The immutable retirement record disappeared")?;
    if digest(&retirement) != proof.retirement_sha256 {
        return Err("The immutable retirement bytes changed during verification".into());
    }
    read_checkpoint(&proof.archive_path)?.ok_or(
        "The Prepared archive move has not completed; metadata alone is not recovery proof".into(),
    )
}

fn validate_bytes(
    checkpoint: &Path,
    settled: &AlreadyRecovered,
    bytes: Vec<u8>,
) -> Result<Vec<u8>, String> {
    if digest(&bytes) != settled.witness.checkpoint_hash {
        return Err("The actual Prepared checkpoint or archive changed".into());
    }
    super::super::handoff::validate_checkpoint_bytes(&bytes)?;
    let issued: IssuedCheckpoint =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if issued.phase != super::super::handoff::Phase::Prepared || issued.proof != settled.source {
        return Err("The actual checkpoint archive belongs to a different source or phase".into());
    }
    let outcome = read_checkpoint(
        &checkpoint.with_file_name(recovery_name(&settled.witness.checkpoint_hash)),
    )?
    .ok_or("The immutable original recovery outcome disappeared")?;
    if digest(&outcome) != settled.witness.record_hash {
        return Err("The actual immutable original recovery outcome changed".into());
    }
    Ok(bytes)
}

fn checkpoint_bytes(
    checkpoint: &Path,
    target: &Path,
    settled: &AlreadyRecovered,
) -> Result<Vec<u8>, String> {
    let bytes = match read_checkpoint(checkpoint)? {
        Some(bytes) => bytes,
        None => archive_bytes(checkpoint, target, settled)?,
    };
    validate_bytes(checkpoint, settled, bytes)
}

/// Mechanical archival evidence only. This says nothing about whether the
/// original inventory is still usable and grants no pending/replay authority.
pub(crate) fn completed_archive(
    checkpoint: &Path,
    target: &Path,
    recovered: &AlreadyRecovered,
) -> Result<bool, String> {
    if read_checkpoint(checkpoint)?.is_some() {
        return Ok(false);
    }
    // Force an actual archive read: a canonical checkpoint appearing during
    // verification must never substitute for a completed archive move.
    validate_bytes(
        checkpoint,
        recovered,
        archive_bytes(checkpoint, target, recovered)?,
    )?;
    Ok(read_checkpoint(checkpoint)?.is_none())
}

impl VerifiedView {
    /// `target` must come from the authenticated serving peer, not `current`
    /// or another installation pointer. The caller fences this read with the
    /// same ownership/readiness/inventory checks as the live descriptors.
    pub(crate) fn load(
        serving: &HostSessionProof,
        target: &Path,
        terminals: &[TerminalDescriptor],
        chats: &[Session],
    ) -> Result<Option<Self>, String> {
        let Some(store) = super::super::handoff::Store::existing()? else {
            return Ok(None);
        };
        Self::load_at(&store.path, serving, target, terminals, chats)
    }

    pub(crate) fn load_for_source(
        expected_source: &HostSessionProof,
        serving: &HostSessionProof,
        target: &Path,
        terminals: &[TerminalDescriptor],
        chats: &[Session],
    ) -> Result<Option<Self>, String> {
        let Some(store) = super::super::handoff::Store::existing()? else {
            return Ok(None);
        };
        Self::load_at_with(
            &store.path,
            Some(expected_source),
            serving,
            target,
            terminals,
            chats,
            Retention::load,
        )
    }

    fn load_at(
        checkpoint_path: &Path,
        serving: &HostSessionProof,
        target: &Path,
        terminals: &[TerminalDescriptor],
        chats: &[Session],
    ) -> Result<Option<Self>, String> {
        Self::load_at_with(
            checkpoint_path,
            None,
            serving,
            target,
            terminals,
            chats,
            Retention::load,
        )
    }

    fn load_at_with(
        checkpoint_path: &Path,
        expected_source: Option<&HostSessionProof>,
        serving: &HostSessionProof,
        target: &Path,
        terminals: &[TerminalDescriptor],
        chats: &[Session],
        read_retention: impl FnOnce(&HostSessionProof) -> Result<Retention, String>,
    ) -> Result<Option<Self>, String> {
        let settled = match expected_source {
            Some(source) => pending_retirement_for_source(checkpoint_path, target, source)?,
            None => pending_retirement(checkpoint_path, target)?,
        };
        let Some(settled) = settled else {
            return Ok(None);
        };
        if expected_source.is_some_and(|source| source != &settled.source) {
            return Ok(None);
        }
        let retained = read_retention(&settled.source)?;
        let bytes = checkpoint_bytes(checkpoint_path, target, &settled)?;
        let cold = if &settled.witness.successor == serving {
            None
        } else {
            Some(cold::ColdView::load(
                checkpoint_path,
                &settled,
                serving,
                target,
                terminals,
                chats,
                &retained,
            )?)
        };
        let (record_serving, record_terminals, record_chats) = if cold.is_some() {
            (
                &settled.witness.successor,
                settled.witness.adopted_terminals.as_slice(),
                settled.witness.adopted_chats.as_slice(),
            )
        } else {
            (serving, terminals, chats)
        };
        let record = read_verified_record(
            checkpoint_path,
            &settled.witness.checkpoint_hash,
            &settled.source,
            &settled.census,
            target,
            None,
            None,
            record_serving,
            record_terminals,
            record_chats,
        )?;
        if checkpoint_bytes(checkpoint_path, target, &settled)? != bytes {
            return Err(
                "The actual checkpoint evidence changed while verifying the serving inventory"
                    .into(),
            );
        }
        let issued: IssuedCheckpoint = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        Ok(Some(Self {
            record,
            issued_chats: issued.chats,
            cold,
            retained,
        }))
    }

    fn issued_chat(&self, original: &Session) -> bool {
        if original.proof.as_ref() != Some(&self.record.source) {
            return false;
        }
        self.record.census.chats.contains(original)
            || self.issued_chats.iter().any(|issued| {
                std::iter::once(super::super::handoff::ChatSource {
                    source: issued.source.clone(),
                    launch: issued.launch.clone(),
                })
                .chain(issued.previous_sources.iter().cloned())
                .any(|alias| {
                    if alias.source == *original {
                        return true;
                    }
                    let mut confirmed = alias.source;
                    confirmed.launch.session_id = alias.launch.session_id;
                    confirmed == *original
                })
            })
    }

    pub(crate) fn proves_chat(&self, original: &Session, current: &Session) -> bool {
        if let Some(cold) = &self.cold {
            // The authenticated holder may also have issued the intervening
            // B/C descriptors. Their full aliases can prove a later census.
            cold.proves_chat(original, current)
        } else {
            self.issued_chat(original)
                && original.id == current.id
                && current.proof.as_ref() == self.record.successor.as_ref()
                && self.record.adopted_chats.contains(&original.id)
                && self.record.adopted_chat_descriptors.contains(current)
        }
    }

    pub(crate) fn paused_chat(&self, original: &Session) -> bool {
        self.issued_chat(original)
            && self.record.paused_chats.contains(&original.id)
            && self.retained.chat(original)
    }

    pub(crate) fn paused_terminal(&self, original: &TerminalSession) -> bool {
        original.continuity.live_proof().as_ref() == Some(&self.record.source)
            && self.record.paused_terminals.contains(&original.id)
            && self.retained.terminal(original)
            && self
                .record
                .census
                .terminals
                .iter()
                .chain(
                    self.record
                        .census
                        .chats
                        .iter()
                        .filter_map(|chat| chat.terminal.as_ref()),
                )
                .any(|known| same_terminal(known, original))
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::{agent::Provider, model::LaunchRecipe};

    pub(super) fn proof(incarnation: &str) -> HostSessionProof {
        HostSessionProof {
            host_session_id: "host".into(),
            boot_id: "boot".into(),
            host_incarnation: incarnation.into(),
        }
    }

    pub(super) fn source(provider: Provider, root: &Path) -> Session {
        let mut source = Session {
            id: TerminalId::new(),
            launch: crate::chat::Launch {
                provider,
                recipe: LaunchRecipe {
                    program: provider.executable().into(),
                    arguments: vec![],
                    working_directory: root.into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: Some(uuid::Uuid::new_v4().to_string()),
            },
            proof: Some(proof("source")),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 },
        };
        if provider == Provider::Codex {
            source.launch.recipe.arguments = vec!["--remote".into(), "unix://".into()];
            // Allocate the source's listener before it has a provider thread,
            // then record the identity learned at its initial start. A begun
            // bare remote names the shared service and cannot be claimed by
            // this ordinary allocator without the host's writer checks.
            let thread = source.launch.session_id.take().unwrap();
            let requested = source.clone();
            assert!(source.claim_owned_codex_listener(root, &proof("source")));
            source.launch.session_id = Some(thread);
            assert!(source.owns_codex_listener());
            assert!(requested.resolved_to_owned_codex_listener(&source));
            source.native_continuity = crate::chat::NativeContinuity::Durable {
                provider_version: "1.0".into(),
            };
        }
        source
    }

    pub(super) fn private_root() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("cagents-recovery-proof-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in("/var/tmp")
            .unwrap()
    }

    pub(super) fn write_private(path: &Path, bytes: &[u8]) {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        file.write_all(bytes).unwrap();
    }

    pub(super) fn root_census(fixture: &Fixture) -> crate::update::recovery::Census {
        serde_json::from_value(serde_json::json!({
            "version":1, "transaction":fixture.record.job, "source":fixture.record.source,
            "terminals":fixture.record.census.terminals, "chats":fixture.record.census.chats,
            "checkpoint_before":null,
        }))
        .unwrap()
    }

    pub(super) struct Fixture {
        pub(super) _root: tempfile::TempDir,
        pub(super) checkpoint: PathBuf,
        pub(super) target: PathBuf,
        pub(super) source: Session,
        pub(super) current: Session,
        pub(super) record: RecoveryRecord,
        pub(super) retained: Retention,
    }

    impl Fixture {
        pub(super) fn new(provider: Provider, adopted: bool) -> Self {
            let root = private_root();
            let source = source(provider, root.path());
            let paused_peer = self::source(Provider::Claude, root.path());
            let mut original_terminal = TerminalDescriptor::relaunchable(
                TerminalId::new(),
                LaunchRecipe {
                    program: "/bin/sh".into(),
                    arguments: vec![],
                    working_directory: root.path().into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                None,
            );
            original_terminal.session.continuity = crate::model::TerminalContinuity::Live {
                host_session_id: proof("source").host_session_id,
                boot_id: proof("source").boot_id,
                host_incarnation: proof("source").host_incarnation,
            };
            let mut adopted_terminal = original_terminal.clone();
            adopted_terminal.session.continuity = crate::model::TerminalContinuity::Live {
                host_session_id: proof("successor").host_session_id,
                boot_id: proof("successor").boot_id,
                host_incarnation: proof("successor").host_incarnation,
            };
            adopted_terminal
                .upgrade_sources
                .push(original_terminal.session.clone());
            let mut current = source.clone();
            current.proof = Some(proof("successor"));
            let prefix = root.path().join("install");
            let bin = prefix.join("versions/fixture/bin");
            fs::create_dir_all(&bin).unwrap();
            write_private(
                &prefix.join(".cagents-install-root"),
                b"cagents install root\n",
            );
            for name in ["cagents", "cagents-ui", "cagentsd"] {
                write_private(
                    &bin.join(name),
                    format!("fixture {name}; never executed").as_bytes(),
                );
            }
            let target = fs::canonicalize(bin.join("cagentsd")).unwrap();
            let state = prefix.join(".update");
            crate::secure_file::prepare_private_directory(&state).unwrap();
            let job = uuid::Uuid::new_v4().to_string();
            let request = b"exact private original fixture request";
            write_private(&state.join(format!("{job}.request")), request);
            let mut reviewed = DaemonImpact {
                capability: crate::terminal_ipc::ShutdownCapability {
                    proof: proof("source"),
                    token: "fixture".into(),
                    inventory_generation: 1,
                    live_terminals: 0,
                    background_subagents: 0,
                    background_identity: String::new(),
                    expires_at_unix_ms: 1,
                },
                policy: crate::update::impact::Policy::WhenIdle,
                sessions: vec![crate::update::impact::SessionImpact {
                    id: source.id,
                    kind: SessionKind::NativeChat,
                    title: String::new(),
                    identity: "exact fixture actor".into(),
                    disposition: crate::update::impact::Disposition::Continues,
                    reason: String::new(),
                    interrupted_subagents: vec![],
                }],
                blockers: vec![],
            };
            reviewed.capability.live_terminals = 1;
            for (id, kind) in [
                (paused_peer.id, SessionKind::NativeChat),
                (original_terminal.session.id, SessionKind::Terminal),
            ] {
                reviewed
                    .sessions
                    .push(crate::update::impact::SessionImpact {
                        id,
                        kind,
                        title: String::new(),
                        identity: "exact additional fixture owner".into(),
                        disposition: crate::update::impact::Disposition::Continues,
                        reason: String::new(),
                        interrupted_subagents: vec![],
                    });
            }
            let checkpoint = root.path().join("daemon-upgrade.json");
            let chat = super::super::super::handoff::Chat {
                source: source.clone(),
                launch: source.launch.clone(),
                previous_sources: vec![],
            };
            let peer = super::super::super::handoff::Chat {
                source: paused_peer.clone(),
                launch: paused_peer.launch.clone(),
                previous_sources: vec![],
            };
            let carried = super::super::super::handoff::Carried {
                source: original_terminal.session.clone(),
                previous_sources: vec![],
                relaunches: vec![],
                options: Default::default(),
            };
            let bytes = serde_json::to_vec(&serde_json::json!({ "version":5, "proof":proof("source"), "phase":"Prepared", "terminals":[], "carried":[carried], "preserve_carried":true, "chats":[chat, peer] })).unwrap();
            write_private(&checkpoint, &bytes);
            let census = Census {
                terminals: vec![original_terminal.session.clone()],
                chats: vec![source.clone(), paused_peer.clone()],
                snapshots: [source.clone(), paused_peer.clone()]
                    .into_iter()
                    .map(|source| super::super::super::floor::StoppedSnapshot {
                        source,
                        snapshot: Default::default(),
                        limitation: None,
                    })
                    .collect(),
                ..Default::default()
            };
            let capture = CapturedCensus {
                producer: Some(CaptureProducer::Acknowledged {
                    job: job.clone(),
                    request_hash: digest(request),
                }),
                source: proof("source"),
                checkpoint_hash: Some(digest(&bytes)),
                census: census.clone(),
                windows: vec![],
            };
            write_private(
                &bound_capture_path(&checkpoint, &digest(&bytes)),
                &serde_json::to_vec(&capture).unwrap(),
            );
            let record = RecoveryRecord {
                version: 2,
                origin: Origin::AcknowledgedPrepare,
                job,
                request_identity: digest(&serde_json::to_vec(&reviewed).unwrap()),
                job_request_hash: digest(request),
                reviewed: Some(reviewed),
                windows: vec![],
                source: proof("source"),
                checkpoint_hash: digest(&bytes),
                census,
                adopted_terminals: vec![original_terminal.session.id],
                adopted_chats: if adopted { vec![source.id] } else { vec![] },
                adopted_chat_descriptors: if adopted {
                    vec![current.clone()]
                } else {
                    vec![]
                },
                adopted_terminal_descriptors: vec![adopted_terminal],
                paused_terminals: vec![],
                paused_chats: if adopted {
                    vec![paused_peer.id]
                } else {
                    vec![source.id, paused_peer.id]
                },
                settled: true,
                successor: Some(proof("successor")),
                target_hash: executable_hash(&target).unwrap(),
                target: Some(target.clone()),
                #[cfg(feature = "host")]
                authority: None,
            };
            let mut retained = super::super::super::floor::Record::new(
                record.source.clone(),
                record
                    .census
                    .chats
                    .iter()
                    .filter(|source| record.paused_chats.contains(&source.id))
                    .cloned()
                    .collect(),
                record.paused_terminals.clone(),
            );
            retained.snapshots = record
                .census
                .snapshots
                .iter()
                .filter(|saved| record.paused_chats.contains(&saved.source.id))
                .cloned()
                .collect();
            let fixture = Self {
                _root: root,
                checkpoint,
                target,
                source,
                current,
                record,
                retained: Retention(Some(retained)),
            };
            fixture.save();
            fixture
        }
        pub(super) fn save(&self) {
            write_private(
                &self
                    .checkpoint
                    .with_file_name(recovery_name(&self.record.checkpoint_hash)),
                &serde_json::to_vec(&self.record).unwrap(),
            );
        }
        pub(super) fn archive(&self) -> ArchiveProof {
            let current = self.record.adopted_chat_descriptors.clone();
            let view = self.load(&current).unwrap().unwrap();
            let outcome = crate::update::recovery::compare_with_recovery(
                &root_census(self),
                &proof("successor"),
                &self.record.adopted_terminal_descriptors,
                &current,
                &Default::default(),
                Some(&view),
                None,
            );
            assert!(matches!(
                outcome,
                crate::update::recovery::Outcome::Usable { .. }
            ));
            let settled = pending_retirement(&self.checkpoint, &self.target)
                .unwrap()
                .unwrap();
            settled
                .archive_completed(
                    &proof("successor"),
                    &self.record.adopted_terminal_descriptors,
                    &current,
                    &self.record.paused_terminals,
                    &self.record.paused_chats,
                )
                .unwrap();
            archived_proof(&self.checkpoint, &self.target)
                .unwrap()
                .unwrap()
        }
        pub(super) fn load_current(
            &self,
            serving: &HostSessionProof,
            terminals: &[TerminalDescriptor],
            chats: &[Session],
        ) -> Result<Option<VerifiedView>, String> {
            VerifiedView::load_at_with(
                &self.checkpoint,
                None,
                serving,
                &self.target,
                terminals,
                chats,
                |_| Ok(self.retained.clone()),
            )
        }
        pub(super) fn load_for_source(
            &self,
            source: &HostSessionProof,
            serving: &HostSessionProof,
            terminals: &[TerminalDescriptor],
            chats: &[Session],
        ) -> Result<Option<VerifiedView>, String> {
            VerifiedView::load_at_with(
                &self.checkpoint,
                Some(source),
                serving,
                &self.target,
                terminals,
                chats,
                |_| Ok(self.retained.clone()),
            )
        }
        pub(super) fn load(&self, live: &[Session]) -> Result<Option<VerifiedView>, String> {
            self.load_current(
                &proof("successor"),
                &self.record.adopted_terminal_descriptors,
                live,
            )
        }
        pub(super) fn publish_cold(
            &self,
            settled: &AlreadyRecovered,
            serving: &HostSessionProof,
            terminals: &[TerminalDescriptor],
            chats: &[Session],
            aliases: &std::collections::BTreeMap<
                TerminalId,
                Vec<super::super::super::handoff::ChatSource>,
            >,
        ) -> Result<(), String> {
            cold::publish_with_retention(
                &self.checkpoint,
                &self.target,
                settled,
                serving,
                terminals,
                chats,
                aliases,
                &self.retained,
            )
        }
    }

    #[test]
    fn settled_recovery_view_proves_exact_claude_and_codex_replacements_read_only() {
        for provider in [Provider::Claude, Provider::Codex] {
            let fixture = Fixture::new(provider, true);
            let before = fs::read(&fixture.checkpoint).unwrap();
            let view = fixture.load(&[fixture.current.clone()]).unwrap().unwrap();
            assert!(view.proves_chat(&fixture.source, &fixture.current));
            assert!(!view.paused_chat(&fixture.source));
            for mutation in 0..4 {
                let mut different = fixture.current.clone();
                match mutation {
                    0 => different.proof = Some(proof("foreign")),
                    1 => different.launch.session_id = Some(uuid::Uuid::new_v4().to_string()),
                    2 => different.launch.recipe.arguments.push("--different".into()),
                    _ => different.id = TerminalId::new(),
                }
                assert!(
                    !view.proves_chat(&fixture.source, &different),
                    "{provider:?} mutation {mutation}"
                );
                assert!(fixture.load(&[different]).is_err());
            }
            let mut foreign = fixture.source.clone();
            foreign.proof = Some(proof("foreign"));
            assert!(!view.proves_chat(&foreign, &fixture.current));
            assert_eq!(fs::read(&fixture.checkpoint).unwrap(), before);
        }
    }

    #[test]
    fn settled_recovery_view_accounts_paused_exact_source_without_claiming_live() {
        let fixture = Fixture::new(Provider::Claude, false);
        let view = fixture.load(&[]).unwrap().unwrap();
        assert!(view.paused_chat(&fixture.source));
        assert!(!view.proves_chat(&fixture.source, &fixture.current));
        let mut changed = fixture.source.clone();
        changed.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        assert!(!view.paused_chat(&changed));
        assert!(fixture.load(&[fixture.current.clone()]).is_err());
    }

    #[test]
    fn settled_recovery_view_refuses_missing_partial_or_incomplete_issued_record() {
        for mutation in 0..6 {
            let mut fixture = Fixture::new(Provider::Claude, true);
            match mutation {
                0 => fixture.record.settled = false,
                1 => fixture.record.adopted_chat_descriptors.clear(),
                2 => fixture.record.target = None,
                3 => fixture.record.successor = Some(proof("foreign")),
                4 => fixture.record.census.chats[0].launch.recipe.program = "other".into(),
                _ => fixture.record.target_hash = "different".into(),
            }
            fixture.save();
            assert!(
                fixture.load(&[fixture.current.clone()]).is_err(),
                "mutation {mutation}"
            );
        }
        let fixture = Fixture::new(Provider::Claude, true);
        fs::remove_file(
            fixture
                .checkpoint
                .with_file_name(recovery_name(&fixture.record.checkpoint_hash)),
        )
        .unwrap();
        assert!(fixture.load(&[fixture.current.clone()]).is_err());
        fixture.save();
        fs::remove_file(bound_capture_path(
            &fixture.checkpoint,
            &fixture.record.checkpoint_hash,
        ))
        .unwrap();
        assert!(
            fixture.load(&[fixture.current.clone()]).unwrap().is_some(),
            "a settled read-only proof uses the immutable reviewed outcome, without minting a prepare receipt from its original capture"
        );
    }
    #[test]
    fn archived_recovery_view_preserves_live_and_paused_proof_and_normal_chat_journal() {
        let fixture = Fixture::new(Provider::Claude, true);
        let checkpoint = fs::read(&fixture.checkpoint).unwrap();
        let original_path = fixture
            .checkpoint
            .with_file_name(recovery_name(&fixture.record.checkpoint_hash));
        let original_outcome = fs::read(&original_path).unwrap();
        let store = super::super::super::handoff::Store::open(fixture.checkpoint.parent().unwrap())
            .unwrap();
        store
            .record_recovered(&[super::super::super::handoff::RecoveredChat {
                evidence: super::super::super::handoff::Chat {
                    source: fixture.source.clone(),
                    launch: fixture.source.launch.clone(),
                    previous_sources: vec![],
                },
                replacement: fixture.current.clone(),
                aliases: vec![],
            }])
            .unwrap();
        let journal_path = fixture
            .checkpoint
            .with_file_name("daemon-upgrade.recovered.json");
        let journal = fs::read(&journal_path).unwrap();
        let archive = fixture.archive();
        assert!(!fixture.checkpoint.exists());
        assert_eq!(fs::read(&archive.archive_path).unwrap(), checkpoint);
        assert_eq!(fs::read(&original_path).unwrap(), original_outcome);
        assert_eq!(
            fs::read(&journal_path).unwrap(),
            journal,
            "retirement cannot overwrite normal issued chat lineage"
        );
        assert!(
            PendingRecovery::inspect_at(&fixture.checkpoint)
                .unwrap()
                .is_none(),
            "archive evidence is never pending prepare authority"
        );
        let view = fixture.load(&[fixture.current.clone()]).unwrap().unwrap();
        assert!(view.proves_chat(&fixture.source, &fixture.current));
        assert!(view.paused_chat(&fixture.record.census.chats[1]));
        assert!(
            matches!(crate::update::recovery::compare_with_recovery(&root_census(&fixture), &proof("successor"),
            &fixture.record.adopted_terminal_descriptors, &[fixture.current.clone()], &Default::default(), Some(&view), None),
            crate::update::recovery::Outcome::Usable { terminals:1, chats:1, paused_chats, .. } if paused_chats == fixture.record.paused_chats)
        );
    }

    #[test]
    fn archived_recovery_view_requires_actual_archive_bytes_beyond_planned_metadata() {
        let fixture = Fixture::new(Provider::Claude, true);
        let archive = fixture.archive();
        let original = fs::read(&archive.archive_path).unwrap();
        let recovered = pending_retirement(&fixture.checkpoint, &fixture.target)
            .unwrap()
            .unwrap();
        assert!(completed_archive(&fixture.checkpoint, &fixture.target, &recovered).unwrap());
        let missing = archive.archive_path.with_extension("temporarily-missing");
        // The durable plan can precede its rename. Canonical bytes remain a
        // valid pre-retirement proof even while its archive is still absent.
        fs::rename(&archive.archive_path, &fixture.checkpoint).unwrap();
        assert!(!archive.archive_path.exists());
        assert!(
            !completed_archive(&fixture.checkpoint, &fixture.target, &recovered).unwrap(),
            "canonical proof is not mechanical archive completion"
        );
        assert!(fixture.load(&[fixture.current.clone()]).unwrap().is_some());
        fs::rename(&fixture.checkpoint, &missing).unwrap();
        assert!(
            archived_proof(&fixture.checkpoint, &fixture.target).is_err(),
            "routing metadata without the actual archive is not archival proof"
        );
        assert!(
            fixture.load(&[fixture.current.clone()]).is_err(),
            "metadata alone cannot prove missing actual archive bytes"
        );
        assert!(
            completed_archive(&fixture.checkpoint, &fixture.target, &recovered).is_err(),
            "a planned pointer with neither actual file is incomplete"
        );
        fs::rename(&missing, &archive.archive_path).unwrap();
        write_private(&archive.archive_path, b"changed actual archive");
        assert!(completed_archive(&fixture.checkpoint, &fixture.target, &recovered).is_err());
        assert!(fixture.load(&[fixture.current.clone()]).is_err());
        write_private(&archive.archive_path, &original);
        assert!(fixture.load(&[fixture.current.clone()]).unwrap().is_some());
        let retirement = fs::read(&archive.retirement_path).unwrap();
        write_private(&archive.retirement_path, b"changed immutable retirement");
        assert!(fixture.load(&[fixture.current.clone()]).is_err());
        write_private(&archive.retirement_path, &retirement);
        assert!(fixture.load(&[fixture.current.clone()]).unwrap().is_some());
    }
    #[test]
    fn canonical_ready_checkpoint_suppresses_older_archived_recovery_proof() {
        let fixture = Fixture::new(Provider::Claude, true);
        let archive = fixture.archive();
        let mut next: serde_json::Value =
            serde_json::from_slice(&fs::read(&archive.archive_path).unwrap()).unwrap();
        next["phase"] = "Ready".into();
        write_private(&fixture.checkpoint, &serde_json::to_vec(&next).unwrap());
        assert!(
            fixture.load(&[fixture.current.clone()]).unwrap().is_none(),
            "an existing canonical checkpoint prevents falling through to unrelated archived scope"
        );
        assert!(archive.archive_path.exists());
    }
    #[test]
    fn paused_native_requires_current_retained_page_even_with_immutable_settled_outcome() {
        let mut fixture = Fixture::new(Provider::Claude, true);
        let paused = fixture.record.census.chats[1].clone();
        assert!(
            fixture
                .load(&[fixture.current.clone()])
                .unwrap()
                .unwrap()
                .paused_chat(&paused)
        );
        let original_outcome = fs::read(
            fixture
                .checkpoint
                .with_file_name(recovery_name(&fixture.record.checkpoint_hash)),
        )
        .unwrap();
        fixture.retained.0.as_mut().unwrap().snapshots.clear();
        let view = fixture.load(&[fixture.current.clone()]).unwrap().unwrap();
        assert!(!view.paused_chat(&paused));
        assert!(view.proves_chat(&fixture.source, &fixture.current));
        assert!(
            matches!(crate::update::recovery::compare_with_recovery(&root_census(&fixture), &proof("successor"), &fixture.record.adopted_terminal_descriptors,
            &[fixture.current.clone()], &Default::default(), Some(&view), None),
            crate::update::recovery::Outcome::Unverified { serving:Some(_), surviving_terminals:1, missing, .. } if missing.contains(&paused.id))
        );
        assert_eq!(
            fs::read(
                fixture
                    .checkpoint
                    .with_file_name(recovery_name(&fixture.record.checkpoint_hash))
            )
            .unwrap(),
            original_outcome
        );
        assert!(
            fixture.retained.0.as_ref().unwrap().snapshots.is_empty(),
            "a proof probe cannot recreate the closed page"
        );
    }

    #[test]
    fn paused_terminal_requires_current_retained_marker_for_exact_source() {
        let mut fixture = Fixture::new(Provider::Claude, true);
        let terminal = fixture.record.census.terminals[0].clone();
        fixture.record.adopted_terminals.clear();
        fixture.record.adopted_terminal_descriptors.clear();
        fixture.record.paused_terminals.push(terminal.id);
        fixture
            .retained
            .0
            .as_mut()
            .unwrap()
            .terminals
            .push(terminal.id);
        fixture.save();
        assert!(
            fixture
                .load(&[fixture.current.clone()])
                .unwrap()
                .unwrap()
                .paused_terminal(&terminal)
        );
        fixture.retained.0.as_mut().unwrap().terminals.clear();
        let view = fixture.load(&[fixture.current.clone()]).unwrap().unwrap();
        assert!(!view.paused_terminal(&terminal));
        assert!(
            matches!(crate::update::recovery::compare_with_recovery(&root_census(&fixture), &proof("successor"), &[],
            &[fixture.current.clone()], &Default::default(), Some(&view), None),
            crate::update::recovery::Outcome::Unverified { missing, .. } if missing.contains(&terminal.id))
        );
    }

    #[test]
    fn requested_source_b_uses_own_issued_proof_despite_unrelated_broken_archive_a() {
        let fixture = Fixture::new(Provider::Claude, true);
        let archive = fixture.archive();
        let serving = proof("normal-ready-d");
        let mut current = fixture.current.clone();
        current.proof = Some(serving.clone());
        let mut terminal = fixture.record.adopted_terminal_descriptors[0].clone();
        let source_terminal = terminal.session.clone();
        terminal.upgrade_sources.push(source_terminal.clone());
        terminal.session.continuity = crate::model::TerminalContinuity::Live {
            host_session_id: serving.host_session_id.clone(),
            boot_id: serving.boot_id.clone(),
            host_incarnation: serving.host_incarnation.clone(),
        };
        let store = super::super::super::handoff::Store::open(fixture.checkpoint.parent().unwrap())
            .unwrap();
        store
            .record_recovered(&[super::super::super::handoff::RecoveredChat {
                evidence: super::super::super::handoff::Chat {
                    source: fixture.current.clone(),
                    launch: fixture.current.launch.clone(),
                    previous_sources: vec![],
                },
                replacement: current.clone(),
                aliases: vec![],
            }])
            .unwrap();
        let own_lineage = store.recovered_chats().unwrap();
        let expected: crate::update::recovery::Census = serde_json::from_value(serde_json::json!({
            "version":1,"transaction":uuid::Uuid::new_v4().to_string(),"source":proof("successor"),
            "terminals":[source_terminal],"chats":[fixture.current.clone()],"checkpoint_before":null,
        })).unwrap();
        write_private(&archive.archive_path, b"unrelated A archive is corrupt");
        fs::remove_file(&archive.outcome_path).unwrap();
        let unrelated = fixture
            .load_for_source(
                &proof("successor"),
                &serving,
                &[terminal.clone()],
                &[current.clone()],
            )
            .unwrap();
        assert!(unrelated.is_none());
        assert!(
            fixture
                .load_for_source(
                    &proof("source"),
                    &serving,
                    &[terminal.clone()],
                    &[current.clone()]
                )
                .is_err(),
            "matching A still needs strict actual evidence"
        );
        assert!(
            matches!(crate::update::recovery::compare_with_recovery(&expected, &serving, &[terminal], &[current], &own_lineage, unrelated.as_ref(), None),
            crate::update::recovery::Outcome::Usable { terminals:1,chats:1,paused_chats,.. } if paused_chats.is_empty())
        );
    }
}
