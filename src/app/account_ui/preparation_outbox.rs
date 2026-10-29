//! Private local intent only: no RPC, launch, cancellation ACK or retirement.
//! Call these blocking operations on a worker, never on the UI thread. Each
//! operation releases its process lock before returning to the effect owner.
use crate::account::preparation::{
    ExistingPendingBinding, ExistingPendingState, ExistingPendingTarget, PreparationBinding,
    PreparationPayload,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::fd::AsRawFd,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
};
use uuid::Uuid;

const VERSION: u32 = 3;
// Migration shares the original namespace and lock: a saved Cancel must never
// disappear behind a newly missing version-specific filename.
const JOURNAL: &str = "preparations-v1.json";
const LOCK: &str = ".preparations.lock";
const MAX_BYTES: u64 = 1024 * 1024;

/// These are public routing fields, never credentials or a launch recipe.
/// Equality cannot prove SSH destination or first-Plan installation continuity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Route {
    Local {
        application: String,
        state_directory: PathBuf,
    },
    Remote {
        name: String,
        host: String,
        executable: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RouteSnapshot {
    route: Route,
    route_revision: String,
}

impl RouteSnapshot {
    pub(super) fn capture(route: Route) -> io::Result<Self> {
        let route_revision = route_revision(&route)?;
        Ok(Self {
            route,
            route_revision,
        })
    }
}

fn route_revision(route: &Route) -> io::Result<String> {
    let text = |value: &str| {
        !value.is_empty() && value.len() <= 4096 && !value.chars().any(char::is_control)
    };
    let valid = match route {
        Route::Local {
            application,
            state_directory,
        } => {
            text(application)
                && state_directory.is_absolute()
                && state_directory.to_str().is_some_and(text)
        }
        Route::Remote {
            name,
            host,
            executable,
        } => text(name) && text(host) && text(executable),
    };
    if !valid {
        return Err(invalid("The original account setup route is invalid"));
    }
    let bytes = serde_json::to_vec(route).map_err(invalid)?;
    Ok(format!("route-v1:{:x}", Sha256::digest(bytes)))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Intent {
    pub(super) request_id: Uuid,
    pub(super) target: Target,
    pub(super) original_route: RouteSnapshot,
    pub(super) device_code: bool,
    pub(super) blocked_login: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Target {
    Prepared { payload: PreparationPayload },
    ExistingPending { original: ExistingPendingTarget },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Binding {
    Prepared { binding: PreparationBinding },
    ExistingPending { binding: ExistingPendingBinding },
}

impl Binding {
    fn home(&self) -> (Uuid, Uuid) {
        match self {
            Self::Prepared { binding } => (binding.installation_id, binding.key),
            Self::ExistingPending { binding } => (binding.installation_id, binding.key),
        }
    }
}

fn supported_provider(provider: crate::agent::Provider) -> bool {
    matches!(
        provider,
        crate::agent::Provider::Claude | crate::agent::Provider::Codex
    )
}

fn validate_existing_binding(binding: &ExistingPendingBinding) -> io::Result<()> {
    if !supported_provider(binding.provider)
        || binding.installation_id.is_nil()
        || binding.key.is_nil()
        || binding.generation_id.is_nil()
    {
        return Err(invalid(
            "The existing account setup binding is inconsistent",
        ));
    }
    Ok(())
}

impl Intent {
    fn validate(&self) -> io::Result<()> {
        if self.request_id.is_nil()
            || self.blocked_login.is_some_and(|id| id.is_nil())
            || self.original_route.route_revision != route_revision(&self.original_route.route)?
        {
            return Err(invalid("The account setup intent is inconsistent"));
        }
        match &self.target {
            Target::Prepared { payload } if !supported_provider(payload.provider) => {
                return Err(invalid("The account setup provider is unsupported"));
            }
            Target::ExistingPending { original } => match original {
                ExistingPendingTarget::Observed {
                    provider,
                    key,
                    expected_installation,
                    ..
                } => {
                    if !supported_provider(*provider)
                        || key.is_nil()
                        || expected_installation.is_some_and(|id| id.is_nil())
                    {
                        return Err(invalid(
                            "The existing account setup observation is inconsistent",
                        ));
                    }
                }
                ExistingPendingTarget::Bound { binding } => validate_existing_binding(binding)?,
            },
            _ => {}
        }
        Ok(())
    }

    fn initial_binding(&self) -> Option<Binding> {
        match &self.target {
            Target::ExistingPending {
                original: ExistingPendingTarget::Bound { binding },
            } => Some(Binding::ExistingPending {
                binding: binding.clone(),
            }),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Desired {
    /// ExistingPending only resumes metadata; it never plans or creates a home.
    Prepare,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ParentIntent {
    pub(super) request_id: Uuid,
    pub(super) revision_at_consent: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum AttemptAction {
    Resume,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Attempt {
    pub(super) id: Uuid,
    pub(super) action: AttemptAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Phase {
    Retained,
    InFlight {
        attempt: Attempt,
    },
    NeedsReconcile {
        attempt: Attempt,
    },
    Refused {
        attempt: Attempt,
    },
    MetadataConfirmed {
        attempt: Attempt,
        state: ExistingPendingState,
    },
}

impl Phase {
    fn attempt(&self) -> Option<&Attempt> {
        match self {
            Self::Retained => None,
            Self::InFlight { attempt }
            | Self::NeedsReconcile { attempt }
            | Self::Refused { attempt }
            | Self::MetadataConfirmed { attempt, .. } => Some(attempt),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Record {
    pub(super) intent: Intent,
    pub(super) desired: Desired,
    pub(super) binding: Option<Binding>,
    pub(super) revision: u64,
    pub(super) caused_by: Option<ParentIntent>,
    pub(super) phase: Phase,
}

/// A successful worker return owns one exact durable attempt, not merely a
/// Resume payload echoed by a remote installation. Keep this with HomeRequest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AttemptEnvelope {
    pub(super) intent: Intent,
    pub(super) desired: Desired,
    pub(super) binding: Option<Binding>,
    pub(super) revision: u64,
    pub(super) attempt: Attempt,
}

impl Record {
    fn envelope(&self) -> io::Result<AttemptEnvelope> {
        let Phase::InFlight { attempt } = &self.phase else {
            return Err(conflict("The account setup attempt is not in flight"));
        };
        Ok(AttemptEnvelope {
            intent: self.intent.clone(),
            desired: self.desired,
            binding: self.binding.clone(),
            revision: self.revision,
            attempt: attempt.clone(),
        })
    }
}

impl Record {
    fn validate(&self) -> io::Result<()> {
        self.intent.validate()?;
        if self.revision == 0 {
            return Err(invalid("The account setup intent has no local revision"));
        }
        if let Some(parent) = &self.caused_by
            && (parent.request_id.is_nil()
                || parent.request_id == self.intent.request_id
                || parent.revision_at_consent == 0)
        {
            return Err(invalid("The account setup consent link is invalid"));
        }
        if let Some(attempt) = self.phase.attempt() {
            if attempt.id.is_nil() || !matches!(self.intent.target, Target::ExistingPending { .. })
            {
                return Err(invalid("The account setup attempt is inconsistent"));
            }
            if matches!(self.phase, Phase::MetadataConfirmed { .. }) && self.binding.is_none() {
                return Err(invalid("The account setup confirmation has no binding"));
            }
        }
        if let Target::ExistingPending {
            original: ExistingPendingTarget::Bound { binding: original },
        } = &self.intent.target
            && !matches!(&self.binding, Some(Binding::ExistingPending { binding }) if binding == original)
        {
            return Err(invalid(
                "The original account setup binding was not retained",
            ));
        }
        if let Some(binding) = &self.binding {
            validate_binding(&self.intent, binding)?;
        }
        Ok(())
    }
}

fn validate_binding(intent: &Intent, binding: &Binding) -> io::Result<()> {
    let matches = match (&intent.target, binding) {
        (Target::Prepared { payload }, Binding::Prepared { binding }) => {
            binding.request_id == intent.request_id
                && binding.payload == *payload
                && !binding.installation_id.is_nil()
                && !binding.key.is_nil()
        }
        (Target::ExistingPending { original }, Binding::ExistingPending { binding }) => {
            validate_existing_binding(binding)?;
            match original {
                ExistingPendingTarget::Observed {
                    provider,
                    key,
                    started_at,
                    expected_installation,
                } => {
                    binding.provider == *provider
                        && binding.key == *key
                        && binding.started_at == *started_at
                        && expected_installation
                            .is_none_or(|expected| binding.installation_id == expected)
                }
                ExistingPendingTarget::Bound { binding: original } => binding == original,
            }
        }
        _ => false,
    };
    if !matches {
        return Err(invalid(
            "The account setup binding does not match its intent",
        ));
    }
    Ok(())
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    records: Vec<Record>,
}

impl Journal {
    fn exact(&self, intent: &Intent) -> io::Result<&Record> {
        let record = self
            .records
            .iter()
            .find(|record| record.intent.request_id == intent.request_id)
            .ok_or_else(missing_intent)?;
        if &record.intent != intent {
            return Err(conflict(
                "The account setup intent or original route changed",
            ));
        }
        Ok(record)
    }

    fn replace(&mut self, record: Record) {
        let saved = self
            .records
            .iter_mut()
            .find(|saved| saved.intent.request_id == record.intent.request_id)
            .expect("the exact stored record was checked under this lock");
        *saved = record;
    }

    fn validate(&self) -> io::Result<()> {
        if self.version != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "The account setup outbox version is unsupported",
            ));
        }
        let mut ids = BTreeSet::new();
        let mut homes = BTreeSet::new();
        let mut successors = BTreeSet::new();
        for record in &self.records {
            record.validate()?;
            if !ids.insert(record.intent.request_id) {
                return Err(invalid("The account setup outbox repeats an intent"));
            }
            if let Some(binding) = &record.binding
                && !homes.insert(binding.home())
            {
                return Err(invalid("The account setup outbox repeats a bound home"));
            }
            if let Some(link) = &record.caused_by {
                let parent = self
                    .records
                    .iter()
                    .find(|parent| parent.intent.request_id == link.request_id)
                    .ok_or_else(|| invalid("The account setup consent parent is missing"))?;
                let installation = consent_installation(parent, &record.intent)?;
                if link.revision_at_consent > parent.revision
                    || !successors.insert((link.request_id, installation))
                {
                    return Err(invalid("The account setup consent history is inconsistent"));
                }
            }
        }
        Ok(())
    }

    fn has_child(&self, id: Uuid) -> bool {
        self.records.iter().any(|record| {
            record
                .caused_by
                .as_ref()
                .is_some_and(|parent| parent.request_id == id)
        })
    }

    fn mutable_owner(&self, id: Uuid) -> io::Result<()> {
        if self.has_child(id) {
            return Err(conflict(
                "The original account setup has a retained successor",
            ));
        }
        Ok(())
    }

    fn parent_cancelled(&self, record: &Record) -> bool {
        record.caused_by.as_ref().is_some_and(|link| {
            self.records.iter().any(|parent| {
                parent.intent.request_id == link.request_id && parent.desired == Desired::Cancel
            })
        })
    }

    fn admit_attempt(&self, record: &Record, attempt: &Attempt) -> io::Result<()> {
        self.mutable_owner(record.intent.request_id)?;
        let dispatchable = matches!(
            record.intent.target,
            Target::ExistingPending {
                original: ExistingPendingTarget::Observed {
                    expected_installation: Some(_),
                    ..
                } | ExistingPendingTarget::Bound { .. }
            }
        );
        if !dispatchable
            || attempt.id.is_nil()
            || !matches!(
                (record.desired, attempt.action),
                (Desired::Prepare, AttemptAction::Resume)
                    | (Desired::Cancel, AttemptAction::Cancel)
            )
        {
            return Err(conflict(
                "The account setup attempt is not currently admitted",
            ));
        }
        if attempt.action == AttemptAction::Resume && self.parent_cancelled(record) {
            return Err(conflict(
                "The original account setup was cancelled; retain reconciliation",
            ));
        }
        Ok(())
    }
}

fn consent_installation(parent: &Record, child: &Intent) -> io::Result<Uuid> {
    let matches = match (&parent.intent.target, &child.target) {
        (
            Target::ExistingPending {
                original:
                    ExistingPendingTarget::Observed {
                        provider,
                        key,
                        started_at,
                        expected_installation: None,
                    },
            },
            Target::ExistingPending {
                original:
                    ExistingPendingTarget::Observed {
                        provider: child_provider,
                        key: child_key,
                        started_at: child_started,
                        expected_installation: Some(installation),
                    },
            },
        ) if provider == child_provider && key == child_key && started_at == child_started => {
            Some(*installation)
        }
        _ => None,
    };
    if parent.caused_by.is_some()
        || parent.binding.is_some()
        || parent.intent.request_id == child.request_id
        || parent.intent.original_route != child.original_route
        || parent.intent.device_code != child.device_code
        || parent.intent.blocked_login != child.blocked_login
    {
        return Err(conflict(
            "The original account setup must remain held unchanged",
        ));
    }
    matches.ok_or_else(|| conflict("The refreshed account setup does not match its original"))
}

fn require_revision(record: &Record, expected: u64) -> io::Result<()> {
    if record.revision != expected {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "The account setup intent has a newer local revision",
        ));
    }
    Ok(())
}

fn advance(record: &mut Record) -> io::Result<()> {
    record.revision = record
        .revision
        .checked_add(1)
        .ok_or_else(|| invalid("The account setup revision is exhausted"))?;
    Ok(())
}

// Keep the original wire schema intact until its exact fields are checked.
// In particular, mapping first would discard unknown nested backend fields.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct V1Intent {
    request_id: Uuid,
    payload: PreparationPayload,
    original_route: RouteSnapshot,
    device_code: bool,
    blocked_login: Option<Uuid>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct V1Record {
    intent: V1Intent,
    desired: Desired,
    binding: Option<PreparationBinding>,
    revision: u64,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct V1Journal {
    version: u32,
    records: Vec<V1Record>,
}

impl V1Journal {
    fn into_current(self) -> Journal {
        Journal {
            version: VERSION,
            records: self
                .records
                .into_iter()
                .map(|record| Record {
                    intent: Intent {
                        request_id: record.intent.request_id,
                        target: Target::Prepared {
                            payload: record.intent.payload,
                        },
                        original_route: record.intent.original_route,
                        device_code: record.intent.device_code,
                        blocked_login: record.intent.blocked_login,
                    },
                    desired: record.desired,
                    binding: record.binding.map(|binding| Binding::Prepared { binding }),
                    revision: record.revision,
                    caused_by: None,
                    phase: Phase::Retained,
                })
                .collect(),
        }
    }
}

// Frozen V2 has no causal or phase fields. Do not deserialize V2 through the
// expanded V3 record, even with serde defaults: check its exact original wire.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct V2Record {
    intent: Intent,
    desired: Desired,
    binding: Option<Binding>,
    revision: u64,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct V2Journal {
    version: u32,
    records: Vec<V2Record>,
}

impl V2Journal {
    fn into_current(self) -> Journal {
        Journal {
            version: VERSION,
            records: self
                .records
                .into_iter()
                .map(|record| Record {
                    intent: record.intent,
                    desired: record.desired,
                    binding: record.binding,
                    revision: record.revision,
                    caused_by: None,
                    phase: Phase::Retained,
                })
                .collect(),
        }
    }
}

// Reject duplicates in the original document, including inside internally
// tagged backend DTOs. A Value-only round trip would silently keep one value.
struct UniqueJson(serde_json::Value);

impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UniqueVisitor;
        impl<'de> serde::de::Visitor<'de> for UniqueVisitor {
            type Value = UniqueJson;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("JSON without duplicate fields")
            }

            fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }

            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }

            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }

            fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(value)
                    .map(|number| UniqueJson(serde_json::Value::Number(number)))
                    .ok_or_else(|| E::custom("The account setup outbox has an invalid number"))
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }

            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueJson(serde_json::Value::Null))
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element::<UniqueJson>()? {
                    values.push(value.0);
                }
                Ok(UniqueJson(serde_json::Value::Array(values)))
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(serde::de::Error::custom(
                            "The account setup outbox repeats a field",
                        ));
                    }
                    values.insert(key, map.next_value::<UniqueJson>()?.0);
                }
                Ok(UniqueJson(serde_json::Value::Object(values)))
            }
        }
        deserializer.deserialize_any(UniqueVisitor)
    }
}

