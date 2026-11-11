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
}

#[derive(Deserialize)]
struct IssuedCheckpoint {
    chats: Vec<super::super::handoff::Chat>,
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

    fn load_at(
        checkpoint_path: &Path,
        serving: &HostSessionProof,
        target: &Path,
        terminals: &[TerminalDescriptor],
        chats: &[Session],
    ) -> Result<Option<Self>, String> {
        let Some(pending) = PendingRecovery::inspect_at(checkpoint_path)? else {
            return Ok(None);
        };
        if !pending.original_census_complete {
            return Err("The complete original recovery census is unavailable; retained state was not treated as usable recovery".into());
        }
        let settled = classify_retained_prepared(checkpoint_path, target)?
            .ok_or("The retained checkpoint has no exact settled recovery")?;
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
            &pending.checkpoint_hash,
            &pending.source,
            &pending.census,
            target,
            None,
            None,
            record_serving,
            record_terminals,
            record_chats,
        )?;
        let bytes = read_checkpoint(checkpoint_path)?
            .ok_or("The Prepared checkpoint disappeared while verifying issued lineage")?;
        pending.validate(&bytes)?;
        let issued: IssuedCheckpoint = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        Ok(Some(Self {
            record,
            issued_chats: issued.chats,
            cold,
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
        self.issued_chat(original) && self.record.paused_chats.contains(&original.id)
    }

    pub(crate) fn paused_terminal(&self, original: &TerminalSession) -> bool {
        original.continuity.live_proof().as_ref() == Some(&self.record.source)
            && self.record.paused_terminals.contains(&original.id)
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
            assert!(source.claim_owned_codex_listener(root, &proof("source")));
            source.native_continuity = crate::chat::NativeContinuity::Durable {
                provider_version: "1.0".into(),
            };
        }
        source
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

    pub(super) struct Fixture {
        pub(super) _root: tempfile::TempDir,
        pub(super) checkpoint: PathBuf,
        pub(super) target: PathBuf,
        pub(super) source: Session,
        pub(super) current: Session,
        pub(super) record: RecoveryRecord,
    }

    impl Fixture {
        pub(super) fn new(provider: Provider, adopted: bool) -> Self {
            let root = tempfile::tempdir().unwrap();
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
            fs::create_dir(&state).unwrap();
            fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
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
            let fixture = Self {
                _root: root,
                checkpoint,
                target,
                source,
                current,
                record,
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
        pub(super) fn load(&self, live: &[Session]) -> Result<Option<VerifiedView>, String> {
            VerifiedView::load_at(
                &self.checkpoint,
                &proof("successor"),
                &self.target,
                &self.record.adopted_terminal_descriptors,
                live,
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
        assert!(fixture.load(&[fixture.current.clone()]).is_err());
    }
}
