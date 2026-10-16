//! Fence native registered admins, never deduplicated forward-locator guesses.
use crate::{
    fs_safety::{self, Root},
    repository::{
        CancellationToken, CommandSpec, GitFileLock, ProcessExecutor, RepositoryService,
        WorktreeRecord,
    },
};
use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    fs,
    io::{Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

const LIMIT: u64 = 8192;
const MAX_REGISTRATIONS: usize = 4096;

fn io(error: impl std::fmt::Display) -> String {
    error.to_string()
}
fn anchor(root: &Root) -> PathBuf {
    PathBuf::from(format!(
        "/proc/{}/fd/{}/.",
        std::process::id(),
        root.descriptor()
    ))
}
fn identity(root: &Root) -> Result<(u64, u64), String> {
    let metadata = root.metadata().map_err(io)?;
    Ok((metadata.dev(), metadata.ino()))
}
fn strip_line(bytes: &[u8]) -> &[u8] {
    bytes.strip_suffix(b"\n").unwrap_or(bytes)
}
fn path(bytes: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(bytes))
}

struct NamedRoot {
    root: Root,
    identity: (u64, u64),
}
impl NamedRoot {
    fn new(root: Root) -> Result<Self, String> {
        if fs::canonicalize(root.path()).map_err(io)? != root.path() {
            return Err(
                "Registration directory names must be canonical; repair the worktree first".into(),
            );
        }
        let identity = identity(&root)?;
        Ok(Self { root, identity })
    }
    fn check(&self) -> Result<(), String> {
        if fs::canonicalize(self.root.path()).map_err(io)? != self.root.path() {
            return Err("A registration directory name changed; repair it and retry".into());
        }
        let named = Root::open(self.root.path()).map_err(io)?;
        if identity(&named)? != self.identity || identity(&self.root)? != self.identity {
            return Err("A registered directory moved or was replaced; repair it and retry".into());
        }
        Ok(())
    }
}

struct MetadataFile {
    root: usize,
    name: OsString,
    bytes: Vec<u8>,
}
fn read_regular(root: &Root, name: &OsStr) -> Result<Vec<u8>, String> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(anchor(root).join(name))
        .map_err(io)?;
    let before = file.metadata().map_err(io)?;
    if !before.is_file() || before.len() > LIMIT {
        return Err("Registration metadata is not a bounded regular file".into());
    }
    let mut bytes = Vec::new();
    let mut file = file;
    (&mut file)
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    let after = file.metadata().map_err(io)?;
    let named = fs_safety::metadata_beneath(root, Path::new(name)).map_err(io)?;
    let version = |m: &fs::Metadata| {
        (
            m.dev(),
            m.ino(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        )
    };
    if bytes.len() > LIMIT as usize
        || bytes.contains(&0)
        || version(&before) != version(&after)
        || version(&after) != version(&named)
    {
        return Err("Registration metadata changed while being read".into());
    }
    Ok(bytes)
}

