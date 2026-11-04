//! Bounded depth-first traversal without an unbounded ancestor prefetch.
use std::{
    collections::LinkedList,
    fs::{FileType, ReadDir},
    io,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy)]
pub(super) struct Limits {
    pub(super) open: usize,
    pub(super) buffered_entries: usize,
    pub(super) buffered_path_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            open: 10,
            buffered_entries: 4096,
            buffered_path_bytes: 1024 * 1024,
        }
    }
}

pub(super) fn descriptor_exhausted(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::EMFILE | libc::ENFILE))
}

#[derive(Debug)]
pub(super) enum Stop {
    Cancelled,
    Resource(String),
}

impl Stop {
    pub(super) fn message(self) -> Option<String> {
        match self {
            Self::Cancelled => None,
            Self::Resource(reason) => Some(format!(
                "Search stopped before every folder could be checked. Existing results are incomplete. {reason}"
            )),
        }
    }
}

/// DirEntry can retain its originating directory descriptor on Linux. Keep
/// only these independently owned values, including while scanning a file.
pub(super) struct Entry {
    pub(super) path: PathBuf,
    pub(super) kind: FileType,
}

type Buffered = LinkedList<io::Result<Entry>>;

enum Frame {
    Open { directory: ReadDir, parent: PathBuf },
    Buffered(Buffered),
}

pub(super) struct Traversal {
    frames: Vec<Frame>,
    open: usize,
    buffered_entries: usize,
    buffered_path_bytes: usize,
    limits: Limits,
}

impl Traversal {
    pub(super) fn new(limits: Limits) -> Self {
        assert!(limits.open > 0);
        Self {
            frames: Vec::new(),
            open: 0,
            buffered_entries: 0,
            buffered_path_bytes: 0,
            limits,
        }
    }

    fn clear(&mut self) {
        self.frames.clear();
        self.open = 0;
        self.buffered_entries = 0;
        self.buffered_path_bytes = 0;
    }

    fn stop<T>(&mut self, stop: Stop) -> Result<T, Stop> {
        self.clear();
        Err(stop)
    }

    fn poll(&mut self, cancelled: &mut impl FnMut() -> bool) -> Result<(), Stop> {
        if cancelled() {
            self.stop(Stop::Cancelled)
        } else {
            Ok(())
        }
    }

    fn entry(
        entry: io::Result<std::fs::DirEntry>,
        parent: &Path,
        cancelled: &mut impl FnMut() -> bool,
    ) -> Result<io::Result<Entry>, Stop> {
        let result = match entry {
            Ok(entry) => {
                if cancelled() {
                    return Err(Stop::Cancelled);
                }
                entry.file_type().map(|kind| Entry {
                    // The iterator was opened through /proc. Keep only the
                    // logical path beneath the selected root outside it.
                    path: parent.join(entry.file_name()),
                    kind,
                })
            }
            Err(error) => Err(error),
        };
        match result {
            Err(error) if descriptor_exhausted(&error) => {
                Err(Stop::Resource(format!("Directory descriptors: {error}")))
            }
            result => Ok(result),
        }
    }

    fn path_bytes(entry: &io::Result<Entry>) -> usize {
        entry.as_ref().map_or(0, |entry| entry.path.capacity())
    }

    fn spill_oldest(&mut self, cancelled: &mut impl FnMut() -> bool) -> Result<(), Stop> {
        let index = self
            .frames
            .iter()
            .position(|frame| matches!(frame, Frame::Open { .. }))
            .expect("the open-descriptor limit has an open frame");
        let Frame::Open {
            mut directory,
            parent,
        } = std::mem::replace(&mut self.frames[index], Frame::Buffered(Buffered::new()))
        else {
            unreachable!()
        };
        self.open -= 1;
        let mut buffered = Buffered::new();
        loop {
            self.poll(cancelled)?;
            let Some(entry) = directory.next() else {
                break;
            };
            let entry = match Self::entry(entry, &parent, cancelled) {
                Ok(entry) => entry,
                Err(stop) => return self.stop(stop),
            };
            let bytes = Self::path_bytes(&entry);
            if self.buffered_entries >= self.limits.buffered_entries
                || bytes
                    > self
                        .limits
                        .buffered_path_bytes
                        .saturating_sub(self.buffered_path_bytes)
            {
                return self.stop(Stop::Resource(
                    "The bounded directory-entry buffer is full.".into(),
                ));
            }
            self.buffered_entries += 1;
            self.buffered_path_bytes += bytes;
            // One node per entry is released on pop, with no spare vector
            // capacities retained by partially consumed ancestor queues. The
            // limits count records and PathBuf capacities, not allocator RSS.
            buffered.push_back(entry);
        }
        // Close the evicted descriptor before opening the descendant. On any
        // early return the local iterator/buffer also drop before the caller.
        drop(directory);
        self.frames[index] = Frame::Buffered(buffered);
        Ok(())
    }

