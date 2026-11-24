//! Pairing a phone with this computer, and the phones already paired.
//!
//! A phone reaches the computer over SSH with a key that `authorized_keys`
//! restricts to `cagents daemon bridge --mobile`. Pairing puts such a line
//! there without anyone copying a key by hand:
//!
//! 1. [`Pairing::start`] makes a one-time Ed25519 key and authorizes it, for
//!    a few minutes, to run only `cagents daemon bridge --pair DIR`. sshd
//!    itself stops accepting it at its `expiry-time`, even if nothing here
//!    gets to remove it.
//! 2. The desktop shows [`Pairing::code`], which carries that key, the
//!    computer's addresses and its host key, as a QR code.
//! 3. The phone connects with the one-time key; [`serve_pairing`] reads the
//!    phone's own public key and records the request in `DIR`.
//! 4. The person allows or declines it on the desktop: [`Pairing::allow`]
//!    authorizes the phone's key for the bridge alone and removes the
//!    one-time line, and [`Pairing::finish`] ends it otherwise.
//!
//! The desktop is the only writer of `authorized_keys`; see
//! [`AuthorizedKeys`] for how it changes that file.

use std::{
    fs,
    io::{self, BufRead, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::Engine as _;
use cagents_mobile_api::pairing as api;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// How long a pairing code works.
pub const PAIRING_LIFETIME: Duration = Duration::from_secs(5 * 60);
/// What starts the comment of a one-time pairing key's line.
const PAIRING_COMMENT: &str = "cagents-pairing-";
/// Beside `authorized_keys`: its contents before the latest change.
const BACKUP_NAME: &str = "authorized_keys.cagents-bak";
/// Beside `authorized_keys`: serializes this app's own changes to it.
const LOCK_NAME: &str = ".authorized_keys.cagents-lock";
/// Under the runtime directory: one directory per pairing.
const PAIRINGS: &str = "cagents-phone-pairing";
const ED25519: &str = "ssh-ed25519";

// Files in a pairing's directory.
const EXPIRES: &str = "expires";
const REQUEST: &str = "request.json";
const DECISION: &str = "decision";
const WITHDRAWN: &str = "withdrawn";

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// An OpenSSH public key: its type and its wire-format blob.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicKey {
    kind: String,
    blob: Vec<u8>,
}

impl PublicKey {
    /// Reads `TYPE BASE64 [COMMENT]`, accepting it only when the blob names
    /// the same type.
    pub fn parse(text: &str) -> Option<Self> {
        let mut words = text.split_ascii_whitespace();
        let kind = words.next()?;
        let blob = base64::engine::general_purpose::STANDARD
            .decode(words.next()?)
            .ok()?;
        let named = blob
            .get(..4)
            .map(|length| u32::from_be_bytes(length.try_into().unwrap()) as usize)?;
        (blob.get(4..4 + named)? == kind.as_bytes()).then(|| Self {
            kind: kind.to_owned(),
            blob,
        })
    }

    /// An Ed25519 key, from its 32 bytes.
    pub fn ed25519(public: &[u8; 32]) -> Self {
        let mut blob = Vec::with_capacity(51);
        for part in [ED25519.as_bytes(), public.as_slice()] {
            blob.extend_from_slice(&(part.len() as u32).to_be_bytes());
            blob.extend_from_slice(part);
        }
        Self {
            kind: ED25519.into(),
            blob,
        }
    }

    /// Whether this is a well-formed Ed25519 key, the only kind a phone may
    /// pair with.
    pub fn is_ed25519(&self) -> bool {
        self.kind == ED25519 && self.blob.len() == 4 + ED25519.len() + 4 + 32
    }

    /// OpenSSH's fingerprint, as `ssh-keygen -l` prints it: `SHA256:` and the
    /// unpadded base64 of the blob's digest.
    pub fn fingerprint(&self) -> String {
        format!(
            "SHA256:{}",
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(Sha256::digest(&self.blob))
        )
    }

    /// `TYPE BASE64`, re-encoded from the parsed key.
    pub fn openssh(&self) -> String {
        format!(
            "{} {}",
            self.kind,
            base64::engine::general_purpose::STANDARD.encode(&self.blob)
        )
    }

    pub fn kind(&self) -> &str {
        &self.kind
    }
}

/// The computer's Ed25519 host key, which the phone pins.
pub fn host_key(path: &Path) -> io::Result<PublicKey> {
    let text = fs::read_to_string(path)?;
    PublicKey::parse(&text)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not an OpenSSH public key"))
}

/// Where sshd keeps the host key the phone pins.
pub const HOST_KEY: &str = "/etc/ssh/ssh_host_ed25519_key.pub";

// ---------------------------------------------------------------------------
// authorized_keys lines
// ---------------------------------------------------------------------------

/// One key line of `authorized_keys`: its options, key and comment.
#[derive(Debug)]
struct Line<'a> {
    options: Vec<(&'a str, Option<String>)>,
    key: PublicKey,
    comment: &'a str,
}

impl Line<'_> {
    fn option(&self, name: &str) -> Option<&(&str, Option<String>)> {
        self.options
            .iter()
            .find(|(option, _)| option.eq_ignore_ascii_case(name))
    }

    fn command(&self) -> Option<Vec<String>> {
        shell_words(self.option("command")?.1.as_deref()?)
    }

    /// Whether the key can do nothing but run its command: `restrict`, or
    /// the four older options that add up to it.
    fn restricted(&self) -> bool {
        self.option("restrict").is_some()
            || [
                "no-port-forwarding",
                "no-agent-forwarding",
                "no-X11-forwarding",
                "no-pty",
            ]
            .iter()
            .all(|name| self.option(name).is_some())
    }
}

