//! Read-only orphan ownership census. Discovery absence or failed adoption is
//! never treated as process completion, and inspection never adopts or closes.
use super::*;
use crate::daemon::holder_registry::{self as holders, Kind, Record};
use std::{fs, os::unix::fs::MetadataExt, path::Path};

/// Unlike startup discovery this must neither ignore unsafe evidence nor
/// remove stale records. A socket without its record is uncertain ownership.
fn records(directory: &Path, kind: Kind) -> Result<Vec<Record>, String> {
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    crate::secure_file::reject_symlink_components(directory).map_err(|error| error.to_string())?;
    if !metadata.is_dir() || metadata.uid() != crate::daemon::current_uid() || metadata.mode() & 0o077 != 0 {
        return Err("Holder inventory directory is not private".into());
    }
    let mut records = Vec::new();
    let mut seen = HashSet::new();
    for (index, entry) in fs::read_dir(directory).map_err(|error| error.to_string())?.enumerate() {
        if index >= 8192 { return Err("Holder inventory exceeds its safe census bound".into()); }
        let path = entry.map_err(|error| error.to_string())?.path();
        let endpoint = if path.extension().is_some_and(|extension| extension == "json") {
            path.with_extension("")
        } else if path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.parse::<crate::model::TerminalId>().is_ok()) {
            path.clone()
        } else {
            // Durable cleanup receipts and temporary publication files do not
            // represent an active discoverable endpoint.
            continue;
        };
        let record = holders::evidence(&endpoint, kind)?.ok_or("Holder endpoint has no authenticated discovery record")?;
        if seen.insert(record.terminal) { records.push(record); }
        if records.len() > MAX_SESSIONS { return Err("Holder inventory exceeds its safe session bound".into()); }
    }
    Ok(records)
}

pub(super) fn require_no_unhosted_owners(host: &SharedHost, reservation: &Reservation) -> Result<(), ProtocolError> {
    let (proof, installed) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let installed = state.terminals.values().filter_map(|record| record.handle.holder_identity().cloned())
            .chain(state.chats.values().filter(|record| record.handle.owns_work()).filter_map(|record| record.handle.native_holder_record().cloned())).map(|record| ((record.kind == Kind::Native, record.terminal), record)).collect::<std::collections::HashMap<_,_>>();
        (state.proof().clone(), installed)
    };
    let registry = super::super::agent_execution::registry(host).map_err(error)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    for kind in [Kind::Native, Kind::Terminal] {
        let directory = holders::directory(kind).map_err(error)?;
        for record in records(&directory, kind).map_err(|reason| error(format!("Retained session census is uncertain: {reason}. No project sessions were closed.")))? {
            if installed.get(&(kind == Kind::Native, record.terminal)) == Some(&record) { continue; }
            if Instant::now() >= deadline { return Err(error("Retained session census reached its time limit. No project sessions were closed; retry after resolving retained sessions.")); }
            let inspected = match kind {
                Kind::Native => crate::chat::native_holder::inspect(&record, &proof).map(|(recovery, _)| {
                    recovery.previous_sources.into_iter().chain(std::iter::once(recovery.source)).map(HostedSession::Chat).collect::<Vec<_>>()
                }),
                Kind::Terminal => crate::terminal::inspect_holder(&holders::socket(&directory, record.terminal), &record, &proof).map(|(recovery, _)| {
                    recovery.previous_sources.into_iter().chain(std::iter::once(recovery.source)).map(HostedSession::Terminal).collect::<Vec<_>>()
                }),
            };
            let sources = inspected.map_err(|_| error(format!("Retained session {} could not be authenticated for project ownership. Reconnect or close its original pane, then retry; no project sessions were closed.", record.terminal)))?;
            let repository = registry.lock().unwrap_or_else(|p| p.into_inner()).retained_session_repository(&sources).map_err(error)?;
            if !reservation.proves_foreign_orphan(&sources, repository.as_deref()) {
                return Err(error(format!("Retained session {} still has unresolved ownership in this project or an uncertain linked worktree. Reconnect or close its original pane before removing the project; no project sessions were closed.", record.terminal)));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn strict_census_never_discards_unsafe_or_incomplete_evidence() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(records(root.path(), Kind::Native).unwrap().is_empty());
        let endpoint = root.path().join(crate::model::TerminalId::new().to_string());
        let _socket = std::os::unix::net::UnixListener::bind(&endpoint).unwrap();
        assert!(records(root.path(), Kind::Native).is_err());
        fs::write(endpoint.with_extension("json"), b"unsafe").unwrap();
        fs::set_permissions(endpoint.with_extension("json"), fs::Permissions::from_mode(0o600)).unwrap();
        assert!(records(root.path(), Kind::Native).is_err());
        assert!(endpoint.exists());
        assert_eq!(fs::read(endpoint.with_extension("json")).unwrap(), b"unsafe");
    }
}