fn canonical_fields(original: &serde_json::Value, wire: &impl Serialize) -> io::Result<()> {
    // Shared backend DTOs have broader serde compatibility. Their omitted or
    // unsupported fields must not be discarded by a private journal rewrite.
    if *original != serde_json::to_value(wire).map_err(invalid)? {
        return Err(invalid("The account setup outbox has unsupported fields"));
    }
    Ok(())
}

fn decode_journal(bytes: &[u8]) -> io::Result<(Journal, u32)> {
    let original = serde_json::from_slice::<UniqueJson>(bytes)
        .map_err(invalid)?
        .0;
    let version = original
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| invalid("The account setup outbox version is invalid"))?;
    let journal = match version {
        1 => {
            let wire: V1Journal = serde_json::from_slice(bytes).map_err(invalid)?;
            canonical_fields(&original, &wire)?;
            wire.into_current()
        }
        2 => {
            let wire: V2Journal = serde_json::from_slice(bytes).map_err(invalid)?;
            canonical_fields(&original, &wire)?;
            wire.into_current()
        }
        version if version == u64::from(VERSION) => {
            let wire: Journal = serde_json::from_slice(bytes).map_err(invalid)?;
            canonical_fields(&original, &wire)?;
            wire
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "The account setup outbox version is unsupported",
            ));
        }
    };
    journal.validate()?;
    Ok((journal, version as u32))
}