/// Reads one line of `authorized_keys`, as sshd does; `None` for a blank
/// line, a comment or one that isn't a key.
fn parse_line(text: &str) -> Option<Line<'_>> {
    let text = text.trim_start();
    if text.is_empty() || text.starts_with('#') {
        return None;
    }
    let (options, rest) = if PublicKey::parse(text).is_some() {
        (Vec::new(), text)
    } else {
        let end = options_end(text)?;
        (parse_options(&text[..end])?, text[end..].trim_start())
    };
    let key = PublicKey::parse(rest)?;
    // The comment is what follows the type and the key.
    let mut comment = rest;
    for _ in 0..2 {
        comment = comment
            .trim_start()
            .split_once([' ', '\t'])
            .map_or("", |(_, after)| after);
    }
    let comment = comment.trim();
    Some(Line {
        options,
        key,
        comment,
    })
}

/// Where the options end: the first whitespace outside double quotes.
fn options_end(text: &str) -> Option<usize> {
    let mut quoted = false;
    let mut escaped = false;
    for (index, character) in text.char_indices() {
        match character {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ' ' | '\t' if !quoted => return Some(index),
            _ => {}
        }
    }
    None
}

fn parse_options(text: &str) -> Option<Vec<(&str, Option<String>)>> {
    let mut options = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let name_end = rest.find(['=', ',']).unwrap_or(rest.len());
        let name = &rest[..name_end];
        rest = &rest[name_end..];
        let value = if let Some(after) = rest.strip_prefix("=\"") {
            let mut value = String::new();
            let mut characters = after.char_indices();
            let close = loop {
                match characters.next()? {
                    (_, '\\') => match characters.next()? {
                        (_, '"') => value.push('"'),
                        (_, other) => {
                            value.push('\\');
                            value.push(other);
                        }
                    },
                    (index, '"') => break index,
                    (_, other) => value.push(other),
                }
            };
            rest = &after[close + 1..];
            Some(value)
        } else {
            None
        };
        options.push((name, value));
        rest = match rest.strip_prefix(',') {
            Some(rest) => rest,
            None if rest.is_empty() => rest,
            None => return None,
        };
    }
    Some(options)
}

/// A forced command's words, for the simple quoting this module writes and
/// people do: whitespace, single quotes, double quotes and backslashes.
/// `None` for anything the shell would expand.
fn shell_words(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = None::<String>;
    let mut characters = command.chars();
    while let Some(character) = characters.next() {
        match character {
            ' ' | '\t' => words.extend(word.take()),
            '\'' => {
                let word = word.get_or_insert_default();
                loop {
                    match characters.next()? {
                        '\'' => break,
                        other => word.push(other),
                    }
                }
            }
            '"' => {
                let word = word.get_or_insert_default();
                loop {
                    match characters.next()? {
                        '"' => break,
                        '\\' => word.push(characters.next()?),
                        '$' | '`' => return None,
                        other => word.push(other),
                    }
                }
            }
            '\\' => word.get_or_insert_default().push(characters.next()?),
            '$' | '`' | ';' | '&' | '|' | '<' | '>' | '(' | ')' | '*' | '?' | '~' => return None,
            other => word.get_or_insert_default().push(other),
        }
    }
    words.extend(word);
    Some(words)
}

/// `path` as one shell word that needs no escaping inside the double quotes
/// of `command="…"`: bare when it is plain, single-quoted otherwise. A path
/// holding a quote, a backslash or a control character is refused.
fn shell_word(path: &Path) -> Result<String, String> {
    let text = path
        .to_str()
        .ok_or_else(|| format!("{} isn't valid UTF-8.", path.display()))?;
    if text.is_empty()
        || text
            .chars()
            .any(|c| c.is_control() || matches!(c, '\'' | '"' | '\\'))
    {
        return Err(format!(
            "The path {text} can't be written into authorized_keys safely."
        ));
    }
    Ok(
        if text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._+-@:,=".contains(c))
        {
            text.to_owned()
        } else {
            format!("'{text}'")
        },
    )
}

