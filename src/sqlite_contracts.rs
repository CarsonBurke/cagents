//! Linked-engine assertions shared by the isolated host and UI test lanes.
//! The concurrent WAL regression lives with the real host Registry connections.
use rusqlite::Connection;

const SOURCE_ID: &str =
    "2026-03-05 01:44:06 c7facf7ac58d2cdac619a2da838c5fd5c5270f52b3458c7881b5a4fca1ca4791";

#[test]
fn bundled_engine_is_the_upstream_wal_reset_fixed_source() {
    assert_eq!(rusqlite::version(), "3.50.7");
    assert_eq!(rusqlite::version_number(), 3_050_007);
    let connection = Connection::open_in_memory().unwrap();
    let source: String = connection
        .query_row("SELECT sqlite_source_id()", [], |row| row.get(0))
        .unwrap();
    assert_eq!(source, SOURCE_ID);
    assert_eq!(rusqlite::ffi::SQLITE_VERSION.to_str().unwrap(), "3.50.7");
    assert_eq!(rusqlite::ffi::SQLITE_VERSION_NUMBER, 3_050_007);
    assert_eq!(rusqlite::ffi::SQLITE_SOURCE_ID.to_str().unwrap(), source);
    println!("SQLITE_FIXED_ENGINE_VERIFIED version=3.50.7 source={source}");
}
