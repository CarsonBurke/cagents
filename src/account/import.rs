//! Metadata contracts for importing one explicitly staged Codex file cache.
//!
//! Credential bytes remain in the host-only opaque cache leaf. Deserializing an
//! observation or receipt grants no file, home-creation, or provider authority.
//! Registration and execution measurements are separate, explicitly requested
//! stages; none of these types represents a task-readiness permit.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[cfg(feature = "host")]
pub(crate) mod cache;
#[cfg(feature = "host")]
pub(crate) mod journal;

/// Application bound, not a provider promise about every credential format.
pub(crate) const MAX_CACHE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalIdentity {
    pub(crate) device: u64,
    pub(crate) inode: u64,
}

impl PhysicalIdentity {
    pub fn device(self) -> u64 {
        self.device
    }
    pub fn inode(self) -> u64 {
        self.inode
    }
}

/// A stale-able observation of an explicitly selected file, never its contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceObservation {
    pub(crate) path: PathBuf,
    pub(crate) parent: PhysicalIdentity,
    pub(crate) file: PhysicalIdentity,
    pub(crate) size: u64,
    pub(crate) mtime_seconds: i64,
    pub(crate) mtime_nanoseconds: i64,
    pub(crate) ctime_seconds: i64,
    pub(crate) ctime_nanoseconds: i64,
}

impl SourceObservation {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn parent(&self) -> PhysicalIdentity {
        self.parent
    }
    pub fn file(&self) -> PhysicalIdentity {
        self.file
    }
    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.path.is_absolute()
            || self.path.as_os_str().as_encoded_bytes().contains(&0)
            || self.parent.inode == 0
            || self.file.inode == 0
            || self.size == 0
            || self.size > MAX_CACHE_BYTES
            || !(0..1_000_000_000).contains(&self.mtime_nanoseconds)
            || !(0..1_000_000_000).contains(&self.ctime_nanoseconds)
        {
            return Err("The staged cache observation is invalid; inspect the file again");
        }
        Ok(())
    }
}

/// The exact new inode prepared before publication. This is metadata only and
/// cannot reconstruct an opaque Prepared cache or a fresh-home creation receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationReceipt {
    pub(crate) operation: Uuid,
    pub(crate) home: PhysicalIdentity,
    pub(crate) file: PhysicalIdentity,
    pub(crate) size: u64,
}

impl PublicationReceipt {
    pub fn operation(&self) -> Uuid {
        self.operation
    }
    pub fn home(&self) -> PhysicalIdentity {
        self.home
    }
    pub fn file(&self) -> PhysicalIdentity {
        self.file
    }
    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.operation.is_nil()
            || self.home.inode == 0
            || self.file.inode == 0
            || self.size == 0
            || self.size > MAX_CACHE_BYTES
        {
            return Err("The cache publication receipt is invalid");
        }
        Ok(())
    }
}

/// The UI/CLI records explicit consent; this is not proof that another process
/// or machine has stopped using a copied provider cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopyConsent {
    pub staged_file_only: bool,
    pub source_session_limit_acknowledged: bool,
}

/// No implicit System configuration or network permission is inherited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodingProfile {
    Unconfigured,
    WorkspaceWrite { network_access: bool },
}

// Human-readable plans keep their existing tagged JSON/TOML schema. Postcard
// cannot deserialize an internal tag's self-describing visitor; its matched
// protocol uses an ordinary external enum with an explicit variant order.
mod coding_profile_serde {
    use super::CodingProfile;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize, Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
    enum Human {
        Unconfigured {},
        WorkspaceWrite { network_access: bool },
    }
    #[derive(Serialize, Deserialize)]
    enum Binary {
        Unconfigured,
        WorkspaceWrite { network_access: bool },
    }
    impl Serialize for CodingProfile {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            if serializer.is_human_readable() {
                match *self {
                    Self::Unconfigured => Human::Unconfigured {},
                    Self::WorkspaceWrite { network_access } => {
                        Human::WorkspaceWrite { network_access }
                    }
                }
                .serialize(serializer)
            } else {
                match *self {
                    Self::Unconfigured => Binary::Unconfigured,
                    Self::WorkspaceWrite { network_access } => {
                        Binary::WorkspaceWrite { network_access }
                    }
                }
                .serialize(serializer)
            }
        }
    }
    impl<'de> Deserialize<'de> for CodingProfile {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            if deserializer.is_human_readable() {
                Ok(match Human::deserialize(deserializer)? {
                    Human::Unconfigured {} => Self::Unconfigured,
                    Human::WorkspaceWrite { network_access } => {
                        Self::WorkspaceWrite { network_access }
                    }
                })
            } else {
                Ok(match Binary::deserialize(deserializer)? {
                    Binary::Unconfigured => Self::Unconfigured,
                    Binary::WorkspaceWrite { network_access } => {
                        Self::WorkspaceWrite { network_access }
                    }
                })
            }
        }
    }
}

/// Immutable original intent. The operation is also the destination managed key.
/// Later diagnostics use their existing registered-account APIs and own explicit
/// opt-ins; they cannot be silently added by an import completion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportPlan {
    pub operation: Uuid,
    pub destination: crate::model::HostSessionProof,
    pub observation: SourceObservation,
    pub expected_email: String,
    pub consent: CopyConsent,
    pub profile: CodingProfile,
}

pub(crate) fn expected_email_is_valid(email: &str) -> bool {
    !email.is_empty()
        && email.len() <= 320
        && email.trim() == email
        && !email.chars().any(|c| c.is_control() || c.is_whitespace())
        && email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && !domain.is_empty() && !domain.contains('@')
        })
}

