//! Downloads a model, resuming what an earlier attempt left and verifying
//! every byte against the catalog before anything can load it.

use std::{
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};

use crate::{
    catalog::{Model, ModelFile},
    protocol::{InstallError, InstallEvent},
    store::{self, LeaseError},
};

type Failure = (InstallError, String);

/// Room to leave on the disk beyond the model itself.
const SPARE_BYTES: u64 = 64 << 20;

/// How often progress is reported.
const PROGRESS_EVERY: Duration = Duration::from_millis(200);

/// Fetches one URL from `offset` on. Ureq in the helper; a table in tests.
pub trait Fetch {
    /// The body from `offset`, and whether the server honoured the range;
    /// if not, the body starts at 0.
    fn fetch(&self, url: &str, offset: u64) -> Result<(Box<dyn Read>, bool), String>;
}

/// HTTPS through ureq, with timeouts that notice a stalled connection but
/// let a slow one finish.
pub struct Https(ureq::Agent);

impl Default for Https {
    fn default() -> Self {
        Self(
            ureq::Agent::config_builder()
                .timeout_connect(Some(Duration::from_secs(30)))
                .timeout_recv_response(Some(Duration::from_secs(60)))
                .timeout_per_read(Some(Duration::from_secs(60)))
                .user_agent(concat!("cagents-dictate/", env!("CARGO_PKG_VERSION")))
                .build()
                .into(),
        )
    }
}

impl Fetch for Https {
    fn fetch(&self, url: &str, offset: u64) -> Result<(Box<dyn Read>, bool), String> {
        let mut request = self.0.get(url);
        if offset > 0 {
            request = request.header("Range", format!("bytes={offset}-"));
        }
        let response = request.call().map_err(|error| error.to_string())?;
        let ranged = response.status().as_u16() == 206;
        Ok((Box::new(response.into_body().into_reader()), ranged))
    }
}

/// Installs `model` under `root`, reporting progress, and returns once every
/// file is verified and the manifest written.
pub fn install(
    root: &Path,
    model: &'static Model,
    fetch: &dyn Fetch,
    report: &mut dyn FnMut(InstallEvent),
) -> Result<(), Failure> {
    let _lease = store::lease(root, model).map_err(|error| match error {
        LeaseError::Busy => (
            InstallError::Busy,
            format!("{} is being installed or removed elsewhere", model.name),
        ),
        LeaseError::Io(error) => storage(root, &error),
    })?;
    let total = model.size();
    if store::status(root, model) == store::Status::Installed {
        report(InstallEvent::Progress { done: total, total });
        report(InstallEvent::Installed {
            model: model.id.to_owned(),
        });
        return Ok(());
    }
    let directory = store::directory(root, model);
    fs::create_dir_all(&directory).map_err(|error| storage(&directory, &error))?;

    let finished = |file: &ModelFile| {
        fs::metadata(directory.join(file.name)).is_ok_and(|meta| meta.len() == file.size)
    };
    // What each file already has on disk, whole or in part.
    let kept_by_file: Vec<u64> = model
        .files
        .iter()
        .map(|file| {
            if finished(file) {
                file.size
            } else {
                fs::metadata(store::partial(&directory, file.name))
                    .map_or(0, |meta| meta.len().min(file.size))
            }
        })
        .collect();
    let kept: u64 = kept_by_file.iter().sum();
    let needed = total - kept + SPARE_BYTES;
    if let Some(free) = free_space(&directory)
        && free < needed
    {
        return Err((
            InstallError::NoSpace,
            format!(
                "{} needs {} MB free on {} and {} MB are",
                model.name,
                needed >> 20,
                directory.display(),
                free >> 20
            ),
        ));
    }

    // Progress counts every file's kept bytes from the start, so resuming
    // never moves it backwards.
    let mut done = kept;
    let mut reported = Instant::now();
    report(InstallEvent::Progress { done, total });
    for (file, file_kept) in model.files.iter().zip(kept_by_file) {
        let before = done - file_kept;
        if !finished(file) {
            download(&directory, file, fetch, &mut |file_done| {
                if reported.elapsed() >= PROGRESS_EVERY {
                    reported = Instant::now();
                    report(InstallEvent::Progress {
                        done: before + file_done,
                        total,
                    });
                }
            })?;
        }
        done = before + file.size;
        report(InstallEvent::Progress { done, total });
    }

    let manifest = directory.join(store::MANIFEST);
    let staged = directory.join(".installed.json.new");
    fs::write(&staged, store::manifest(model))
        .and_then(|()| File::open(&staged)?.sync_all())
        .and_then(|()| fs::rename(&staged, &manifest))
        .and_then(|()| sync_directory(&directory))
        .map_err(|error| storage(&manifest, &error))?;
    report(InstallEvent::Installed {
        model: model.id.to_owned(),
    });
    Ok(())
}