pub(super) struct Store {
    directory: PathBuf,
}

impl Store {
    pub(super) fn at(directory: PathBuf) -> Self {
        Self { directory }
    }

    /// None means the journal is missing, not an empty/acknowledged outbox.
    /// A read normalizes V1/V2 in memory without writing. It establishes visibility
    /// only, never a durability acknowledgement or a completed migration.
    pub(super) fn load(&self) -> io::Result<Option<Vec<Record>>> {
        let Some(held) = self.lock(false)? else {
            return Ok(None);
        };
        let result = self.read(&held)?.map(|(journal, _, _)| journal.records);
        held.revalidate()?;
        Ok(result)
    }

    /// A complete Cancel may arrive before the first Prepare worker. An old
    /// Prepare can confirm persistence but can never downgrade that Cancel.
    pub(super) fn upsert(&self, intent: &Intent, desired: Desired) -> io::Result<Record> {
        self.change(intent, Change::Intent(desired), Namespace::sync)
    }

    /// Attach only an exact binding to an existing intent at its current local
    /// revision. A concurrent Cancel requires rereading and retrying this CAS.
    pub(super) fn bind(
        &self,
        intent: &Intent,
        expected_revision: u64,
        binding: &Binding,
    ) -> io::Result<Record> {
        self.change(
            intent,
            Change::Bind(expected_revision, binding),
            Namespace::sync,
        )
    }