/// Whether `words` run `cagents daemon bridge` with exactly `mode`.
fn runs_bridge(words: &[String], mode: &[&str]) -> bool {
    words.len() == 3 + mode.len()
        && Path::new(&words[0])
            .file_name()
            .is_some_and(|name| name == "cagents")
        && words[1] == "daemon"
        && words[2] == "bridge"
        && words[3..].iter().zip(mode).all(|(word, mode)| word == mode)
}

fn is_phone_line(line: &Line<'_>) -> bool {
    line.command()
        .is_some_and(|words| runs_bridge(&words, &["--mobile"]))
}

fn is_pairing_line(line: &Line<'_>) -> bool {
    line.comment.starts_with(PAIRING_COMMENT)
        && line
            .command()
            .is_some_and(|words| words.len() == 5 && runs_bridge(&words, &["--pair", &words[4]]))
}

/// A phone whose key `authorized_keys` lets run the bridge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Phone {
    /// The line's comment, which pairing sets to the device's name.
    pub name: String,
    pub key: PublicKey,
    /// Whether the key can do nothing but run the bridge.
    pub restricted: bool,
    /// The `cagents` the line runs, when it no longer exists.
    pub missing_executable: Option<String>,
}

/// The phones `contents` authorizes, in file order.
pub fn phones(contents: &str) -> Vec<Phone> {
    contents
        .lines()
        .filter_map(parse_line)
        .filter(is_phone_line)
        .map(|line| {
            let executable = line.command().and_then(|words| words.into_iter().next());
            Phone {
                name: if line.comment.is_empty() {
                    "Phone".into()
                } else {
                    line.comment.to_owned()
                },
                restricted: line.restricted(),
                missing_executable: executable.filter(|path| !Path::new(path).exists()),
                key: line.key,
            }
        })
        .collect()
}

/// `contents` without the lines of one-time pairing keys.
fn without_pairing_lines(contents: &str) -> String {
    contents
        .split_inclusive('\n')
        .filter(|line| !parse_line(line).is_some_and(|line| is_pairing_line(&line)))
        .collect()
}

fn appended(contents: &str, line: &str) -> String {
    let mut next = contents.to_owned();
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(line);
    next.push('\n');
    next
}

/// The forced command a paired phone's key runs.
pub fn bridge_command(executable: &Path) -> Result<String, String> {
    Ok(format!(
        "{} daemon bridge --mobile",
        shell_word(executable)?
    ))
}

/// The `cagents` a forced command should name: the installation's stable
/// `current/bin/cagents` when this is an installed release, so that an update
/// keeps paired phones working; otherwise the one beside this executable.
pub fn installed_cli() -> io::Result<PathBuf> {
    let cli = crate::companion::Product::Cli.beside_current()?;
    if let Ok(layout) = crate::update::locate(&cli) {
        let stable = layout
            .prefix
            .join("current/bin")
            .join(crate::companion::Product::Cli.file_name());
        if stable.canonicalize().ok() == cli.canonicalize().ok() {
            return Ok(stable);
        }
    }
    Ok(cli)
}

// ---------------------------------------------------------------------------
// Changing authorized_keys
// ---------------------------------------------------------------------------

/// The person's `~/.ssh/authorized_keys`, changed only by whole-file
/// replacement under a lock, with the previous contents kept beside it.
///
/// Every change adds or removes only lines this module writes, and every
/// line it writes carries `restrict` and a forced command. It creates
/// `~/.ssh` (0700) and the file (0600) when they are missing, and refuses a
/// file it shouldn't replace: a symlink, which something else manages, or
/// one that another user owns or can write, which sshd ignores.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizedKeys {
    path: PathBuf,
}

/// What [`AuthorizedKeys::add_phone`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Added {
    Added,
    /// The key was already paired; nothing changed.
    AlreadyPaired,
}

impl AuthorizedKeys {
    /// `~/.ssh/authorized_keys` under `home`.
    pub fn in_home(home: &Path) -> Self {
        Self {
            path: home.join(".ssh/authorized_keys"),
        }
    }

    /// The person's own, from `HOME`.
    pub fn of_user() -> io::Result<Self> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
        Ok(Self::in_home(&home))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn directory(&self) -> &Path {
        self.path.parent().expect("authorized_keys has a directory")
    }

    /// The file's contents; empty when it doesn't exist.
    pub fn read(&self) -> io::Result<String> {
        match fs::read_to_string(&self.path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
            other => other,
        }
    }

    /// The phones it authorizes.
    pub fn phones(&self) -> io::Result<Vec<Phone>> {
        Ok(phones(&self.read()?))
    }

