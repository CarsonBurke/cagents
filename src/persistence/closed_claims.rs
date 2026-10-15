//! Small close authority independent of mutable workspace snapshots and archives.
use super::*;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::os::unix::ffi::OsStrExt;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum RecoveryContent {
    Conversation(crate::agent::Provider, uuid::Uuid),
    Plain([u8; 32]),
}

impl RecoveryContent {
    fn of(session: &TerminalSession) -> Option<Self> {
        let direct = crate::upgrade::exact_conversation(&session.launch);
        let resumed = session
            .agent_resume_recipe()
            .as_ref()
            .and_then(crate::upgrade::exact_conversation);
        if direct.is_some() && resumed.is_some() && direct != resumed {
            return None;
        }
        if let Some((provider, id)) = direct.or(resumed) {
            return Some(Self::Conversation(provider, id));
        }
        if session.resume_hint.is_some()
            || crate::agent::Provider::from_program(&session.launch.program).is_some()
        {
            return None;
        }
        let crate::model::LaunchRecipe {
            program,
            arguments,
            working_directory,
            environment,
            unset_environment,
        } = &session.launch;
        let mut digest = Sha256::new();
        fn field(digest: &mut Sha256, bytes: &[u8]) {
            digest.update((bytes.len() as u64).to_le_bytes());
            digest.update(bytes);
        }
        field(&mut digest, program.as_bytes());
        digest.update((arguments.len() as u64).to_le_bytes());
        for argument in arguments {
            field(&mut digest, argument.as_bytes());
        }
        field(&mut digest, working_directory.as_os_str().as_bytes());
        digest.update((environment.len() as u64).to_le_bytes());
        for (key, value) in environment {
            field(&mut digest, key.as_bytes());
            field(&mut digest, value.as_bytes());
        }
        digest.update((unset_environment.len() as u64).to_le_bytes());
        for name in unset_environment {
            field(&mut digest, name.as_bytes());
        }
        Some(Self::Plain(digest.finalize().into()))
    }
}

/// IDs, exact host proof and a bounded recovery-content digest; no launch
/// arguments, environment, transcript, draft or rendered text remain resident.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ClosedTerminalClaim {
    id: TerminalId,
    proof: Option<HostSessionProof>,
    origin: Option<TerminalId>,
    content: Option<RecoveryContent>,
}

impl ClosedTerminalClaim {
    pub(crate) fn of(session: &TerminalSession) -> Self {
        Self {
            id: session.id,
            proof: session.continuity.live_proof(),
            origin: session.recovery_origin,
            content: RecoveryContent::of(session),
        }
    }
    fn identity(id: TerminalId) -> Self {
        Self {
            id,
            proof: None,
            origin: None,
            content: None,
        }
    }
    pub(crate) fn claims(&self, hosted: &TerminalSession) -> bool {
        self.id == hosted.id
            || (self.proof.is_some()
                && self.proof != hosted.continuity.live_proof()
                && hosted.recovery_origin == Some(self.origin.unwrap_or(self.id))
                && self
                    .content
                    .as_ref()
                    .is_some_and(|content| RecoveryContent::of(hosted).as_ref() == Some(content)))
    }
}

#[derive(Default)]
pub(super) struct ClosedAuthority {
    // Explicit INTEGER primary key remains stable across database compaction.
    last_row: i64,
    unknown: bool,
    cards: HashSet<CardId>,
    terminal_ids: HashSet<TerminalId>,
    lineages: HashMap<TerminalId, Vec<ClosedTerminalClaim>>,
}

impl ClosedAuthority {
    pub(super) fn sync(&mut self, connection: &Connection) -> Result<()> {
        self.unknown |= connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM closed_terminal_claims WHERE unknown = 1)",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        let mut query = connection.prepare_cached(
            "SELECT c.rowid, c.card_id, t.claims_json, COALESCE(t.unknown, 0) FROM closed_cards c
             LEFT JOIN closed_terminal_claims t ON t.card_id = c.card_id
             WHERE c.rowid > ?1 ORDER BY c.rowid",
        )?;
        for row in query.query_map([self.last_row], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, bool>(3)?,
            ))
        })? {
            let (row, id, claims, unknown) = row?;
            self.unknown |= unknown;
            self.cards.insert(parse_id("closed card", &id)?);
            if let Some(claims) = claims {
                for claim in
                    from_json::<Vec<ClosedTerminalClaim>>("closed terminal claims", &claims)?
                {
                    self.terminal_ids.insert(claim.id);
                    if claim.proof.is_some() && claim.content.is_some() {
                        self.lineages
                            .entry(claim.origin.unwrap_or(claim.id))
                            .or_default()
                            .push(claim);
                    }
                }
            }
            self.last_row = row;
        }
        Ok(())
    }
}

