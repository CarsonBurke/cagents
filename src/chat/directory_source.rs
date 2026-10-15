//! A small comparison reference for a directory-start request. It is neither
//! an owner capability nor a directory grant. The host still authenticates the
//! exact current source and retains its approved FD before acknowledging it.
use super::Session;
use crate::model::{HostSessionProof, TerminalId};
use sha2::{Digest, Sha256};
use std::io::{self, Write};

const MAX_DESCRIPTOR_BYTES: usize = 2 * 1024 * 1024;
const MAX_PROOF_COMPONENT_BYTES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectorySourceRef {
    session: TerminalId,
    host: HostSessionProof,
    descriptor_sha256: [u8; 32],
}

impl DirectorySourceRef {
    /// Compare the exact descriptor without repeating its potentially large
    /// environment or argv in a fresh-start request. No filesystem is read and
    /// no ambient environment is consulted. A reference cannot authorize start.
    pub fn requested(source: &Session) -> Result<Self, String> {
        let host = source.proof.as_ref().ok_or(
            "The original conversation has no proven daemon; reconnect it before continuing",
        )?;
        validate_identity(source.id, host)?;
        let mut writer = DescriptorHash {
            hash: Sha256::new(),
            bytes: 0,
        };
        writer.hash.update(b"cagents:directory-source-descriptor:v1\0");
        serde_json::to_writer(&mut writer, source).map_err(|error| {
            format!("Cannot compare the original conversation descriptor: {error}")
        })?;
        Ok(Self {
            session: source.id,
            host: host.clone(),
            descriptor_sha256: writer.hash.finalize().into(),
        })
    }

    pub fn session(&self) -> TerminalId {
        self.session
    }

    pub fn host(&self) -> &HostSessionProof {
        &self.host
    }

    pub fn validate(&self) -> Result<(), String> {
        validate_identity(self.session, &self.host)
    }

    /// The caller must obtain `current` from its exact hosted owner, not from
    /// another request field. Equality does not replace transition admission,
    /// current actor/instance checks, or approved physical-directory proof.
    pub fn matches(&self, current: &Session) -> Result<bool, String> {
        self.validate()?;
        Ok(Self::requested(current)? == *self)
    }
}

fn validate_identity(session: TerminalId, host: &HostSessionProof) -> Result<(), String> {
    if session.as_uuid().is_nil()
        || !host.is_complete()
        || [
            host.host_session_id.as_str(),
            host.boot_id.as_str(),
            host.host_incarnation.as_str(),
        ]
            .iter()
            .any(|part| part.len() > MAX_PROOF_COMPONENT_BYTES || part.contains('\0'))
    {
        return Err("The original conversation has an invalid daemon identity".into());
    }
    Ok(())
}

struct DescriptorHash {
    hash: Sha256,
    bytes: usize,
}

impl Write for DescriptorHash {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.len() > MAX_DESCRIPTOR_BYTES.saturating_sub(self.bytes) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "The original conversation descriptor exceeds the bounded comparison limit",
            ));
        }
        self.hash.update(buffer);
        self.bytes += buffer.len();
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::Provider,
        model::{BootId, HostIncarnation, HostSessionId, LaunchRecipe},
    };

    fn source() -> Session {
        Session {
            id: TerminalId::new(),
            launch: super::super::Launch {
                provider: Provider::Claude,
                recipe: LaunchRecipe {
                    program: "claude".into(),
                    arguments: Vec::new(),
                    working_directory: "/private/original-project".into(),
                    environment: [(
                        "API_KEY".into(),
                        "private-secret-never-in-reference".into(),
                    )]
                    .into(),
                    unset_environment: Default::default(),
                },
                session_id: Some("22222222-0000-4000-8000-000000000001".into()),
            },
            proof: Some(HostSessionProof {
                host_session_id: HostSessionId::new("source-daemon"),
                boot_id: BootId::new("boot"),
                host_incarnation: HostIncarnation::new("incarnation"),
            }),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        }
    }

    #[test]
    fn reference_is_small_and_matches_only_the_exact_descriptor() {
        let original = source();
        let reference = DirectorySourceRef::requested(&original).unwrap();
        let encoded = serde_json::to_vec(&reference).unwrap();
        assert!(encoded.len() < 512);
        let text = std::str::from_utf8(&encoded).unwrap();
        assert!(!text.contains("private-secret"));
        assert!(!text.contains("original-project"));
        assert!(reference.matches(&original).unwrap());
        assert_eq!(reference.session(), original.id);
        let decoded: DirectorySourceRef = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, reference);
        let wire = postcard::to_allocvec(&reference).unwrap();
        assert_eq!(
            postcard::from_bytes::<DirectorySourceRef>(&wire).unwrap(),
            reference
        );
        for change in 0..5 {
            let mut changed = original.clone();
            match change {
                0 => changed.id = TerminalId::new(),
                1 => changed.launch.recipe.working_directory = "/other".into(),
                2 => {
                    changed
                        .launch
                        .recipe
                        .environment
                        .insert("API_KEY".into(), "different".into());
                }
                3 => changed.launch.session_id = Some("another-thread".into()),
                _ => {
                    changed.proof.as_mut().unwrap().host_incarnation =
                        HostIncarnation::new("replacement");
                }
            }
            assert!(!reference.matches(&changed).unwrap());
        }
    }

    #[test]
    fn oversized_source_is_refused_without_allocating_an_encoded_descriptor() {
        let mut source = source();
        source
            .launch
            .recipe
            .arguments
            .push("x".repeat(MAX_DESCRIPTOR_BYTES));
        let error = DirectorySourceRef::requested(&source).unwrap_err();
        assert!(error.contains("bounded comparison limit"));
        let mut writer = DescriptorHash {
            hash: Sha256::new(),
            bytes: MAX_DESCRIPTOR_BYTES - 2,
        };
        assert_eq!(writer.write(b"ab").unwrap(), 2);
        assert_eq!(
            writer.write(b"c").unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(writer.bytes, MAX_DESCRIPTOR_BYTES);
    }

    #[test]
    fn missing_or_oversized_owner_identity_never_becomes_a_reference() {
        let mut source = source();
        source.proof = None;
        assert!(DirectorySourceRef::requested(&source).is_err());
        let mut source = self::source();
        source.proof.as_mut().unwrap().boot_id =
            BootId::new("x".repeat(MAX_PROOF_COMPONENT_BYTES + 1));
        assert!(DirectorySourceRef::requested(&source).is_err());
        let mut source = self::source();
        source.id = TerminalId::from_uuid(uuid::Uuid::nil());
        assert!(DirectorySourceRef::requested(&source).is_err());
    }
}
