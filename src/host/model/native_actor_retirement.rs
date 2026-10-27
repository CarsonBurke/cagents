//! Transfer exact semantic-retirement proof before consuming helper cleanup.
//! This proves one actor retired, never the absence of other session writers.
use super::Registry;
use crate::{account::scope::LaunchAccountScope, chat, daemon::holder_registry::Record};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const MAX_RECEIPT_BYTES: usize = 256 * 1024;

/// Pending is cleanup evidence only; installation requires the helper ACK and
/// committed Registry projection. Missing phase is explicitly legacy Installed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
enum NativeActorPhase {
    Pending,
    #[default]
    Installed,
}

/// A durable roster includes helpers which have never emitted a semantic event.
/// Unknown scope remains explicit and cannot be treated as another account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "OwnershipWire", into = "OwnershipWire")]
pub(in crate::host) struct NativeActorOwnership {
    phase: NativeActorPhase,
    actor: Uuid,
    owner: Record,
    source: chat::Session,
    scope: Result<LaunchAccountScope, String>,
}
// Pending deliberately lacks the legacy reader's required top-level identity.
// Installed preserves the original wire shape byte-for-byte (field order too).
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingEnvelope<T> {
    pending: T,
}
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum OwnershipWire {
    Pending(PendingEnvelope<OwnershipBody>),
    Installed(OwnershipBody),
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnershipBody {
    actor: Uuid,
    owner: Record,
    source: chat::Session,
    scope: Result<LaunchAccountScope, String>,
}
impl From<OwnershipWire> for NativeActorOwnership {
    fn from(wire: OwnershipWire) -> Self {
        let (phase, body) = match wire {
            OwnershipWire::Pending(PendingEnvelope { pending }) => {
                (NativeActorPhase::Pending, pending)
            }
            OwnershipWire::Installed(body) => (NativeActorPhase::Installed, body),
        };
        let OwnershipBody {
            actor,
            owner,
            source,
            scope,
        } = body;
        Self {
            phase,
            actor,
            owner,
            source,
            scope,
        }
    }
}
impl From<NativeActorOwnership> for OwnershipWire {
    fn from(value: NativeActorOwnership) -> Self {
        let NativeActorOwnership {
            phase,
            actor,
            owner,
            source,
            scope,
        } = value;
        let body = OwnershipBody {
            actor,
            owner,
            source,
            scope,
        };
        match phase {
            NativeActorPhase::Pending => Self::Pending(PendingEnvelope { pending: body }),
            NativeActorPhase::Installed => Self::Installed(body),
        }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum RetirementWire {
    Pending(PendingEnvelope<RetirementBody>),
    Installed(RetirementBody),
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetirementBody {
    actor: Uuid,
    owner: Record,
    source: chat::Session,
    scope: Result<LaunchAccountScope, String>,
    confirmed_session: Option<String>,
}
impl From<RetirementWire> for NativeActorRetirement {
    fn from(wire: RetirementWire) -> Self {
        let (phase, body) = match wire {
            RetirementWire::Pending(PendingEnvelope { pending }) => {
                (NativeActorPhase::Pending, pending)
            }
            RetirementWire::Installed(body) => (NativeActorPhase::Installed, body),
        };
        let RetirementBody {
            actor,
            owner,
            source,
            scope,
            confirmed_session,
        } = body;
        Self {
            phase,
            actor,
            owner,
            source,
            scope,
            confirmed_session,
        }
    }
}
impl From<NativeActorRetirement> for RetirementWire {
    fn from(value: NativeActorRetirement) -> Self {
        let NativeActorRetirement {
            phase,
            actor,
            owner,
            source,
            scope,
            confirmed_session,
        } = value;
        let body = RetirementBody {
            actor,
            owner,
            source,
            scope,
            confirmed_session,
        };
        match phase {
            NativeActorPhase::Pending => Self::Pending(PendingEnvelope { pending: body }),
            NativeActorPhase::Installed => Self::Installed(body),
        }
    }
}
impl NativeActorOwnership {
    pub(in crate::host) fn is_pending(&self) -> bool {
        self.phase == NativeActorPhase::Pending
    }
    pub(in crate::host) fn actor(&self) -> Uuid {
        self.actor
    }
    pub(in crate::host) fn owner(&self) -> &Record {
        &self.owner
    }
    pub(in crate::host) fn source(&self) -> &chat::Session {
        &self.source
    }
    pub(in crate::host) fn scope(&self) -> Result<&LaunchAccountScope, &str> {
        self.scope.as_ref().map_err(String::as_str)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "RetirementWire", into = "RetirementWire")]
pub(in crate::host) struct NativeActorRetirement {
    phase: NativeActorPhase,
    actor: Uuid,
    owner: Record,
    source: chat::Session,
    scope: Result<LaunchAccountScope, String>,
    confirmed_session: Option<String>,
}

impl NativeActorRetirement {
    pub(in crate::host) fn is_pending(&self) -> bool {
        self.phase == NativeActorPhase::Pending
    }
    pub(in crate::host) fn actor(&self) -> Uuid {
        self.actor
    }
    pub(in crate::host) fn owner(&self) -> &Record {
        &self.owner
    }
    pub(in crate::host) fn source(&self) -> &chat::Session {
        &self.source
    }
    pub(in crate::host) fn scope(&self) -> Result<&LaunchAccountScope, &str> {
        self.scope.as_ref().map_err(String::as_str)
    }
    pub(in crate::host) fn confirmed_session(&self) -> Option<&str> {
        self.confirmed_session.as_deref()
    }

    fn validate(&self) -> Result<(), String> {
        if self.actor.is_nil()
            || self.owner.kind != crate::daemon::holder_registry::Kind::Native
            || !crate::daemon::holder_registry::record_application_matches(&self.owner)
            || self.owner.terminal != self.source.id
            || !self
                .source
                .proof
                .as_ref()
                .is_some_and(|proof| proof.is_complete() && proof.boot_id == self.owner.boot)
            || self.source.launch.provider != crate::agent::Provider::Claude
            || self.scope.as_ref().is_ok_and(|scope| {
                scope.provider != self.source.launch.provider || !scope.canonical_home.is_absolute()
            })
            || !valid_continuity(&self.source, self.phase)
            || self.source.owned_codex_listener.is_some()
            || !self.source.has_valid_native_continuity()
            || self
                .source
                .launch
                .session_id
                .as_ref()
                .is_some_and(|id| Uuid::parse_str(id).is_err())
            || self
                .confirmed_session
                .as_ref()
                .is_some_and(|id| uuid::Uuid::parse_str(id).is_err())
            || self
                .source
                .launch
                .session_id
                .as_ref()
                .zip(self.confirmed_session.as_ref())
                .is_some_and(|(issued, confirmed)| issued != confirmed)
        {
            return Err(
                "Retained actor retirement has invalid exact source or physical account identity"
                    .into(),
            );
        }
        Ok(())
    }
}

fn valid_continuity(source: &chat::Session, phase: NativeActorPhase) -> bool {
    matches!(
        source.native_continuity,
        chat::NativeContinuity::RetainedClaudeActor { protocol: 2 }
    ) || (phase == NativeActorPhase::Pending
        && matches!(
            source.native_continuity,
            chat::NativeContinuity::ReconnectOnly
        ))
}

// The issued actor core remains immutable. The authenticated helper and caller
// own conversation-reset lineage and pair replacement/removal checks; this row
// has no independent provider-reset or pair-stop proof and must not reject an
// acknowledged projection merely because its conversation or pair changed.
fn compatible_source(old: &chat::Session, new: &chat::Session) -> bool {
    old.id == new.id
        && old.launch.provider == new.launch.provider
        && old.launch.recipe == new.launch.recipe
        && old.owned_codex_listener == new.owned_codex_listener
}

fn same_scope(
    left: &Result<LaunchAccountScope, String>,
    right: &Result<LaunchAccountScope, String>,
) -> bool {
    match (left, right) {
        (Ok(old), Ok(new)) => {
            old.provider == new.provider
                && old.canonical_home == new.canonical_home
                && old.device == new.device
                && old.inode == new.inode
        }
        (Err(old), Err(new)) => old == new,
        _ => false,
    }
}

pub(super) fn migrate(tx: &rusqlite::Transaction<'_>) -> Result<(), String> {
    tx.execute_batch("CREATE TABLE IF NOT EXISTS native_actor_ownerships(actor TEXT PRIMARY KEY, ownership TEXT NOT NULL) STRICT; CREATE TABLE IF NOT EXISTS native_actor_retirements(actor TEXT PRIMARY KEY REFERENCES native_actor_ownerships(actor), receipt TEXT NOT NULL) STRICT;")
        .map_err(|error| error.to_string())
}

impl Registry {
    /// Publish after authenticating installation/adoption and before exposing
    /// a handle that accepts input. This is a known-owner census, not launch
    /// authority; only that exact helper's authenticated route may refresh it.
    pub(in crate::host) fn register_native_actor_ownership(
        &mut self,
        actor: Uuid,
        owner: &Record,
        source: &chat::Session,
        scope: Result<&LaunchAccountScope, &str>,
    ) -> Result<(), String> {
        self.register_native_actor_phase(actor, owner, source, scope, NativeActorPhase::Installed)
    }

    /// Record exact authenticated cleanup intent before adoption/installation.
    /// This does not validate the current pathname or authorize history access.
    pub(in crate::host) fn register_pending_native_actor_ownership(
        &mut self,
        actor: Uuid,
        owner: &Record,
        source: &chat::Session,
        scope: Result<&LaunchAccountScope, &str>,
    ) -> Result<(), String> {
        self.register_native_actor_phase(actor, owner, source, scope, NativeActorPhase::Pending)
    }

    fn register_native_actor_phase(
        &mut self,
        actor: Uuid,
        owner: &Record,
        source: &chat::Session,
        scope: Result<&LaunchAccountScope, &str>,
        phase: NativeActorPhase,
    ) -> Result<(), String> {
        if actor.is_nil()
            || owner.kind != crate::daemon::holder_registry::Kind::Native
            || !crate::daemon::holder_registry::record_application_matches(owner)
            || owner.terminal != source.id
            || source.launch.provider != crate::agent::Provider::Claude
            || !source
                .proof
                .as_ref()
                .is_some_and(|proof| proof.is_complete() && proof.boot_id == owner.boot)
            || !valid_continuity(source, phase)
            || source.owned_codex_listener.is_some()
            || source
                .launch
                .session_id
                .as_ref()
                .is_some_and(|id| Uuid::parse_str(id).is_err())
            || !source.has_valid_native_continuity()
        {
            return Err("Retained actor census has invalid authenticated ownership".into());
        }
        let scope = match scope {
            Ok(scope)
                if scope.provider == source.launch.provider
                    && scope.canonical_home.is_absolute() =>
            {
                if phase == NativeActorPhase::Installed {
                    scope.revalidate()?;
                }
                Ok(scope.clone())
            }
            Ok(_) => return Err("Retained actor census account provider changed".into()),
            Err(reason) => Err(reason.chars().take(4096).collect()),
        };
        let ownership = NativeActorOwnership {
            phase,
            actor,
            owner: owner.clone(),
            source: source.clone(),
            scope,
        };
        let encoded = serde_json::to_string(&ownership).map_err(|e| e.to_string())?;
        if encoded.len() > MAX_RECEIPT_BYTES {
            return Err("Retained actor census exceeds its metadata bound".into());
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        let previous: Option<String> = tx
            .query_row(
                "SELECT ownership FROM native_actor_ownerships WHERE actor=?1",
                [actor.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some(previous) = previous {
            if previous.len() > MAX_RECEIPT_BYTES {
                return Err("Stored actor census exceeds its metadata bound".into());
            }
            let previous: NativeActorOwnership =
                serde_json::from_str(&previous).map_err(|e| e.to_string())?;
            if previous.actor != actor
                || previous.owner != *owner
                || previous.source.id != source.id
            {
                return Err("Retained actor census changed its exact helper".into());
            }
            if !same_scope(&previous.scope, &ownership.scope) {
                return Err("Retained actor census changed its captured physical account".into());
            }
            // A repeated pending discovery may contain the original pre-ID
            // descriptor even after a successful installation. Never regress it.
            let compatible = if phase == NativeActorPhase::Pending && !previous.is_pending() {
                compatible_source(source, &previous.source)
            } else {
                compatible_source(&previous.source, source)
            };
            if !compatible {
                return Err("Retained actor census changed its immutable source".into());
            }
            let retired: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM native_actor_retirements WHERE actor=?1)",
                    [actor.to_string()],
                    |row| row.get(0),
                )
                .map_err(|e| e.to_string())?;
            if retired {
                return Err("A finally retired actor cannot be registered as a new writer".into());
            }
            if phase == NativeActorPhase::Pending {
                // A cleanup intent is immutable, including its original raw
                // source. A current authenticated projection may only promote
                // it through the Installed path.
                return tx.commit().map_err(|e| e.to_string());
            }
        }
        tx.execute("INSERT INTO native_actor_ownerships(actor,ownership) VALUES (?1,?2) ON CONFLICT(actor) DO UPDATE SET ownership=excluded.ownership",params![actor.to_string(),encoded]).map_err(|e|e.to_string())?;
        tx.commit().map_err(|e| e.to_string())
    }

    /// Exact bounded census lookup. Absence is distinct from an unreadable row;
    /// callers must not turn a decode failure into unknown-owner permission.
    pub(in crate::host) fn native_actor_ownership(
        &self,
        actor: Uuid,
    ) -> Result<Option<NativeActorOwnership>, String> {
        let encoded: Option<String> = self
            .db
            .query_row(
                "SELECT ownership FROM native_actor_ownerships WHERE actor=?1",
                [actor.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        encoded
            .map(|encoded| {
                if encoded.len() > MAX_RECEIPT_BYTES {
                    return Err("Stored actor census exceeds its metadata bound".into());
                }
                let ownership: NativeActorOwnership =
                    serde_json::from_str(&encoded).map_err(|e| e.to_string())?;
                if ownership.actor != actor || ownership.actor.is_nil() {
                    return Err("Stored actor census key changed its identity".into());
                }
                Ok(ownership)
            })
            .transpose()
    }

    /// Bounded keyset pages, including retired owners. Under the caller's
    /// account reservation, walk until an empty page; truncation is never a
    /// complete census. Unregistered/legacy discovery remains a separate error.
    pub(in crate::host) fn native_actor_ownerships(
        &self,
        after: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<NativeActorOwnership>, String> {
        if !(1..=128).contains(&limit) {
            return Err("Retained actor census page must contain between one and 128 rows".into());
        }
        let after = after.map(|id| id.to_string()).unwrap_or_default();
        let mut statement=self.db.prepare("SELECT actor,ownership FROM native_actor_ownerships WHERE actor>?1 ORDER BY actor LIMIT ?2").map_err(|e|e.to_string())?;
        let rows = statement
            .query_map(params![after, limit as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| e.to_string())?;
        rows.map(|row| {
            let (actor, encoded) = row.map_err(|e| e.to_string())?;
            if encoded.len() > MAX_RECEIPT_BYTES {
                return Err("Stored actor census exceeds its metadata bound".into());
            }
            let ownership: NativeActorOwnership =
                serde_json::from_str(&encoded).map_err(|e| e.to_string())?;
            if ownership.actor.to_string() != actor || ownership.actor.is_nil() {
                return Err("Stored actor census key changed its identity".into());
            }
            Ok(ownership)
        })
        .collect()
    }

    /// Call only after the authenticated handle's finish_retained_close proves
    /// semantic/loss ACK, exact helper disappearance and discovery retirement.
    /// A completed provider, Stop ACK or an absent socket is never sufficient.
    pub(in crate::host) fn commit_native_actor_retirement(
        &mut self,
        actor: Uuid,
        owner: &Record,
        source: &chat::Session,
        scope: Result<&LaunchAccountScope, &str>,
        confirmed_session: Option<&str>,
    ) -> Result<(), String> {
        let scope = scope
            .map(Clone::clone)
            .map_err(|reason| reason.chars().take(4096).collect::<String>());
        let mut receipt = NativeActorRetirement {
            phase: NativeActorPhase::Installed,
            actor,
            owner: owner.clone(),
            source: source.clone(),
            scope,
            confirmed_session: confirmed_session.map(str::to_owned),
        };
        // Preserve the original captured physical tuple even if its pathname
        // was externally removed/replaced after startup. This records process
        // retirement, not current read/delete authority. The deletion permit
        // separately validates its selected current target under reservation.
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        let known: Option<String> = tx
            .query_row(
                "SELECT ownership FROM native_actor_ownerships WHERE actor=?1",
                [actor.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let known =
            known.ok_or("The retiring actor has no authenticated durable census registration")?;
        if known.len() > MAX_RECEIPT_BYTES {
            return Err("Stored actor census exceeds its metadata bound".into());
        }
        let known: NativeActorOwnership =
            serde_json::from_str(&known).map_err(|e| e.to_string())?;
        receipt.phase = known.phase;
        receipt.validate()?;
        let encoded = serde_json::to_string(&receipt).map_err(|e| e.to_string())?;
        if encoded.len() > MAX_RECEIPT_BYTES {
            return Err("Retained actor retirement exceeds its metadata bound".into());
        }
        if known.actor != actor
            || known.owner != *owner
            || !compatible_source(&known.source, source)
            || !same_scope(&known.scope, &receipt.scope)
        {
            return Err(
                "The retirement proof changed its registered helper or physical account".into(),
            );
        }
        let existing: Option<String> = tx
            .query_row(
                "SELECT receipt FROM native_actor_retirements WHERE actor=?1",
                [actor.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some(existing) = existing {
            if existing != encoded {
                return Err(
                    "Retained actor retirement changed an already committed exact proof".into(),
                );
            }
        } else {
            tx.execute(
                "INSERT INTO native_actor_retirements(actor,receipt) VALUES (?1,?2)",
                params![actor.to_string(), encoded],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())
    }

    /// Evidence for this exact actor only. The caller still needs a mutation
    /// reservation and a fresh complete account/session ownership census.
    pub(in crate::host) fn native_actor_retirement(
        &self,
        actor: Uuid,
    ) -> Result<Option<NativeActorRetirement>, String> {
        let encoded: Option<String> = self
            .db
            .query_row(
                "SELECT receipt FROM native_actor_retirements WHERE actor=?1",
                [actor.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        encoded
            .map(|encoded| {
                if encoded.len() > MAX_RECEIPT_BYTES {
                    return Err("Stored actor retirement exceeds its metadata bound".into());
                }
                let receipt: NativeActorRetirement =
                    serde_json::from_str(&encoded).map_err(|e| e.to_string())?;
                receipt.validate()?;
                if receipt.actor != actor {
                    return Err("Stored actor retirement belongs to another actor".into());
                }
                Ok(receipt)
            })
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    // Exact a96 ownership/retirement field layout: serde accepted unknown fields.
    // Do not reuse the new deny_unknown_fields payload as an old-reader oracle.
    #[derive(Debug, Serialize, Deserialize)]
    struct LegacyOwnership {
        actor: Uuid,
        owner: Record,
        source: chat::Session,
        scope: Result<LaunchAccountScope, String>,
    }
    #[derive(Debug, Serialize, Deserialize)]
    struct LegacyRetirement {
        actor: Uuid,
        owner: Record,
        source: chat::Session,
        scope: Result<LaunchAccountScope, String>,
        confirmed_session: Option<String>,
    }

    fn fixture(root: &std::path::Path) -> (Uuid, Record, chat::Session, LaunchAccountScope) {
        let actor = Uuid::new_v4();
        let proof = crate::model::HostSessionProof {
            host_session_id: "retirement".into(),
            boot_id: "boot".into(),
            host_incarnation: "incarnation".into(),
        };
        let id = crate::model::TerminalId::new();
        let source = chat::Session {
            id,
            launch: chat::Launch {
                provider: crate::agent::Provider::Claude,
                session_id: None,
                recipe: crate::model::LaunchRecipe {
                    program: "claude".into(),
                    arguments: vec![],
                    working_directory: root.into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
            },
            proof: Some(proof.clone()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: chat::NativeContinuity::RetainedClaudeActor { protocol: 2 },
        };
        let owner = Record {
            version: 4,
            build: None,
            kind: crate::daemon::holder_registry::Kind::Native,
            application: crate::instance::APPLICATION_ID.into(),
            boot: proof.boot_id,
            terminal: id,
            holder: crate::daemon::holder_registry::Process { pid: 42, start: 1 },
            root: crate::daemon::holder_registry::Process { pid: 43, start: 2 },
        };
        let metadata = std::fs::metadata(root).unwrap();
        let scope = LaunchAccountScope {
            provider: crate::agent::Provider::Claude,
            canonical_home: root.canonicalize().unwrap(),
            device: metadata.dev(),
            inode: metadata.ino(),
            registered_id: None,
            purge_ids: Vec::new(),
        };
        (actor, owner, source, scope)
    }

    /// Root's serialized build lane runs this producer before the actual a96
    /// Registry load/register/retire/reconcile controls. It never launches a helper.
    #[test]
    #[ignore = "explicit typed old-reader fixture export"]
    fn export_pending_old_reader_fixture() {
        let root = std::path::PathBuf::from(
            std::env::var_os("CAGENTS_TEST_PENDING_READER_FIXTURE").expect(
                "CAGENTS_TEST_PENDING_READER_FIXTURE must name the fixture output directory",
            ),
        );
        assert!(
            root.is_absolute(),
            "fixture destination must be explicit and absolute"
        );
        std::fs::create_dir_all(&root).unwrap();
        let (_, owner, mut source, scope) = fixture(&root);
        let actor = Uuid::parse_str("00000000-0000-4000-8000-000000000100").unwrap();
        source.native_continuity = chat::NativeContinuity::ReconnectOnly;
        let ownership = NativeActorOwnership {
            phase: NativeActorPhase::Pending,
            actor,
            owner: owner.clone(),
            source: source.clone(),
            scope: Ok(scope.clone()),
        };
        let retirement = NativeActorRetirement {
            phase: NativeActorPhase::Pending,
            actor,
            owner,
            source,
            scope: Ok(scope),
            confirmed_session: None,
        };
        retirement.validate().unwrap();
        std::fs::write(
            root.join("ownership.json"),
            serde_json::to_vec(&ownership).unwrap(),
        )
        .unwrap();
        std::fs::write(
            root.join("retirement.json"),
            serde_json::to_vec(&retirement).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn census_accepts_authenticated_conversation_reset_projection_for_registration_and_retirement()
    {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut registry = Registry::at(&directory.path().join("registry")).unwrap();
        let (actor, owner, mut source, scope) = fixture(directory.path());
        source.launch.session_id = Some("11111111-1111-4111-8111-111111111111".into());
        registry
            .register_native_actor_ownership(actor, &owner, &source, Ok(&scope))
            .unwrap();
        // The authenticated helper/caller selects this reset projection. This
        // exercises census persistence, not provider parsing or native execution.
        let mut reset = source.clone();
        reset.launch.session_id = Some("22222222-2222-4222-8222-222222222222".into());
        registry
            .register_native_actor_ownership(actor, &owner, &reset, Ok(&scope))
            .unwrap();
        assert_eq!(
            registry
                .native_actor_ownership(actor)
                .unwrap()
                .unwrap()
                .source(),
            &reset
        );
        registry
            .commit_native_actor_retirement(
                actor,
                &owner,
                &reset,
                Ok(&scope),
                reset.launch.session_id.as_deref(),
            )
            .unwrap();
        let retired = registry.native_actor_retirement(actor).unwrap().unwrap();
        assert_eq!(retired.source(), &reset);
        assert_eq!(
            retired.confirmed_session(),
            reset.launch.session_id.as_deref()
        );

        // Retirement can be the first publication after the acknowledged reset.
        let (other_actor, other_owner, mut original, other_scope) = fixture(directory.path());
        original.launch.session_id = source.launch.session_id.clone();
        registry
            .register_native_actor_ownership(other_actor, &other_owner, &original, Ok(&other_scope))
            .unwrap();
        original.launch.session_id = reset.launch.session_id.clone();
        registry
            .commit_native_actor_retirement(
                other_actor,
                &other_owner,
                &original,
                Ok(&other_scope),
                original.launch.session_id.as_deref(),
            )
            .unwrap();
        assert_eq!(
            registry
                .native_actor_retirement(other_actor)
                .unwrap()
                .unwrap()
                .source(),
            &original
        );

        let (invalid_actor, invalid_owner, mut invalid, invalid_scope) = fixture(directory.path());
        invalid.launch.session_id = Some("invalid-uuid".into());
        assert!(
            registry
                .register_native_actor_ownership(
                    invalid_actor,
                    &invalid_owner,
                    &invalid,
                    Ok(&invalid_scope)
                )
                .is_err(),
            "each projection still requires a valid UUID"
        );
    }

    #[test]
    fn census_accepts_authenticated_pair_projection_and_unpair_without_changing_actor_core() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut registry = Registry::at(&directory.path().join("registry")).unwrap();
        let (actor, owner, mut source, scope) = fixture(directory.path());
        let first = source.proof.clone().unwrap();
        source.terminal = Some(crate::model::TerminalSession {
            id: crate::model::TerminalId::new(),
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            launch: source.launch.recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: crate::model::TerminalContinuity::Live {
                host_session_id: first.host_session_id,
                boot_id: first.boot_id,
                host_incarnation: first.host_incarnation,
            },
        });
        registry
            .register_native_actor_ownership(actor, &owner, &source, Ok(&scope))
            .unwrap();
        let mut current = source.clone();
        let proof = current.proof.as_mut().unwrap();
        proof.host_incarnation = "successor".into();
        current.terminal.as_mut().unwrap().continuity = crate::model::TerminalContinuity::Live {
            host_session_id: proof.host_session_id.clone(),
            boot_id: proof.boot_id.clone(),
            host_incarnation: proof.host_incarnation.clone(),
        };
        registry
            .register_native_actor_ownership(actor, &owner, &current, Ok(&scope))
            .unwrap();
        current.terminal = None;
        registry
            .register_native_actor_ownership(actor, &owner, &current, Ok(&scope))
            .unwrap();
        assert_eq!(
            registry
                .native_actor_ownership(actor)
                .unwrap()
                .unwrap()
                .source(),
            &current
        );
        current
            .launch
            .recipe
            .arguments
            .push("--replace-issued-recipe".into());
        assert!(
            registry
                .register_native_actor_ownership(actor, &owner, &current, Ok(&scope))
                .is_err()
        );
    }

    #[test]
    fn keyed_ownership_distinguishes_absence_corruption_and_exact_pending() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut registry = Registry::at(&directory.path().join("registry")).unwrap();
        let (actor, owner, source, scope) = fixture(directory.path());
        assert!(registry.native_actor_ownership(actor).unwrap().is_none());
        registry
            .register_pending_native_actor_ownership(actor, &owner, &source, Ok(&scope))
            .unwrap();
        let ownership = registry.native_actor_ownership(actor).unwrap().unwrap();
        assert!(ownership.is_pending());
        assert_eq!(ownership.owner(), &owner);
        assert_eq!(ownership.source(), &source);
        let mut wrong = ownership.clone();
        wrong.actor = Uuid::new_v4();
        registry
            .db
            .execute(
                "UPDATE native_actor_ownerships SET ownership=?1 WHERE actor=?2",
                params![serde_json::to_string(&wrong).unwrap(), actor.to_string()],
            )
            .unwrap();
        assert!(registry.native_actor_ownership(actor).is_err());
        registry
            .db
            .execute(
                "UPDATE native_actor_ownerships SET ownership=?1 WHERE actor=?2",
                params![" ".repeat(MAX_RECEIPT_BYTES + 1), actor.to_string()],
            )
            .unwrap();
        assert!(registry.native_actor_ownership(actor).is_err());
    }

    #[test]
    fn pending_wire_roundtrips_raw_source_and_legacy_reader_fails_closed() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let path = directory.path().join("registry");
        let mut registry = Registry::at(&path).unwrap();
        let (actor, owner, mut source, scope) = fixture(directory.path());
        source.native_continuity = chat::NativeContinuity::ReconnectOnly;
        registry
            .register_pending_native_actor_ownership(actor, &owner, &source, Ok(&scope))
            .unwrap();
        registry
            .register_pending_native_actor_ownership(actor, &owner, &source, Ok(&scope))
            .unwrap();
        let row = registry
            .native_actor_ownerships(None, 128)
            .unwrap()
            .pop()
            .unwrap();
        assert!(row.is_pending());
        assert_eq!(row.source(), &source);
        let encoded = serde_json::to_string(&row).unwrap();
        assert!(
            serde_json::from_str::<LegacyOwnership>(&encoded).is_err(),
            "legacy reader must refuse cleanup-only rows"
        );
        assert_eq!(
            serde_json::from_str::<NativeActorOwnership>(&encoded).unwrap(),
            row
        );
        let mut hybrid = serde_json::to_value(&row).unwrap();
        hybrid.as_object_mut().unwrap().extend(
            serde_json::to_value(OwnershipBody {
                actor,
                owner: owner.clone(),
                source: source.clone(),
                scope: Ok(scope.clone()),
            })
            .unwrap()
            .as_object()
            .unwrap()
            .clone(),
        );
        assert!(
            serde_json::from_value::<NativeActorOwnership>(hybrid).is_err(),
            "ambiguous pending/installed authority must fail closed"
        );
        assert!(
            serde_json::from_value::<NativeActorOwnership>(
                serde_json::json!({"future_phase": {"actor": actor}})
            )
            .is_err()
        );
        let mut refreshed = source.clone();
        refreshed.proof.as_mut().unwrap().host_incarnation = "next".into();
        refreshed.native_continuity = chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
        registry
            .register_pending_native_actor_ownership(actor, &owner, &refreshed, Ok(&scope))
            .unwrap();
        assert_eq!(
            registry.native_actor_ownerships(None, 128).unwrap(),
            vec![row.clone()],
            "pending refresh cannot rewrite original cleanup evidence"
        );

        drop(registry);
        let registry = Registry::at(&path).unwrap();
        assert_eq!(
            registry.native_actor_ownerships(None, 128).unwrap(),
            vec![row]
        );
    }

    #[test]
    fn pending_promotes_once_and_never_downgrades_or_changes_exact_identity() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut registry = Registry::at(&directory.path().join("registry")).unwrap();
        let (actor, owner, installed, scope) = fixture(directory.path());
        let mut pending = installed.clone();
        pending.native_continuity = chat::NativeContinuity::ReconnectOnly;
        registry
            .register_pending_native_actor_ownership(actor, &owner, &pending, Ok(&scope))
            .unwrap();
        let mut wrong = scope.clone();
        wrong.inode += 1;
        assert!(
            registry
                .register_pending_native_actor_ownership(actor, &owner, &pending, Ok(&wrong))
                .is_err()
        );
        assert!(
            registry
                .register_pending_native_actor_ownership(actor, &owner, &pending, Err("lost scope"))
                .is_err()
        );
        let mut wrong_owner = owner.clone();
        wrong_owner.root.start += 1;
        assert!(
            registry
                .register_pending_native_actor_ownership(actor, &wrong_owner, &pending, Ok(&scope))
                .is_err()
        );
        let mut wrong_source = pending.clone();
        wrong_source
            .launch
            .recipe
            .arguments
            .push("--changed".into());
        assert!(
            registry
                .register_pending_native_actor_ownership(actor, &owner, &wrong_source, Ok(&scope))
                .is_err()
        );
        assert!(
            registry
                .register_native_actor_ownership(actor, &owner, &pending, Ok(&scope))
                .is_err()
        );
        registry
            .register_native_actor_ownership(actor, &owner, &installed, Ok(&scope))
            .unwrap();
        registry
            .register_pending_native_actor_ownership(actor, &owner, &pending, Ok(&scope))
            .unwrap();
        let row = registry
            .native_actor_ownerships(None, 128)
            .unwrap()
            .pop()
            .unwrap();
        assert!(!row.is_pending());
        assert_eq!(row.source(), &installed);
        let encoded = serde_json::to_string(&row).unwrap();
        let legacy: LegacyOwnership = serde_json::from_str(&encoded).unwrap();
        let mut with_unknown = serde_json::to_value(&legacy).unwrap();
        with_unknown
            .as_object_mut()
            .unwrap()
            .insert("unknown_to_a96".into(), serde_json::json!(true));
        assert!(
            serde_json::from_value::<LegacyOwnership>(with_unknown).is_ok(),
            "the a96 decoder accepted unknown fields"
        );
        assert_eq!(
            encoded,
            serde_json::to_string(&legacy).unwrap(),
            "installed wire must stay exactly legacy"
        );
        assert!(
            !serde_json::from_str::<NativeActorOwnership>(&encoded)
                .unwrap()
                .is_pending()
        );
    }

    #[test]
    fn retirement_continuity_uses_durable_phase_without_skipping_uuid_validation() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut registry = Registry::at(&directory.path().join("registry")).unwrap();
        let (actor, owner, retained, scope) = fixture(directory.path());
        registry
            .register_native_actor_ownership(actor, &owner, &retained, Ok(&scope))
            .unwrap();
        let mut raw = retained.clone();
        raw.native_continuity = chat::NativeContinuity::ReconnectOnly;
        assert!(
            registry
                .commit_native_actor_retirement(actor, &owner, &raw, Ok(&scope), None)
                .is_err(),
            "Installed ownership cannot retire through a raw reconnect descriptor"
        );

        let (pending_actor, pending_owner, mut pending, pending_scope) = fixture(directory.path());
        pending.native_continuity = chat::NativeContinuity::ReconnectOnly;
        registry
            .register_pending_native_actor_ownership(
                pending_actor,
                &pending_owner,
                &pending,
                Ok(&pending_scope),
            )
            .unwrap();
        let mut invalid = pending.clone();
        invalid.launch.session_id = Some("not-a-conversation-uuid".into());
        assert!(
            registry
                .commit_native_actor_retirement(
                    pending_actor,
                    &pending_owner,
                    &invalid,
                    Ok(&pending_scope),
                    None
                )
                .is_err(),
            "ReconnectOnly still validates its source UUID without a confirmed ID"
        );
        registry
            .commit_native_actor_retirement(
                pending_actor,
                &pending_owner,
                &pending,
                Ok(&pending_scope),
                None,
            )
            .unwrap();
        assert!(
            registry
                .native_actor_retirement(pending_actor)
                .unwrap()
                .unwrap()
                .is_pending()
        );
    }

    #[test]
    fn pending_cleanup_preserves_raw_reconnect_source_and_removed_scope() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let home = directory.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let mut registry = Registry::at(&directory.path().join("registry")).unwrap();
        let (actor, owner, retained, scope) = fixture(&home);
        let mut pending = retained.clone();
        pending.native_continuity = chat::NativeContinuity::ReconnectOnly;
        std::fs::remove_dir(&home).unwrap();
        assert!(scope.revalidate().is_err());
        registry
            .register_pending_native_actor_ownership(actor, &owner, &pending, Ok(&scope))
            .unwrap();
        assert!(
            registry
                .register_native_actor_ownership(actor, &owner, &retained, Ok(&scope))
                .is_err()
        );
        registry
            .commit_native_actor_retirement(actor, &owner, &pending, Ok(&scope), None)
            .unwrap();
        registry
            .commit_native_actor_retirement(actor, &owner, &pending, Ok(&scope), None)
            .unwrap();
        let receipt = registry.native_actor_retirement(actor).unwrap().unwrap();
        assert!(receipt.is_pending());
        assert_eq!(receipt.source(), &pending);
        assert_eq!(receipt.scope().unwrap(), &scope);
        let encoded = serde_json::to_string(&receipt).unwrap();
        assert!(
            serde_json::from_str::<LegacyRetirement>(&encoded).is_err(),
            "legacy reader cannot grant history authority from pending retirement"
        );
        assert_eq!(
            serde_json::from_str::<NativeActorRetirement>(&encoded).unwrap(),
            receipt
        );
        assert!(
            registry
                .register_pending_native_actor_ownership(actor, &owner, &pending, Ok(&scope))
                .is_err(),
            "retired pending actor cannot resurrect"
        );
    }

    #[test]
    fn no_event_and_unknown_scope_owners_remain_in_complete_paged_census() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut registry = Registry::at(&directory.path().join("registry")).unwrap();
        let (first, owner, source, scope) = fixture(directory.path());
        registry
            .register_native_actor_ownership(first, &owner, &source, Ok(&scope))
            .unwrap();
        let (unknown, other, mut other_source, _) = fixture(directory.path());
        other_source.launch.session_id = Some("11111111-1111-4111-8111-111111111111".into());
        registry
            .register_native_actor_ownership(
                unknown,
                &other,
                &other_source,
                Err("original captured home unavailable"),
            )
            .unwrap();
        assert_eq!(
            registry
                .db
                .query_row("SELECT COUNT(*) FROM native_actor_cursors", [], |row| row
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            0,
            "census must not depend on any input/outcome having occurred"
        );
        assert!(
            registry.native_actor_retirement(first).unwrap().is_none(),
            "registration is never semantic retirement"
        );
        let mut after = None;
        let mut seen = Vec::new();
        loop {
            let page = registry.native_actor_ownerships(after, 1).unwrap();
            if page.is_empty() {
                break;
            }
            after = Some(page[0].actor());
            seen.extend(page);
        }
        assert_eq!(seen.len(), 2);
        assert!(
            seen.iter()
                .find(|row| row.actor() == unknown)
                .unwrap()
                .scope()
                .is_err()
        );
        assert!(registry.native_actor_ownerships(None, 0).is_err());
        assert!(registry.native_actor_ownerships(None, 129).is_err());
        assert!(
            registry
                .commit_native_actor_retirement(unknown, &other, &other_source, Ok(&scope), None)
                .is_err(),
            "unknown physical scope cannot certify the requested account"
        );
        registry
            .commit_native_actor_retirement(
                unknown,
                &other,
                &other_source,
                Err("original captured home unavailable"),
                None,
            )
            .unwrap();
        let retired = registry.native_actor_retirement(unknown).unwrap().unwrap();
        assert!(
            retired.scope().is_err(),
            "actor retirement does not invent a physical account"
        );
        assert!(
            retired.confirmed_session().is_none(),
            "actor retirement does not invent a named conversation"
        );
        assert_eq!(
            retired.source().launch.session_id,
            other_source.launch.session_id,
            "a resume hint remains recorded without becoming provider-confirmed authority"
        );
        assert_eq!(retired.owner(), &other);
        assert!(
            registry
                .commit_native_actor_retirement(unknown, &other, &other_source, Ok(&scope), None)
                .is_err(),
            "a finally retired unknown owner cannot acquire a later caller's account identity"
        );
    }

    #[test]
    fn exact_retirement_survives_reopen_without_inventing_an_unnamed_session() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let path = directory.path().join("registry");
        let mut registry = Registry::at(&path).unwrap();
        let (actor, owner, source, scope) = fixture(directory.path());
        assert!(
            registry
                .commit_native_actor_retirement(actor, &owner, &source, Ok(&scope), None)
                .is_err(),
            "unregistered owners are not a complete census"
        );
        registry
            .register_native_actor_ownership(actor, &owner, &source, Ok(&scope))
            .unwrap();
        registry
            .commit_native_actor_retirement(actor, &owner, &source, Ok(&scope), None)
            .unwrap();
        registry
            .commit_native_actor_retirement(actor, &owner, &source, Ok(&scope), None)
            .unwrap();
        assert!(
            registry
                .register_native_actor_ownership(actor, &owner, &source, Ok(&scope))
                .is_err(),
            "retired UUID cannot become a new writer"
        );
        drop(registry);
        let mut registry = Registry::at(&path).unwrap();
        let receipt = registry.native_actor_retirement(actor).unwrap().unwrap();
        assert_eq!(receipt.actor(), actor);
        assert_eq!(receipt.owner(), &owner);
        assert_eq!(receipt.source(), &source);
        assert_eq!(receipt.scope().unwrap(), &scope);
        assert!(receipt.confirmed_session().is_none());
        assert!(
            registry
                .commit_native_actor_retirement(
                    actor,
                    &owner,
                    &source,
                    Ok(&scope),
                    Some("11111111-1111-4111-8111-111111111111")
                )
                .is_err(),
            "later callers cannot invent a named history for an already unnamed retirement"
        );
        let mut wrong = scope.clone();
        wrong.inode += 1;
        assert!(
            registry
                .commit_native_actor_retirement(actor, &owner, &source, Ok(&wrong), None)
                .is_err()
        );
        let mut wrong = owner.clone();
        wrong.holder.start += 1;
        assert!(
            registry
                .commit_native_actor_retirement(actor, &wrong, &source, Ok(&scope), None)
                .is_err()
        );
    }

    #[test]
    fn removed_or_replaced_home_does_not_erase_proved_retirement_of_original_scope() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let home = directory.path().join("original-home");
        std::fs::create_dir(&home).unwrap();
        let mut registry = Registry::at(&directory.path().join("registry")).unwrap();
        let (actor, owner, source, scope) = fixture(&home);
        registry
            .register_native_actor_ownership(actor, &owner, &source, Ok(&scope))
            .unwrap();
        let _old_home = std::fs::File::open(&home).unwrap();
        std::fs::remove_dir(&home).unwrap();
        assert!(scope.revalidate().is_err());
        registry
            .commit_native_actor_retirement(actor, &owner, &source, Ok(&scope), None)
            .unwrap();
        std::fs::create_dir(&home).unwrap();
        let (_, _, _, replacement) = fixture(&home);
        assert_ne!(replacement.inode, scope.inode);
        assert!(scope.revalidate().is_err());
        let receipt = registry.native_actor_retirement(actor).unwrap().unwrap();
        assert_eq!(receipt.scope().unwrap(), &scope);
        assert!(
            registry
                .commit_native_actor_retirement(actor, &owner, &source, Ok(&replacement), None)
                .is_err()
        );
        registry
            .commit_native_actor_retirement(actor, &owner, &source, Ok(&scope), None)
            .unwrap();
    }
}
