//! How many agent runs may be active or unconfirmed at once, and the refusal
//! that says which runs hold the slots when a submission would exceed it.
//!
//! The daemon owns the limits; a person sets them in a file of its own,
//! separate from window settings, which the daemon reads again whenever it
//! changes. A refusal travels as its sentence: [`LimitReached::recognize`]
//! reads the counts and holders back out of it, so a client reports them
//! without a protocol change.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use super::State;
use crate::model::{AgentId, RunId};

/// The defaults: each caller's runs, and every caller's together.
pub const DEFAULT_PER_CALLER: usize = 8;
pub const DEFAULT_TOTAL: usize = 32;
pub const MIN_LIMIT: usize = 1;
/// A safety bound on the setting, not a promise that this many providers fit
/// in memory.
pub const MAX_LIMIT: usize = 256;
/// The most holders a refusal names; it counts the rest.
pub const MAX_NAMED_HOLDERS: usize = 64;

/// The configured concurrency limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// Runs one caller may hold: a session lineage and its successors, the
    /// user's shells together, or one automation.
    pub per_caller: usize,
    /// Runs every caller may hold together.
    pub total: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            per_caller: DEFAULT_PER_CALLER,
            total: DEFAULT_TOTAL,
        }
    }
}

impl Limits {
    pub fn validate(self) -> Result<Self, String> {
        for (name, value) in [("per_caller", self.per_caller), ("total", self.total)] {
            if !(MIN_LIMIT..=MAX_LIMIT).contains(&value) {
                return Err(format!(
                    "The agent concurrency limit {name} must be {MIN_LIMIT} to {MAX_LIMIT}, not {value}"
                ));
            }
        }
        if self.per_caller > self.total {
            return Err(format!(
                "The per-caller agent concurrency limit ({}) cannot exceed the total ({})",
                self.per_caller, self.total
            ));
        }
        Ok(self)
    }
}

/// Which limit a refused submission reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// The caller's own runs.
    Caller,
    /// Every caller's runs together.
    Global,
}

impl Scope {
    const fn phrase(self) -> &'static str {
        match self {
            Self::Caller => "agent runs this caller may have at once",
            Self::Global => "agent runs all callers may have at once",
        }
    }
}

/// A run that holds a slot, by its agent and state. An interrupted run holds
/// its slot until its agent is closed, since its effects are unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    pub agent: AgentId,
    pub run: RunId,
    /// Its lowercase key, as the refusal and `agent list` write it.
    #[serde(with = "state_key")]
    pub state: State,
}

mod state_key {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    use super::State;

    pub fn serialize<S: Serializer>(state: &State, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(state.key())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<State, D::Error> {
        let key = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        State::from_key(&key).ok_or_else(|| D::Error::custom(format!("unknown run state {key}")))
    }
}

/// A submission refused because the runs already active or unconfirmed reach
/// a limit. Nothing was accepted, so the same request succeeds once a slot
/// frees.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitReached {
    pub scope: Scope,
    /// Runs holding a slot in this scope.
    pub running: usize,
    pub limit: usize,
    /// Up to [`MAX_NAMED_HOLDERS`] of them, oldest first.
    pub holders: Vec<Holder>,
}

/// How the refusal's sentence begins.
pub const REFUSAL: &str = "Agent concurrency limit reached: ";
const HELD_BY: &str = ". Yours: ";
const MORE: &str = " and ";

impl Scope {
    /// How a refusal ends its list of holders it did not name: a global
    /// refusal names only the caller's own.
    const fn unnamed(self) -> &'static str {
        match self {
            Self::Caller => " more",
            Self::Global => " held by other callers",
        }
    }
}
const ADVICE: &str = ". Nothing was started; retry once a run finishes or an agent is closed. An interrupted run keeps its slot until its agent is closed.";