    /// Why this app can't change the file, for the person; `None` when it
    /// can. A missing `~/.ssh` or file is fine: they are created.
    pub fn unwritable(&self) -> Option<String> {
        self.check(false).err()
    }

    fn check(&self, create: bool) -> Result<(), String> {
        let uid = effective_uid();
        let directory = self.directory();
        match fs::metadata(directory) {
            Ok(meta) => {
                if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o022 != 0 {
                    return Err(format!(
                        "{} isn't a private directory you own, so sshd won't read keys there.",
                        directory.display()
                    ));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if create {
                    fs::DirBuilder::new()
                        .mode(0o700)
                        .create(directory)
                        .map_err(|error| {
                            format!("Couldn't create {}: {error}", directory.display())
                        })?;
                }
            }
            Err(error) => return Err(format!("Couldn't read {}: {error}", directory.display())),
        }
        match fs::symlink_metadata(&self.path) {
            Ok(meta) if meta.file_type().is_symlink() => Err(format!(
                "{} is a link, so something else manages it.",
                self.path.display()
            )),
            Ok(meta) if !meta.is_file() || meta.uid() != uid || meta.mode() & 0o022 != 0 => {
                Err(format!(
                    "{} isn't a private file you own, so sshd won't use it.",
                    self.path.display()
                ))
            }
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("Couldn't read {}: {error}", self.path.display())),
        }
    }

    /// Replace the contents with what `change` makes of them, if anything.
    /// The previous contents, less one-time pairing lines, go to
    /// [`BACKUP_NAME`] first.
    ///
    /// Only cagents takes the lock, so another tool (an editor,
    /// `ssh-copy-id`, `>>`) can write meanwhile. When the file changed
    /// between reading it and replacing it, the change is made again on the
    /// new contents rather than writing over them.
    fn edit<T>(
        &self,
        mut change: impl FnMut(&str) -> Result<(Option<String>, T), String>,
    ) -> Result<T, String> {
        self.check(true)?;
        let directory = self.directory();
        let _lock = crate::instance::owned_lock(&directory.join(LOCK_NAME), libc::LOCK_EX)
            .map_err(|error| format!("Couldn't lock {}: {error}", self.path.display()))?;
        for _ in 0..EDIT_ATTEMPTS {
            // Checked again under the lock, in case it changed meanwhile.
            self.check(false)?;
            let before = self.version();
            let current = self
                .read()
                .map_err(|error| format!("Couldn't read {}: {error}", self.path.display()))?;
            let (next, result) = change(&current)?;
            let Some(next) = next.filter(|next| *next != current) else {
                return Ok(result);
            };
            let mode = fs::metadata(&self.path).map_or(0o600, |meta| meta.mode() & 0o700);
            let previous = without_pairing_lines(&current);
            if !previous.is_empty() {
                replace(&directory.join(BACKUP_NAME), &previous, 0o600).map_err(|error| {
                    format!("Couldn't back up {}: {error}", self.path.display())
                })?;
            }
            if self.version() != before {
                continue;
            }
            replace(&self.path, &next, mode)
                .map_err(|error| format!("Couldn't write {}: {error}", self.path.display()))?;
            return Ok(result);
        }
        Err(format!(
            "{} kept changing while cagents tried to update it. Try again.",
            self.path.display()
        ))
    }

    /// What identifies the file's contents without reading them: its inode,
    /// size and modification time. `None` when it is missing.
    fn version(&self) -> Option<(u64, u64, u64, i64, i64)> {
        fs::symlink_metadata(&self.path).ok().map(|meta| {
            (
                meta.dev(),
                meta.ino(),
                meta.size(),
                meta.mtime(),
                meta.mtime_nsec(),
            )
        })
    }

    /// Authorize `key` to run `command` (see [`bridge_command`]) and nothing
    /// else, under `name`.
    pub fn add_phone(&self, key: &PublicKey, name: &str, command: &str) -> Result<Added, String> {
        self.edit(|current| {
            for line in current.lines().filter_map(parse_line) {
                if line.key != *key {
                    continue;
                }
                if is_phone_line(&line) {
                    return Ok((None, Added::AlreadyPaired));
                }
                if !is_pairing_line(&line) {
                    return Err(
                        "This phone's key already has other access in authorized_keys; \
                         remove that line first."
                            .into(),
                    );
                }
            }
            let line = phone_line(key, name, command);
            Ok((Some(appended(current, &line)), Added::Added))
        })
    }

    /// Remove every bridge line for `key`. Its other lines, if any, stay.
    pub fn remove_phone(&self, key: &PublicKey) -> Result<(), String> {
        self.edit(|current| {
            let next = current
                .split_inclusive('\n')
                .filter(|line| {
                    !parse_line(line).is_some_and(|line| line.key == *key && is_phone_line(&line))
                })
                .collect();
            Ok((Some(next), ()))
        })
    }

