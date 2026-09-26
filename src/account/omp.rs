//! Omp's accounts, from the usage record it keeps.
//!
//! Omp signs in to several services, and to several accounts of one, and
//! picks between them itself. Its CLI reports no limits the way `claude` and
//! `codex` do, but it records every limit it observes - per service, account
//! and limit - in the `usage_history` table of its agent database, updating a
//! series' latest row in place while the figure holds. cagents reads that
//! table and nothing else there: the same database keeps Omp's logins, which
//! belong to Omp alone.
//!
//! The rows become one [`Brokered`] account per service login, with the
//! latest figure of each of its limits. A record written by another version
//! of Omp may have columns cagents does not know, or lack ones it would like:
//! only the columns that identify a figure are required, and a record without
//! them reads as unavailable rather than as an error.
//!
//! Turning rows into accounts is a pure function, shared by every lane and
//! tested here. Reading the database is the interface's job, and is done
//! without writing to it, locking it against Omp, or leaving a file beside it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::{
    Account, Identity, UsageSnapshot, UsageSource, UsageStatus, UsageWindow, WindowKind, fraction,
    humanize, status_for,
};
use crate::agent::Provider;
use crate::model::LaunchRecipe;

const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;
const WEEK: u64 = 7 * DAY;

/// How long after Omp last recorded anything for an account it is still
/// listed. Longer than any window Omp reports, so a figure is shown for as
/// long as it can still hold; past that, an account Omp has stopped using,
/// or signed out of, leaves the list.
const HORIZON_MS: u64 = 8 * DAY * 1_000;
/// How long before an account's newest figure another of its figures, one
/// that names no reset, may have been recorded and still be current. A limit
/// Omp stopped reporting keeps its last row forever, and is not shown as
/// though it held.
const READING_SPREAD_MS: u64 = 10 * MINUTE * 1_000;
/// Words in Omp's limit ids that name how an account-wide allowance is
/// metered rather than a model or product it is set aside for.
const ACCOUNT_WIDE: [&str; 3] = ["primary", "secondary", "credits"];

/// One row of Omp's usage record, as far as cagents reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// When Omp last observed the figure, in Unix milliseconds - or seconds,
    /// which a record from another version might hold instead.
    pub recorded_at: i64,
    /// The service the login is for, in Omp's words: `anthropic`,
    /// `openai-codex`.
    pub service: String,
    /// The key Omp files the login under.
    pub account_key: String,
    pub email: Option<String>,
    pub limit_id: String,
    pub label: Option<String>,
    /// The window's length in words: `5 Hours`, `7 days`, `Weekly`.
    pub window_label: Option<String>,
    pub used_fraction: Option<f64>,
    /// `ok`, `warning` or `exhausted`.
    pub status: Option<String>,
    /// When the window resets, in Unix milliseconds or seconds.
    pub resets_at: Option<i64>,
}

/// One of the logins Omp keeps, with what it last recorded about it.
#[derive(Debug, Clone, PartialEq)]
pub struct Brokered {
    pub account: Account,
    pub identity: Identity,
    pub snapshot: UsageSnapshot,
}

/// The accounts `rows` describe as of `now`, in Unix milliseconds: the
/// latest figure of each of an account's limits, the account Omp recorded
/// most recently first.
pub fn accounts(rows: impl IntoIterator<Item = Row>, now: u64) -> Vec<Brokered> {
    // The latest row of each series. The query already asks for that; a
    // record that repeats a series is still read right.
    let mut series: BTreeMap<(String, String, String), (u64, Row)> = BTreeMap::new();
    for row in rows {
        let Some(recorded_at) = millis(row.recorded_at) else {
            continue;
        };
        if row.service.is_empty() || row.account_key.is_empty() || row.limit_id.is_empty() {
            continue;
        }
        let key = (
            row.service.clone(),
            row.account_key.clone(),
            row.limit_id.clone(),
        );
        match series.get(&key) {
            Some((latest, _)) if *latest >= recorded_at => {}
            _ => {
                series.insert(key, (recorded_at, row));
            }
        }
    }
    let mut logins: BTreeMap<(String, String), Vec<(u64, Row)>> = BTreeMap::new();
    for ((service, account_key, _), latest) in series {
        logins
            .entry((service, account_key))
            .or_default()
            .push(latest);
    }
    let mut accounts: Vec<Brokered> = logins
        .into_iter()
        .filter_map(|((service, key), rows)| {
            let newest = rows.iter().map(|(recorded_at, _)| *recorded_at).max()?;
            if newest.saturating_add(HORIZON_MS) < now {
                return None;
            }
            // Omp records an account's limits as it hears of them, often a
            // few at a time, so a figure holds until the window it measures
            // resets however long ago it was recorded, and one past its reset
            // no longer says anything.
            let reading: Vec<&Row> = rows
                .iter()
                .filter(|(recorded_at, row)| match row.resets_at.and_then(millis) {
                    Some(reset) => reset > now,
                    None => recorded_at.saturating_add(READING_SPREAD_MS) >= newest,
                })
                .map(|(_, row)| row)
                .collect();
            let email = rows
                .iter()
                .filter(|(_, row)| {
                    row.email
                        .as_deref()
                        .is_some_and(|email| !email.trim().is_empty())
                })
                .max_by_key(|(recorded_at, _)| *recorded_at)
                .and_then(|(_, row)| row.email.as_deref())
                .map(|email| email.trim().to_owned());
            Some(Brokered {
                account: Account::brokered(Provider::Omp, service, key),
                identity: Identity {
                    email,
                    organization: None,
                    plan: None,
                },
                snapshot: snapshot(&reading, newest),
            })
        })
        .collect();
    accounts.sort_by(|left, right| {
        right
            .snapshot
            .observed_at
            .cmp(&left.snapshot.observed_at)
            .then_with(|| left.account.id().cmp(&right.account.id()))
    });
    accounts
}