impl fmt::Display for LimitReached {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{REFUSAL}{} of {} {} are active or unconfirmed",
            self.running,
            self.limit,
            self.scope.phrase()
        )?;
        let named = &self.holders[..self.holders.len().min(MAX_NAMED_HOLDERS)];
        if !named.is_empty() {
            formatter.write_str(HELD_BY)?;
            for (index, holder) in named.iter().enumerate() {
                if index > 0 {
                    formatter.write_str(", ")?;
                }
                write!(
                    formatter,
                    "agent {} run {} {}",
                    holder.agent,
                    holder.run,
                    holder.state.key()
                )?;
            }
            let unnamed = self.running.saturating_sub(named.len());
            if unnamed > 0 {
                write!(formatter, "{MORE}{unnamed}{}", self.scope.unnamed())?;
            }
        }
        formatter.write_str(ADVICE)
    }
}

impl LimitReached {
    /// The refusal a daemon's reply carries, if it is this one: retryable,
    /// in this refusal's own words.
    pub fn recognize(message: &str, retryable: bool) -> Option<Self> {
        if !retryable {
            return None;
        }
        // The counts and holders contain no full stop, so the sentence ends
        // at the first; what follows is advice that may be reworded.
        let (counts, rest) = message.strip_prefix(REFUSAL)?.split_once('.')?;
        let held = rest
            .strip_prefix(&HELD_BY[1..])
            .map(|held| held.split_once('.').map_or(held, |(held, _)| held));
        let (running, rest) = counts.split_once(" of ")?;
        let (limit, phrase) = rest.split_once(' ')?;
        let scope = [Scope::Caller, Scope::Global]
            .into_iter()
            .find(|scope| phrase == format!("{} are active or unconfirmed", scope.phrase()))?;
        let mut holders = Vec::new();
        if let Some(held) = held {
            let held = match held.rsplit_once(MORE) {
                Some((named, more))
                    if more
                        .strip_suffix(scope.unnamed())
                        .is_some_and(|count| count.parse::<usize>().is_ok()) =>
                {
                    named
                }
                _ => held,
            };
            for item in held.split(", ") {
                let mut words = item.split(' ');
                let (Some("agent"), Some(agent), Some("run"), Some(run), Some(state), None) = (
                    words.next(),
                    words.next(),
                    words.next(),
                    words.next(),
                    words.next(),
                    words.next(),
                ) else {
                    return None;
                };
                holders.push(Holder {
                    agent: AgentId::from_str(agent).ok()?,
                    run: RunId::from_str(run).ok()?,
                    state: State::from_key(state)?,
                });
            }
        }
        Some(Self {
            scope,
            running: running.parse().ok()?,
            limit: limit.parse().ok()?,
            holders,
        })
    }
}

/// The limits' file, in the configuration directory. Absent keys keep their
/// defaults.
pub const FILE_NAME: &str = "agent-concurrency.toml";
const MAX_FILE_BYTES: u64 = 1024;

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    per_caller: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    total: Option<usize>,
}

fn directory() -> std::io::Result<std::path::PathBuf> {
    crate::instance::project_dirs()
        .map(|dirs| dirs.config_dir().to_path_buf())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "configuration directory unavailable",
            )
        })
}

/// This installation's limits file.
pub fn path() -> std::io::Result<std::path::PathBuf> {
    Ok(directory()?.join(FILE_NAME))
}