/// Downloads `file` into `directory` through its `.part` file, resuming
/// what is there. `progress` hears the bytes of this file on disk.
fn download(
    directory: &Path,
    file: &ModelFile,
    fetch: &dyn Fetch,
    progress: &mut dyn FnMut(u64),
) -> Result<(), Failure> {
    let partial = store::partial(directory, file.name);
    let mut part = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&partial)
        .map_err(|error| storage(&partial, &error))?;
    let mut have = part.metadata().map_err(|error| storage(&partial, &error))?.len();
    if have > file.size {
        have = 0;
    }
    // What is kept is hashed again: the checksum covers the whole file.
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 16];
    if have > 0 {
        let mut kept = (&mut part).take(have);
        loop {
            let read = kept
                .read(&mut buffer)
                .map_err(|error| storage(&partial, &error))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
    }
    let mut body = if have < file.size {
        let (body, ranged) = fetch
            .fetch(file.url, have)
            .map_err(|error| (InstallError::Network, format!("{}: {error}", file.name)))?;
        if !ranged {
            have = 0;
            hasher = Sha256::new();
        }
        Some(body)
    } else {
        None
    };
    part.set_len(have)
        .and_then(|()| part.seek(SeekFrom::Start(have)).map(drop))
        .map_err(|error| storage(&partial, &error))?;
    progress(have);
    if let Some(body) = &mut body {
        loop {
            let read = body.read(&mut buffer).map_err(|error| {
                (
                    InstallError::Network,
                    format!("{} stopped downloading: {error}", file.name),
                )
            })?;
            if read == 0 {
                break;
            }
            have += read as u64;
            if have > file.size {
                break;
            }
            hasher.update(&buffer[..read]);
            part.write_all(&buffer[..read])
                .map_err(|error| storage(&partial, &error))?;
            progress(have);
        }
    }
    if have != file.size || format!("{:x}", hasher.finalize()) != file.sha256 {
        if have < file.size {
            return Err((
                InstallError::Network,
                format!("{} was cut off after {have} bytes", file.name),
            ));
        }
        // A bad file must not be resumed: start it again next time.
        drop(part);
        let _ = fs::remove_file(&partial);
        return Err((
            InstallError::Corrupt,
            format!("{} does not match its published checksum", file.name),
        ));
    }
    let target = directory.join(file.name);
    part.sync_all()
        .and_then(|()| fs::rename(&partial, &target))
        .and_then(|()| sync_directory(directory))
        .map_err(|error| storage(&target, &error))
}

fn storage(path: &Path, error: &io::Error) -> Failure {
    let kind = if error.kind() == io::ErrorKind::StorageFull {
        InstallError::NoSpace
    } else {
        InstallError::Storage
    };
    (kind, format!("{}: {error}", path.display()))
}

fn sync_directory(directory: &Path) -> io::Result<()> {
    File::open(directory)?.sync_all()
}