    /// Only a distinct explicit consent action may insert this successor.
    /// Refresh/load cannot mint it, and the original record is never rewritten.
    pub(super) fn reaffirm(
        &self,
        parent: &ParentIntent,
        intent: &Intent,
        desired: Desired,
    ) -> io::Result<Record> {
        intent.validate()?;
        self.transaction(
            false,
            |journal| {
                let original = journal
                    .records
                    .iter()
                    .find(|record| record.intent.request_id == parent.request_id)
                    .ok_or_else(missing_intent)?;
                if parent.revision_at_consent == 0 || parent.revision_at_consent > original.revision
                {
                    return Err(conflict("The account setup consent revision is invalid"));
                }
                let installation = consent_installation(original, intent)?;
                if original.desired == Desired::Cancel && desired != Desired::Cancel {
                    return Err(conflict(
                        "The original account setup was cancelled before consent",
                    ));
                }
                let inserted = Record {
                    intent: intent.clone(),
                    desired,
                    binding: None,
                    revision: 1,
                    caused_by: Some(parent.clone()),
                    phase: Phase::Retained,
                };
                if let Some(existing) = journal
                    .records
                    .iter()
                    .find(|record| record.intent.request_id == intent.request_id)
                {
                    if existing != &inserted {
                        return Err(conflict("The account setup successor changed"));
                    }
                    return Ok(existing.clone());
                }
                if journal.records.iter().any(|record| {
                    record
                        .caused_by
                        .as_ref()
                        .is_some_and(|link| link.request_id == parent.request_id)
                        && matches!(&record.intent.target, Target::ExistingPending {
                        original: ExistingPendingTarget::Observed {
                            expected_installation: Some(id), ..
                        }
                    } if *id == installation)
                }) {
                    return Err(conflict(
                        "The account setup already has a different retained successor",
                    ));
                }
                journal.records.push(inserted.clone());
                journal
                    .records
                    .sort_by_key(|record| record.intent.request_id);
                Ok(inserted)
            },
            Namespace::sync,
        )
    }