/// Retained through the native command and its synchronous policy callbacks.
/// This is a registered HEAD fence, not a certificate of descendant-family exit.
pub(crate) struct RegisteredHeads {
    roots: Vec<NamedRoot>,
    files: Vec<MetadataFile>,
    heads: Vec<usize>,
    locks: Vec<GitFileLock>,
    lock_ids: Vec<(u64, u64)>,
    namespace: Option<usize>,
    names: Vec<OsString>,
    marker_directories: Vec<(usize, usize)>,
    bindings: Vec<(usize, usize)>,
    source_admin: usize,
    source_checkout: Option<usize>,
}
impl RegisteredHeads {
    pub(crate) fn capture<E: ProcessExecutor>(
        service: &RepositoryService<E>,
        repository: &Path,
        cancel: &CancellationToken,
    ) -> Result<Self, String> {
        if !cfg!(target_os = "linux") {
            return Err(
                "Registered HEAD fencing currently requires Linux; the branch was retained".into(),
            );
        }
        let common = service.common_dir(repository, cancel).map_err(io)?;
        let common = NamedRoot::new(Root::open(&common).map_err(io)?)?;
        let records = service.list_worktrees(repository, cancel).map_err(io)?;
        let source = service.git_dir(repository, cancel).map_err(io)?;
        let mut result = Self {
            roots: vec![common],
            files: vec![],
            heads: vec![0],
            locks: vec![],
            lock_ids: vec![],
            namespace: None,
            names: vec![],
            marker_directories: vec![],
            bindings: vec![],
            source_admin: 0,
            source_checkout: None,
        };
        let common_path = result.roots[0].root.path().to_owned();
        if !service
            .uses_files_ref_storage(repository, cancel)
            .map_err(io)?
        {
            return Err(
                "Registered HEAD fencing requires Git's files reference backend; branch retained"
                    .into(),
            );
        }
        result.file(0, "HEAD")?;
        let registry = match fs_safety::open_directory_beneath(
            &result.roots[0].root,
            Path::new("worktrees"),
        ) {
            Ok(root) => Some(root),
            Err(error) if error.is_not_found() => None,
            Err(error) => return Err(io(error)),
        };
        let mut linked = HashSet::new();
        if let Some(registry) = registry {
            let index = result.root(registry)?;
            result.namespace = Some(index);
            result.names = result.namespace_names()?;
            for name in result.names.clone() {
                let admin =
                    fs_safety::open_directory_beneath(&result.roots[index].root, Path::new(&name))
                        .map_err(io)?;
                let admin = result.root(admin)?;
                result.heads.push(admin);
                let backlink = result.file(admin, "gitdir")?;
                let marker = path(strip_line(&backlink));
                let record=records.iter().find(|record| !record.bare && record.path.join(".git")==marker)
                    .ok_or("Registered admin has no matching checkout; repair or prune it before deleting branches")?;
                if !linked.insert(record.path.clone()) {
                    return Err(
                        "Multiple registered admins claim the same checkout; repair them first"
                            .into(),
                    );
                }
                let relative = result.file(admin, "commondir")?;
                let resolved = fs::canonicalize(
                    result.roots[admin]
                        .root
                        .path()
                        .join(path(strip_line(&relative))),
                )
                .map_err(io)?;
                if resolved != common_path {
                    return Err("Registered admin points to another common directory".into());
                }
                result.associate(record, admin)?;
                let head = result.file(admin, "HEAD")?;
                if let Some(branch) = &record.branch {
                    if strip_line(&head) != format!("ref: {branch}").as_bytes() {
                        return Err("Registered branch changed during its HEAD census".into());
                    }
                } else if record.detached
                    && record.head_oid.as_deref().map(str::as_bytes) != Some(strip_line(&head))
                {
                    return Err("Registered detached HEAD changed during its census".into());
                }
            }
        }
        let primary = records
            .iter()
            .filter(|record| !record.bare && !linked.contains(&record.path))
            .collect::<Vec<_>>();
        if primary.len() > 1 {
            return Err("Primary checkout registration is ambiguous".into());
        }
        for record in primary {
            // Native worktree-list can report the metadata directory itself
            // for a separate primary Git directory. Resolve only that special
            // case from common-local core.worktree, then still verify its
            // physical checkout and exact forward locator to our held common.
            let mut record = record.clone();
            if record.path == common_path {
                let configured = if source == common_path {
                    // An actual primary checkout needs no core.worktree. Ask
                    // only this invocation, whose private metadata is common;
                    // metadata-only invocations without a backlink refuse.
                    service
                        .invocation_worktree(repository, cancel)
                        .map_err(io)?
                } else {
                    service
                        .configured_primary_worktree(repository, cancel)
                        .map_err(io)?
                };
                record.path = fs::canonicalize(common_path.join(configured)).map_err(io)?;
            }
            result.associate(&record, 0)?;
        }
        result.source_admin = result
            .heads
            .iter()
            .copied()
            .find(|index| result.roots[*index].root.path() == source)
            .ok_or("Invocation does not name an actual registered administrative directory")?;
        result.source_checkout = result
            .bindings
            .iter()
            .find_map(|(admin, checkout)| (*admin == result.source_admin).then_some(*checkout));
        if result.source_checkout.is_none()
            && !records
                .iter()
                .any(|record| record.bare && record.path == common_path)
        {
            return Err("Invocation has no matching checkout or bare registration".into());
        }
        result.check()?;
        Ok(result)
    }