    /// Remove the one-time pairing lines `stale` picks. Returns how many
    /// there were.
    fn remove_pairing_lines(&self, stale: impl Fn(&Path) -> bool) -> Result<usize, String> {
        let picked = |text: &str| {
            parse_line(text).is_some_and(|line| {
                is_pairing_line(&line)
                    && line
                        .command()
                        .is_some_and(|words| stale(Path::new(&words[4])))
            })
        };
        if !self
            .read()
            .is_ok_and(|current| current.lines().any(&picked))
        {
            return Ok(0);
        }
        self.edit(|current| {
            let mut removed = 0;
            let next = current
                .split_inclusive('\n')
                .filter(|line| {
                    let stale = picked(line);
                    removed += usize::from(stale);
                    !stale
                })
                .collect();
            Ok((Some(next), removed))
        })
    }
}

/// How many times [`AuthorizedKeys::edit`] starts over when another tool
/// changes the file under it.
const EDIT_ATTEMPTS: usize = 3;

fn phone_line(key: &PublicKey, name: &str, command: &str) -> String {
    format!(
        "command=\"{command}\",restrict {} {}",
        key.openssh(),
        device_name(name)
    )
}

/// Write `contents` to a new file beside `path` and rename it over `path`.
fn replace(path: &Path, contents: &str, mode: u32) -> io::Result<()> {
    let directory = path.parent().expect("a file has a directory");
    let mut file = tempfile::Builder::new()
        .prefix(".cagents-")
        .permissions(fs::Permissions::from_mode(mode))
        .tempfile_in(directory)?;
    file.write_all(contents.as_bytes())?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    fs::File::open(directory)?.sync_all()
}

/// A device name as it may appear in `authorized_keys` and on screen:
/// letters, digits, spaces and common punctuation, at most
/// [`api::MAX_DEVICE_NAME_CHARS`] of them.
pub fn device_name(name: &str) -> String {
    let kept: String = name
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|c| c.is_alphanumeric() || " -_.'()&+#".contains(*c))
        .collect();
    let name = kept
        .split(' ')
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(api::MAX_DEVICE_NAME_CHARS)
        .collect::<String>();
    // A pairing line's comment is how cleanup finds it.
    if name.is_empty() || name.starts_with(PAIRING_COMMENT) {
        "Phone".into()
    } else {
        name
    }
}

// ---------------------------------------------------------------------------
// A pairing under way
// ---------------------------------------------------------------------------

/// What the pairing code tells the phone about the computer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Connect {
    /// The computer's name, to show on the phone.
    pub name: String,
    pub user: String,
    pub port: u16,
    /// The host key's fingerprint, for the phone to pin.
    pub host_key: Option<String>,
    /// Addresses, in the order the phone should try them.
    pub hosts: Vec<String>,
}

/// What a phone asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub key: PublicKey,
    pub name: String,
}

/// Where a pairing stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Progress {
    /// No phone has used the code yet.
    Waiting,
    /// A phone used it and waits for the person.
    Requested(Request),
    /// The phone that used it hung up.
    Withdrawn,
    Expired,
}

/// How a pairing ended, as the phone is told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Allowed,
    Declined,
    Cancelled,
}

impl Outcome {
    fn word(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::Declined => "declined",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Serialize, Deserialize)]
struct StoredRequest {
    key: String,
    name: String,
}

/// One pairing code and its one-time key's line in `authorized_keys`.
#[derive(Debug)]
pub struct Pairing {
    keys: AuthorizedKeys,
    directory: PathBuf,
    /// The one-time line's comment, which names this pairing.
    comment: String,
    line: String,
    seed: [u8; 32],
    /// The one-time key, which can never become a phone's.
    one_time: PublicKey,
    expires: SystemTime,
}

