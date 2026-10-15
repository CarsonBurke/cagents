//! Serializable association evidence. None of these values grants a read.
use super::{Error, Result};
use crate::{agent::Provider, model::TerminalId};
use serde::{Deserialize, Serialize};
use std::{ffi::OsString, os::unix::ffi::{OsStrExt, OsStringExt}, path::{Path, PathBuf}};
use uuid::Uuid;

pub(super) const VERSION: u32 = 1;
pub(super) const MAX_RECORD: usize = 16 * 1024;
pub(super) const MAX_RECORDS: usize = 4096;
pub(super) const MAX_TOTAL: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UnixPath(Vec<u8>);
impl UnixPath {
    pub(crate) fn capture(path: &Path) -> Result<Self> {
        let value = Self(path.as_os_str().as_bytes().to_vec());
        value.validate()?;
        Ok(value)
    }
    pub(crate) fn path(&self) -> PathBuf { PathBuf::from(OsString::from_vec(self.0.clone())) }
    pub(super) fn validate(&self) -> Result<()> {
        if self.0.len() < 2 || self.0.len() > 4096 || self.0[0] != b'/' || self.0.contains(&0)
            || self.0[1..].split(|byte| *byte == b'/').any(|part| part.is_empty() || part == b"." || part == b"..") {
            return Err(Error::InvalidMetadata("Export path must be an exact bounded absolute Unix path"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileIdentity { pub(crate) device: u64, pub(crate) inode: u64 }
impl FileIdentity {
    pub(super) fn validate(self) -> Result<()> {
        if self.inode == 0 { return Err(Error::InvalidMetadata("Export identity has no inode")); }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccountBindingData {
    pub(crate) provider: Provider,
    pub(crate) canonical_home: UnixPath,
    pub(crate) home: FileIdentity,
    pub(crate) registered_id: Option<String>,
    pub(crate) purge_ids: Vec<String>,
}
impl AccountBindingData {
    pub(super) fn validate(&self) -> Result<()> {
        self.canonical_home.validate()?;
        self.home.validate()?;
        if self.registered_id.as_deref().is_some_and(|id| !label(id, 512))
            || self.purge_ids.len() > 64 || self.purge_ids.iter().any(|id| !label(id, 512))
            || self.purge_ids.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(Error::InvalidMetadata("Export account aliases are invalid, excessive, or not canonical"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum SessionNamespace { Unconfirmed, Confirmed { native_id: String } }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CacheIdentity {
    pub(crate) nonce: Uuid,
    pub(crate) root: FileIdentity,
    pub(crate) directory: FileIdentity,
    pub(crate) database: FileIdentity,
    pub(crate) readers_lock: FileIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceReadProofData {
    // These UUIDs are different identities, captured from the actual Store.
    pub(crate) source_incarnation: Uuid,
    pub(crate) raw_owner_nonce: Uuid,
    pub(crate) raw_schema: u32,
    pub(crate) reset_schema: u32,
    pub(crate) text_schema: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BindingData {
    pub(crate) format: u32,
    pub(crate) installation: Uuid,
    pub(crate) application: String,
    pub(crate) association: Uuid,
    pub(crate) cache: CacheIdentity,
    pub(crate) cache_root: UnixPath,
    pub(crate) cache_directory: UnixPath,
    pub(crate) account: AccountBindingData,
    pub(crate) source_session: TerminalId,
    pub(crate) native_namespace: SessionNamespace,
    pub(crate) source: SourceReadProofData,
    pub(crate) export_generation: Uuid,
}
impl BindingData {
    pub(super) fn validate(&self) -> Result<()> {
        if self.format != VERSION { return Err(Error::Unsupported("Export binding format")); }
        if [self.installation, self.association, self.cache.nonce, self.source.source_incarnation,
            self.source.raw_owner_nonce, self.export_generation].iter().any(Uuid::is_nil)
            || !label(&self.application, 256)
            || !self.application.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')) {
            return Err(Error::InvalidMetadata("Export binding identity is invalid"));
        }
        self.account.validate()?;
        self.cache_root.validate()?;
        self.cache_directory.validate()?;
        for identity in [self.cache.root, self.cache.directory, self.cache.database, self.cache.readers_lock] { identity.validate()?; }
        if let SessionNamespace::Confirmed { native_id } = &self.native_namespace
            && !label(native_id, 1024) { return Err(Error::InvalidMetadata("Export native session is invalid")); }
        if self.source.raw_schema != 1 || self.source.reset_schema != 1 || self.source.text_schema != 1 {
            return Err(Error::Unsupported("Export source schema profile"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Phase { Prepared, Committed, Revoked, Removing, Removed }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BindingRecord {
    pub(super) format: u32,
    pub(super) phase: Phase,
    pub(super) commit: Uuid,
    pub(super) binding: BindingData,
}
impl BindingRecord {
    pub(super) fn validate(&self) -> Result<()> {
        if self.format != VERSION { return Err(Error::Unsupported("Export index record format")); }
        if self.commit.is_nil() { return Err(Error::InvalidMetadata("Export index record has no commit identity")); }
        self.binding.validate()
    }
}

fn label(value: &str, limit: usize) -> bool {
    !value.is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_path_codec_keeps_non_utf8_without_lossy_authority_aliases() {
        let bytes = b"/private/account-\xff".to_vec();
        let path = PathBuf::from(OsString::from_vec(bytes.clone()));
        let captured = UnixPath::capture(&path).unwrap();
        let decoded: UnixPath = serde_json::from_slice(&serde_json::to_vec(&captured).unwrap()).unwrap();
        assert_eq!(decoded.path().as_os_str().as_bytes(), bytes);
        for bad in [b"relative".as_slice(), b"/a/../b", b"/a/./b", b"/a//b", b"/a\0b"] {
            assert!(UnixPath(bad.to_vec()).validate().is_err());
        }
    }
}