    pub(super) fn descend(
        &mut self,
        root: &crate::fs_safety::Root,
        path: &Path,
        cancelled: &mut impl FnMut() -> bool,
    ) -> Result<io::Result<()>, Stop> {
        self.poll(cancelled)?;
        if self.open == self.limits.open {
            self.spill_oldest(cancelled)?;
        }
        self.poll(cancelled)?;
        let Ok(relative) = path.strip_prefix(root.path()) else {
            return Ok(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Search directory is outside its selected root",
            )));
        };
        let opened = if relative.as_os_str().is_empty() {
            // Root is already retained. Avoid an extra clone descriptor so
            // the root iterator still needs only its own new descriptor.
            std::fs::read_dir(root.process_path())
        } else {
            let directory = match crate::fs_safety::open_directory_beneath(root, relative) {
                Ok(directory) => directory,
                Err(error) if error.is_resource_exhausted() => {
                    return self.stop(Stop::Resource(format!("Directory descriptors: {error}")));
                }
                Err(error) => return Ok(Err(io::Error::other(error))),
            };
            self.poll(cancelled)?;
            // ReadDir owns its new descriptor before this temporary retained
            // directory drops. Never reopen a replaceable directory pathname.
            std::fs::read_dir(directory.process_path())
        };
        match opened {
            Ok(directory) => {
                self.frames.push(Frame::Open {
                    directory,
                    parent: path.to_path_buf(),
                });
                self.open += 1;
                Ok(Ok(()))
            }
            Err(error) if descriptor_exhausted(&error) => {
                self.stop(Stop::Resource(format!("Directory descriptors: {error}")))
            }
            Err(error) => Ok(Err(error)),
        }
    }

    #[cfg(test)]
    pub(super) fn open_directories(&self) -> usize {
        self.open
    }

    pub(super) fn next(
        &mut self,
        cancelled: &mut impl FnMut() -> bool,
    ) -> Result<Option<io::Result<Entry>>, Stop> {
        loop {
            self.poll(cancelled)?;
            match self.frames.last_mut() {
                Some(Frame::Open { directory, parent }) => match directory.next() {
                    Some(entry) => {
                        return match Self::entry(entry, parent, cancelled) {
                            Ok(entry) => Ok(Some(entry)),
                            Err(stop) => self.stop(stop),
                        };
                    }
                    None => {
                        self.frames.pop();
                        self.open -= 1;
                    }
                },
                Some(Frame::Buffered(buffered)) => match buffered.pop_front() {
                    Some(entry) => {
                        self.buffered_entries -= 1;
                        self.buffered_path_bytes -= Self::path_bytes(&entry);
                        return Ok(Some(entry));
                    }
                    None => {
                        self.frames.pop();
                    }
                },
                None => return Ok(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::CancellationToken;
    use crate::project_search::{Options, Progress, search, search_with_limits};
    use std::{
        fs::File,
        process::Command,
        time::{Duration, Instant},
    };

    fn branching_fixture() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        for first in 0..3 {
            for second in 0..3 {
                std::fs::create_dir_all(directory.path().join(format!("{first}/{second}")))
                    .unwrap();
            }
        }
        directory
    }

    fn empty(traversal: &Traversal) {
        assert!(traversal.frames.is_empty());
        assert_eq!(traversal.open, 0);
        assert_eq!(traversal.buffered_entries, 0);
        assert_eq!(traversal.buffered_path_bytes, 0);
    }

    #[test]
    fn spill_budgets_are_global_and_cancellation_releases_partial_buffers() {
        let directory = branching_fixture();
        let root = crate::fs_safety::Root::open(directory.path()).unwrap();
        let mut traversal = Traversal::new(Limits {
            open: 1,
            buffered_entries: 2,
            buffered_path_bytes: usize::MAX,
        });
        let mut running = || false;
        traversal
            .descend(&root, directory.path(), &mut running)
            .ok()
            .unwrap()
            .unwrap();
        let first = traversal.next(&mut running).ok().unwrap().unwrap().unwrap();
        traversal
            .descend(&root, &first.path, &mut running)
            .ok()
            .unwrap()
            .unwrap();
        assert_eq!(traversal.buffered_entries, 2);
        let next = traversal.next(&mut running).ok().unwrap().unwrap().unwrap();
        assert!(matches!(
            traversal.descend(&root, &next.path, &mut running),
            Err(Stop::Resource(_))
        ));
        empty(&traversal);

        let mut traversal = Traversal::new(Limits {
            open: 1,
            buffered_entries: 10,
            buffered_path_bytes: 1,
        });
        traversal
            .descend(&root, directory.path(), &mut running)
            .ok()
            .unwrap()
            .unwrap();
        assert!(matches!(
            traversal.descend(&root, &directory.path().join("0"), &mut running),
            Err(Stop::Resource(_))
        ));
        empty(&traversal);

        let mut traversal = Traversal::new(Limits {
            open: 1,
            ..Limits::default()
        });
        traversal
            .descend(&root, directory.path(), &mut running)
            .ok()
            .unwrap()
            .unwrap();
        let mut polls = 0;
        // Two entries have been converted to owned values when this poll
        // cancels before the next read_dir iteration. No worker thread/race.
        let mut cancel = || {
            polls += 1;
            polls == 6
        };
        assert!(matches!(
            traversal.descend(&root, &directory.path().join("0"), &mut cancel),
            Err(Stop::Cancelled)
        ));
        assert_eq!(polls, 6);
        empty(&traversal);
        // Ordinary permission/absence failures retain the caller's skipped
        // accounting, rather than becoming a resource error.
        assert!(matches!(
            traversal.descend(&root, &directory.path().join("absent"), &mut running),
            Ok(Err(_))
        ));
        empty(&traversal);
    }

    struct RestoreLimit(libc::rlimit);
    impl RestoreLimit {
        fn lower() -> Self {
            let mut original = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: this runs only in the exact isolated same-ELF child;
            // both pointers refer to initialized, live rlimit values.
            assert_eq!(
                unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut original) },
                0
            );
            assert!(
                original.rlim_cur >= 64,
                "the isolated fixture needs a soft limit of at least64"
            );
            let limit = libc::rlimit {
                rlim_cur: 64,
                rlim_max: original.rlim_max,
            };
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
            Self(original)
        }
    }
    impl Drop for RestoreLimit {
        fn drop(&mut self) {
            // SAFETY: restore only this child process's saved soft limit.
            let restored = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &self.0) };
            assert_eq!(restored, 0);
        }
    }

    fn descriptor_capacity() -> usize {
        // A finite probe of this isolated child's own64-slot table. No proc
        // census, global limit change, runtime/helper/daemon or live user state.
        let files = fill_descriptors();
        files.len()
    }

    fn fill_descriptors() -> Vec<File> {
        let mut files = Vec::new();
        for _ in 0..64 {
            match File::open("/dev/null") {
                Ok(file) => files.push(file),
                Err(error) => {
                    assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
                    return files;
                }
            }
        }
        panic!("the private child's64-descriptor limit was not installed");
    }

    fn child_cases() {
        // All fixture creation precedes the lower soft limit.
        let deep = tempfile::tempdir().unwrap();
        let mut relative = PathBuf::new();
        for _ in 0..96 {
            relative.push("d");
        }
        std::fs::create_dir_all(deep.path().join(&relative)).unwrap();
        let target = relative.join("hit.txt");
        std::fs::write(deep.path().join(&target), "λ needle\n").unwrap();
        let wide = branching_fixture();
        let files = tempfile::tempdir().unwrap();
        for name in ["one", "two"] {
            std::fs::write(files.path().join(name), "needle\n".repeat(64)).unwrap();
        }
        let stream = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(stream.path().join(&relative)).unwrap();
        std::fs::write(stream.path().join(&target), "λ needle\n".repeat(192)).unwrap();
        let _limit = RestoreLimit::lower();
        let available = descriptor_capacity();
        assert!(
            available >= 32,
            "the child must retain room for ordinary search handles"
        );
        let options = Options {
            query: "needle".into(),
            include_ignored: true,
            ..Options::default()
        };
        let mut found = Vec::new();
        let mut finished = false;
        search(
            deep.path(),
            &options,
            &CancellationToken::default(),
            |event| {
                match event {
                    Progress::Batch(batch) => found.extend(batch),
                    Progress::Finished { capped, skipped } => {
                        assert!(!capped);
                        assert_eq!(skipped, 0);
                        finished = true;
                    }
                    Progress::Error(error) => panic!("deep narrow search must fit: {error}"),
                }
                true
            },
        );
        assert!(finished);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, target);
        assert_eq!((found[0].line, found[0].column, found[0].length), (0, 3, 6));
        assert!(found[0].line_identity.matches("λ needle"));
        assert_eq!(descriptor_capacity(), available);

        for (entries, path_bytes) in [(1, usize::MAX), (10, 1)] {
            let mut events = Vec::new();
            search_with_limits(
                wide.path(),
                &options,
                &CancellationToken::default(),
                |event| {
                    assert!(
                        matches!(&event, Progress::Error(message) if message.contains("incomplete") && message.contains("buffer")),
                        "{event:?}"
                    );
                    // Only the independently retained search Root descriptor may
                    // remain; all active/evicted iterator owners are already gone.
                    assert_eq!(descriptor_capacity(), available - 1);
                    events.push(event);
                    true
                },
                Limits {
                    open: 1,
                    buffered_entries: entries,
                    buffered_path_bytes: path_bytes,
                },
            );
            assert_eq!(events.len(), 1);
            assert_eq!(descriptor_capacity(), available);
        }

        // Leave exactly two slots for Root and the root ReadDir. The actual
        // confined file open must then return EMFILE; this does not depend on
        // a timed flush or a concurrent descriptor race.
        let mut pressure = fill_descriptors();
        assert!(pressure.pop().is_some());
        assert!(pressure.pop().is_some());
        let mut failed = false;
        search(
            files.path(),
            &options,
            &CancellationToken::default(),
            |event| {
                match event {
                    Progress::Batch(_) => panic!("the confined file open has no free descriptor"),
                    Progress::Error(message) => {
                        assert!(
                            message.contains("incomplete") && message.contains("File descriptors"),
                            "{message}"
                        );
                        pressure.clear();
                        assert_eq!(descriptor_capacity(), available - 1);
                        failed = true;
                    }
                    Progress::Finished { .. } => {
                        panic!("EMFILE cannot be reported as successful skipped files")
                    }
                }
                true
            },
        );
        assert!(failed);
        assert_eq!(descriptor_capacity(), available);

        for cancel in [false, true] {
            let token = CancellationToken::default();
            let mut events = Vec::new();
            search(stream.path(), &options, &token, |event| {
                assert!(
                    matches!(&event, Progress::Batch(batch) if !batch.is_empty() && batch.len() <= 64)
                );
                events.push(event);
                if cancel {
                    token.cancel();
                }
                cancel
            });
            assert_eq!(
                events.len(),
                1,
                "refusal/cancellation cannot publish again or finish"
            );
            assert_eq!(descriptor_capacity(), available);
        }
    }

    #[test]
    fn deep_search_and_resource_failures_release_descriptors_with_a_low_child_limit() {
        const CHILD: &str = "CAGENTS_SEARCH_FD_BUDGET_CHILD";
        const TEST: &str = "project_search::traversal::tests::deep_search_and_resource_failures_release_descriptors_with_a_low_child_limit";
        if std::env::var_os(CHILD).is_some() {
            child_cases();
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let stdout = directory.path().join("stdout");
        let stderr = directory.path().join("stderr");
        struct OwnedChild(std::process::Child);
        impl Drop for OwnedChild {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", TEST, "--nocapture", "--test-threads=1"]);
        // Keep inherited caller authority while isolating the filesystem fixture.
        for (name, _) in std::env::vars_os() {
            if name != std::ffi::OsStr::new(crate::agent_execution::AUTHORITY_ENV) {
                command.env_remove(name);
            }
        }
        let mut child = OwnedChild(
            command
                .env(CHILD, "1")
                .env("HOME", directory.path())
                .current_dir(directory.path())
                .stdout(File::create(&stdout).unwrap())
                .stderr(File::create(&stderr).unwrap())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut timed_out = false;
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                timed_out = true;
                child.0.kill().unwrap();
                break child.0.wait().unwrap();
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let stdout = std::fs::read_to_string(stdout).unwrap();
        let stderr = std::fs::read_to_string(stderr).unwrap();
        assert!(!timed_out && status.success()
            && stdout.lines().any(|line| line == format!("test {TEST} ... ok"))
            && stdout.lines().any(|line| line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;")),
            "isolated child must execute exactly its owning test: {status}\n{stdout}\n{stderr}");
        print!(
            "BEGIN isolated search FD child [{TEST}]\n{stdout}\nEND isolated search FD child [{TEST}]\n"
        );
        eprint!(
            "BEGIN isolated search FD child stderr [{TEST}]\n{stderr}\nEND isolated search FD child stderr [{TEST}]\n"
        );
    }
}