/// What one reading of an account's limits says about it.
fn snapshot(reading: &[&Row], observed_at: u64) -> UsageSnapshot {
    let mut windows: Vec<UsageWindow> = Vec::new();
    let mut limited = false;
    let mut warning = false;
    for row in reading {
        let Some(window) = window(row) else {
            continue;
        };
        if window.kind != WindowKind::Scoped {
            match row.status.as_deref() {
                Some("exhausted") => limited = true,
                Some("warning") => warning = true,
                _ => {}
            }
        }
        windows.push(window);
    }
    if windows.is_empty() {
        let mut snapshot = UsageSnapshot::state(
            UsageStatus::Unavailable,
            "Omp has recorded no figures for this account",
            observed_at,
        );
        snapshot.source = UsageSource::Recorded;
        return snapshot;
    }
    windows.sort_by(|left, right| {
        kind_order(left.kind)
            .cmp(&kind_order(right.kind))
            .then_with(|| left.span_secs.cmp(&right.span_secs))
            .then_with(|| left.label.cmp(&right.label))
    });
    UsageSnapshot {
        status: status_for(&windows, limited, warning),
        windows,
        credits: None,
        observed_at,
        source: UsageSource::Recorded,
        plan: None,
        detail: None,
    }
}

fn kind_order(kind: WindowKind) -> u8 {
    match kind {
        WindowKind::Session => 0,
        WindowKind::Weekly => 1,
        WindowKind::Scoped => 2,
        WindowKind::Other => 3,
    }
}

/// The window a row reports, if it carries a figure.
fn window(row: &Row) -> Option<UsageWindow> {
    let used = row.used_fraction.filter(|used| used.is_finite())?;
    let span_secs = row
        .window_label
        .as_deref()
        .and_then(span_of_words)
        .or_else(|| row.limit_id.split(':').find_map(span_of_token));
    let kind = if scoped(row) {
        WindowKind::Scoped
    } else {
        match span_secs {
            Some(span) if span <= DAY => WindowKind::Session,
            Some(_) => WindowKind::Weekly,
            None => WindowKind::Other,
        }
    };
    let label = [row.label.as_deref(), row.window_label.as_deref()]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|label| !label.is_empty())
        .map_or_else(|| humanize(&row.limit_id), str::to_owned);
    Some(UsageWindow {
        key: row.limit_id.clone(),
        kind,
        label,
        used: fraction(used),
        resets_at: row
            .resets_at
            .and_then(millis)
            .and_then(|reset| i64::try_from(reset / 1_000).ok()),
        span_secs,
        binding: false,
    })
}

/// Whether a row's limit is set aside for one model or product rather than
/// shared by everything the account does: its id names something beside
/// how the window is metered - `anthropic:7d:fable`,
/// `openai-codex:spark:primary` - or its label qualifies the window with
/// something other than its length, as in `7 days (Spark)`. An id that is
/// one unknown word says nothing either way.
fn scoped(row: &Row) -> bool {
    let segments: Vec<&str> = row
        .limit_id
        .split(':')
        .enumerate()
        .filter(|(index, segment)| !segment.is_empty() && !(*index == 0 && *segment == row.service))
        .map(|(_, segment)| segment)
        .collect();
    let metering =
        |segment: &&str| ACCOUNT_WIDE.contains(segment) || span_of_token(segment).is_some();
    let named = segments.iter().any(metering) && !segments.iter().all(metering);
    let qualified = row.label.as_deref().is_some_and(|label| {
        label
            .split_once('(')
            .and_then(|(_, rest)| rest.split_once(')'))
            .is_some_and(|(inner, _)| span_of_words(inner).is_none())
    });
    named || qualified
}

