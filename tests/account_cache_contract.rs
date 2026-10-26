#![cfg(target_os = "linux")]
//! Real cache writers in separate processes; no provider or daemon is started.
use cagents::{
    account::{
        UsageSnapshot, UsageStatus,
        cache::{Cache, Catalog, Entry},
    },
    agent::Provider,
    chat::Model,
};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const TEST: &str = "independent_account_and_catalog_writers_preserve_each_others_updates";
const WRITER: &str = "CAGENTS_ACCOUNT_CACHE_CONTRACT_WRITER";
const ROOT: &str = "CAGENTS_ACCOUNT_CACHE_CONTRACT_ROOT";
const WRITERS: [&str; 5] = ["account-a", "account-b", "claude", "codex", "omp"];
const NOW: u64 = 1_000;

fn entry(label: &str) -> Entry {
    Entry {
        identity: None,
        snapshot: UsageSnapshot::state(UsageStatus::NeedsLogin, label, NOW),
    }
}

fn catalog(label: &str) -> Catalog {
    Catalog {
        models: vec![Model {
            id: label.into(),
            label: format!("Fixture {label}"),
            reasoning: vec!["low".into()],
        }],
        observed_at: NOW,
    }
}

fn write(cache: &Cache, writer: &str) {
    match writer {
        "account-a" | "account-b" => cache.store(writer, entry(writer)).unwrap(),
        "claude" => cache
            .store_catalog(Provider::Claude, catalog(writer))
            .unwrap(),
        "codex" => cache
            .store_catalog(Provider::Codex, catalog(writer))
            .unwrap(),
        "omp" => cache.store_catalog(Provider::Omp, catalog(writer)).unwrap(),
        _ => panic!("unknown private cache writer: {writer}"),
    }
}

struct Writer {
    child: Child,
    log: PathBuf,
    finished: bool,
}

impl Writer {
    fn start(root: &Path, writer: &str) -> Self {
        let log = root.join(format!("{writer}.log"));
        let output = File::create(&log).unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                TEST,
                "--nocapture",
                "--test-threads=1",
                "--color=never",
            ])
            .env(WRITER, writer)
            .env(ROOT, root)
            .stdin(Stdio::null())
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap();
        Self {
            child,
            log,
            finished: false,
        }
    }

    fn poll(&mut self) -> bool {
        assert!(
            fs::metadata(&self.log).unwrap().len() <= 64 * 1024,
            "cache writer exceeded its output limit: {}",
            self.log.display()
        );
        if !self.finished
            && let Some(status) = self.child.try_wait().unwrap()
        {
            self.finished = true;
            assert!(
                status.success(),
                "cache writer failed: {status}: {}",
                fs::read_to_string(&self.log).unwrap()
            );
            let output = fs::read_to_string(&self.log).unwrap();
            assert!(
                output.contains("1 passed; 0 failed"),
                "cache writer did not execute its exact test: {output}"
            );
        }
        self.finished
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // These exact-test children only perform synchronous cache I/O and
        // start no descendants. Always reap the original child, even on panic.
        if !self.finished {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// The kernel's waiting FLOCK records identify the process and exact inode.
/// Inspecting them proves arrival at the real cache lock, without a timed
/// assumption that a child has had enough CPU to attempt its write.
fn blocked_writers(lock: &File) -> BTreeSet<u32> {
    let metadata = lock.metadata().unwrap();
    let identity = format!(
        "{:02x}:{:02x}:{}",
        libc::major(metadata.dev()),
        libc::minor(metadata.dev()),
        metadata.ino()
    );
    fs::read_to_string("/proc/locks")
        .expect("Linux cache contract requires observable kernel lock waiters")
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            (fields.len() >= 7
                && fields[1..5] == ["->", "FLOCK", "ADVISORY", "WRITE"]
                && fields[6] == identity)
                .then(|| fields[5].parse().ok())
                .flatten()
        })
        .collect()
}

#[test]
fn independent_account_and_catalog_writers_preserve_each_others_updates() {
    if let Some(writer) = std::env::var_os(WRITER) {
        let root = std::env::var_os(ROOT).expect("private cache writer root");
        write(
            &Cache::at(PathBuf::from(root).join("cache")),
            writer.to_str().expect("ASCII private cache writer"),
        );
        return;
    }

    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("cache");
    let cache = Cache::at(&directory);
    cache.store("existing", entry("existing")).unwrap();
    let before = fs::read(directory.join("account-usage.json")).unwrap();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(directory.join("account-usage.lock"))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "taking the private cache lock: {}",
        std::io::Error::last_os_error()
    );

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut writers: Vec<_> = WRITERS
        .iter()
        .map(|writer| Writer::start(root.path(), writer))
        .collect();
    let expected: BTreeSet<_> = writers.iter().map(|writer| writer.child.id()).collect();
    loop {
        for writer in &mut writers {
            assert!(
                !writer.poll(),
                "writer completed while another process held the cache lock"
            );
        }
        if expected.is_subset(&blocked_writers(&lock)) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cache writers never all reached the held advisory lock"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        fs::read(directory.join("account-usage.json")).unwrap(),
        before,
        "a blocked writer changed the shared document"
    );
    // All five operations are now pending on the same original lock inode.
    // Removing either write path's lock makes the barrier fail; reading the
    // document before taking it loses concurrent updates in the checks below.
    drop(lock);

    loop {
        let finished = writers
            .iter_mut()
            .map(|writer| usize::from(writer.poll()))
            .sum::<usize>();
        if finished == writers.len() {
            break;
        }
        assert!(Instant::now() < deadline, "cache writers failed to settle");
        thread::sleep(Duration::from_millis(5));
    }

    let entries = cache.load(NOW);
    assert_eq!(entries.len(), 3);
    for key in ["existing", "account-a", "account-b"] {
        assert_eq!(entries.get(key), Some(&entry(key)), "lost {key}");
    }
    let catalogs = cache.catalogs(NOW);
    assert_eq!(catalogs.len(), 3);
    for (provider, label) in [
        (Provider::Claude, "claude"),
        (Provider::Codex, "codex"),
        (Provider::Omp, "omp"),
    ] {
        assert_eq!(catalogs.get(&provider), Some(&catalog(label)));
    }
    let document: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.join("account-usage.json")).unwrap()).unwrap();
    assert_eq!(document["version"], 1);
    assert_eq!(document["entries"].as_object().unwrap().len(), 3);
    assert_eq!(document["catalogs"].as_object().unwrap().len(), 3);
}