    /// A returned envelope proves the local pre-dispatch durability barrier.
    /// It never confers backend, provider, login or physical-home authority.
    pub(super) fn begin(
        &self,
        intent: &Intent,
        expected_revision: u64,
        attempt: &Attempt,
    ) -> io::Result<AttemptEnvelope> {
        self.begin_with_sync(intent, expected_revision, attempt, Namespace::sync)
    }

    fn begin_with_sync(
        &self,
        intent: &Intent,
        expected_revision: u64,
        attempt: &Attempt,
        sync: impl FnOnce(&Namespace) -> io::Result<()>,
    ) -> io::Result<AttemptEnvelope> {
        self.transaction(
            false,
            |journal| {
                let mut record = journal.exact(intent)?.clone();
                require_revision(&record, expected_revision)?;
                journal.admit_attempt(&record, attempt)?;
                if matches!(record.phase, Phase::InFlight { .. })
                    || record
                        .phase
                        .attempt()
                        .is_some_and(|previous| previous.id == attempt.id)
                {
                    return Err(conflict("The account setup attempt is already owned"));
                }
                record.phase = Phase::InFlight {
                    attempt: attempt.clone(),
                };
                advance(&mut record)?;
                journal.replace(record.clone());
                Ok(record)
            },
            sync,
        )?
        .envelope()
    }