/// A window's length written in words: `5 Hours`, `7 days`, `Weekly`.
fn span_of_words(words: &str) -> Option<u64> {
    let words = words.trim().to_ascii_lowercase();
    match words.as_str() {
        "hourly" => return Some(HOUR),
        "daily" => return Some(DAY),
        "weekly" => return Some(WEEK),
        _ => {}
    }
    let mut parts = words.split_whitespace();
    let count: u64 = parts.next()?.parse().ok()?;
    let unit = unit(parts.next()?)?;
    count.checked_mul(unit).filter(|span| *span > 0)
}

/// A window's length written as one of Omp's id tokens: `5h`, `7d`, `1w`,
/// `300m`.
fn span_of_token(token: &str) -> Option<u64> {
    let split = token.find(|character: char| !character.is_ascii_digit())?;
    let (count, unit_name) = token.split_at(split);
    let count: u64 = count.parse().ok()?;
    let unit = match unit_name {
        "m" => MINUTE,
        "h" => HOUR,
        "d" => DAY,
        "w" => WEEK,
        _ => return None,
    };
    count.checked_mul(unit).filter(|span| *span > 0)
}

fn unit(word: &str) -> Option<u64> {
    Some(match word.trim_end_matches('s') {
        "minute" | "min" => MINUTE,
        "hour" | "hr" => HOUR,
        "day" => DAY,
        "week" | "wk" => WEEK,
        _ => return None,
    })
}

/// Unix milliseconds for a stamp in milliseconds or seconds. A stamp in
/// seconds would stay below this for another three thousand years; one in
/// milliseconds passed it in 1973.
fn millis(stamp: i64) -> Option<u64> {
    const SECONDS_BELOW: u64 = 100_000_000_000;
    let stamp = u64::try_from(stamp).ok().filter(|stamp| *stamp > 0)?;
    Some(if stamp < SECONDS_BELOW {
        stamp.saturating_mul(1_000)
    } else {
        stamp
    })
}

/// A service, in Omp's words, as a person would name it.
pub fn service_label(service: &str) -> String {
    let service = service.strip_suffix("-oauth").unwrap_or(service);
    match service {
        "anthropic" => "Anthropic".into(),
        "openai" => "OpenAI".into(),
        "openai-codex" => "OpenAI Codex".into(),
        "xai" => "xAI".into(),
        "github-copilot" => "GitHub Copilot".into(),
        "google" | "google-gemini-cli" | "gemini" => "Google".into(),
        other => humanize(other),
    }
}

/// Where Omp keeps its agent database, resolved against `environment` the
/// way Omp resolves it, as the same rules find its sessions. A relative
/// `PI_CODING_AGENT_DIR` is taken from the home directory, since Omp's own
/// working directory is not known here.
pub fn database(environment: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    const READ: [&str; 5] = [
        "OMP_PROFILE",
        "PI_PROFILE",
        "PI_CONFIG_DIR",
        "PI_CODING_AGENT_DIR",
        "XDG_DATA_HOME",
    ];
    let home = PathBuf::from(environment("HOME").filter(|home| !home.is_empty())?);
    let launch = LaunchRecipe {
        unset_environment: Default::default(),
        program: "omp".into(),
        arguments: Vec::new(),
        working_directory: home.clone(),
        environment: READ
            .into_iter()
            .filter_map(|name| Some((name.to_owned(), environment(name)?)))
            .collect(),
    };
    let root = crate::agent::omp_data_root(&launch, &home)?;
    root.is_absolute().then(|| root.join("agent.db"))
}

#[cfg(feature = "ui")]
pub use reader::{History, read};

#[cfg(feature = "ui")]
mod reader {
    use std::path::Path;
    use std::time::Duration;

    use rusqlite::{Connection, OpenFlags, types::ValueRef};

    use super::{Brokered, Row, accounts};

    /// The table cagents reads, and the only one.
    const TABLE: &str = "usage_history";
    /// The columns a figure cannot be placed without.
    const REQUIRED: [&str; 5] = [
        "recorded_at",
        "provider",
        "account_key",
        "limit_id",
        "used_fraction",
    ];
    /// Columns read where the record has them.
    const OPTIONAL: [&str; 5] = ["email", "label", "window_label", "status", "resets_at"];
    /// The most series read: far more than a person has logins and limits,
    /// and a bound on what a damaged record can cost.
    const MAX_SERIES: usize = 10_000;
    /// How long a read waits for Omp to finish a write.
    const BUSY_TIMEOUT: Duration = Duration::from_millis(500);

    /// What reading Omp's usage record found.
    #[derive(Debug, Clone, PartialEq)]
    pub enum History {
        /// There is no record: Omp is not set up, or does not keep one.
        Missing,
        /// There is a record, but it could not be read, or is in a shape
        /// cagents does not know.
        Unreadable(String),
        Read(Vec<Brokered>),
    }

