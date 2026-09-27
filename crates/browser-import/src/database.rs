use crate::{Batch, Cookie, Error, Keys, Profile, SameSite, Site, cancelled};
use rusqlite::{
    Connection, OpenFlags,
    backup::{Backup, StepResult},
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
        _ => Error::Unreadable,
    }
}

fn snapshot(profile: &Profile, cancel: &AtomicBool) -> Result<Connection, Error> {
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

pub(super) fn scan(profile: &Profile, cancel: &AtomicBool) -> Result<Vec<Site>, Error> {
    let connection = snapshot(profile, cancel)?;
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
        return Err(Error::Unsupported);
    }
    Ok(version)
}

fn columns(connection: &Connection, table: &str) -> Result<BTreeSet<String>, Error> {
    // `table` is one of two static identifiers chosen below.
    connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(sql)?
        .query_map([], |row| row.get(1))
        .map_err(sql)?
        .collect::<Result<_, _>>()
        .map_err(sql)
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
        });
    }
    let connection = snapshot(profile, cancel)?;
    let chromium = profile.browser.chromium();
    let firefox_version = firefox_version(profile, &connection)?;
    let columns = columns(
        &connection,
        if chromium { "cookies" } else { "moz_cookies" },
    )?;
    let version: i64 = if chromium {
        connection
            .query_row("SELECT value FROM meta WHERE key = 'version'", [], |row| {
                row.get::<_, String>(0)
            })
            .map_err(sql)?
            .parse()
            .map_err(|_| Error::Unsupported)?
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
    };
    let mut cached = None;
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
        let expires_unix = if expiry == 0 {
            None
        } else if chromium {
            Some(expiry / 1_000_000 - 11_644_473_600)
        } else if firefox_version >= 16 {
            Some(expiry / 1000)
        } else {
            Some(expiry)
        };
        // Pinned Servo does not enforce SameSite at request dispatch. Import
        // only explicitly unrestricted cookies; retaining Lax/Strict/default
        // attributes alone would silently broaden their credential scope.
        if partition || site != 0 || expires_unix.is_some_and(|expiry| expiry <= now_unix) {
            batch.skipped += 1;
            continue;
        }
        let path: String = row.get(1).map_err(sql)?;
        let name: String = row.get(2).map_err(sql)?;
        let encrypted = Zeroizing::new(row.get::<_, Vec<u8>>(4).map_err(sql)?);
        let value = if encrypted.is_empty() {
            Zeroizing::new(row.get::<_, String>(3).map_err(sql)?)
        } else {
            crate::chromium::decrypt(
                &encrypted,
                &raw_host,
                version,
                profile.browser,
                keys,
                &mut cached,
            )?
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
            return Err(Error::TooLarge);
        }
        let same_site = match site {
            0 => SameSite::None,
            1 => SameSite::Lax,
            2 => SameSite::Strict,
            _ => SameSite::Unspecified,
        };
        batch.cookies.push(Cookie {
            host,
            domain: raw_host.starts_with('.'),
            path,
            name,
            value,
            secure: row.get(5).map_err(sql)?,
            http_only: row.get(6).map_err(sql)?,
            same_site,
            expires_unix,
        });
    }
    Ok(batch)
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
    use crate::Browser;
    struct NoKeys;
    impl Keys for NoKeys {
        fn password(&mut self, _: Browser) -> Result<Zeroizing<Vec<u8>>, Error> {
            panic!("Firefox needs no keyring")
        }
    }
    #[test]
    fn firefox_wal_selection_and_cookie_scope_are_preserved_without_source_writes() {
        let directory = tempfile::tempdir().unwrap();
        let profile = Profile {
            browser: Browser::Firefox,
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
                ("partitioned", 0, (now + 60) * factor, 1),
                ("lax", 1, (now + 60) * factor, 0),
                ("strict", 2, (now + 60) * factor, 0),
                ("unspecified", -1, (now + 60) * factor, 0),
            ] {
                source.execute("INSERT INTO moz_cookies VALUES('example.test','/',?1,'fixture',1,1,?2,?3,'',?4)", rusqlite::params![name, same_site, expiry, partition]).unwrap();
            }
            let batch = extract(&profile, &selected, &mut NoKeys, &cancel, now).unwrap();
            assert_eq!(batch.cookies.len(), 1, "schema {version}");
            assert_eq!(batch.cookies[0].name, "allowed");
            assert_eq!(batch.cookies[0].expires_unix, Some(now + 60));
            assert_eq!(batch.skipped, 5);
        }
        for unknown in [-1, 0, 18] {
            source.pragma_update(None, "user_version", unknown).unwrap();
            assert_eq!(scan(&profile, &cancel).err(), Some(Error::Unsupported));
            assert_eq!(
                extract(&profile, &selected, &mut NoKeys, &cancel, now).err(),
                Some(Error::Unsupported)
            );
        }
    }

    #[test]
    fn a_locked_database_is_a_retryable_error_and_cancel_never_opens_it() {
        let directory = tempfile::tempdir().unwrap();
        let profile = Profile {
            browser: Browser::Firefox,
            name: "Locked".into(),
            database: directory.path().join("cookies.sqlite"),
        };
        let source = Connection::open(&profile.database).unwrap();
        source.execute_batch("PRAGMA user_version=15; CREATE TABLE moz_cookies(host TEXT); BEGIN EXCLUSIVE; INSERT INTO moz_cookies VALUES('example.test');").unwrap();
        assert_eq!(
            scan(&profile, &AtomicBool::new(false)).err(),
            Some(Error::Busy)
        );
        assert_eq!(
            scan(&profile, &AtomicBool::new(true)).err(),
            Some(Error::Cancelled)
        );
        source.execute_batch("ROLLBACK").unwrap();
        assert!(scan(&profile, &AtomicBool::new(false)).unwrap().is_empty());
    }

    #[test]
    fn chromium_ancestor_bit_does_not_partition_an_ordinary_cookie() {
        let directory = tempfile::tempdir().unwrap();
        let profile = Profile {
            browser: Browser::Chrome,
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
            fn password(&mut self, _: Browser) -> Result<Zeroizing<Vec<u8>>, Error> {
                Err(Error::KeyringDenied)
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let profile = Profile {
            browser: Browser::Chrome,
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
}