/// The limits configured in `directory`, or the defaults when it has no file.
pub fn load_from(directory: &std::path::Path) -> std::io::Result<Limits> {
    use crate::fs_safety::{self, Root};
    use std::io;
    crate::secure_file::reject_symlink_components(directory)?;
    let root = match Root::open(directory) {
        Ok(root) => root,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Limits::default()),
        Err(error) => return Err(error),
    };
    let mut file = match fs_safety::open_beneath(&root, std::path::Path::new(FILE_NAME)) {
        Ok(file) => file,
        Err(error) if error.is_not_found() => return Ok(Limits::default()),
        Err(error) => return Err(io::Error::other(error)),
    };
    let bytes = fs_safety::read_bounded(&mut file, MAX_FILE_BYTES)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{FILE_NAME} exceeds {MAX_FILE_BYTES} bytes"),
        ));
    }
    let text =
        std::str::from_utf8(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let configuration: Configuration =
        toml::from_str(text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let defaults = Limits::default();
    Limits {
        per_caller: configuration.per_caller.unwrap_or(defaults.per_caller),
        total: configuration.total.unwrap_or(defaults.total),
    }
    .validate()
    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// This installation's configured limits.
pub fn load() -> std::io::Result<Limits> {
    load_from(&directory()?)
}

/// Replaces the limits configured in `directory` with `limits`, atomically.
/// A daemon reading them takes the new limits at its next submission.
pub fn save_to(directory: &std::path::Path, limits: Limits) -> std::io::Result<()> {
    use crate::fs_safety::{self, Root};
    use std::io::{self, Write};
    let limits = limits
        .validate()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    crate::secure_file::prepare_private_directory(directory)?;
    let root = Root::open(directory)?;
    let document = toml::to_string(&Configuration {
        per_caller: Some(limits.per_caller),
        total: Some(limits.total),
    })
    .map_err(io::Error::other)?;
    let (mut file, name) =
        fs_safety::create_temporary_in(&root, 0o600).map_err(io::Error::other)?;
    let result = (|| {
        file.write_all(document.as_bytes())?;
        file.sync_all()?;
        fs_safety::rename_within(&root, &name, FILE_NAME.as_ref()).map_err(io::Error::other)?;
        // The rename took effect; a failed directory sync risks only a crash.
        let _ = fs_safety::sync_directory(&root);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs_safety::unlink_within(&root, &name);
    }
    result
}

/// Replaces this installation's configured limits.
pub fn save(limits: Limits) -> std::io::Result<()> {
    save_to(&directory()?, limits)
}

/// The limits a daemon applies, read again from their file whenever it
/// changes, so a new setting takes effect without a restart.
#[cfg(feature = "host")]
#[derive(Debug)]
pub struct Setting {
    /// The configuration directory, or none for fixed limits.
    directory: Option<std::path::PathBuf>,
    cached: std::sync::Mutex<(Option<Stamp>, Limits)>,
}

/// What identifies one version of the file without reading it.
#[cfg(feature = "host")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stamp {
    Absent,
    Unreadable(std::io::ErrorKind),
    Present {
        device: u64,
        inode: u64,
        length: u64,
        modified: (i64, i64),
    },
}

#[cfg(feature = "host")]
impl Setting {
    /// Limits that never change, for a registry with no configuration.
    pub fn fixed(limits: Limits) -> Self {
        Self {
            directory: None,
            cached: std::sync::Mutex::new((None, limits)),
        }
    }

    /// This installation's configured limits.
    pub fn configured() -> std::io::Result<Self> {
        Ok(Self::in_directory(directory()?))
    }

    pub fn in_directory(directory: std::path::PathBuf) -> Self {
        Self {
            directory: Some(directory),
            cached: std::sync::Mutex::new((None, Limits::default())),
        }
    }

    /// The limits in effect now. A file that cannot be read or holds invalid
    /// limits leaves the defaults in effect, and is reported once per change.
    pub fn current(&self) -> Limits {
        let mut cached = self.cached.lock().unwrap_or_else(|p| p.into_inner());
        let Some(directory) = &self.directory else {
            return cached.1;
        };
        let stamp = stamp(&directory.join(FILE_NAME));
        if cached.0 != Some(stamp) {
            let limits = match stamp {
                Stamp::Absent => Limits::default(),
                Stamp::Unreadable(_) | Stamp::Present { .. } => load_from(directory)
                    .unwrap_or_else(|error| {
                        tracing::warn!(
                            %error,
                            file = FILE_NAME,
                            "the agent concurrency limits cannot be used; the defaults apply"
                        );
                        Limits::default()
                    }),
            };
            *cached = (Some(stamp), limits);
        }
        cached.1
    }
}

#[cfg(feature = "host")]
fn stamp(path: &std::path::Path) -> Stamp {
    use std::os::unix::fs::MetadataExt;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Stamp::Present {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Stamp::Absent,
        // Reading it reports why it cannot be read.
        Err(error) => Stamp::Unreadable(error.kind()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refusal(scope: Scope, running: usize, limit: usize, named: usize) -> LimitReached {
        let states = [
            State::Accepted,
            State::Running,
            State::Waiting,
            State::Interrupted,
        ];
        LimitReached {
            scope,
            running,
            limit,
            holders: (0..named)
                .map(|index| Holder {
                    agent: AgentId::new(),
                    run: RunId::new(),
                    state: states[index % states.len()],
                })
                .collect(),
        }
    }

    #[test]
    fn a_refusal_reads_back_from_its_sentence_only_when_retryable() {
        for expected in [
            refusal(Scope::Caller, 8, 8, 8),
            refusal(Scope::Global, 32, 32, 32),
            refusal(Scope::Global, 3, 2, 0),
        ] {
            let message = expected.to_string();
            assert!(message.starts_with(REFUSAL), "{message}");
            assert_eq!(LimitReached::recognize(&message, true), Some(expected));
            assert_eq!(LimitReached::recognize(&message, false), None);
        }
        let message = refusal(Scope::Caller, 1, 1, 1).to_string();
        assert!(message.contains("1 of 1 agent runs this caller may have at once"));
        assert!(message.contains("Nothing was started"));
        assert_eq!(
            LimitReached::recognize("Agent concurrency limit reached; wait", true),
            None
        );
        // A daemon that words its advice differently still reports its counts
        // and holders.
        let expected = refusal(Scope::Caller, 2, 2, 2);
        let reworded = expected
            .to_string()
            .replace(ADVICE, ". Wait for a run to finish.");
        assert_eq!(LimitReached::recognize(&reworded, true), Some(expected));
    }

    #[test]
    fn a_refusal_names_its_first_holders_and_counts_the_rest() {
        let mut expected = refusal(Scope::Global, 100, 100, 100);
        let message = expected.to_string();
        assert!(
            message.contains(&format!(
                " and {} held by other callers.",
                100 - MAX_NAMED_HOLDERS
            )),
            "{message}"
        );
        expected.holders.truncate(MAX_NAMED_HOLDERS);
        assert_eq!(LimitReached::recognize(&message, true), Some(expected));
    }

    #[test]
    fn limits_save_load_and_reject_invalid_values_without_overwrite() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("configuration");
        assert_eq!(load_from(&directory).unwrap(), Limits::default());
        let chosen = Limits {
            per_caller: 20,
            total: 60,
        };
        save_to(&directory, chosen).unwrap();
        assert_eq!(load_from(&directory).unwrap(), chosen);
        for invalid in [
            Limits {
                per_caller: 0,
                total: 32,
            },
            Limits {
                per_caller: 8,
                total: MAX_LIMIT + 1,
            },
            Limits {
                per_caller: 9,
                total: 8,
            },
        ] {
            assert!(save_to(&directory, invalid).is_err());
            assert_eq!(load_from(&directory).unwrap(), chosen);
        }
        std::fs::write(directory.join(FILE_NAME), "total = 40\n").unwrap();
        assert_eq!(
            load_from(&directory).unwrap(),
            Limits {
                per_caller: DEFAULT_PER_CALLER,
                total: 40
            }
        );
        for text in ["total = 0", "per_caller = '8'", "total = 40\nunknown = 1"] {
            std::fs::write(directory.join(FILE_NAME), text).unwrap();
            assert!(load_from(&directory).is_err(), "{text}");
        }
    }

    #[cfg(feature = "host")]
    #[test]
    fn a_setting_follows_its_file_and_falls_back_to_the_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let setting = Setting::in_directory(temp.path().to_path_buf());
        assert_eq!(setting.current(), Limits::default());
        let raised = Limits {
            per_caller: 16,
            total: 64,
        };
        save_to(temp.path(), raised).unwrap();
        assert_eq!(setting.current(), raised);
        std::fs::write(temp.path().join(FILE_NAME), "per_caller = 0\n").unwrap();
        assert_eq!(setting.current(), Limits::default());
        std::fs::remove_file(temp.path().join(FILE_NAME)).unwrap();
        assert_eq!(setting.current(), Limits::default());
        let fixed = Limits {
            per_caller: 2,
            total: 3,
        };
        assert_eq!(Setting::fixed(fixed).current(), fixed);
    }
}
