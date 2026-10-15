//! Bounded descriptive wire data for the original-UI journal bridge.
//!
//! Encoding requires actual producer validation. Decoding yields plain data,
//! never authority: the bridge must first authenticate its one-use challenge,
//! and the receiver must independently validate the original six FD roles.
use super::*;

pub(crate) const CREATION_JOURNAL_BODY_LIMIT: usize = 1024 * 1024;
const PAYLOAD_LIMIT: usize = 512 * 1024;
const TEXT_LIMIT: usize = 64;
const ENTITY_LIMIT: usize = 65536;

#[derive(Serialize)]
struct EncodedRow<'a> {
    schema_version: u8,
    id: OperationId,
    entity_kind: &'a str,
    entity_id: &'a str,
    action: &'a str,
    base_generation: u64,
    payload: &'a serde_json::Value,
    status: &'a str,
    created_at_unix_ms: i64,
    updated_at_unix_ms: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DescribedRow {
    schema_version: u8,
    id: OperationId,
    entity_kind: String,
    entity_id: String,
    action: String,
    base_generation: u64,
    payload: serde_json::Value,
    status: String,
    created_at_unix_ms: i64,
    updated_at_unix_ms: i64,
}

/// Only the original creator's checked row can enter this encoder. The bytes
/// must still be bound to the actual peer/challenge by the owning bridge.
pub(crate) fn encode_original_creation_operation(
    checked: &ValidatedCreationOperation,
) -> Result<Vec<u8>, RepositoryError> {
    let row = checked.operation();
    validate_fields(row)?;
    let description = EncodedRow {
        schema_version: 1,
        id: row.id,
        entity_kind: &row.entity_kind,
        entity_id: &row.entity_id,
        action: &row.action,
        base_generation: row.base_generation,
        payload: &row.payload,
        status: status_name(row.status),
        created_at_unix_ms: row.created_at_unix_ms,
        updated_at_unix_ms: row.updated_at_unix_ms,
    };
    let mut output = LimitedWriter::new(CREATION_JOURNAL_BODY_LIMIT);
    serde_json::to_writer(&mut output, &description).map_err(|_| invalid())?;
    Ok(output.bytes)
}

/// Plain description ONLY. Call after the bridge challenge gate; this cannot
/// construct ValidatedCreationOperation, a scope, a receipt or a host grant.
pub(crate) fn decode_creation_operation_description(
    bytes: &[u8],
) -> Result<PendingOperation, RepositoryError> {
    if bytes.len() > CREATION_JOURNAL_BODY_LIMIT {
        return Err(invalid());
    }
    let description: DescribedRow = serde_json::from_slice(bytes).map_err(|_| invalid())?;
    if description.schema_version != 1 || description.status.len() > TEXT_LIMIT {
        return Err(invalid());
    }
    let status = match description.status.as_str() {
        "pending" => PendingOperationStatus::Pending,
        "needs_reconciliation" => PendingOperationStatus::NeedsReconciliation,
        "applied" => PendingOperationStatus::Applied,
        "rolled_back" => PendingOperationStatus::RolledBack,
        _ => return Err(invalid()),
    };
    let row = PendingOperation {
        id: description.id,
        entity_kind: description.entity_kind,
        entity_id: description.entity_id,
        action: description.action,
        base_generation: description.base_generation,
        payload: description.payload,
        status,
        created_at_unix_ms: description.created_at_unix_ms,
        updated_at_unix_ms: description.updated_at_unix_ms,
    };
    validate_fields(&row)?;
    Ok(row)
}

fn validate_fields(row: &PendingOperation) -> Result<(), RepositoryError> {
    if row.entity_kind.len() > TEXT_LIMIT || row.action.len() > TEXT_LIMIT
        || row.entity_id.len() > ENTITY_LIMIT
    {
        return Err(invalid());
    }
    // Serialize into a bounded sink: escaping cannot allocate an oversized
    // intermediate payload/string even when the in-memory value is large.
    let mut payload = LimitedWriter::new(PAYLOAD_LIMIT);
    serde_json::to_writer(&mut payload, &row.payload).map_err(|_| invalid())?;
    Ok(())
}

fn status_name(status: PendingOperationStatus) -> &'static str {
    match status {
        PendingOperationStatus::Pending => "pending",
        PendingOperationStatus::NeedsReconciliation => "needs_reconciliation",
        PendingOperationStatus::Applied => "applied",
        PendingOperationStatus::RolledBack => "rolled_back",
    }
}
fn invalid() -> RepositoryError {
    ValidationError::Io("The bounded original creation journal description is invalid".into()).into()
}
struct LimitedWriter {
    bytes: Vec<u8>,
    limit: usize,
}
impl LimitedWriter {
    fn new(limit: usize) -> Self { Self { bytes: Vec::new(), limit } }
}
impl io::Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("Creation journal byte limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn description() -> serde_json::Value {
        serde_json::json!({
            "schema_version": 1, "id": OperationId::new(),
            "entity_kind": "worktree_creation", "entity_id": "fixture",
            "action": "create", "base_generation": 7, "payload": {"plain": true},
            "status": "applied", "created_at_unix_ms": 1, "updated_at_unix_ms": 2
        })
    }
    #[test]
    fn descriptive_codec_refuses_schema_fields_status_and_all_byte_bounds() {
        let original = description();
        let parsed = decode_creation_operation_description(&serde_json::to_vec(&original).unwrap()).unwrap();
        assert_eq!(parsed.entity_kind, "worktree_creation");
        assert_eq!(parsed.status, PendingOperationStatus::Applied);
        for (key, value) in [
            ("schema_version", serde_json::json!(2)),
            ("extra", serde_json::json!(true)),
            ("status", serde_json::json!("Applied")),
            ("entity_kind", serde_json::json!("x".repeat(65))),
            ("action", serde_json::json!("x".repeat(65))),
            ("entity_id", serde_json::json!("x".repeat(ENTITY_LIMIT + 1))),
            ("payload", serde_json::json!("x".repeat(PAYLOAD_LIMIT))),
        ] {
            let mut value_with_error = original.clone();
            value_with_error[key] = value;
            assert!(decode_creation_operation_description(&serde_json::to_vec(&value_with_error).unwrap()).is_err(), "{key}");
        }
        assert!(decode_creation_operation_description(&vec![b' '; CREATION_JOURNAL_BODY_LIMIT + 1]).is_err());
        let mut missing = original.clone();
        missing.as_object_mut().unwrap().remove("updated_at_unix_ms");
        assert!(decode_creation_operation_description(&serde_json::to_vec(&missing).unwrap()).is_err());
        let duplicate = format!("{{\"schema_version\":1,{}", &serde_json::to_string(&original).unwrap()[1..]);
        assert!(decode_creation_operation_description(duplicate.as_bytes()).is_err());
    }
    #[test]
    fn bounded_writer_never_keeps_oversized_or_escaping_output() {
        let mut sink = LimitedWriter::new(8);
        assert!(serde_json::to_writer(&mut sink, &"\n".repeat(8)).is_err());
        assert!(sink.bytes.len() <= 8);
    }
}
