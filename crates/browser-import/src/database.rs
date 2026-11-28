use crate::{Batch, Cookie, Error, Keys, Profile, SameSite, Site, cancelled, chromium::Decryptor};
use rusqlite::{
    Connection, MAIN_DB, OpenFlags,
    backup::{Backup, StepResult},
    config::DbConfig,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

const MAX_DATABASE: u64 = 256 * 1024 * 1024;
const MAX_COOKIES: usize = 100_000;

fn sql(error: rusqlite::Error) -> Error {
    match error.sqlite_error_code() {
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => {
            Error::Busy
        }
        Some(rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt) => {
            Error::Damaged
        }
        _ => Error::Unreadable,
    }
}

/// A private in-memory copy of the profile's database, so reading never
/// writes to the browser's files or holds their locks while records are read.
fn snapshot(profile: &Profile, cancel: &AtomicBool) -> Result<Connection, Error> {
    match backup(profile, cancel) {
        // A running Firefox holds its cookie database under an exclusive lock
        // that SQLite readers cannot pass, but the files remain readable.
        Err(Error::Busy) => {
            let image = crate::locked::image(&profile.database, MAX_DATABASE, cancel)?;
            let mut connection = Connection::open_in_memory().map_err(sql)?;
            harden(&connection)?;
            connection
                .deserialize_read_exact(MAIN_DB, &image[..], image.len(), true)
                .map_err(sql)?;
            Ok(connection)
        }
        result => result,
    }
}

fn backup(profile: &Profile, cancel: &AtomicBool) -> Result<Connection, Error> {
    cancelled(cancel)?;
    let metadata = std::fs::metadata(&profile.database).map_err(|_| Error::Unreadable)?;
    if !metadata.is_file() {
        return Err(Error::Unreadable);
    }
    if metadata.len() > MAX_DATABASE {
        return Err(Error::TooLarge);
    }
    let source = Connection::open_with_flags(
        &profile.database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(sql)?;
    harden(&source)?;
    source
        .busy_timeout(Duration::from_millis(200))
        .map_err(sql)?;
    let pages: u64 = source
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .map_err(sql)?;
    let page_size: u64 = source
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .map_err(sql)?;
    if pages.saturating_mul(page_size) > MAX_DATABASE {
        return Err(Error::TooLarge);
    }
    let mut target = Connection::open_in_memory().map_err(sql)?;
    harden(&target)?;
    {
        let backup = Backup::new(&source, &mut target).map_err(sql)?;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            cancelled(cancel)?;
            if Instant::now() >= deadline {
                return Err(Error::Busy);
            }
            let step = backup.step(256).map_err(sql)?;
            if (backup.progress().pagecount.max(0) as u64).saturating_mul(page_size) > MAX_DATABASE
            {
                return Err(Error::TooLarge);
            }
            match step {
                StepResult::Done => break,
                StepResult::More => {}
                StepResult::Busy | StepResult::Locked => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                _ => return Err(Error::Unreadable),
            }
        }
    }
    Ok(target)
}

fn harden(connection: &Connection) -> Result<(), Error> {
    // Profile databases are data, not executable SQL. A replaced cookies table
    // could otherwise be a view that expands a tiny file into unbounded work.
    connection
        .set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_VIEW, false)
        .map_err(sql)?;
    connection
        .set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)
        .map_err(sql)?;
    // SQLite may spill intermediate results to disk even for an in-memory DB.
    // Keep profile data in memory throughout scanning and extraction.
    connection
        .pragma_update(None, "temp_store", "MEMORY")
        .map_err(sql)?;
    Ok(())
}