/// Called once at migration and once per newly committed tombstone, never for
/// an unchanged closed snapshot. The compact descriptor survives compaction.
pub(super) fn save_claims(transaction: &Transaction<'_>, id: CardId) -> Result<()> {
    let kind: Option<String> = transaction
        .query_row(
            "SELECT kind_tag FROM cards WHERE card_id = ?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    let claims = match kind.as_deref() {
        Some("terminal") => {
            let session = closed_terminal_source(transaction, id)?
                .ok_or_else(|| StoreError::InvalidState("closed terminal disappeared".into()))?;
            let CardKind::Terminal(session) = session.kind else {
                unreachable!()
            };
            vec![ClosedTerminalClaim::of(&session)]
        }
        Some("chat") => {
            let json: String = transaction.query_row(
                "SELECT content_json FROM cards WHERE card_id = ?1",
                [id.to_string()],
                |row| row.get(0),
            )?;
            let session: crate::chat::Session = from_json("closed chat claim", &json)?;
            let mut claims = vec![ClosedTerminalClaim::identity(session.id)];
            if let Some(terminal) = &session.terminal {
                claims.push(ClosedTerminalClaim::of(terminal));
            }
            claims
        }
        _ => Vec::new(),
    };
    if !claims.is_empty() {
        transaction.execute("INSERT INTO closed_terminal_claims(card_id, claims_json) VALUES (?1, ?2) ON CONFLICT(card_id) DO NOTHING", params![id.to_string(), to_json(&claims)?])?;
    }
    Ok(())
}

pub(super) fn preserve_unknown_claim(transaction: &Transaction<'_>, id: CardId) -> Result<()> {
    transaction.execute("INSERT INTO closed_terminal_claims(card_id, claims_json, unknown) VALUES (?1, '[]', 1) ON CONFLICT(card_id) DO UPDATE SET unknown = 1", [id.to_string()])?;
    Ok(())
}

impl StateStore {
    /// Narrow RAM authority, available before automatic adoption without SQLite I/O.
    pub fn card_was_closed(&self, id: CardId) -> bool {
        self.closed_authority
            .read()
            .map_or(true, |authority| authority.cards.contains(&id))
    }
    pub fn has_unknown_closed_claims(&self) -> bool {
        self.closed_authority
            .read()
            .map_or(true, |authority| authority.unknown)
    }
    pub fn terminal_was_closed(&self, hosted: &TerminalSession) -> bool {
        self.closed_authority.read().map_or(true, |authority| {
            authority.unknown
                || authority.terminal_ids.contains(&hosted.id)
                || hosted
                    .recovery_origin
                    .and_then(|root| authority.lineages.get(&root))
                    .is_some_and(|claims| claims.iter().any(|claim| claim.claims(hosted)))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{HostIncarnation, HostSessionId, LaunchRecipe};

    fn plain() -> TerminalSession {
        TerminalSession {
            id: TerminalId::new(),
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            launch: LaunchRecipe {
                program: "/bin/sh".into(),
                arguments: vec!["-c".into(), "x".repeat(512 * 1024)],
                working_directory: "/project".into(),
                environment: BTreeMap::new(),
                unset_environment: BTreeSet::new(),
            },
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::Live {
                host_session_id: HostSessionId::new("old"),
                boot_id: BootId::new("boot"),
                host_incarnation: HostIncarnation::new("old"),
            },
        }
    }

    #[test]
    fn compact_claim_keeps_exact_lineage_without_retaining_the_launch_payload() {
        let saved = plain();
        let claim = ClosedTerminalClaim::of(&saved);
        assert!(serde_json::to_vec(&claim).unwrap().len() < 1024);
        let mut hosted = saved.clone();
        hosted.id = TerminalId::new();
        hosted.recovery_origin = Some(saved.id);
        // A new ID without a changed issued host proof is no recovery.
        assert!(!claim.claims(&hosted));
        hosted.continuity = TerminalContinuity::Live {
            host_session_id: HostSessionId::new("new"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("new"),
        };
        assert!(claim.claims(&hosted));
        hosted.launch.unset_environment.insert("PROFILE".into());
        assert!(!claim.claims(&hosted));
        hosted.launch.unset_environment.clear();
        hosted
            .launch
            .environment
            .insert("PROFILE".into(), "other".into());
        assert!(!claim.claims(&hosted));
        hosted.launch.environment.clear();
        hosted.recovery_origin = Some(TerminalId::new());
        assert!(!claim.claims(&hosted));
    }

    #[test]
    fn native_claim_matches_only_the_exact_unambiguous_resumed_conversation() {
        let mut saved = plain();
        saved.launch.program = "omp".into();
        saved.launch.arguments = vec![
            "--resume".into(),
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
        ];
        let claim = ClosedTerminalClaim::of(&saved);
        let mut hosted = saved.clone();
        hosted.id = TerminalId::new();
        hosted.predecessor = Some(saved.id);
        hosted.recovery_origin = Some(saved.id);
        hosted.continuity = TerminalContinuity::Live {
            host_session_id: HostSessionId::new("new"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("new"),
        };
        assert!(claim.claims(&hosted));
        hosted.launch.arguments[1] = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".into();
        assert!(!claim.claims(&hosted));
        // Direct ID ownership remains authoritative even if the daemon's
        // descriptor changed content: it cannot reopen a locally closed ID.
        hosted.id = saved.id;
        assert!(claim.claims(&hosted));
    }
}