impl Pairing {
    /// Make a one-time key and authorize it to run `executable daemon bridge
    /// --pair` until [`PAIRING_LIFETIME`] after `now`. The pairing's
    /// directory goes under `runtime`, the private runtime directory.
    pub fn start(
        keys: AuthorizedKeys,
        executable: &Path,
        runtime: &Path,
        now: SystemTime,
    ) -> Result<Self, String> {
        let mut random = [0u8; 40];
        getrandom::fill(&mut random).map_err(|error| format!("No randomness: {error}"))?;
        let seed: [u8; 32] = random[..32].try_into().unwrap();
        let id: String = random[32..]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let parent = runtime.join(PAIRINGS);
        let directory = parent.join(&id);
        let expires = now + PAIRING_LIFETIME;
        let public = ed25519_dalek::SigningKey::from_bytes(&seed)
            .verifying_key()
            .to_bytes();
        let one_time = PublicKey::ed25519(&public);
        let comment = format!("{PAIRING_COMMENT}{id}");
        // sshd stops taking the key at this UTC minute; the margin covers
        // rounding. The bridge enforces the exact expiry itself.
        let line = format!(
            "expiry-time=\"{}\",command=\"{} daemon bridge --pair {}\",restrict {} {comment}",
            utc_minute(expires + Duration::from_secs(120)),
            shell_word(executable)?,
            shell_word(&directory)?,
            one_time.openssh(),
        );
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&parent)
            .and_then(|()| fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)))
            .map_err(|error| format!("Couldn't create {}: {error}", parent.display()))?;
        // Made complete under a hidden name, then renamed, so another
        // process's sweep never sees it without its expiry.
        let making = parent.join(format!(".{id}"));
        let made = fs::DirBuilder::new()
            .mode(0o700)
            .create(&making)
            .and_then(|()| fs::write(making.join(EXPIRES), unix_seconds(expires).to_string()))
            .and_then(|()| fs::rename(&making, &directory));
        if let Err(error) = made {
            let _ = fs::remove_dir_all(&making);
            return Err(format!("Couldn't create {}: {error}", directory.display()));
        }
        let pairing = Self {
            keys,
            directory,
            comment,
            line,
            seed,
            one_time,
            expires,
        };
        if let Err(error) = pairing
            .keys
            .edit(|current| Ok((Some(appended(current, &pairing.line)), ())))
        {
            let _ = fs::remove_dir_all(&pairing.directory);
            return Err(error);
        }
        Ok(pairing)
    }

    pub fn expires(&self) -> SystemTime {
        self.expires
    }

    /// The pairing code: a [`api::PAIRING_SCHEME`] URI.
    pub fn code(&self, connect: &Connect) -> String {
        let mut url = url::Url::parse(&format!("{}://pair", api::PAIRING_SCHEME))
            .expect("the pairing scheme parses");
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("v", &api::PAIRING_CODE_VERSION.to_string())
                .append_pair("name", &connect.name)
                .append_pair("user", &connect.user)
                .append_pair("port", &connect.port.to_string());
            if let Some(host_key) = &connect.host_key {
                query.append_pair("hostkey", host_key);
            }
            for host in &connect.hosts {
                query.append_pair("host", host);
            }
            query
                .append_pair(
                    "key",
                    &base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(self.seed),
                )
                .append_pair("expires", &unix_seconds(self.expires).to_string());
        }
        url.into()
    }

    /// Where it stands at `now`.
    pub fn progress(&self, now: SystemTime) -> Progress {
        if self.directory.join(WITHDRAWN).exists() {
            return Progress::Withdrawn;
        }
        if let Some(request) = fs::read(self.directory.join(REQUEST))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<StoredRequest>(&bytes).ok())
            .and_then(|stored| {
                Some(Request {
                    key: PublicKey::parse(&stored.key)?,
                    name: device_name(&stored.name),
                })
            })
        {
            return Progress::Requested(request);
        }
        if now >= self.expires {
            Progress::Expired
        } else {
            Progress::Waiting
        }
    }

    /// Authorize the phone that asked, with `command`, remove the one-time
    /// line and tell the phone.
    pub fn allow(&self, request: &Request, command: &str) -> Result<Added, String> {
        if !request.key.is_ed25519() {
            return Err("Only Ed25519 keys can be paired.".into());
        }
        if request.key == self.one_time {
            return Err("The phone offered the pairing code's own key.".into());
        }
        let added = self.keys.edit(|current| {
            let without = self.without_own_line(current);
            for line in without.lines().filter_map(parse_line) {
                if line.key == request.key {
                    if is_phone_line(&line) {
                        return Ok((Some(without), Added::AlreadyPaired));
                    }
                    return Err(
                        "This phone's key already has other access in authorized_keys; \
                         remove that line first."
                            .into(),
                    );
                }
            }
            let next = appended(&without, &phone_line(&request.key, &request.name, command));
            Ok((Some(next), Added::Added))
        })?;
        self.decide(Outcome::Allowed);
        Ok(added)
    }

    /// End the pairing without allowing a phone: remove the one-time line and
    /// tell a waiting phone why.
    pub fn finish(&self, outcome: Outcome) -> Result<(), String> {
        let removed = self
            .keys
            .edit(|current| Ok((Some(self.without_own_line(current)), ())));
        self.decide(outcome);
        removed
    }

    /// `contents` less this pairing's one-time line: the line as written,
    /// or a pairing line with its comment, should something have reformatted it.
    fn without_own_line(&self, contents: &str) -> String {
        contents
            .split_inclusive('\n')
            .filter(|candidate| {
                let text = candidate.trim_end_matches(['\n', '\r']);
                text != self.line
                    && !parse_line(text)
                        .is_some_and(|line| is_pairing_line(&line) && line.comment == self.comment)
            })
            .collect()
    }

    /// Tell the phone, whose bridge then removes the directory; with no
    /// phone waiting, remove it here.
    fn decide(&self, outcome: Outcome) {
        if self.directory.join(REQUEST).exists() && !self.directory.join(WITHDRAWN).exists() {
            // Renamed into place, so the bridge never reads it half-written.
            let decided = tempfile::NamedTempFile::new_in(&self.directory)
                .and_then(|mut file| {
                    file.write_all(outcome.word().as_bytes())?;
                    Ok(file)
                })
                .and_then(|file| {
                    file.persist(self.directory.join(DECISION))
                        .map_err(|error| error.error)
                });
            if decided.is_ok() {
                return;
            }
        }
        let _ = fs::remove_dir_all(&self.directory);
    }
}