pub(super) fn scan(profile: &Profile, cancel: &AtomicBool) -> Result<Vec<Site>, Error> {
    let connection = snapshot(profile, cancel)?;
    table_columns(
        &connection,
        if profile.browser.chromium() {
            "cookies"
        } else {
            "moz_cookies"
        },
    )?;
    firefox_version(profile, &connection)?;
    let query = if profile.browser.chromium() {
        "SELECT host_key FROM cookies LIMIT 100001"
    } else {
        "SELECT host FROM moz_cookies LIMIT 100001"
    };
    let mut statement = connection.prepare(query).map_err(sql)?;
    let mut rows = statement.query([]).map_err(sql)?;
    let mut sites = BTreeMap::<String, usize>::new();
    let mut count = 0;
    while let Some(row) = rows.next().map_err(sql)? {
        cancelled(cancel)?;
        count += 1;
        if count > MAX_COOKIES {
            return Err(Error::TooLarge);
        }
        let host: String = row.get(0).map_err(sql)?;
        if let Some(host) = valid_host(&host) {
            *sites.entry(host).or_default() += 1;
        }
    }
    Ok(sites
        .into_iter()
        .map(|(host, cookies)| Site { host, cookies })
        .collect())
}

fn firefox_version(profile: &Profile, connection: &Connection) -> Result<i64, Error> {
    if profile.browser.chromium() {
        return Ok(0);
    }
    let version = connection
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .map_err(sql)?;
    if !(1..=17).contains(&version) {
        return Err(Error::UnsupportedSchema);
    }
    Ok(version)
}

fn table_columns(connection: &Connection, table: &str) -> Result<BTreeSet<String>, Error> {
    let (kind, definition): (String, String) = connection
        .query_row(
            "SELECT type, sql FROM sqlite_schema WHERE name = ?1 AND type IN ('table', 'view')",
            [table],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(sql)?;
    // `pragma_table_list` connects virtual tables to determine their columns,
    // which can execute expressions in unrelated FTS shadow tables. Inspect
    // stored declarations instead, conservatively allowing ordinary browser
    // CREATE TABLE statements before requesting any column metadata.
    let mut words = definition.split_whitespace();
    if kind != "table"
        || !words
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("CREATE"))
        || !words
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("TABLE"))
    {
        return Err(Error::UnsupportedSchema);
    }
    // These static identifiers name ordinary browser tables. `table_info`
    // omits generated columns; `table_xinfo` lets us reject those too, before
    // innocuous schema functions can expand a tiny DB into huge allocations.
    let mut statement = connection
        .prepare(&format!("PRAGMA table_xinfo({table})"))
        .map_err(sql)?;
    let mut rows = statement.query([]).map_err(sql)?;
    let mut columns = BTreeSet::new();
    while let Some(row) = rows.next().map_err(sql)? {
        if row.get::<_, i64>(6).map_err(sql)? != 0 {
            return Err(Error::UnsupportedSchema);
        }
        columns.insert(row.get(1).map_err(sql)?);
    }
    Ok(columns)
}