    fn root(&mut self, root: Root) -> Result<usize, String> {
        let index = self.roots.len();
        self.roots.push(NamedRoot::new(root)?);
        Ok(index)
    }
    fn file(&mut self, root: usize, name: &str) -> Result<Vec<u8>, String> {
        let bytes = read_regular(&self.roots[root].root, OsStr::new(name))?;
        self.files.push(MetadataFile {
            root,
            name: name.into(),
            bytes: bytes.clone(),
        });
        Ok(bytes)
    }
    fn associate(&mut self, record: &WorktreeRecord, admin: usize) -> Result<(), String> {
        let checkout = self.root(Root::open(&record.path).map_err(io)?)?;
        self.bindings.push((admin, checkout));
        let marker = fs_safety::metadata_beneath(&self.roots[checkout].root, Path::new(".git"))
            .map_err(io)?;
        if marker.is_dir() {
            let actual =
                fs_safety::open_directory_beneath(&self.roots[checkout].root, Path::new(".git"))
                    .map_err(io)?;
            if identity(&actual)? != self.roots[admin].identity {
                return Err("Checkout directory names a different registered admin".into());
            }
            self.marker_directories.push((checkout, admin));
        } else {
            let locator = self.file(checkout, ".git")?;
            let locator = strip_line(&locator)
                .strip_prefix(b"gitdir: ")
                .ok_or("Invalid checkout Git locator; repair the registration")?;
            let resolved = fs::canonicalize(record.path.join(path(locator))).map_err(io)?;
            if resolved != self.roots[admin].root.path() {
                return Err("Checkout locator and registered admin backlink disagree; run git worktree repair and retry".into());
            }
        }
        Ok(())
    }
    fn namespace_names(&self) -> Result<Vec<OsString>, String> {
        let Some(index) = self.namespace else {
            return Ok(vec![]);
        };
        let mut names = Vec::new();
        let mut bytes = 0;
        for entry in fs::read_dir(anchor(&self.roots[index].root)).map_err(io)? {
            if names.len() >= MAX_REGISTRATIONS {
                return Err("Registered admin census exceeds its safe bound".into());
            }
            let name = entry.map_err(io)?.file_name();
            bytes += name.as_bytes().len() + 1;
            if bytes > 1_048_576 {
                return Err("Registered admin names exceed their byte bound".into());
            }
            names.push(name);
        }
        names.sort();
        Ok(names)
    }
    /// Keep the invocation on the captured original private/common metadata.
    /// RegisteredHeads owns all referenced descriptors through command return.
    pub(crate) fn scope(&self, mut command: CommandSpec) -> Result<CommandSpec, String> {
        self.check()?;
        let mut arguments = vec![OsString::from(format!(
            "--git-dir={}",
            anchor(&self.roots[self.source_admin].root).display()
        ))];
        if let Some(index) = self.source_checkout {
            arguments.push(OsString::from(format!(
                "--work-tree={}",
                anchor(&self.roots[index].root).display()
            )));
        }
        command.arguments.splice(0..0, arguments);
        Ok(command.env("GIT_COMMON_DIR", anchor(&self.roots[0].root)))
    }

    pub(crate) fn acquire(&mut self) -> Result<(), String> {
        self.check()?;
        for index in &self.heads {
            let lock = GitFileLock::acquire_beneath(&self.roots[*index].root, OsStr::new("HEAD"))?;
            self.lock_ids.push(lock.retained_identity()?);
            self.locks.push(lock);
        }
        self.check()
    }
    pub(crate) fn check(&self) -> Result<(), String> {
        for root in &self.roots {
            root.check()?;
        }
        if self.namespace_names()? != self.names {
            return Err(
                "Registered admin inventory changed; retry after the worktree operation settles"
                    .into(),
            );
        }
        if self.namespace.is_none() {
            match fs_safety::metadata_beneath(&self.roots[0].root, Path::new("worktrees")) {
                Ok(_) => return Err("A worktree registration appeared during the census".into()),
                Err(error) if error.is_not_found() => {}
                Err(error) => return Err(io(error)),
            }
        }
        for file in &self.files {
            if read_regular(&self.roots[file.root].root, &file.name)? != file.bytes {
                return Err("Worktree registration changed; repair it and retry".into());
            }
        }
        for (checkout, admin) in &self.marker_directories {
            let actual =
                fs_safety::open_directory_beneath(&self.roots[*checkout].root, Path::new(".git"))
                    .map_err(io)?;
            if identity(&actual)? != self.roots[*admin].identity {
                return Err("Primary checkout marker changed".into());
            }
        }
        for lock in &self.locks {
            lock.check()?;
        }
        Ok(())
    }