/// Remove pairings that ended without being finished, such as when the
/// desktop quit while showing a code: those whose code expired before `now`.
/// Another window's pairing still under way is kept. Returns how many
/// one-time lines were removed.
pub fn remove_stale_pairings(
    keys: &AuthorizedKeys,
    runtime: &Path,
    now: SystemTime,
) -> Result<usize, String> {
    let pairings = runtime.join(PAIRINGS);
    // A directory without a readable expiry is one being made, or left half
    // made; it is stale once older than any pairing could be.
    let stale = |directory: &Path| {
        match fs::read_to_string(directory.join(EXPIRES))
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
        {
            // A phone that used the code waits up to a lifetime more for
            // the person's answer.
            Some(expires) => {
                let answer_by = if directory.join(REQUEST).exists() {
                    PAIRING_LIFETIME
                } else {
                    Duration::ZERO
                };
                UNIX_EPOCH + Duration::from_secs(expires) + answer_by <= now
            }
            None => fs::symlink_metadata(directory)
                .and_then(|meta| meta.modified())
                .map_or(true, |modified| modified + PAIRING_LIFETIME <= now),
        }
    };
    if let Ok(entries) = fs::read_dir(&pairings) {
        for entry in entries.flatten() {
            if stale(&entry.path()) {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }
    // A line naming another machine's runtime directory, with a home shared
    // between machines, is that machine's to remove.
    keys.remove_pairing_lines(|directory| {
        directory.parent() == Some(pairings.as_path()) && stale(directory)
    })
}

/// `time` as `expiry-time` takes it in UTC: `YYYYMMDDHHMMZ`.
fn utc_minute(time: SystemTime) -> String {
    let seconds = unix_seconds(time) as libc::time_t;
    // SAFETY: zeroed is a valid `tm`; gmtime_r writes only into it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::gmtime_r(&seconds, &mut tm) };
    format!(
        "{:04}{:02}{:02}{:02}{:02}Z",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

// ---------------------------------------------------------------------------
// The phone's side: `cagents daemon bridge --pair DIR`
// ---------------------------------------------------------------------------

/// How often the bridge looks for the person's decision.
const DECISION_POLL: Duration = Duration::from_millis(200);

/// Serve one pairing request: the forced command of a one-time pairing key.
pub fn serve_pairing(directory: &Path) -> io::Result<()> {
    if std::env::var_os(crate::agent_execution::AUTHORITY_ENV).is_some() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Pairing requires your own terminal",
        ));
    }
    let mut output = io::stdout().lock();
    let (lines, input) = mpsc::channel();
    std::thread::Builder::new()
        .name("pair-read".into())
        .spawn(move || {
            let mut stdin = io::stdin().lock();
            let mut line = Vec::new();
            let first = read_bounded(&mut stdin, &mut line);
            let _ = lines.send(first.map(|()| line));
            // Whatever follows is read into a fixed buffer and ignored; the
            // end of input means the phone hung up.
            let mut rest = [0u8; 4096];
            while io::Read::read(&mut stdin, &mut rest).is_ok_and(|read| read > 0) {}
            let _ = lines.send(Err(io::ErrorKind::UnexpectedEof.into()));
        })?;
    let reply = pair(directory, &input, SystemTime::now, &mut output);
    let line = serde_json::to_string(&reply).map_err(io::Error::other)?;
    writeln!(output, "{line}")?;
    output.flush()
}

fn read_bounded(input: &mut impl BufRead, line: &mut Vec<u8>) -> io::Result<()> {
    let limit = api::MAX_PAIR_LINE_BYTES as u64 + 1;
    let read = io::Read::take(&mut *input, limit).read_until(b'\n', line)?;
    if read == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    if line.last() == Some(&b'\n') {
        line.pop();
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"))
    }
}

fn pair_error(message: &str) -> api::PairServerLine {
    api::PairServerLine::PairError(api::PairError {
        message: message.into(),
    })
}

const EXPIRED: &str = "This pairing code has expired. Show a new one on the computer.";

/// The exchange: `input` carries the phone's first line, then the end of its
/// input. Writes `pair_waiting` to `output` once the request is recorded and
/// returns the final line.
fn pair(
    directory: &Path,
    input: &mpsc::Receiver<io::Result<Vec<u8>>>,
    now: impl Fn() -> SystemTime,
    output: &mut impl Write,
) -> api::PairServerLine {
    let expires = match open_pairing(directory) {
        Ok(expires) => expires,
        Err(message) => return pair_error(message),
    };
    let remaining = |now: SystemTime| {
        expires
            .duration_since(now)
            .ok()
            .filter(|left| !left.is_zero())
    };
    let Some(left) = remaining(now()) else {
        return pair_error(EXPIRED);
    };
    let line = match input.recv_timeout(left) {
        Ok(Ok(line)) => line,
        Ok(Err(_)) => return pair_error("The phone sent no pairing request."),
        Err(_) => return pair_error(EXPIRED),
    };
    let request = match serde_json::from_slice::<api::PairClientLine>(&line) {
        Ok(api::PairClientLine::Pair(request)) => request,
        Err(_) => return pair_error("The phone's pairing request wasn't understood."),
    };
    let Some(key) = PublicKey::parse(&request.key).filter(PublicKey::is_ed25519) else {
        return pair_error("Only Ed25519 keys can be paired.");
    };
    if let Err(message) = record_request(directory, &key, &request.name) {
        return pair_error(message);
    }
    let waiting = serde_json::to_string(&api::PairServerLine::PairWaiting {})
        .expect("a fixed line serializes");
    if writeln!(output, "{waiting}")
        .and_then(|()| output.flush())
        .is_err()
    {
        let _ = fs::write(directory.join(WITHDRAWN), "");
        return pair_error("The phone stopped pairing.");
    }
    // The code had to be used before it expired; the person then has a
    // full lifetime to decide, however late the phone scanned it.
    let decide_by = now() + PAIRING_LIFETIME;
    loop {
        let decision = fs::read_to_string(directory.join(DECISION)).ok();
        let reply = match decision.as_deref().map(str::trim) {
            Some("allowed") => Some(api::PairServerLine::Paired(api::Paired {
                host: super::host_name(),
            })),
            Some("declined") => Some(api::PairServerLine::PairDeclined {}),
            Some("cancelled") => Some(pair_error("Pairing was cancelled on the computer.")),
            _ => None,
        };
        if let Some(reply) = reply {
            let _ = fs::remove_dir_all(directory);
            return reply;
        }
        let Some(left) = decide_by
            .duration_since(now())
            .ok()
            .filter(|left| !left.is_zero())
        else {
            let _ = fs::write(directory.join(WITHDRAWN), "");
            return pair_error("Nobody answered on the computer. Show a new code there.");
        };
        match input.recv_timeout(DECISION_POLL.min(left)) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            // The phone hung up, or its reader is gone.
            Ok(_) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = fs::write(directory.join(WITHDRAWN), "");
                return pair_error("The phone stopped pairing.");
            }
        }
    }
}