pub(super) fn extract(
    profile: &Profile,
    selected: &BTreeSet<String>,
    keys: &mut impl Keys,
    cancel: &AtomicBool,
    now_unix: i64,
) -> Result<Batch, Error> {
    if selected.is_empty() {
        return Ok(Batch {
            cookies: Vec::new(),
            skipped: 0,
            undecryptable: 0,
        });
    }
    let connection = snapshot(profile, cancel)?;
    let chromium = profile.browser.chromium();
    let firefox_version = firefox_version(profile, &connection)?;
    let columns = table_columns(
        &connection,
        if chromium { "cookies" } else { "moz_cookies" },
    )?;
    let version: i64 = if chromium {
        table_columns(&connection, "meta")?;
        connection
            .query_row("SELECT value FROM meta WHERE key = 'version'", [], |row| {
                row.get::<_, String>(0)
            })
            .map_err(sql)?
            .parse()
            .map_err(|_| Error::UnsupportedSchema)?
    } else {
        0
    };
    let partition_key = if chromium {
        if columns.contains("top_frame_site_key") {
            "top_frame_site_key"
        } else {
            "''"
        }
    } else if columns.contains("originAttributes") {
        "originAttributes"
    } else {
        "''"
    };
    // Chromium's ancestor bit is true even for unpartitioned cookies. Only
    // the nonempty top-frame key identifies a Chromium partition.
    let partition_flag = if !chromium && columns.contains("isPartitionedAttributeSet") {
        "isPartitionedAttributeSet"
    } else {
        "0"
    };
    let partition =
        format!("(coalesce({partition_key}, '') <> '' OR coalesce({partition_flag}, 0) <> 0)");
    let same_site = if columns.contains(if chromium { "samesite" } else { "sameSite" }) {
        if chromium { "samesite" } else { "sameSite" }
    } else {
        "-1"
    };
    let query = if chromium {
        format!(
            "SELECT host_key, path, name, value, encrypted_value, is_secure, is_httponly, {same_site}, expires_utc, {partition} FROM cookies LIMIT 100001"
        )
    } else {
        format!(
            "SELECT host, path, name, value, X'', isSecure, isHttpOnly, {same_site}, expiry, {partition} FROM moz_cookies LIMIT 100001"
        )
    };
    let mut statement = connection.prepare(&query).map_err(sql)?;
    let mut rows = statement.query([]).map_err(sql)?;
    let mut batch = Batch {
        cookies: Vec::new(),
        skipped: 0,
        undecryptable: 0,
    };
    let mut decryptor = profile.browser.keyring().map(Decryptor::new);
    let mut unsupported = 0;
    let mut count = 0;
    let mut imported_bytes = 0usize;
    while let Some(row) = rows.next().map_err(sql)? {
        cancelled(cancel)?;
        count += 1;
        if count > MAX_COOKIES {
            return Err(Error::TooLarge);
        }
        let raw_host: String = row.get(0).map_err(sql)?;
        let Some(host) = valid_host(&raw_host) else {
            batch.skipped += 1;
            continue;
        };
        if !selected.contains(&host) {
            continue;
        }
        let partition: bool = row.get(9).map_err(sql)?;
        let site: i64 = row.get(7).map_err(sql)?;
        let expiry: i64 = row.get(8).map_err(sql)?;
        let expires_unix = if chromium && expiry == 0 {
            None
        } else if chromium {
            Some(expiry / 1_000_000 - 11_644_473_600)
        } else if firefox_version >= 16 {
            Some(expiry / 1000)
        } else {
            Some(expiry)
        };
        let secure: bool = row.get(5).map_err(sql)?;
        if partition || expires_unix.is_some_and(|expiry| expiry <= now_unix) {
            batch.skipped += 1;
            continue;
        }
        let path: String = row.get(1).map_err(sql)?;
        let name: String = row.get(2).map_err(sql)?;
        let encrypted = Zeroizing::new(row.get::<_, Vec<u8>>(4).map_err(sql)?);
        let value = if encrypted.is_empty() {
            Zeroizing::new(row.get::<_, String>(3).map_err(sql)?)
        } else {
            let Some(decryptor) = decryptor.as_mut() else {
                return Err(Error::UnsupportedSchema);
            };
            match decryptor.decrypt(&encrypted, &raw_host, version, keys) {
                Ok(value) => value,
                // One value written under an older key, or in a scheme this
                // importer can't read, must not cost the rest of the profile.
                Err(error @ (Error::Decryption | Error::UnsupportedEncryption)) => {
                    batch.undecryptable += 1;
                    unsupported += usize::from(error == Error::UnsupportedEncryption);
                    continue;
                }
                Err(error) => return Err(error),
            }
        };
        if !path.starts_with('/')
            || path.len() > 4096
            || name.len() > 4096
            || value.len() > 65536
            || name
                .bytes()
                .any(|b| b <= 32 || b >= 127 || b"()<>@,;:\\\"/[]?={}".contains(&b))
            || value.bytes().any(|b| b < 32 || b == 127 || b == b';')
        {
            batch.skipped += 1;
            continue;
        }
        imported_bytes = imported_bytes.saturating_add(value.len() + name.len() + path.len());
        if imported_bytes > 16 * 1024 * 1024 || batch.cookies.len() >= 20_000 {
            return Err(Error::TooManySelected);
        }
        let same_site = same_site_scope(site, secure);
        batch.cookies.push(Cookie {
            host,
            domain: raw_host.starts_with('.'),
            path,
            name,
            value,
            secure,
            http_only: row.get(6).map_err(sql)?,
            same_site,
            expires_unix,
        });
    }
    // If nothing selected could be decrypted, the key is wrong for the whole
    // profile: say so instead of reporting an empty import.
    if batch.cookies.is_empty() && batch.undecryptable > 0 {
        return Err(if unsupported == batch.undecryptable {
            Error::UnsupportedEncryption
        } else {
            Error::Decryption
        });
    }
    Ok(batch)
}