    /// Binding and settlement share one CAS, journal publication and barrier.
    /// The caller must additionally validate the full echoed backend request.
    pub(super) fn settle(
        &self,
        owner: &AttemptEnvelope,
        binding: Option<&Binding>,
        phase: &Phase,
    ) -> io::Result<Record> {
        self.settle_with_sync(owner, binding, phase, Namespace::sync)
    }

    fn settle_with_sync(
        &self,
        owner: &AttemptEnvelope,
        binding: Option<&Binding>,
        phase: &Phase,
        sync: impl FnOnce(&Namespace) -> io::Result<()>,
    ) -> io::Result<Record> {
        if matches!(phase, Phase::Retained | Phase::InFlight { .. })
            || phase.attempt() != Some(&owner.attempt)
        {
            return Err(conflict("The account setup result lost its exact attempt"));
        }
        if let Some(binding) = binding {
            validate_binding(&owner.intent, binding)?;
        }
        self.transaction(
            false,
            |journal| {
                let mut record = journal.exact(&owner.intent)?.clone();
                if record.envelope()? != *owner {
                    return Err(conflict(
                        "The account setup result belongs to an older local attempt",
                    ));
                }
                journal.admit_attempt(&record, &owner.attempt)?;
                if let Some(binding) = binding {
                    if record
                        .binding
                        .as_ref()
                        .is_some_and(|saved| saved != binding)
                    {
                        return Err(conflict("The account setup binding changed"));
                    }
                    record.binding = Some(binding.clone());
                }
                record.phase = phase.clone();
                advance(&mut record)?;
                journal.replace(record.clone());
                Ok(record)
            },
            sync,
        )
    }

    /// Explicit reconciliation HOLD preserves the previous attempt and grants
    /// no dispatch/settlement ACK, including after the parent was cancelled.
    pub(super) fn hold(
        &self,
        intent: &Intent,
        expected_revision: u64,
        attempt: &Attempt,
    ) -> io::Result<Record> {
        self.transaction(
            false,
            |journal| {
                journal.mutable_owner(intent.request_id)?;
                let mut record = journal.exact(intent)?.clone();
                require_revision(&record, expected_revision)?;
                if record.phase.attempt() != Some(attempt) {
                    return Err(conflict(
                        "The account setup reconciliation lost its exact attempt",
                    ));
                }
                let phase = Phase::NeedsReconcile {
                    attempt: attempt.clone(),
                };
                if record.phase != phase {
                    record.phase = phase;
                    advance(&mut record)?;
                    journal.replace(record.clone());
                }
                Ok(record)
            },
            Namespace::sync,
        )
    }

    /// A visible rename after a failed barrier is not an ACK. Only exact
    /// post-state replay under the same owner guards may confirm its durability.
    pub(super) fn replay(&self, expected: &Record) -> io::Result<Record> {
        self.replay_with_sync(expected, Namespace::sync)
    }

    /// Recover an uncertain pre-dispatch write through the exact barrier again.
    /// A load alone cannot produce this return value.
    pub(super) fn replay_attempt(&self, expected: &Record) -> io::Result<AttemptEnvelope> {
        self.replay(expected)?.envelope()
    }

    fn replay_with_sync(
        &self,
        expected: &Record,
        sync: impl FnOnce(&Namespace) -> io::Result<()>,
    ) -> io::Result<Record> {
        self.transaction(
            false,
            |journal| {
                journal.mutable_owner(expected.intent.request_id)?;
                let record = journal.exact(&expected.intent)?;
                if record != expected {
                    return Err(conflict(
                        "The account setup post-state changed before durability replay",
                    ));
                }
                // Replaying HOLD confirms local retention only, including a
                // Resume owner whose parent was cancelled. It cannot yield an
                // outbound envelope: replay_attempt requires InFlight.
                if !matches!(record.phase, Phase::NeedsReconcile { .. })
                    && let Some(attempt) = record.phase.attempt()
                {
                    journal.admit_attempt(record, attempt)?;
                }
                Ok(record.clone())
            },
            sync,
        )
    }