    /// Reads the record in the database at `path` as of `now`, in Unix
    /// milliseconds. Blocking.
    ///
    /// The database is opened read-only and queried only for the
    /// `usage_history` table's shape and rows. While Omp has it open - its
    /// write-ahead log and index beside it - it is read through them like
    /// any reader, which never holds up Omp's writes. Otherwise it is read
    /// as it stands on disk, since opening it the usual way would leave an
    /// empty log and index beside it.
    pub fn read(path: &Path, now: u64) -> History {
        if !path.is_file() {
            return History::Missing;
        }
        match query(path) {
            Ok(Some(rows)) => History::Read(accounts(rows, now)),
            Ok(None) => History::Missing,
            Err(error) => {
                tracing::debug!(%error, "Omp's usage record could not be read");
                History::Unreadable(error)
            }
        }
    }

    /// The latest row of every series, or `None` if the database keeps no
    /// usage record.
    fn query(path: &Path) -> Result<Option<Vec<Row>>, String> {
        let connection = open(path).map_err(|error| describe(&error))?;
        let columns: Vec<String> = connection
            .prepare("SELECT name FROM pragma_table_info(?1)")
            .and_then(|mut statement| {
                statement
                    .query_map([TABLE], |row| row.get::<_, String>(0))?
                    .collect()
            })
            .map_err(|error| describe(&error))?;
        if columns.is_empty() {
            return Ok(None);
        }
        let has = |name: &str| columns.iter().any(|column| column == name);
        if !REQUIRED.iter().all(|name| has(name)) {
            return Err("Omp's usage record is in a form cagents does not know".into());
        }
        // A bare column beside `MAX` is taken from the row holding the
        // maximum, so each series comes back as its latest row.
        let optional: Vec<String> = OPTIONAL
            .iter()
            .map(|name| {
                if has(name) {
                    format!("\"{name}\"")
                } else {
                    "NULL".to_owned()
                }
            })
            .collect();
        let sql = format!(
            "SELECT MAX(recorded_at), provider, account_key, limit_id, used_fraction, {} \
             FROM {TABLE} GROUP BY provider, account_key, limit_id LIMIT {MAX_SERIES}",
            optional.join(", ")
        );
        let mut statement = connection.prepare(&sql).map_err(|error| describe(&error))?;
        let rows = statement
            .query_map([], |row| {
                let value = |index: usize| row.get_ref(index);
                Ok(Row {
                    recorded_at: integer(value(0)?).unwrap_or(0),
                    service: text(value(1)?).unwrap_or_default(),
                    account_key: text(value(2)?).unwrap_or_default(),
                    limit_id: text(value(3)?).unwrap_or_default(),
                    used_fraction: real(value(4)?),
                    email: text(value(5)?),
                    label: text(value(6)?),
                    window_label: text(value(7)?),
                    status: text(value(8)?),
                    resets_at: integer(value(9)?),
                })
            })
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
            .map_err(|error| describe(&error))?;
        Ok(Some(rows))
    }