/// The engine's `SameSite` for a source record, never broader than the
/// source browser's. The engine enforces `Lax` and `Strict`, but it treats a
/// cookie with no attribute as unrestricted, while Chromium treats one as `Lax`
/// (`-1`, unspecified) and Firefox repairs `None` without `Secure` to unset on
/// load (and stores unset as `256`). Those, and values this importer doesn't
/// know, import as `Lax`. Only `None` with `Secure` imports as `None`; the
/// engine rejects `None` without it.
fn same_site_scope(source: i64, secure: bool) -> SameSite {
    match source {
        0 if secure => SameSite::None,
        2 => SameSite::Strict,
        _ => SameSite::Lax,
    }
}

fn valid_host(raw: &str) -> Option<String> {
    let host = raw.trim_start_matches('.');
    if host.is_empty() || host.len() > 253 || host.contains(['/', '\\', ':', '@', '?', '#']) {
        return None;
    }
    url::Host::parse(host).ok().map(|host| host.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Browser, Install, Keyring};
    struct NoKeys;
    impl Keys for NoKeys {
        fn passwords(&mut self, _: Keyring) -> Result<Vec<Zeroizing<Vec<u8>>>, Error> {
            panic!("Firefox needs no keyring")
        }
    }
    #[test]
    fn firefox_wal_selection_and_cookie_scope_are_preserved_without_source_writes() {
        let directory = tempfile::tempdir().unwrap();
        let profile = Profile {
            browser: Browser::Firefox,
            install: Install::System,
            name: "Fixture".into(),
            database: directory.path().join("cookies.sqlite"),
        };
        let source = Connection::open(&profile.database).unwrap();
        source.execute_batch("PRAGMA user_version=15; PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE moz_cookies(host TEXT,path TEXT,name TEXT,value TEXT,isSecure INTEGER,isHttpOnly INTEGER,sameSite INTEGER,expiry INTEGER,originAttributes TEXT); INSERT INTO moz_cookies VALUES('.example.test','/scope','session','fixture',1,1,0,2000000000,''),('other.test','/','other','excluded',0,0,0,2000000000,''),('example.test','/','container','private',1,0,1,2000000000,'^userContextId=1'),('example.test','/','expired','expired',0,0,0,1,'');").unwrap();
        let before = std::fs::read(&profile.database).unwrap();
        let cancel = AtomicBool::new(false);
        assert_eq!(scan(&profile, &cancel).unwrap().len(), 2);
        let batch = extract(
            &profile,
            &BTreeSet::from(["example.test".into()]),
            &mut NoKeys,
            &cancel,
            1700000000,
        )
        .unwrap();
        assert_eq!(batch.cookies.len(), 1);
        assert_eq!(batch.skipped, 2);
        let cookie = &batch.cookies[0];
        assert!(cookie.domain && cookie.secure && cookie.http_only);
        assert_eq!(cookie.path, "/scope");
        assert_eq!(cookie.same_site, SameSite::None);
        assert_eq!(cookie.value.as_str(), "fixture");
        assert_eq!(std::fs::read(&profile.database).unwrap(), before);
        assert_eq!(
            source
                .query_row("SELECT count(*) FROM moz_cookies", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            4
        );
    }

    #[test]
    fn firefox_schema_expiry_and_partition_flags_do_not_broaden_cookie_scope() {
        let directory = tempfile::tempdir().unwrap();
        let profile = Profile {
            browser: Browser::Firefox,
            install: Install::System,
            name: "Schema fixture".into(),
            database: directory.path().join("cookies.sqlite"),
        };
        let source = Connection::open(&profile.database).unwrap();
        source.execute_batch("CREATE TABLE moz_cookies(host TEXT,path TEXT,name TEXT,value TEXT,isSecure INTEGER,isHttpOnly INTEGER,sameSite INTEGER,expiry INTEGER,originAttributes TEXT,isPartitionedAttributeSet INTEGER);").unwrap();
        let selected = BTreeSet::from(["example.test".into()]);
        let cancel = AtomicBool::new(false);
        let now = 1_700_000_000;
        for version in [15, 16, 17] {
            source.pragma_update(None, "user_version", version).unwrap();
            source.execute("DELETE FROM moz_cookies", []).unwrap();
            let factor = if version >= 16 { 1000 } else { 1 };
            for (name, same_site, expiry, partition) in [
                ("allowed", 0, (now + 60) * factor, 0),
                ("expired", 0, (now - 1) * factor, 0),
                ("epoch", 0, 0, 0),
                ("partitioned", 0, (now + 60) * factor, 1),
                ("lax", 1, (now + 60) * factor, 0),
                ("strict", 2, (now + 60) * factor, 0),
                ("unspecified", -1, (now + 60) * factor, 0),
                ("firefox-unset", 256, (now + 60) * factor, 0),
            ] {
                source.execute("INSERT INTO moz_cookies VALUES('example.test','/',?1,'fixture',1,1,?2,?3,'',?4)", rusqlite::params![name, same_site, expiry, partition]).unwrap();
            }
            let batch = extract(&profile, &selected, &mut NoKeys, &cancel, now).unwrap();
            let imported: Vec<_> = batch
                .cookies
                .iter()
                .map(|cookie| (cookie.name.as_str(), cookie.same_site))
                .collect();
            // Restricted cookies keep their scope, and unknown or unset
            // values never import broader than Lax.
            assert_eq!(
                imported,
                [
                    ("allowed", SameSite::None),
                    ("lax", SameSite::Lax),
                    ("strict", SameSite::Strict),
                    ("unspecified", SameSite::Lax),
                    ("firefox-unset", SameSite::Lax),
                ],
                "schema {version}"
            );
            assert!(
                batch
                    .cookies
                    .iter()
                    .all(|cookie| cookie.expires_unix == Some(now + 60))
            );
            assert_eq!(batch.skipped, 3);
        }
        for unknown in [-1, 0, 18] {
            source.pragma_update(None, "user_version", unknown).unwrap();
            assert_eq!(
                scan(&profile, &cancel).err(),
                Some(Error::UnsupportedSchema)
            );
            assert_eq!(
                extract(&profile, &selected, &mut NoKeys, &cancel, now).err(),
                Some(Error::UnsupportedSchema)
            );
        }
    }

    #[test]
    fn insecure_none_cookies_import_as_lax() {
        let directory = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        let selected = BTreeSet::from(["example.test".into()]);
        for browser in [Browser::Chrome, Browser::Firefox] {
            let profile = Profile {
                browser,
                install: Install::System,
                name: "Security fixture".into(),
                database: directory.path().join(browser.label()),
            };
            let source = Connection::open(&profile.database).unwrap();
            if browser.chromium() {
                source.execute_batch("CREATE TABLE meta(key TEXT,value TEXT); INSERT INTO meta VALUES('version','24'); CREATE TABLE cookies(host_key TEXT,path TEXT,name TEXT,value TEXT,encrypted_value BLOB,is_secure INTEGER,is_httponly INTEGER,samesite INTEGER,expires_utc INTEGER); INSERT INTO cookies VALUES('example.test','/','insecure','fixture',X'',0,1,0,0),('example.test','/','secure-session','fixture',X'',1,1,0,0);").unwrap();
            } else {
                source.execute_batch("PRAGMA user_version=17; CREATE TABLE moz_cookies(host TEXT,path TEXT,name TEXT,value TEXT,isSecure INTEGER,isHttpOnly INTEGER,sameSite INTEGER,expiry INTEGER); INSERT INTO moz_cookies VALUES('example.test','/','insecure','fixture',0,1,0,2000000000000),('example.test','/','secure','fixture',1,1,0,2000000000000);").unwrap();
            }
            let batch = extract(&profile, &selected, &mut NoKeys, &cancel, 1_700_000_000).unwrap();
            // SameSite=None without Secure imports as Lax, which the engine
            // enforces, instead of being sent cross-site.
            let imported: Vec<_> = batch
                .cookies
                .iter()
                .map(|cookie| (cookie.secure, cookie.same_site))
                .collect();
            assert_eq!(
                imported,
                [(false, SameSite::Lax), (true, SameSite::None)],
                "{browser:?}"
            );
            assert_eq!(batch.skipped, 0);
            if browser.chromium() {
                assert!(
                    batch
                        .cookies
                        .iter()
                        .all(|cookie| cookie.expires_unix.is_none())
                );
            }
        }
    }

    #[test]
    fn executable_schemas_are_rejected_before_scan_or_extraction() {
        let directory = tempfile::tempdir().unwrap();
        let profile = Profile {
            browser: Browser::Firefox,
            install: Install::System,
            name: "Untrusted schema".into(),
            database: directory.path().join("cookies.sqlite"),
        };
        let source = Connection::open(&profile.database).unwrap();
        let cancel = AtomicBool::new(false);
        source.execute_batch("PRAGMA user_version=17;").unwrap();
        for schema in [
            "CREATE VIEW moz_cookies AS SELECT 'example.test' AS host;",
            "CREATE TABLE moz_cookies(host TEXT, value TEXT GENERATED ALWAYS AS (hex(zeroblob(1024))) VIRTUAL); INSERT INTO moz_cookies(host) VALUES('example.test');",
            "CREATE VIRTUAL TABLE moz_cookies USING fts5(host);",
        ] {
            source.execute_batch(schema).unwrap();
            assert_eq!(
                scan(&profile, &cancel).err(),
                Some(Error::UnsupportedSchema)
            );
            assert_eq!(
                extract(
                    &profile,
                    &BTreeSet::from(["example.test".into()]),
                    &mut NoKeys,
                    &cancel,
                    1_700_000_000
                )
                .err(),
                Some(Error::UnsupportedSchema)
            );
            source
                .execute_batch(if schema.starts_with("CREATE VIEW") {
                    "DROP VIEW moz_cookies"
                } else {
                    "DROP TABLE moz_cookies"
                })
                .unwrap();
        }
        let snapshot = snapshot(&profile, &cancel).unwrap();
        assert_eq!(
            snapshot
                .query_row("PRAGMA temp_store", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert!(
            !snapshot
                .db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA)
                .unwrap()
        );
    }

    #[test]
    fn schema_inspection_never_reads_unrelated_virtual_table_data() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        use std::sync::{Arc, atomic::AtomicUsize};

        let directory = tempfile::tempdir().unwrap();
        let profile = Profile {
            browser: Browser::Firefox,
            install: Install::System,
            name: "Metadata fixture".into(),
            database: directory.path().join("cookies.sqlite"),
        };
        let source = Connection::open(&profile.database).unwrap();
        source.execute_batch("CREATE TABLE moz_cookies(host TEXT); CREATE VIRTUAL TABLE unrelated USING fts5(content);").unwrap();
        let snapshot = snapshot(&profile, &AtomicBool::new(false)).unwrap();
        let reads = Arc::new(AtomicUsize::new(0));
        let observed = reads.clone();
        snapshot.authorizer(Some(move |context: AuthContext<'_>| {
            if let AuthAction::Read { table_name, .. } = context.action
                && table_name == "unrelated_config"
            {
                observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Authorization::Allow
        }));
        assert_eq!(
            table_columns(&snapshot, "moz_cookies").unwrap(),
            BTreeSet::from(["host".into()])
        );
        assert_eq!(reads.load(std::sync::atomic::Ordering::Relaxed), 0);
        // Prove the observer sees FTS initialization, so the zero-read check
        // above would fail if inspection used the table-list API again.
        snapshot
            .prepare("PRAGMA table_list")
            .unwrap()
            .query_map([], |_| Ok(()))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(reads.load(std::sync::atomic::Ordering::Relaxed) > 0);
    }

    /// A running Firefox keeps `cookies.sqlite` in WAL mode under an
    /// exclusive lock. Its committed cookies, including those only in the
    /// log, are still read; an open transaction's are not.
    #[test]
    fn a_running_browsers_locked_database_is_read_and_cancel_never_opens_it() {
        let directory = tempfile::tempdir().unwrap();
        let profile = Profile {
            browser: Browser::Firefox,
            install: Install::System,
            name: "Locked".into(),
            database: directory.path().join("cookies.sqlite"),
        };
        let source = Connection::open(&profile.database).unwrap();
        source.execute_batch("PRAGMA user_version=17; PRAGMA journal_mode=WAL; PRAGMA locking_mode=EXCLUSIVE; CREATE TABLE moz_cookies(host TEXT); INSERT INTO moz_cookies VALUES('committed.test'); BEGIN EXCLUSIVE; INSERT INTO moz_cookies VALUES('pending.test');").unwrap();
        assert_eq!(
            scan(&profile, &AtomicBool::new(false)).unwrap(),
            [Site {
                host: "committed.test".into(),
                cookies: 1
            }]
        );
        assert_eq!(
            scan(&profile, &AtomicBool::new(true)).err(),
            Some(Error::Cancelled)
        );
        source.execute_batch("COMMIT").unwrap();
        assert_eq!(scan(&profile, &AtomicBool::new(false)).unwrap().len(), 2);
    }

    #[test]
    fn chromium_ancestor_bit_does_not_partition_an_ordinary_cookie() {
        let directory = tempfile::tempdir().unwrap();
        let profile = Profile {
            browser: Browser::Chrome,
            install: Install::System,
            name: "Chromium partition fixture".into(),
            database: directory.path().join("Cookies"),
        };
        let source = Connection::open(&profile.database).unwrap();
        source.execute_batch("CREATE TABLE meta(key TEXT,value TEXT); INSERT INTO meta VALUES('version','24'); CREATE TABLE cookies(host_key TEXT,path TEXT,name TEXT,value TEXT,encrypted_value BLOB,is_secure INTEGER,is_httponly INTEGER,samesite INTEGER,expires_utc INTEGER,top_frame_site_key TEXT,has_cross_site_ancestor INTEGER); INSERT INTO cookies VALUES('example.test','/','ordinary','fixture',X'',1,1,0,0,'',1),('example.test','/','partitioned','fixture',X'',1,1,0,0,'https://other.test',0);").unwrap();
        let batch = extract(
            &profile,
            &BTreeSet::from(["example.test".into()]),
            &mut NoKeys,
            &AtomicBool::new(false),
            0,
        )
        .unwrap();
        assert_eq!(batch.cookies.len(), 1);
        assert_eq!(batch.cookies[0].name, "ordinary");
        assert_eq!(batch.skipped, 1);
    }

    #[test]
    fn selected_chromium_profiles_report_keyring_denial_without_exposing_values() {
        struct Denied;
        impl Keys for Denied {
            fn passwords(&mut self, _: Keyring) -> Result<Vec<Zeroizing<Vec<u8>>>, Error> {
                Err(Error::KeyringDenied)
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let profile = Profile {
            browser: Browser::Chrome,
            install: Install::System,
            name: "Fixture".into(),
            database: directory.path().join("Cookies"),
        };
        let source = Connection::open(&profile.database).unwrap();
        source.execute_batch("CREATE TABLE meta(key TEXT,value TEXT); INSERT INTO meta VALUES('version','24'); CREATE TABLE cookies(host_key TEXT,path TEXT,name TEXT,value TEXT,encrypted_value BLOB,is_secure INTEGER,is_httponly INTEGER,samesite INTEGER,expires_utc INTEGER,top_frame_site_key TEXT); INSERT INTO cookies VALUES('example.test','/','session','',X'763131010203',1,1,0,0,'');").unwrap();
        let cancel = AtomicBool::new(false);
        assert_eq!(scan(&profile, &cancel).unwrap().len(), 1);
        assert!(
            extract(&profile, &BTreeSet::new(), &mut Denied, &cancel, 0)
                .unwrap()
                .cookies
                .is_empty()
        );
        let error = extract(
            &profile,
            &BTreeSet::from(["example.test".into()]),
            &mut Denied,
            &cancel,
            0,
        )
        .err()
        .unwrap();
        assert_eq!(error, Error::KeyringDenied);
        assert!(!error.to_string().contains("session"));
    }

    /// Helium stores its key under Chromium's name. A value written under
    /// an older key is skipped and counted; the rest of the profile imports.
    #[test]
    fn chromium_forks_use_their_keyring_name_and_skip_stale_values() {
        struct Keyrings(Vec<Keyring>);
        impl Keys for Keyrings {
            fn passwords(&mut self, keyring: Keyring) -> Result<Vec<Zeroizing<Vec<u8>>>, Error> {
                self.0.push(keyring);
                Ok(vec![Zeroizing::new(b"fixture-password".to_vec())])
            }
        }
        let encrypt = crate::chromium::tests::encrypt;
        let directory = tempfile::tempdir().unwrap();
        let profile = Profile {
            browser: Browser::Helium,
            install: Install::System,
            name: "Primary".into(),
            database: directory.path().join("Cookies"),
        };
        let source = Connection::open(&profile.database).unwrap();
        source.execute_batch("CREATE TABLE meta(key TEXT,value TEXT); INSERT INTO meta VALUES('version','24'); CREATE TABLE cookies(host_key TEXT,path TEXT,name TEXT,value TEXT,encrypted_value BLOB,is_secure INTEGER,is_httponly INTEGER,samesite INTEGER,expires_utc INTEGER,top_frame_site_key TEXT);").unwrap();
        for (name, value) in [
            (
                "current",
                encrypt(b"v11", b"fixture-password", "example.test", 24),
            ),
            (
                "stale",
                encrypt(b"v11", b"replaced-password", "example.test", 24),
            ),
            ("portal", b"v12unsupported".to_vec()),
        ] {
            source
                .execute(
                    "INSERT INTO cookies VALUES('example.test','/',?1,'',?2,1,1,0,0,'')",
                    rusqlite::params![name, value],
                )
                .unwrap();
        }
        let selected = BTreeSet::from(["example.test".into()]);
        let cancel = AtomicBool::new(false);
        let mut keys = Keyrings(Vec::new());
        let batch = extract(&profile, &selected, &mut keys, &cancel, 0).unwrap();
        assert_eq!(keys.0, [Keyring::Chromium]);
        assert_eq!(batch.cookies.len(), 1);
        assert_eq!(batch.cookies[0].name, "current");
        assert_eq!(batch.cookies[0].value.as_str(), "fixture-cookie-value");
        assert_eq!(batch.undecryptable, 2);
        // When nothing decrypts, the profile reports why instead of an
        // empty import.
        source
            .execute("DELETE FROM cookies WHERE name = 'current'", [])
            .unwrap();
        assert_eq!(
            extract(&profile, &selected, &mut keys, &cancel, 0).err(),
            Some(Error::Decryption)
        );
        source
            .execute("DELETE FROM cookies WHERE name = 'stale'", [])
            .unwrap();
        assert_eq!(
            extract(&profile, &selected, &mut keys, &cancel, 0).err(),
            Some(Error::UnsupportedEncryption)
        );
    }
}