    fn change(
        &self,
        intent: &Intent,
        change: Change<'_>,
        sync: impl FnOnce(&Namespace) -> io::Result<()>,
    ) -> io::Result<Record> {
        intent.validate()?;
        if let Change::Bind(_, binding) = change {
            validate_binding(intent, binding)?;
        }
        self.transaction(
            matches!(change, Change::Intent(_)),
            |journal| {
                if matches!(change, Change::Bind(..)) {
                    journal.mutable_owner(intent.request_id)?;
                    let record = journal.exact(intent)?;
                    if !matches!(record.phase, Phase::Retained)
                        || (record.desired == Desired::Prepare && journal.parent_cancelled(record))
                    {
                        return Err(conflict(
                            "The account setup binding needs its retained atomic result owner",
                        ));
                    }
                }
                let index = journal
                    .records
                    .iter()
                    .position(|record| record.intent.request_id == intent.request_id);
                let record = match index {
                    Some(index) => {
                        let record = &mut journal.records[index];
                        if &record.intent != intent {
                            return Err(conflict(
                                "The account setup intent or original route changed",
                            ));
                        }
                        let changed = match change {
                            Change::Intent(desired) => {
                                let cancel =
                                    desired == Desired::Cancel && record.desired != desired;
                                if cancel {
                                    record.desired = Desired::Cancel;
                                }
                                cancel
                            }
                            Change::Bind(expected, binding) => {
                                if record
                                    .binding
                                    .as_ref()
                                    .is_some_and(|saved| saved != binding)
                                {
                                    return Err(conflict("The account setup binding changed"));
                                }
                                require_revision(record, expected)?;
                                let unbound = record.binding.is_none();
                                record.binding = Some(binding.clone());
                                unbound
                            }
                        };
                        if changed {
                            advance(record)?;
                        }
                        record.clone()
                    }
                    None => {
                        let Change::Intent(desired) = change else {
                            return Err(missing_intent());
                        };
                        let record = Record {
                            intent: intent.clone(),
                            desired,
                            binding: intent.initial_binding(),
                            revision: 1,
                            caused_by: None,
                            phase: Phase::Retained,
                        };
                        journal.records.push(record.clone());
                        journal
                            .records
                            .sort_by_key(|record| record.intent.request_id);
                        record
                    }
                };
                Ok(record)
            },
            sync,
        )
    }

    fn transaction(
        &self,
        create: bool,
        change: impl FnOnce(&mut Journal) -> io::Result<Record>,
        sync: impl FnOnce(&Namespace) -> io::Result<()>,
    ) -> io::Result<Record> {
        let held = self.lock(create)?.ok_or_else(missing_intent)?;
        let previous = self.read(&held)?;
        let mut journal = previous
            .as_ref()
            .map(|(journal, _, _)| journal.clone())
            .unwrap_or(Journal {
                version: VERSION,
                records: Vec::new(),
            });
        let record = change(&mut journal)?;
        journal.validate()?;
        // Include schema growth even when a legacy record is otherwise equal.
        // Refuse before creating a temporary file or publishing any new bytes.
        let bytes = serde_json::to_vec(&journal).map_err(invalid)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(invalid("The account setup outbox is too large"));
        }
        let persisted = if previous
            .as_ref()
            .is_some_and(|(old, _, version)| *version == VERSION && old == &journal)
        {
            // Equality and a successful reread prove visibility, not durable
            // persistence after an earlier rename/directory-sync failure.
            let file = previous.as_ref().unwrap().1.try_clone()?;
            self.revalidate_journal(&held, Some(&file))?;
            file.sync_all()?;
            file
        } else {
            // An equal V1/V2 replay still rewrites V3 atomically. Only encoding
            // changes in that case; the record's local revision stays put.
            let mut temporary = tempfile::Builder::new()
                .prefix(".preparations-")
                .permissions(fs::Permissions::from_mode(0o600))
                .tempfile_in(held.namespace.path())?;
            temporary.write_all(&bytes)?;
            temporary.as_file().sync_all()?;
            held.revalidate()?;
            self.revalidate_journal(&held, previous.as_ref().map(|(_, file, _)| file))?;
            temporary
                .persist(held.namespace.path().join(JOURNAL))
                .map_err(|error| error.error)?
        };
        sync(&held.namespace)?;
        self.revalidate_journal(&held, Some(&persisted))?;
        held.revalidate()?;
        Ok(record)
    }

    fn read(&self, held: &Locked) -> io::Result<Option<(Journal, File, u32)>> {
        let path = held.namespace.path().join(JOURNAL);
        let mut file = match crate::secure_file::open_existing_private_file_readonly(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        require_private_file(&file)?;
        let mut bytes = Vec::new();
        (&mut file).take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(invalid("The account setup outbox is too large"));
        }
        let (journal, original_version) = decode_journal(&bytes)?;
        self.revalidate_journal(held, Some(&file))?;
        Ok(Some((journal, file, original_version)))
    }

    fn revalidate_journal(&self, held: &Locked, previous: Option<&File>) -> io::Result<()> {
        let named = fs::symlink_metadata(held.namespace.path().join(JOURNAL));
        match (previous, named) {
            (None, Err(error)) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            (Some(file), Ok(named)) if same_file(&file.metadata()?, &named) => {
                require_private_file(file)
            }
            (_, Err(error)) => Err(error),
            _ => Err(invalid("The account setup journal changed during storage")),
        }
    }

    fn lock(&self, create: bool) -> io::Result<Option<Locked>> {
        if !self.directory.is_absolute()
            || self
                .directory
                .components()
                .any(|part| part == Component::ParentDir)
        {
            return Err(invalid(
                "The account setup outbox needs an absolute local directory",
            ));
        }
        crate::secure_file::reject_symlink_components(&self.directory)?;
        match fs::symlink_metadata(&self.directory) {
            Ok(metadata) => {
                crate::secure_file::validate_owned_directory(&self.directory)?;
                if metadata.mode() & 0o7777 != 0o700 {
                    return Err(invalid("The account setup outbox directory is not private"));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound && !create => return Ok(None),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                crate::secure_file::prepare_private_directory(&self.directory)?;
            }
            Err(error) => return Err(error),
        }
        let namespace = Namespace::capture(&self.directory)?;
        let path = namespace.path().join(LOCK);
        let file = match OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                crate::secure_file::open_existing_private_file_readonly(&path)?
            }
            Err(error) => return Err(error),
        };
        require_private_file(&file)?;
        let lock = crate::instance::owned_file_lock(file, libc::LOCK_EX | libc::LOCK_NB)?;
        let held = Locked {
            lock,
            lock_path: path,
            namespace,
        };
        held.revalidate()?;
        Ok(Some(held))
    }
}