/// The pairing's expiry, once its directory proves to be one this user made.
fn open_pairing(directory: &Path) -> Result<SystemTime, &'static str> {
    const INVALID: &str = "This pairing code isn't valid on this computer.";
    let meta = fs::symlink_metadata(directory).map_err(|_| EXPIRED)?;
    if !directory.is_absolute()
        || !meta.is_dir()
        || meta.uid() != effective_uid()
        || meta.mode() & 0o077 != 0
    {
        return Err(INVALID);
    }
    let seconds: u64 = fs::read_to_string(directory.join(EXPIRES))
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .ok_or(INVALID)?;
    if directory.join(REQUEST).exists() {
        return Err("This pairing code was already used. Show a new one on the computer.");
    }
    Ok(UNIX_EPOCH + Duration::from_secs(seconds))
}

/// Record the request for the desktop, claiming the code: a second phone
/// with the same code is refused.
fn record_request(directory: &Path, key: &PublicKey, name: &str) -> Result<(), &'static str> {
    const FAILED: &str = "The computer couldn't record the pairing request.";
    let stored = serde_json::to_vec(&StoredRequest {
        key: key.openssh(),
        name: device_name(name),
    })
    .map_err(|_| FAILED)?;
    let staged = directory.join(format!("request.{}", std::process::id()));
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staged)
        .and_then(|mut file| file.write_all(&stored))
        .map_err(|_| FAILED)?;
    let claimed = fs::hard_link(&staged, directory.join(REQUEST));
    let _ = fs::remove_file(&staged);
    claimed.map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            "This pairing code was already used. Show a new one on the computer."
        } else {
            FAILED
        }
    })
}

#[cfg(all(test, not(any(feature = "host", feature = "ui"))))]
#[path = "pairing_tests.rs"]
mod tests;