impl ImportPlan {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.operation.is_nil() {
            return Err("Choose a nonnil import operation");
        }
        if !self.destination.is_complete() {
            return Err("Inspect the destination daemon before importing the cache");
        }
        self.observation.validate()?;
        if !expected_email_is_valid(&self.expected_email) {
            return Err("Provide the expected ChatGPT account email explicitly");
        }
        if !self.consent.staged_file_only || !self.consent.source_session_limit_acknowledged {
            return Err("Confirm the staged file copy and source-session limitations");
        }
        Ok(())
    }
}

/// Metadata requests only. Inspect accepts a destination-device path; the daemon
/// expands/revalidates it. Verify never accepts a replacement email, key, or plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ImportOperation {
    /// Read all bounded retained import metadata, without advancing any stage.
    List,
    Inspect {
        path: PathBuf,
    },
    Prepare {
        plan: ImportPlan,
    },
    Status {
        operation: Uuid,
    },
    Verify {
        operation: Uuid,
        destination: crate::model::HostSessionProof,
    },
}

/// Selected-daemon inspection. The host proof must match the connection used
/// by Prepare; the file observation alone cannot select another installation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportInspection {
    pub destination: crate::model::HostSessionProof,
    pub observation: SourceObservation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportStage {
    Reserved,
    Prepared,
    Published,
    Registered,
}

/// Read-only metadata presence is not a content or identity verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportPublication {
    Missing,
    Present,
    Different,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportStatus {
    pub observed_destination: crate::model::HostSessionProof,
    pub plan: ImportPlan,
    pub receipt: Option<PublicationReceipt>,
    pub stage: ImportStage,
    pub uncertain: bool,
    pub publication: ImportPublication,
    pub registered: Option<super::AccountId>,
    /// Acknowledgement from this explicit Verify only; None does not mean absent.
    pub coding_profile: Option<super::coding_config::CreateReport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ImportReply {
    Listed(Vec<ImportStatus>),
    Inspected(ImportInspection),
    Status(ImportStatus),
}

#[cfg(all(test, feature = "host"))]
mod tests {
    use super::*;

    fn plan() -> ImportPlan {
        ImportPlan {
            operation: Uuid::from_u128(1),
            destination: crate::model::HostSessionProof {
                host_session_id: crate::model::HostSessionId::new("import-fixture"),
                boot_id: crate::model::BootId::new("fixture-boot"),
                host_incarnation: crate::model::HostIncarnation::new("fixture-incarnation"),
            },
            observation: SourceObservation {
                path: "/private/staged/auth.json".into(),
                parent: PhysicalIdentity {
                    device: 2,
                    inode: 3,
                },
                file: PhysicalIdentity {
                    device: 2,
                    inode: 4,
                },
                size: 64,
                mtime_seconds: 5,
                mtime_nanoseconds: 6,
                ctime_seconds: 5,
                ctime_nanoseconds: 7,
            },
            expected_email: "fixture@example.test".into(),
            consent: CopyConsent {
                staged_file_only: true,
                source_session_limit_acknowledged: true,
            },
            profile: CodingProfile::WorkspaceWrite {
                network_access: false,
            },
        }
    }

    #[test]
    fn explicit_verify_requires_a_current_destination_without_rewriting_the_original_plan() {
        let original = plan();
        let mut current = original.destination.clone();
        current.host_incarnation = crate::model::HostIncarnation::new("new-incarnation");
        let verify = ImportOperation::Verify {
            operation: original.operation,
            destination: current.clone(),
        };
        assert_eq!(
            serde_json::from_value::<ImportOperation>(serde_json::to_value(&verify).unwrap())
                .unwrap(),
            verify
        );
        assert!(
            serde_json::from_value::<ImportOperation>(
                serde_json::json!({"Verify":{"operation":original.operation}})
            )
            .is_err()
        );
        let status = ImportStatus {
            observed_destination: current,
            plan: original.clone(),
            receipt: None,
            stage: ImportStage::Reserved,
            uncertain: false,
            publication: ImportPublication::Missing,
            registered: None,
            coding_profile: None,
        };
        assert_eq!(status.plan, original);
        assert_ne!(status.observed_destination, status.plan.destination);
        assert_eq!(status.plan.operation, original.operation);
    }

    #[test]
    fn imported_intent_requires_explicit_identity_consent_and_a_bounded_observation() {
        let original = plan();
        assert!(original.validate().is_ok());
        for mode in 0..8 {
            let mut invalid = original.clone();
            match mode {
                0 => invalid.operation = Uuid::nil(),
                1 => invalid.expected_email = "wrong".into(),
                2 => invalid.consent.staged_file_only = false,
                3 => invalid.consent.source_session_limit_acknowledged = false,
                4 => invalid.observation.size = MAX_CACHE_BYTES + 1,
                5 => invalid.observation.path = "relative/auth.json".into(),
                6 => invalid.observation.ctime_nanoseconds = 1_000_000_000,
                _ => invalid.destination.boot_id = crate::model::BootId::new(""),
            }
            assert!(invalid.validate().is_err(), "case {mode}");
        }
        let encoded = serde_json::to_value(&original).unwrap();
        assert_eq!(
            serde_json::from_value::<ImportPlan>(encoded.clone()).unwrap(),
            original
        );
        let mut changed = encoded;
        changed
            .as_object_mut()
            .unwrap()
            .insert("credential_bytes".into(), "not accepted".into());
        assert!(serde_json::from_value::<ImportPlan>(changed).is_err());
    }
}

#[cfg(test)]
mod wire_tests;