/// Bytes an unprivileged process may still write where `path` is.
fn free_space(path: &Path) -> Option<u64> {
    let stat = rustix::fs::statvfs(path).ok()?;
    Some(stat.f_bavail.saturating_mul(stat.f_frsize))
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::HashMap};

    use super::*;
    use crate::catalog::Architecture;

    /// Serves fixed bodies, honouring ranges unless told otherwise, and
    /// records each request.
    struct Table {
        bodies: HashMap<&'static str, Vec<u8>>,
        ranges: bool,
        /// Stop each response after this many bytes.
        cut: Option<usize>,
        requests: RefCell<Vec<(String, u64)>>,
    }

    impl Fetch for Table {
        fn fetch(&self, url: &str, offset: u64) -> Result<(Box<dyn Read>, bool), String> {
            self.requests.borrow_mut().push((url.to_owned(), offset));
            let body = self.bodies.get(url).ok_or("404")?;
            let start = if self.ranges { offset as usize } else { 0 };
            let mut body = body[start..].to_vec();
            if let Some(cut) = self.cut {
                body.truncate(cut);
            }
            Ok((Box::new(io::Cursor::new(body)), self.ranges))
        }
    }

    fn sha256(bytes: &[u8]) -> &'static str {
        Box::leak(format!("{:x}", Sha256::digest(bytes)).into_boxed_str())
    }

    fn model(contents: &[(&'static str, &[u8])]) -> &'static Model {
        let files: Vec<ModelFile> = contents
            .iter()
            .map(|&(name, bytes)| ModelFile {
                name,
                url: Box::leak(format!("https://models.test/{name}").into_boxed_str()),
                size: bytes.len() as u64,
                sha256: sha256(bytes),
            })
            .collect();
        Box::leak(Box::new(Model {
            id: "test-model",
            revision: "0123456789abcdef0123456789abcdef01234567",
            name: "Test model",
            languages: "English",
            attribution: "",
            architecture: Architecture::OfflineTransducer {
                encoder: "a",
                decoder: "a",
                joiner: "a",
                tokens: "a",
                vad: "a",
            },
            files: Box::leak(files.into_boxed_slice()),
        }))
    }

    fn table(model: &Model, contents: &[(&'static str, &[u8])]) -> Table {
        Table {
            bodies: model
                .files
                .iter()
                .zip(contents)
                .map(|(file, (_, bytes))| (file.url, bytes.to_vec()))
                .collect(),
            ranges: true,
            cut: None,
            requests: RefCell::default(),
        }
    }

    #[test]
    fn an_interrupted_install_resumes_where_it_stopped() {
        let a = vec![7u8; 100_000];
        let contents: &[(&'static str, &[u8])] = &[("a", &a), ("b", b"tokens")];
        let model = model(contents);
        let root = tempfile::tempdir().unwrap();
        let mut fetch = table(model, contents);
        fetch.cut = Some(30_000);
        let mut events = Vec::new();
        let error = install(root.path(), model, &fetch, &mut |event| events.push(event))
            .unwrap_err();
        assert_eq!(error.0, InstallError::Network);
        assert!(matches!(
            store::status(root.path(), model),
            store::Status::Partial { downloaded: 30_000 }
        ));

        fetch.cut = None;
        fetch.requests.borrow_mut().clear();
        events.clear();
        install(root.path(), model, &fetch, &mut |event| events.push(event)).unwrap();
        assert_eq!(
            *fetch.requests.borrow(),
            vec![
                ("https://models.test/a".to_owned(), 30_000),
                ("https://models.test/b".to_owned(), 0),
            ]
        );
        assert_eq!(store::status(root.path(), model), store::Status::Installed);
        let directory = store::directory(root.path(), model);
        assert_eq!(fs::read(directory.join("a")).unwrap(), a);
        assert_eq!(
            events.last(),
            Some(&InstallEvent::Installed {
                model: "test-model".to_owned()
            })
        );
        assert!(events.contains(&InstallEvent::Progress {
            done: model.size(),
            total: model.size()
        }));

        // Installed is installed: nothing more is fetched.
        fetch.requests.borrow_mut().clear();
        install(root.path(), model, &fetch, &mut |_| {}).unwrap();
        assert!(fetch.requests.borrow().is_empty());
    }

    #[test]
    fn progress_after_a_resume_never_goes_back() {
        let b = vec![5u8; 100_000];
        let contents: &[(&'static str, &[u8])] = &[("a", b"tokens"), ("b", &b)];
        let model = model(contents);
        let root = tempfile::tempdir().unwrap();
        let mut fetch = table(model, contents);
        fetch.cut = Some(30_000);
        install(root.path(), model, &fetch, &mut |_| {}).unwrap_err();
        fetch.cut = None;
        let mut progress = Vec::new();
        install(root.path(), model, &fetch, &mut |event| {
            if let InstallEvent::Progress { done, .. } = event {
                progress.push(done);
            }
        })
        .unwrap();
        assert_eq!(progress.first(), Some(&30_006));
        assert_eq!(progress.last(), Some(&model.size()));
        assert!(progress.is_sorted(), "{progress:?}");
    }

    #[test]
    fn a_server_that_ignores_the_range_starts_the_file_again() {
        let a = vec![3u8; 50_000];
        let contents: &[(&'static str, &[u8])] = &[("a", &a)];
        let model = model(contents);
        let root = tempfile::tempdir().unwrap();
        let mut fetch = table(model, contents);
        fetch.cut = Some(10_000);
        install(root.path(), model, &fetch, &mut |_| {}).unwrap_err();
        fetch.cut = None;
        fetch.ranges = false;
        install(root.path(), model, &fetch, &mut |_| {}).unwrap();
        let directory = store::directory(root.path(), model);
        assert_eq!(fs::read(directory.join("a")).unwrap(), a);
    }

    #[test]
    fn a_file_that_fails_its_checksum_is_discarded_and_never_installed() {
        let contents: &[(&'static str, &[u8])] = &[("a", b"expected")];
        let model = model(contents);
        let root = tempfile::tempdir().unwrap();
        let mut fetch = table(model, contents);
        fetch
            .bodies
            .insert(model.files[0].url, b"tampered".to_vec());
        let error = install(root.path(), model, &fetch, &mut |_| {}).unwrap_err();
        assert_eq!(error.0, InstallError::Corrupt);
        assert_eq!(store::status(root.path(), model), store::Status::Absent);
        // A longer body than published is refused too.
        fetch
            .bodies
            .insert(model.files[0].url, b"expected and more".to_vec());
        let error = install(root.path(), model, &fetch, &mut |_| {}).unwrap_err();
        assert_eq!(error.0, InstallError::Corrupt);
        assert_eq!(store::status(root.path(), model), store::Status::Absent);
    }

    #[test]
    fn a_second_install_of_the_same_model_is_refused_while_one_runs() {
        let contents: &[(&'static str, &[u8])] = &[("a", b"x")];
        let model = model(contents);
        let root = tempfile::tempdir().unwrap();
        let _held = store::lease(root.path(), model).unwrap();
        let error = install(root.path(), model, &table(model, contents), &mut |_| {})
            .unwrap_err();
        assert_eq!(error.0, InstallError::Busy);
    }
}