#[derive(Clone, Copy)]
enum Change<'a> {
    Intent(Desired),
    Bind(u64, &'a Binding),
}

struct Locked {
    lock: crate::instance::OwnedLock,
    lock_path: PathBuf,
    namespace: Namespace,
}

impl Locked {
    fn revalidate(&self) -> io::Result<()> {
        self.namespace.revalidate()?;
        require_private_file(&self.lock)?;
        if !same_file(
            &self.lock.metadata()?,
            &fs::symlink_metadata(&self.lock_path)?,
        ) {
            return Err(invalid("The account setup outbox lock changed"));
        }
        Ok(())
    }
}

// Retain every containing binding so recursively created directories also
// reach disk. This local namespace has no relation to the selected device.
struct Namespace(Vec<(PathBuf, File)>);

impl Namespace {
    fn capture(directory: &Path) -> io::Result<Self> {
        let held = directory
            .ancestors()
            .map(|path| {
                let file = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(path)?;
                Ok((path.to_owned(), file))
            })
            .collect::<io::Result<Vec<_>>>()?;
        let namespace = Self(held);
        namespace.revalidate()?;
        Ok(namespace)
    }

    fn revalidate(&self) -> io::Result<()> {
        for (index, (path, file)) in self.0.iter().enumerate() {
            let named = fs::symlink_metadata(path)?;
            if !named.is_dir()
                || !same_file(&file.metadata()?, &named)
                || (index == 0 && (named.uid() != current_uid() || named.mode() & 0o7777 != 0o700))
            {
                return Err(invalid("The account setup outbox directory changed"));
            }
        }
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        self.revalidate()?;
        for (_, file) in &self.0 {
            file.sync_all()?;
        }
        self.revalidate()
    }

    fn path(&self) -> PathBuf {
        // Resolve journal, lock and temporary names through our retained local
        // directory, even if another process replaces an ancestor path.
        PathBuf::from(format!("/proc/self/fd/{}", self.0[0].1.as_raw_fd()))
    }
}

fn require_private_file(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != current_uid()
        || metadata.nlink() != 1
        || metadata.mode() & 0o7777 != 0o600
    {
        return Err(invalid(
            "The account setup outbox file is not owned and private",
        ));
    }
    Ok(())
}

fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

fn invalid(message: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

fn conflict(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::AlreadyExists, message)
}

fn missing_intent() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        "The account setup intent is missing; it was not acknowledged",
    )
}

fn current_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and does not change process state.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests;
