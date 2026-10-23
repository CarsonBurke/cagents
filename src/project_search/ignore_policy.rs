//! Feed bounded, descriptor-verified ignore text to the ignore crate. Its
//! default pathname loaders can block forever on a FIFO before yielding an
//! entry, so cancellation cannot make those loaders safe.
use crate::{fs_safety::Root, process::CancellationToken};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::{
    fs::{File, FileType, OpenOptions},
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
const FILE_BYTES: usize = 256 * 1024;
const ACTIVE_BYTES: usize = 4 * 1024 * 1024;
const RULES: usize = 4096;
struct Node {
    directory: PathBuf,
    ordinary: Gitignore,
    git: Gitignore,
    exclude: Gitignore,
    has_git: bool,
    bytes: usize,
}
pub(super) struct Policy {
    root: Arc<Root>,
    cancel: CancellationToken,
    stack: Vec<Node>,
    global: Gitignore,
    include_ignored: bool,
    include_hidden: bool,
    error: Option<String>,
}
impl Policy {
    pub(super) fn new(
        root: Arc<Root>,
        cancel: CancellationToken,
        include_ignored: bool,
        include_hidden: bool,
    ) -> Result<Arc<Mutex<Self>>, String> {
        let mut policy = Self {
            root,
            cancel,
            stack: Vec::new(),
            global: Gitignore::empty(),
            include_ignored,
            include_hidden,
            error: None,
        };
        if !include_ignored {
            let global = global_path(&policy.root, &policy.cancel)?;
            if let Some(path) = global {
                policy.global = load(
                    &policy.root,
                    &path,
                    policy.root.path(),
                    &policy.cancel,
                    true,
                )?
                .0;
            }
            let directories = policy
                .root
                .path()
                .ancestors()
                .map(Path::to_path_buf)
                .collect::<Vec<_>>();
            for directory in directories.into_iter().rev() {
                let node = policy.node(directory)?;
                policy.push(node)?;
            }
        }
        Ok(Arc::new(Mutex::new(policy)))
    }
    fn push(&mut self, node: Node) -> Result<(), String> {
        if self
            .stack
            .iter()
            .map(|node| node.bytes)
            .sum::<usize>()
            .saturating_add(node.bytes)
            > ACTIVE_BYTES
        {
            return Err("Ignore rules exceed the search memory limit; simplify the rules or include ignored files.".into());
        }
        self.stack.push(node);
        Ok(())
    }
    fn node(&self, directory: PathBuf) -> Result<Node, String> {
        let (ordinary, a) = load(
            &self.root,
            &directory.join(".ignore"),
            &directory,
            &self.cancel,
            false,
        )?;
        let (git, b) = load(
            &self.root,
            &directory.join(".gitignore"),
            &directory,
            &self.cancel,
            false,
        )?;
        let marker = directory.join(".git");
        let metadata = match std::fs::symlink_metadata(&marker) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(format!(
                    "Could not inspect ignore metadata {}: {error}",
                    marker.display()
                ));
            }
        };
        let mut git_directory = None;
        if metadata.as_ref().is_some_and(|metadata| metadata.is_dir()) {
            git_directory = Some(marker.clone());
        } else if metadata.as_ref().is_some_and(|metadata| metadata.is_file()) {
            if let Some(bytes) = read(&self.root, &marker, &self.cancel, false)? {
                let text = std::str::from_utf8(&bytes)
                    .map_err(|_| format!("Invalid Git directory marker {}", marker.display()))?;
                if let Some(path) = text.trim().strip_prefix("gitdir:") {
                    let path = Path::new(path.trim());
                    git_directory = Some(if path.is_absolute() {
                        path.into()
                    } else {
                        directory.join(path)
                    });
                }
            }
        }
        let has_git = git_directory.is_some();
        let mut exclude = Gitignore::empty();
        let mut c = 0;
        if let Some(mut metadata) = git_directory {
            if let Some(bytes) = read(&self.root, &metadata.join("commondir"), &self.cancel, false)?
            {
                let path = std::str::from_utf8(&bytes)
                    .map_err(|_| "Invalid Git common-directory marker")?
                    .trim();
                let path = Path::new(path);
                metadata = if path.is_absolute() {
                    path.into()
                } else {
                    metadata.join(path)
                };
            }
            let metadata = std::fs::canonicalize(&metadata).map_err(|error| {
                format!(
                    "Could not resolve Git ignore directory {}: {error}",
                    metadata.display()
                )
            })?;
            (exclude, c) = load(
                &self.root,
                &metadata.join("info/exclude"),
                &directory,
                &self.cancel,
                false,
            )?;
        }
        Ok(Node {
            directory,
            ordinary,
            git,
            exclude,
            has_git,
            bytes: a + b + c,
        })
    }
    pub(super) fn allows(&mut self, path: &Path, kind: FileType) -> bool {
        // The caller checks cancellation/errors before enumerating each entry.
        // Also stop policy work if cancellation arrives during enumeration.
        if self.cancel.is_cancelled() || self.error.is_some() {
            return true;
        }
        if path.file_name().is_some_and(|name| name == ".git") {
            return false;
        }
        if path == self.root.path() {
            return true;
        }
        let is_dir = kind.is_dir();
        if !is_dir && !kind.is_file() {
            return false;
        }
        let ignored = if self.include_ignored {
            None
        } else {
            while self
                .stack
                .last()
                .is_some_and(|node| Some(node.directory.as_path()) != path.parent())
            {
                self.stack.pop();
            }
            // Closest rules win within each class. .ignore has precedence over
            // every .gitignore; Git rules stop at a nested repository boundary.
            let ordinary = self
                .stack
                .iter()
                .rev()
                .map(|node| node.ordinary.matched(path, is_dir))
                .find(|matched| !matched.is_none());
            if let Some(matched) = ordinary {
                Some(matched.is_ignore())
            } else {
                let git = self
                    .stack
                    .iter()
                    .rev()
                    .scan(false, |stopped, node| {
                        if *stopped {
                            return None;
                        }
                        *stopped = node.has_git;
                        Some(node.git.matched(path, is_dir))
                    })
                    .find(|matched| !matched.is_none());
                if let Some(matched) = git {
                    Some(matched.is_ignore())
                } else {
                    let exclude = self
                        .stack
                        .iter()
                        .rev()
                        .scan(false, |stopped, node| {
                            if *stopped {
                                return None;
                            }
                            *stopped = node.has_git;
                            Some(node.exclude.matched(path, is_dir))
                        })
                        .find(|matched| !matched.is_none());
                    exclude.map(|matched| matched.is_ignore()).or_else(|| {
                        let matched = self.global.matched(path, is_dir);
                        (!matched.is_none()).then(|| matched.is_ignore())
                    })
                }
            }
        };
        if ignored == Some(true)
            || (ignored.is_none()
                && !self.include_hidden
                && path
                    .file_name()
                    .is_some_and(|name| name.as_encoded_bytes().starts_with(b".")))
        {
            return false;
        }
        if is_dir && !self.include_ignored {
            match self.node(path.into()).and_then(|node| self.push(node)) {
                Ok(()) => {}
                Err(error) => {
                    self.error = Some(error);
                    return false;
                }
            }
        }
        true
    }
    pub(super) fn error(&self) -> Option<String> {
        self.error.clone()
    }
}
fn read(
    root: &Root,
    path: &Path,
    cancel: &CancellationToken,
    follow_global: bool,
) -> Result<Option<Vec<u8>>, String> {
    if cancel.is_cancelled() {
        return Err("Search cancelled".into());
    }
    let relative = (!follow_global)
        .then(|| path.strip_prefix(root.path()).ok())
        .flatten();
    let opened = if let Some(relative) = relative {
        crate::fs_safety::open_beneath(root, relative).map_err(|error| {
            if error.is_not_found() {
                None
            } else {
                Some(error.to_string())
            }
        })
    } else {
        let mut options = OpenOptions::new();
        options.read(true).custom_flags(
            libc::O_NONBLOCK | libc::O_CLOEXEC | if follow_global { 0 } else { libc::O_NOFOLLOW },
        );
        options.open(path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                None
            } else {
                Some(error.to_string())
            }
        })
    };
    let mut file: File = match opened {
        Ok(file) => file,
        Err(None) => return Ok(None),
        Err(Some(error)) => {
            return Err(format!(
                "Could not read ignore metadata {}: {error}",
                path.display()
            ));
        }
    };
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > FILE_BYTES as u64 {
        return Err(format!(
            "Ignore metadata {} must be a regular file no larger than {} KiB; include ignored files to bypass it.",
            path.display(),
            FILE_BYTES / 1024
        ));
    }
    let mut bytes = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        if cancel.is_cancelled() {
            return Err("Search cancelled".into());
        }
        let remaining = FILE_BYTES + 1 - bytes.len();
        let count = match file.read(&mut chunk[..remaining.min(8192)]) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(format!(
                    "Could not read ignore metadata {}: {error}",
                    path.display()
                ));
            }
        };
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.len() > FILE_BYTES {
            return Err(format!(
                "Ignore metadata {} grew beyond the search limit",
                path.display()
            ));
        }
    }
    Ok(Some(bytes))
}
fn load(
    root: &Root,
    path: &Path,
    directory: &Path,
    cancel: &CancellationToken,
    follow_global: bool,
) -> Result<(Gitignore, usize), String> {
    let Some(bytes) = read(root, path, cancel, follow_global)? else {
        return Ok((Gitignore::empty(), 0));
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| format!("Ignore metadata {} is not UTF-8", path.display()))?;
    let mut builder = GitignoreBuilder::new(directory);
    for (index, line) in text.lines().enumerate() {
        if cancel.is_cancelled() {
            return Err("Search cancelled".into());
        }
        if index >= RULES {
            return Err(format!(
                "Ignore metadata {} exceeds {RULES} rules",
                path.display()
            ));
        }
        // Match the crate's ordinary partial-error policy: valid lines remain
        // useful when another glob has a syntax error.
        let _ = builder.add_line(Some(path.into()), line);
    }
    builder
        .build()
        .map(|matcher| (matcher, bytes.len()))
        .map_err(|error| error.to_string())
}
fn global_path(root: &Root, cancel: &CancellationToken) -> Result<Option<PathBuf>, String> {
    #[allow(deprecated)]
    let home = std::env::home_dir();
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".config")));
    let paths = home
        .as_ref()
        .map(|home| home.join(".gitconfig"))
        .into_iter()
        .chain(config.as_ref().map(|path| path.join("git/config")));
    // Preserve the ignore crate's HOME-then-XDG discovery precedence, while
    // reading only a bounded regular descriptor instead of pathname File::open.
    for path in paths {
        let Some(bytes) = read(root, &path, cancel, true)? else {
            continue;
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        static EXCLUDES: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r#"(?im)^\s*excludesfile\s*=\s*"?\s*(\S+?)\s*"?\s*$"#)
                .expect("static excludesFile expression")
        });
        if let Some(value) = EXCLUDES.captures(text).and_then(|captures| captures.get(1)) {
            let value = value.as_str();
            return Ok(Some(if let Some(relative) = value.strip_prefix("~/") {
                home.as_ref()
                    .map(|home| home.join(relative))
                    .unwrap_or_else(|| value.into())
            } else {
                value.into()
            }));
        }
    }
    Ok(config.map(|config| config.join("git/ignore")))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancelled_filter_returns_control_before_visiting_all_filtered_siblings() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join(".ignore"), "*\n").unwrap();
        for index in 0..32 {
            std::fs::write(
                directory.path().join(format!("ignored-{index}")),
                "needle\n",
            )
            .unwrap();
        }
        let root = Arc::new(Root::open(directory.path()).unwrap());
        let cancel = CancellationToken::default();
        let policy = Policy::new(root, cancel.clone(), false, true).unwrap();
        let seen = Arc::new(AtomicUsize::new(0));
        let visits = seen.clone();
        let token = cancel.clone();
        let mut walker = ignore::WalkBuilder::new(directory.path());
        walker.standard_filters(false).filter_entry(move |entry| {
            if entry.depth() > 0 {
                visits.fetch_add(1, Ordering::Relaxed);
                token.cancel();
            }
            policy
                .lock()
                .unwrap()
                .allows(entry.path(), entry.file_type().unwrap())
        });
        for entry in walker.build() {
            // Same admission ordering as the actual search worker: canceled
            // control entries can never reach match/file processing.
            if cancel.is_cancelled() {
                break;
            }
            assert_eq!(entry.unwrap().depth(), 0);
        }
        assert_eq!(
            seen.load(Ordering::Relaxed),
            1,
            "filter must yield the first canceled child, not enumerate every canceled sibling"
        );
    }
    #[test]
    fn ignore_metadata_reads_bound_regular_growth_and_refuse_special_global_sources() {
        use std::os::unix::ffi::OsStrExt;
        let fixture = tempfile::tempdir().unwrap();
        let root = Root::open(fixture.path()).unwrap();
        let path = fixture.path().join("fifo");
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: the temporary path is a valid NUL-terminated string.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        for global in [false, true] {
            assert!(
                read(&root, &path, &CancellationToken::default(), global)
                    .unwrap_err()
                    .contains("regular file")
            );
        }
        let oversized = fixture.path().join("oversized");
        std::fs::write(&oversized, vec![b'x'; FILE_BYTES + 1]).unwrap();
        assert!(
            read(&root, &oversized, &CancellationToken::default(), false)
                .unwrap_err()
                .contains("regular file")
        );
        let token = CancellationToken::default();
        token.cancel();
        assert!(
            read(&root, &path, &token, false)
                .unwrap_err()
                .contains("cancelled")
        );
    }
}
