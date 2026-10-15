//! Actual Store provenance borrowed only inside its owned source read lease.
//! The codec receives no path/UUID constructor and never retains the owner's
//! exclusive lease. Only its owned receipt may leave the finished snapshot.
//! Namespace admission and reader SH locking happen later in codec order; this
//! capture performs no namespace/flock wait while holding the Store snapshot.
use super::*;
use crate::transcript_cache::export::{SourceRevocationEpoch, StoreSourceReceipt};

pub(crate) struct StoreExportSeed<'a> {
    store: &'a Store,
}
impl StoreExportSeed<'_> {
    pub(crate) fn connection(&self) -> &Connection { &self.store.db }
    pub(crate) fn directory(&self) -> &std::path::Path { &self.store.directory }
    pub(crate) fn owner_lease(&self) -> &std::fs::File { &self.store._lease }
    pub(crate) fn source_incarnation(&self) -> uuid::Uuid { self.store.source_incarnation }
    pub(crate) fn raw_owner_nonce(&self) -> uuid::Uuid { self.store.raw_sources.nonce }
    pub(crate) fn source_revocation(&self) -> &SourceRevocationEpoch { &self.store.source_revocation }
}
impl Store {
    pub(in crate::chat::runtime) fn capture_export_source(&self, expires: std::time::Instant) -> Result<StoreSourceReceipt, String> {
        if std::time::Instant::now() >= expires { return Err("Export source capture deadline expired".into()); }
        self.source_guard()?;
        // An inner tracked lease cannot release its outer SQLite snapshot.
        // Receipt consumers may acquire later codec locks only after this
        // method has actually ended the source transaction it owns.
        if !self.db.is_autocommit() {
            return Err("Export source capture requires its own outermost read lease".into());
        }
        self.with_source_read(|error| error, |store| {
            // This seed cannot escape: factory output is owned, and the read
            // lease discards that output if its explicit release fails.
            let seed = StoreExportSeed { store };
            let probe = seed.source_revocation().probe().map_err(|error| error.to_string())?;
            probe.check().map_err(|error| error.to_string())?;
            if probe.incarnation() != seed.source_incarnation() {
                return Err("Export source revocation identity does not match the actual Store".into());
            }
            StoreSourceReceipt::capture(&seed, expires).map_err(|error| error.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_store_seed_refuses_failed_reset_and_future_raw_schema() {
        let mut store=Store::new().unwrap();
        assert!(store.capture_export_source(std::time::Instant::now()+std::time::Duration::from_secs(1)).is_ok());
        store.refuse_source_reset_for_test(true);
        assert!(store.apply(Event::Reset {reason:"refused intent".into()}).is_err());
        assert!(store.capture_export_source(std::time::Instant::now()+std::time::Duration::from_secs(1)).is_err());
        let store=Store::new().unwrap();
        store.db.execute("UPDATE raw_source_schema SET version=999",[]).unwrap();
        assert!(store.capture_export_source(std::time::Instant::now()+std::time::Duration::from_secs(1)).is_err());
    }
    #[test]
    fn export_capture_refuses_a_nested_source_snapshot_without_poisoning_its_owner() {
        let store=Store::new().unwrap();
        store.with_source_read(|error|error,|store| {
            assert!(store.capture_export_source(std::time::Instant::now()+std::time::Duration::from_secs(1)).is_err());
            store.source_guard()
        }).unwrap();
        assert!(store.db.is_autocommit());
        assert!(store.capture_export_source(std::time::Instant::now()+std::time::Duration::from_secs(1)).is_ok());
    }
    #[test]
    fn export_receipt_does_not_escape_denied_source_snapshot_release() {
        let store=Store::new().unwrap();
        let fault=super::super::read_lease::RollbackFault::install(&store);
        assert!(store.capture_export_source(std::time::Instant::now()+std::time::Duration::from_secs(1)).is_err());
        drop(fault);
        store.db.execute_batch("ROLLBACK").unwrap();
        assert!(store.capture_export_source(std::time::Instant::now()+std::time::Duration::from_secs(1)).is_err(),"release failure remains poisoned");
    }
}