    pub(crate) fn write_proof(&self, directory: &Path) -> Result<PathBuf, String> {
        self.check()?;
        let quote = |bytes: &[u8]| {
            let mut out = vec![b'\''];
            for byte in bytes {
                if *byte == b'\'' {
                    out.extend_from_slice(b"'\\''");
                } else {
                    out.push(*byte);
                }
            }
            out.push(b'\'');
            out
        };
        let mut script = b"#!/bin/sh\nset -eu\nexport LC_ALL=C\n".to_vec();
        let mut stat = |path: &Path, id: (u64, u64), directory: bool| {
            script.extend_from_slice(b"[ ! -L ");
            script.extend(quote(path.as_os_str().as_bytes()));
            script.extend_from_slice(b" ]\n");
            script.extend_from_slice(if directory { b"[ -d " } else { b"[ -f " });
            script.extend(quote(path.as_os_str().as_bytes()));
            script.extend_from_slice(b" ]\n");
            script.extend_from_slice(b"[ \"$(/usr/bin/stat -Lc '%d:%i' -- ");
            script.extend(quote(path.as_os_str().as_bytes()));
            script.extend_from_slice(b")\" = ");
            script.extend(quote(format!("{}:{}", id.0, id.1).as_bytes()));
            script.extend_from_slice(b" ]\n");
        };
        for root in &self.roots {
            stat(root.root.path(), root.identity, true);
            stat(&anchor(&root.root), root.identity, true);
        }
        for (checkout, admin) in &self.marker_directories {
            stat(
                &anchor(&self.roots[*checkout].root).join(".git"),
                self.roots[*admin].identity,
                true,
            );
        }
        for ((head, id), _) in self.heads.iter().zip(&self.lock_ids).zip(&self.locks) {
            stat(
                &anchor(&self.roots[*head].root).join("HEAD.lock"),
                *id,
                false,
            );
        }
        for (index, file) in self.files.iter().enumerate() {
            let expected = directory.join(format!("registration-{index}"));
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&expected)
                .map_err(io)?;
            output
                .write_all(&file.bytes)
                .and_then(|()| output.sync_all())
                .map_err(io)?;
            let actual = anchor(&self.roots[file.root].root).join(&file.name);
            script.extend_from_slice(b"[ ! -L ");
            script.extend(quote(actual.as_os_str().as_bytes()));
            script.extend_from_slice(b" ] && [ -f ");
            script.extend(quote(actual.as_os_str().as_bytes()));
            script.extend_from_slice(b" ]\n/usr/bin/cmp -s -- ");
            script.extend(quote(expected.as_os_str().as_bytes()));
            script.push(b' ');
            script.extend(quote(actual.as_os_str().as_bytes()));
            script.push(b'\n');
        }
        // Native policy may itself add/remove a registered checkout. Compare
        // exact byte names as well as all captured physical admin identities.
        if let Some(index) = self.namespace {
            let expected = directory.join("registration-names");
            let mut bytes = Vec::new();
            for name in &self.names {
                bytes.extend_from_slice(name.as_bytes());
                bytes.push(0);
            }
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&expected)
                .map_err(io)?
                .write_all(&bytes)
                .map_err(io)?;
            let current = directory.join("registration-names-current");
            let unsorted = directory.join("registration-names-unsorted");
            let status = directory.join("registration-names-status");
            script.extend_from_slice(b"/bin/rm -f -- ");
            script.extend(quote(status.as_os_str().as_bytes()));
            script.push(b'\n');
            script.extend_from_slice(b"( if /usr/bin/find ");
            script.extend(quote(
                anchor(&self.roots[index].root).as_os_str().as_bytes(),
            ));
            script.extend_from_slice(b" -mindepth 1 -maxdepth 1 -printf '%f\\0'; then printf 0 > ");
            script.extend(quote(status.as_os_str().as_bytes()));
            script.extend_from_slice(b"; else printf 1 > ");
            script.extend(quote(status.as_os_str().as_bytes()));
            script.extend_from_slice(b"; fi ) | /usr/bin/head -c 1048577 > ");
            script.extend(quote(unsorted.as_os_str().as_bytes()));
            script.push(b'\n');
            script.extend_from_slice(b"[ \"$(/bin/cat ");
            script.extend(quote(status.as_os_str().as_bytes()));
            script.extend_from_slice(b")\" = 0 ]\n[ \"$(/usr/bin/wc -c < ");
            script.extend(quote(unsorted.as_os_str().as_bytes()));
            script.extend_from_slice(b")\" -le 1048576 ]\n/usr/bin/sort -z ");
            script.extend(quote(unsorted.as_os_str().as_bytes()));
            script.extend_from_slice(b" > ");
            script.extend(quote(current.as_os_str().as_bytes()));
            script.extend_from_slice(b"\n/usr/bin/cmp -s -- ");
            script.extend(quote(expected.as_os_str().as_bytes()));
            script.push(b' ');
            script.extend(quote(current.as_os_str().as_bytes()));
            script.push(b'\n');
        } else {
            let namespace = anchor(&self.roots[0].root).join("worktrees");
            script.extend_from_slice(b"[ ! -e ");
            script.extend(quote(namespace.as_os_str().as_bytes()));
            script.extend_from_slice(b" ] && [ ! -L ");
            script.extend(quote(namespace.as_os_str().as_bytes()));
            script.extend_from_slice(b" ]\n");
        }
        let proof = directory.join("check-registered-heads");
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&proof)
            .map_err(io)?;
        output
            .write_all(&script)
            .and_then(|()| output.sync_all())
            .map_err(io)?;
        Ok(proof)
    }
}