    fn open(path: &Path) -> rusqlite::Result<Connection> {
        let side = |suffix: &str| {
            let mut name = path.as_os_str().to_owned();
            name.push(suffix);
            Path::new(&name).exists()
        };
        // A log without its index is one a crash or a hand left behind.
        // Reading through it would take an index a read-only connection may
        // not create, so the database is read as it stands, short of what
        // only that log holds, which the next read after Omp's own picks up.
        let live = side("-wal") && side("-shm");
        let mut uri = String::from("file:");
        uri.push_str(&encode(path));
        // Its index is only read, never made or marked: SQLite keeps what
        // it needs of it in memory, so a read that races Omp closing the
        // database leaves no index behind it.
        uri.push_str(if live {
            "?mode=ro&readonly_shm=1"
        } else {
            "?mode=ro&immutable=1"
        });
        let connection = Connection::open_with_flags(
            uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        connection.pragma_update(None, "query_only", true)?;
        Ok(connection)
    }

    /// `path` as the path of a `file:` URI, which reserves `?`, `#` and `%`.
    fn encode(path: &Path) -> String {
        use std::os::unix::ffi::OsStrExt;
        let mut encoded = String::new();
        for byte in path.as_os_str().as_bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                    encoded.push(char::from(*byte))
                }
                other => encoded.push_str(&format!("%{other:02X}")),
            }
        }
        encoded
    }

    /// cagents' own summary of a failure, without the database's words.
    fn describe(error: &rusqlite::Error) -> String {
        match error.sqlite_error_code() {
            Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => {
                "Omp was writing its usage record".into()
            }
            Some(rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt) => {
                "Omp's usage record is damaged".into()
            }
            _ => "Omp's usage record could not be read".into(),
        }
    }

    fn integer(value: ValueRef<'_>) -> Option<i64> {
        match value {
            ValueRef::Integer(value) => Some(value),
            ValueRef::Real(value) if value.is_finite() => Some(value as i64),
            ValueRef::Text(text) => std::str::from_utf8(text).ok()?.trim().parse().ok(),
            _ => None,
        }
    }

    fn real(value: ValueRef<'_>) -> Option<f64> {
        match value {
            ValueRef::Integer(value) => Some(value as f64),
            ValueRef::Real(value) => Some(value),
            ValueRef::Text(text) => std::str::from_utf8(text).ok()?.trim().parse().ok(),
            _ => None,
        }
    }

    fn text(value: ValueRef<'_>) -> Option<String> {
        match value {
            ValueRef::Text(text) => Some(String::from_utf8_lossy(text).into_owned()),
            ValueRef::Integer(value) => Some(value.to_string()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_790_130_000_000;
    const CLAUDE: &str = "oauth|account:1|email:a@example.test";
    const CODEX: &str = "oauth|account:2|email:b@example.test";

    fn row(service: &str, key: &str, limit: &str, recorded_at: u64, used: f64) -> Row {
        Row {
            recorded_at: recorded_at as i64,
            service: service.into(),
            account_key: key.into(),
            email: Some(format!("{}@example.test", &service[..1])),
            limit_id: limit.into(),
            label: None,
            window_label: None,
            used_fraction: Some(used),
            status: Some("ok".into()),
            resets_at: Some((NOW + 3_600_000) as i64),
        }
    }

    fn labelled(mut row: Row, label: &str, window: &str) -> Row {
        row.label = Some(label.into());
        row.window_label = Some(window.into());
        row
    }

    #[test]
    fn each_login_gets_the_latest_figure_of_each_limit() {
        let rows = vec![
            labelled(
                row("anthropic", CLAUDE, "anthropic:5h", NOW - 60_000, 0.31),
                "Claude 5 Hour",
                "5 Hour",
            ),
            labelled(
                row("anthropic", CLAUDE, "anthropic:5h", NOW - 3_600_000, 0.9),
                "Claude 5 Hour",
                "5 Hour",
            ),
            labelled(
                row("anthropic", CLAUDE, "anthropic:7d", NOW - 60_000, 0.26),
                "Claude 7 Day",
                "7 Day",
            ),
            labelled(
                row("anthropic", CLAUDE, "anthropic:7d:fable", NOW - 60_000, 1.0),
                "Claude 7 Day (Fable)",
                "7 Day",
            ),
            labelled(
                row(
                    "openai-codex",
                    CODEX,
                    "openai-codex:primary",
                    NOW - 120_000,
                    0.05,
                ),
                "7 days",
                "7 days",
            ),
        ];
        let accounts = accounts(rows, NOW);
        assert_eq!(accounts.len(), 2);
        let claude = &accounts[0];
        assert_eq!(claude.account.provider, Provider::Omp);
        assert_eq!(claude.account.brokered_service(), Some("anthropic"));
        assert_eq!(claude.identity.email.as_deref(), Some("a@example.test"));
        assert_eq!(claude.snapshot.observed_at, NOW - 60_000);
        assert_eq!(claude.snapshot.source, UsageSource::Recorded);
        let windows: Vec<(&str, WindowKind, f32)> = claude
            .snapshot
            .windows
            .iter()
            .map(|window| (window.label.as_str(), window.kind, window.used))
            .collect();
        assert_eq!(
            windows,
            vec![
                ("Claude 5 Hour", WindowKind::Session, 0.31),
                ("Claude 7 Day", WindowKind::Weekly, 0.26),
                ("Claude 7 Day (Fable)", WindowKind::Scoped, 1.0),
            ]
        );
        assert_eq!(claude.snapshot.windows[0].span_secs, Some(5 * HOUR));
        assert_eq!(
            claude.snapshot.windows[0].resets_at,
            Some(((NOW + 3_600_000) / 1_000) as i64)
        );
        // One model's allowance running out leaves the others working: it
        // is worth a warning, not a limit.
        assert_eq!(claude.snapshot.status, UsageStatus::Warning);
        let codex = &accounts[1];
        assert_eq!(codex.snapshot.windows[0].kind, WindowKind::Weekly);
        assert_ne!(claude.account.id(), codex.account.id());
        assert!(
            !claude.account.id().as_str().contains("example"),
            "an id does not spell out the login"
        );
    }

    #[test]
    fn a_limit_is_placed_by_its_id_where_its_words_do_not_say() {
        let place = |limit: &str, label: Option<&str>, window: Option<&str>| {
            let mut row = row("xai-oauth", CLAUDE, limit, NOW, 0.1);
            row.label = label.map(str::to_owned);
            row.window_label = window.map(str::to_owned);
            let window = super::window(&row).unwrap();
            (window.kind, window.span_secs)
        };
        assert_eq!(
            place(
                "xai-oauth:credits:1w",
                Some("SuperGrok Weekly Credits"),
                Some("Weekly")
            ),
            (WindowKind::Weekly, Some(WEEK))
        );
        assert_eq!(
            place(
                "xai-oauth:product:grokbuild:1w",
                Some("Grok Build (Weekly)"),
                Some("Weekly")
            ),
            (WindowKind::Scoped, Some(WEEK))
        );
        assert_eq!(
            place("300m", None, None),
            (WindowKind::Session, Some(5 * HOUR))
        );
        assert_eq!(
            place("1w", Some("Weekly"), None),
            (WindowKind::Weekly, Some(WEEK))
        );
        assert_eq!(
            place(
                "openai-codex:spark:primary",
                Some("5 hours (Spark)"),
                Some("5 hours")
            ),
            (WindowKind::Scoped, Some(5 * HOUR))
        );
        assert_eq!(
            place("quota", Some("Quota"), None),
            (WindowKind::Other, None)
        );
        assert_eq!(
            place("primary", Some("Quota"), None),
            (WindowKind::Other, None)
        );
    }

    #[test]
    fn a_login_is_as_limited_as_omp_recorded_it() {
        let mut exhausted = row("openai-codex", CODEX, "openai-codex:primary", NOW, 0.99);
        exhausted.status = Some("exhausted".into());
        let accounts = accounts(vec![exhausted], NOW);
        assert_eq!(accounts[0].snapshot.status, UsageStatus::Limited);

        let mut warning = row("anthropic", CLAUDE, "anthropic:7d", NOW, 0.5);
        warning.status = Some("warning".into());
        let accounts = super::accounts(vec![warning], NOW);
        assert_eq!(accounts[0].snapshot.status, UsageStatus::Warning);

        let mut blank = row("anthropic", CLAUDE, "anthropic:7d", NOW, 0.5);
        blank.used_fraction = None;
        let accounts = super::accounts(vec![blank], NOW);
        assert_eq!(accounts[0].snapshot.status, UsageStatus::Unavailable);
        assert_eq!(accounts[0].snapshot.source, UsageSource::Recorded);
    }

    #[test]
    fn limits_and_logins_omp_stopped_recording_are_left_out() {
        let unreset = |limit: &str, recorded_at: u64| {
            let mut row = row("openai-codex", CODEX, limit, recorded_at, 0.0);
            row.resets_at = None;
            row
        };
        let mut reset = row(
            "openai-codex",
            CODEX,
            "openai-codex:spark:primary",
            NOW - DAY * 1_000,
            1.0,
        );
        reset.resets_at = Some((NOW - 1) as i64);
        let accounts = accounts(
            vec![
                row("openai-codex", CODEX, "openai-codex:primary", NOW, 0.1),
                // Recorded an hour before the newest figure, and its window
                // has not reset since: it still holds.
                row(
                    "openai-codex",
                    CODEX,
                    "openai-codex:secondary",
                    NOW - HOUR * 1_000,
                    0.4,
                ),
                // Its window has reset since, so the figure says nothing.
                reset,
                // Naming no reset, it holds only beside the newest figure.
                unreset("openai-codex:tertiary", NOW - READING_SPREAD_MS),
                unreset("openai-codex:quaternary", NOW - READING_SPREAD_MS - 1),
                // A login Omp has not used in longer than any window lasts.
                row(
                    "anthropic",
                    CLAUDE,
                    "anthropic:5h",
                    NOW - HORIZON_MS - 1,
                    0.2,
                ),
            ],
            NOW,
        );
        assert_eq!(accounts.len(), 1);
        let mut keys: Vec<&str> = accounts[0]
            .snapshot
            .windows
            .iter()
            .map(|window| window.key.as_str())
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "openai-codex:primary",
                "openai-codex:secondary",
                "openai-codex:tertiary"
            ]
        );
        // An exhausted window that has reset since no longer limits the login.
        assert_eq!(accounts[0].snapshot.status, UsageStatus::Ok);
    }

    #[test]
    fn stamps_in_seconds_read_as_well_as_milliseconds() {
        let mut row = row("anthropic", CLAUDE, "anthropic:5h", NOW / 1_000, 0.2);
        row.resets_at = Some(((NOW + 60_000) / 1_000) as i64);
        let accounts = accounts(vec![row], NOW);
        assert_eq!(accounts[0].snapshot.observed_at, NOW);
        assert_eq!(
            accounts[0].snapshot.windows[0].resets_at,
            Some(((NOW + 60_000) / 1_000) as i64)
        );
        assert!(
            super::accounts(
                vec![Row {
                    recorded_at: -1,
                    ..super::tests::row("anthropic", CLAUDE, "anthropic:5h", 0, 0.2)
                }],
                NOW
            )
            .is_empty()
        );
    }

    #[test]
    fn services_are_named_as_people_name_them() {
        assert_eq!(service_label("anthropic"), "Anthropic");
        assert_eq!(service_label("openai-codex"), "OpenAI Codex");
        assert_eq!(service_label("xai-oauth"), "xAI");
        assert_eq!(service_label("muse-code"), "Muse code");
    }

    #[test]
    fn the_database_is_where_omp_looks_for_it() {
        let data = tempfile::tempdir().unwrap();
        let data = data.path().to_str().unwrap().to_owned();
        let at = |pairs: &[(&str, &str)]| {
            let pairs: Vec<(String, String)> = pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect();
            database(&move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.clone())
            })
        };
        let path = PathBuf::from;
        assert_eq!(
            at(&[("HOME", "/home/r")]),
            Some(path("/home/r/.omp/agent/agent.db"))
        );
        assert_eq!(
            at(&[("HOME", "/home/r"), ("PI_PROFILE", "work")]),
            Some(path("/home/r/.omp/profiles/work/agent/agent.db"))
        );
        // Omp's own variable wins, and its default profile is no profile.
        assert_eq!(
            at(&[
                ("HOME", "/home/r"),
                ("OMP_PROFILE", "default"),
                ("PI_PROFILE", "work")
            ]),
            Some(path("/home/r/.omp/agent/agent.db"))
        );
        assert_eq!(
            at(&[("HOME", "/home/r"), ("PI_PROFILE", "../x")]),
            None,
            "a profile that is not a plain name is not followed"
        );
        assert_eq!(
            at(&[("HOME", "/home/r"), ("PI_CONFIG_DIR", ".pi")]),
            Some(path("/home/r/.pi/agent/agent.db"))
        );
        assert_eq!(
            at(&[("HOME", "/home/r"), ("PI_CODING_AGENT_DIR", "/elsewhere")]),
            Some(path("/elsewhere/agent.db"))
        );
        // A profile names its directory whatever the agent directory is.
        assert_eq!(
            at(&[
                ("HOME", "/home/r"),
                ("PI_PROFILE", "work"),
                ("PI_CODING_AGENT_DIR", "/elsewhere")
            ]),
            Some(path("/home/r/.omp/profiles/work/agent/agent.db"))
        );
        // Data kept under XDG_DATA_HOME is used once Omp has made it.
        assert_eq!(
            at(&[("HOME", "/home/r"), ("XDG_DATA_HOME", &data)]),
            Some(path("/home/r/.omp/agent/agent.db"))
        );
        std::fs::create_dir(PathBuf::from(&data).join("omp")).unwrap();
        assert_eq!(
            at(&[("HOME", "/home/r"), ("XDG_DATA_HOME", &data)]),
            Some(PathBuf::from(&data).join("omp/agent.db"))
        );
        assert_eq!(at(&[]), None);
    }

    #[cfg(feature = "ui")]
    mod reader {
        use super::*;
        use rusqlite::Connection;
        use std::path::Path;

        /// Omp's own definition of the table.
        const SCHEMA: &str = "CREATE TABLE usage_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            recorded_at INTEGER NOT NULL,
            provider TEXT NOT NULL,
            account_key TEXT NOT NULL,
            email TEXT,
            account_id TEXT,
            limit_id TEXT NOT NULL,
            label TEXT NOT NULL,
            window_label TEXT,
            used_fraction REAL,
            status TEXT,
            resets_at INTEGER
        );
        CREATE INDEX idx_usage_history_series
            ON usage_history(provider, account_key, limit_id, recorded_at);";

        fn insert(connection: &Connection, limit: &str, recorded_at: u64, used: f64) {
            connection
                .execute(
                    "INSERT INTO usage_history (recorded_at, provider, account_key, email,
                        account_id, limit_id, label, window_label, used_fraction, status,
                        resets_at)
                     VALUES (?1, 'anthropic', ?2, 'a@example.test', 'id', ?3, 'Claude 5 Hour',
                        '5 Hour', ?4, 'ok', ?5)",
                    rusqlite::params![
                        recorded_at as i64,
                        CLAUDE,
                        limit,
                        used,
                        (NOW + 3_600_000) as i64
                    ],
                )
                .unwrap();
        }

        fn wal(path: &Path) -> Connection {
            let connection = Connection::open(path).unwrap();
            connection
                .pragma_update(None, "journal_mode", "wal")
                .unwrap();
            connection
        }

        fn listing(directory: &Path) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(directory)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }

        fn used(history: &History) -> Vec<f32> {
            match history {
                History::Read(accounts) => accounts
                    .iter()
                    .flat_map(|account| account.snapshot.windows.iter().map(|window| window.used))
                    .collect(),
                other => panic!("{other:?}"),
            }
        }

        #[test]
        fn a_record_omp_has_closed_is_read_without_leaving_a_file_beside_it() {
            // A directory whose name a URI would misread.
            let directory = tempfile::tempdir().unwrap();
            let directory = directory.path().join("agent ?#%");
            std::fs::create_dir(&directory).unwrap();
            let path = directory.join("agent.db");
            {
                let connection = wal(&path);
                connection.execute_batch(SCHEMA).unwrap();
                insert(&connection, "anthropic:5h", NOW - 60_000, 0.2);
                insert(&connection, "anthropic:5h", NOW - 30_000, 0.4);
            }
            let before = listing(&directory);
            assert_eq!(before, vec!["agent.db"]);
            assert_eq!(used(&read(&path, NOW)), vec![0.4]);
            assert_eq!(listing(&directory), before);
        }

        #[test]
        fn a_record_omp_has_open_is_read_with_what_it_has_not_checkpointed() {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("agent.db");
            let omp = wal(&path);
            omp.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
            omp.execute_batch(SCHEMA).unwrap();
            insert(&omp, "anthropic:5h", NOW - 60_000, 0.2);
            assert_eq!(used(&read(&path, NOW)), vec![0.2]);
            // Omp keeps writing while cagents reads.
            insert(&omp, "anthropic:5h", NOW - 30_000, 0.6);
            assert_eq!(used(&read(&path, NOW)), vec![0.6]);
            insert(&omp, "anthropic:5h", NOW, 0.7);
        }

        /// Omp stopped without closing the database: its log and index are
        /// left beside it, and nobody holds them.
        #[test]
        fn a_record_omp_left_open_is_read_whole_and_left_as_it_was() {
            let live = tempfile::tempdir().unwrap();
            let left = tempfile::tempdir().unwrap();
            let omp = wal(&live.path().join("agent.db"));
            omp.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
            omp.execute_batch(SCHEMA).unwrap();
            insert(&omp, "anthropic:5h", NOW - 30_000, 0.6);
            for name in ["agent.db", "agent.db-wal", "agent.db-shm"] {
                std::fs::copy(live.path().join(name), left.path().join(name)).unwrap();
            }
            let path = left.path().join("agent.db");
            let before: Vec<(String, Vec<u8>)> = listing(left.path())
                .into_iter()
                .map(|name| {
                    let bytes = std::fs::read(left.path().join(&name)).unwrap();
                    (name, bytes)
                })
                .collect();
            assert_eq!(used(&read(&path, NOW)), vec![0.6]);
            let after: Vec<(String, Vec<u8>)> = listing(left.path())
                .into_iter()
                .map(|name| {
                    let bytes = std::fs::read(left.path().join(&name)).unwrap();
                    (name, bytes)
                })
                .collect();
            assert!(before == after, "the read changed Omp's files");
        }

        #[test]
        fn a_record_in_another_shape_is_read_where_it_can_be() {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("agent.db");
            assert_eq!(read(&path, NOW), History::Missing);

            let connection = Connection::open(&path).unwrap();
            connection.execute_batch("CREATE TABLE other (x);").unwrap();
            assert_eq!(read(&path, NOW), History::Missing, "no record kept");

            // Only what places a figure, in another order, with a column
            // cagents does not know and a figure stored as text.
            connection
                .execute_batch(
                    "CREATE TABLE usage_history (novel TEXT, used_fraction, limit_id TEXT,
                        account_key TEXT, provider TEXT, recorded_at INTEGER);
                     INSERT INTO usage_history VALUES
                        ('x', '0.5', '1w', 'key', 'muse-code', 1790129990000);",
                )
                .unwrap();
            let History::Read(accounts) = read(&path, NOW) else {
                panic!("the record was not read");
            };
            assert_eq!(accounts.len(), 1);
            let window = &accounts[0].snapshot.windows[0];
            assert_eq!((window.kind, window.used), (WindowKind::Weekly, 0.5));
            assert_eq!(accounts[0].identity.email, None);

            connection
                .execute_batch("ALTER TABLE usage_history DROP COLUMN used_fraction;")
                .unwrap();
            assert!(matches!(read(&path, NOW), History::Unreadable(_)));
        }

        #[test]
        fn a_file_that_is_not_a_database_is_unreadable() {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("agent.db");
            std::fs::write(&path, vec![7_u8; 4096]).unwrap();
            assert_eq!(
                read(&path, NOW),
                History::Unreadable("Omp's usage record is damaged".into())
            );
        }
    }
}
