//! Local receipts for confirmed stops missing from Claude Code's transcript.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

const RECENT_CARRIERS: usize = 8;
const MAX_ID_BYTES: usize = 256;
const MAX_RECEIPT_BYTES: u64 = 16 * 1024;

#[derive(Default)]
pub(super) struct Carriers {
    first: Option<String>,
    recent: VecDeque<String>,
}

impl Carriers {
    pub(super) fn observe(&mut self, id: &str) {
        if id.is_empty() || id.len() > MAX_ID_BYTES || self.recent.back().is_some_and(|s| s == id) {
            return;
        }
        self.first.get_or_insert_with(|| id.to_owned());
        if self.recent.len() == RECENT_CARRIERS {
            self.recent.pop_front();
        }
        self.recent.push_back(id.to_owned());
    }

    pub(super) fn finish(self, id: String, prompt: Option<&str>) -> Receipt {
        let prompt = prompt.filter(|id| !id.is_empty() && id.len() <= MAX_ID_BYTES);
        Receipt {
            id,
            anchors: self
                .recent
                .into_iter()
                .rev()
                .chain(self.first)
                .chain(prompt.map(str::to_owned))
                .collect(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct Receipt {
    pub(super) id: String,
    /// Newest first, with the opening prompt as a fallback when the last
    /// frames were streamed without saving them.
    pub(super) anchors: Vec<String>,
}

pub(super) fn root() -> Option<PathBuf> {
    Some(
        crate::instance::project_dirs()?
            .data_local_dir()
            .join("claude-stop-markers"),
    )
}

fn file_name(id: &str) -> String {
    format!("{:x}.json", Sha256::digest(id.as_bytes()))
}

pub(super) fn save(root: &Path, session: &str, receipt: &Receipt) -> io::Result<()> {
    if receipt.anchors.is_empty() {
        return Ok(());
    }
    super::validate_id(session).map_err(io::Error::other)?;
    let bytes = serde_json::to_vec(receipt)?;
    if bytes.len() as u64 > MAX_RECEIPT_BYTES {
        return Err(io::Error::other(
            "Claude stop marker exceeds its size limit",
        ));
    }
    let directory = root.join(session);
    crate::secure_file::prepare_private_directory(&directory)?;
    let mut file = tempfile::NamedTempFile::new_in(&directory)?;
    file.write_all(&bytes)?;
    file.as_file().sync_all()?;
    match file.persist_noclobber(directory.join(file_name(&receipt.id))) {
        Ok(_) => File::open(&directory)?.sync_all(),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.error),
    }
}

pub(super) fn load(root: &Path, session: &str) -> io::Result<Vec<Receipt>> {
    super::validate_id(session).map_err(io::Error::other)?;
    let directory = root.join(session);
    crate::secure_file::reject_symlink_components(&directory)?;
    let entries = match fs::read_dir(directory) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        result => result?,
    };
    let mut receipts = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        match read(&path) {
            Ok(receipt) => receipts.push(receipt),
            Err(error) => tracing::warn!(%error, "Cannot read a saved Claude stop marker"),
        }
    }
    Ok(receipts)
}

fn read(path: &Path) -> io::Result<Receipt> {
    let mut bytes = Vec::new();
    crate::secure_file::open_existing_private_file_readonly(path)?
        .take(MAX_RECEIPT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RECEIPT_BYTES {
        return Err(io::Error::other(
            "Claude stop marker exceeds its size limit",
        ));
    }
    let receipt: Receipt = serde_json::from_slice(&bytes)?;
    if path
        .file_name()
        .is_none_or(|name| name != file_name(&receipt.id).as_str())
    {
        return Err(io::Error::other(
            "Claude stop marker identity does not match its file",
        ));
    }
    Ok(receipt)
}
