//! Bounded, cancellable worktree content search. The blocking worker publishes
//! small batches; neither traversal nor matching runs on the UI thread.
use std::{
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use ignore::gitignore::GitignoreBuilder;
use regex::RegexBuilder;
use sha2::{Digest, Sha256};

use crate::process::CancellationToken;

mod ignore_policy;
mod traversal;

pub const RESULT_LIMIT: usize = 2_000;
const BATCH_SIZE: usize = 64;
const BATCH_DELAY: Duration = Duration::from_millis(25);
const MAX_LINE_BYTES: usize = 1024 * 1024;
const SEARCH_BLOCK_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    pub query: String,
    pub case_sensitive: bool,
    pub regex: bool,
    pub whole_word: bool,
    pub include: String,
    pub exclude: String,
    pub include_ignored: bool,
    pub include_hidden: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub path: PathBuf,
    /// Zero-based native editor line and UTF-8 byte offsets.
    pub line: usize,
    pub column: usize,
    pub length: usize,
    pub line_identity: LineIdentity,
    pub snippet: String,
    /// Match range inside the displayed (possibly shortened) snippet.
    pub highlight: std::ops::Range<usize>,
}

/// Identity of one physical line's UTF-8 content, excluding its CR/LF ending.
/// The scanner and native editor both separate CR, LF, CRLF and LFCR endings.
/// Retain fixed-size evidence rather than a potentially megabyte-long line for
/// each result. Matching a changed file never silently relocates the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineIdentity {
    bytes: usize,
    sha256: [u8; 32],
}

impl LineIdentity {
    pub fn new(text: &str) -> Self {
        Self {
            bytes: text.len(),
            sha256: Sha256::digest(text.as_bytes()).into(),
        }
    }

    pub fn matches(&self, text: &str) -> bool {
        text.len() == self.bytes && Self::new(text) == *self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    Batch(Vec<Match>),
    Finished { capped: bool, skipped: usize },
    Error(String),
}

/// Search one local root. Returning false from `publish` cancels delivery.
/// Symlinks are never followed, binary and oversized files are skipped, and
/// each line is bounded independently to avoid an unbounded allocation.
/// Files stream in filesystem order; matches within each file stay in line order.
pub fn search(
    root: &Path,
    options: &Options,
    cancellation: &CancellationToken,
    publish: impl FnMut(Progress) -> bool,
) {
    search_with_limits(
        root,
        options,
        cancellation,
        publish,
        traversal::Limits::default(),
    );
}

fn search_with_limits(
    root: &Path,
    options: &Options,
    cancellation: &CancellationToken,
    mut publish: impl FnMut(Progress) -> bool,
    limits: traversal::Limits,
) {
    if cancellation.is_cancelled() {
        return;
    }
    if options.query.is_empty() {
        publish(Progress::Finished {
            capped: false,
            skipped: 0,
        });
        return;
    }
    let mut pattern = if options.regex {
        options.query.clone()
    } else {
        regex::escape(&options.query)
    };
    if options.whole_word {
        pattern = format!(r"\b{{start-half}}(?:{pattern})\b{{end-half}}");
    }
    let regex = match RegexBuilder::new(&pattern)
        .case_insensitive(!options.case_sensitive)
        .size_limit(4 * 1024 * 1024)
        .build()
    {
        Ok(regex) => regex,
        Err(error) => {
            publish(Progress::Error(error.to_string()));
            return;
        }
    };
    let mut overrides = GitignoreBuilder::new(root);
    overrides.allow_unclosed_class(false);
    let mut directory_includes = GitignoreBuilder::new(root);
    directory_includes.allow_unclosed_class(false);
    for (patterns, excluded) in [(&options.include, false), (&options.exclude, true)] {
        for glob in patterns
            .split(',')
            .map(str::trim)
            .filter(|glob| !glob.is_empty())
        {
            let pattern = if excluded {
                format!("!{glob}")
            } else {
                glob.to_owned()
            };
            if let Err(error) = overrides.add_line(None, &pattern) {
                publish(Progress::Error(format!("Invalid file glob: {error}")));
                return;
            }
            if !excluded && glob.ends_with('/') && !glob.starts_with('!') {
                // Only an explicit directory include inherits to children;
                // a file glob such as *.rs must still filter by file name.
                if let Err(error) = directory_includes.add_line(None, glob) {
                    publish(Progress::Error(format!("Invalid file glob: {error}")));
                    return;
                }
            }
        }
    }
    let overrides = match overrides.build() {
        Ok(overrides) => overrides,
        Err(error) => {
            publish(Progress::Error(error.to_string()));
            return;
        }
    };
    let directory_includes = match directory_includes.build() {
        Ok(includes) => includes,
        Err(error) => {
            publish(Progress::Error(error.to_string()));
            return;
        }
    };
    // Like OverrideBuilder, positive globs include and negated globs exclude.
    let has_includes = overrides.num_ignores() > 0;
    let guarded_root = match crate::fs_safety::Root::open(root) {
        Ok(root) => root,
        Err(error) if traversal::descriptor_exhausted(&error) => {
            publish(Progress::Error(
                traversal::Stop::Resource(format!("Folder descriptors: {error}"))
                    .message()
                    .unwrap(),
            ));
            return;
        }
        Err(error) => {
            publish(Progress::Error(format!(
                "Could not search this folder: {error}"
            )));
            return;
        }
    };
    let guarded_root = std::sync::Arc::new(guarded_root);
    let policy = match ignore_policy::Policy::new(
        guarded_root.clone(),
        cancellation.clone(),
        options.include_ignored,
        options.include_hidden,
    ) {
        Ok(policy) => policy,
        Err(error) => {
            if !cancellation.is_cancelled() {
                publish(Progress::Error(
                    traversal::Stop::Resource(error).message().unwrap(),
                ));
            }
            return;
        }
    };
    let mut batch = Vec::with_capacity(BATCH_SIZE);
    let mut total = 0;
    let mut skipped = 0;
    let mut last_batch = Instant::now();
    let mut bytes = Vec::with_capacity(SEARCH_BLOCK_BYTES);
    if cancellation.is_cancelled() {
        return;
    }
    let mut directories = traversal::Traversal::new(limits);
    let mut cancelled = || cancellation.is_cancelled();
    // Error callbacks can run arbitrary caller work. Release every retained
    // directory iterator before disclosing an incomplete search to that caller.
    macro_rules! stop {
        ($stop:expr) => {{
            let stop = $stop;
            drop(directories);
            if !cancellation.is_cancelled()
                && let Some(message) = stop.message()
            {
                publish(Progress::Error(message));
            }
            return;
        }};
    }
    match directories.descend(root, &mut cancelled) {
        Ok(Ok(())) => {}
        Ok(Err(_)) => skipped += 1,
        Err(stop) => stop!(stop),
    }
    loop {
        if cancellation.is_cancelled() {
            return;
        }
        if let Some(error) = policy
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .error()
        {
            stop!(traversal::Stop::Resource(error));
        }
        let entry = match directories.next(&mut cancelled) {
            Ok(Some(Ok(entry))) => entry,
            Ok(Some(Err(_))) => {
                skipped += 1;
                continue;
            }
            Ok(None) => break,
            Err(stop) => stop!(stop),
        };
        let traversal::Entry { path, kind } = entry;
        // Excluding a directory also excludes its descendants. Include
        // misses must not prune directories that can contain matching files.
        if kind.is_dir() && overrides.matched(&path, true).invert().is_ignore() {
            continue;
        }
        if !policy
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .allows(&path, kind)
        {
            continue;
        }
        if cancellation.is_cancelled() {
            return;
        }
        if kind.is_dir() {
            match directories.descend(&path, &mut cancelled) {
                Ok(Ok(())) => {}
                Ok(Err(_)) => skipped += 1,
                Err(stop) => stop!(stop),
            }
            continue;
        }
        // Apply include/exclude patterns after the ignore-aware walk. A file
        // include must not silently re-include an ignored file or prune a
        // directory that may contain a matching descendant.
        let matched = overrides.matched(&path, false).invert();
        if matched.is_ignore()
            || (matched.is_none()
                && has_includes
                && !directory_includes
                    .matched_path_or_any_parents(&path, false)
                    .is_ignore())
        {
            continue;
        }
        let Ok(path) = path.strip_prefix(root) else {
            skipped += 1;
            continue;
        };
        let file = match crate::fs_safety::open_beneath(&guarded_root, path) {
            Ok(file) => file,
            Err(error) if error.is_resource_exhausted() => {
                stop!(traversal::Stop::Resource(format!(
                    "File descriptors: {error}"
                )));
            }
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        match file.metadata() {
            Ok(metadata) if metadata.is_file() => {}
            Err(error) if traversal::descriptor_exhausted(&error) => {
                drop(file);
                stop!(traversal::Stop::Resource(format!(
                    "File descriptors: {error}"
                )));
            }
            _ => {
                skipped += 1;
                continue;
            }
        }
        let path = path.to_path_buf();
        let mut reader = BufReader::with_capacity(SEARCH_BLOCK_BYTES, file);
        let mut line = 0;
        let mut finished = false;
        while !finished {
            if cancellation.is_cancelled() {
                return;
            }
            // Read the common case in one bulk operation instead of wrapping
            // Read::take, validating UTF-8 and extending the buffer per line.
            // Complete the final line only, with the same strict line bound.
            bytes.resize(SEARCH_BLOCK_BYTES, 0);
            let read = match reader.read(&mut bytes) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if traversal::descriptor_exhausted(&error) => {
                    drop(reader);
                    stop!(traversal::Stop::Resource(format!(
                        "File descriptors: {error}"
                    )));
                }
                Err(_) => {
                    skipped += 1;
                    break;
                }
            };
            bytes.truncate(read);
            let mut invalid_file = false;
            let tail_start = memchr::memrchr2(b'\r', b'\n', &bytes).map_or(0, |index| index + 1);
            if tail_start != bytes.len() {
                match complete_native_line(&mut reader, &mut bytes, tail_start, cancellation) {
                    Ok(eof) => finished = eof,
                    Err(error) if traversal::descriptor_exhausted(&error) => {
                        drop(reader);
                        stop!(traversal::Stop::Resource(format!(
                            "File descriptors: {error}"
                        )));
                    }
                    Err(_) => {
                        bytes.truncate(tail_start);
                        invalid_file = true;
                        finished = true;
                    }
                }
            }
            // CRLF and LFCR are one native editor line ending, including when
            // their two bytes straddle this block. Consume only the paired
            // second byte; a repeated terminator starts another empty line.
            if !finished && let Some(ending) = unpaired_ending(&bytes) {
                loop {
                    if cancellation.is_cancelled() {
                        return;
                    }
                    match reader.fill_buf() {
                        Ok(next) => {
                            if next.first().is_some_and(|&byte| {
                                matches!(byte, b'\r' | b'\n') && byte != ending
                            }) {
                                bytes.push(next[0]);
                                reader.consume(1);
                            }
                            break;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) if traversal::descriptor_exhausted(&error) => {
                            drop(reader);
                            stop!(traversal::Stop::Resource(format!(
                                "File descriptors: {error}"
                            )));
                        }
                        Err(_) => {
                            invalid_file = true;
                            finished = true;
                            break;
                        }
                    }
                }
            }
            if bytes.len() - tail_start > MAX_LINE_BYTES {
                bytes.truncate(tail_start);
                invalid_file = true;
                finished = true;
            }
            // Invalid data skips its line and the suffix, retaining all valid
            // complete lines before it just as the per-line reader did.
            let invalid = memchr::memchr(0, &bytes)
                .into_iter()
                .chain(
                    std::str::from_utf8(&bytes)
                        .err()
                        .map(|error| error.valid_up_to()),
                )
                .min();
            if let Some(invalid) = invalid {
                let valid =
                    memchr::memrchr2(b'\r', b'\n', &bytes[..invalid]).map_or(0, |index| index + 1);
                bytes.truncate(valid);
                invalid_file = true;
                finished = true;
            }
            if invalid_file {
                skipped += 1;
            }
            if cancellation.is_cancelled() {
                return;
            }
            let text =
                std::str::from_utf8(&bytes).expect("only validated complete lines enter the block");
            if options.regex || regex.is_match(text) {
                for text in native_lines(text) {
                    // Several matches in the same long line share one digest.
                    let mut line_identity = None;
                    for matched in regex.find_iter(text) {
                        if cancellation.is_cancelled() {
                            return;
                        }
                        if total == RESULT_LIMIT {
                            if !batch.is_empty()
                                && !publish(Progress::Batch(std::mem::take(&mut batch)))
                            {
                                return;
                            }
                            if cancellation.is_cancelled() {
                                return;
                            }
                            publish(Progress::Finished {
                                capped: true,
                                skipped,
                            });
                            return;
                        }
                        let (snippet, highlight) = snippet(text, matched.range());
                        batch.push(Match {
                            path: path.clone(),
                            line,
                            column: matched.start(),
                            length: matched.len(),
                            line_identity: *line_identity
                                .get_or_insert_with(|| LineIdentity::new(text)),
                            snippet,
                            highlight,
                        });
                        total += 1;
                        if batch.len() >= BATCH_SIZE || last_batch.elapsed() >= BATCH_DELAY {
                            if !publish(Progress::Batch(std::mem::take(&mut batch))) {
                                return;
                            }
                            last_batch = Instant::now();
                        }
                    }
                    if !batch.is_empty() && last_batch.elapsed() >= BATCH_DELAY {
                        if !publish(Progress::Batch(std::mem::take(&mut batch))) {
                            return;
                        }
                        last_batch = Instant::now();
                    }
                    line += 1;
                }
            } else {
                line += native_lines(text).count();
            }
            if !batch.is_empty() && last_batch.elapsed() >= BATCH_DELAY {
                if !publish(Progress::Batch(std::mem::take(&mut batch))) {
                    return;
                }
                last_batch = Instant::now();
            }
        }
    }
    if cancellation.is_cancelled() {
        return;
    }
    if let Some(error) = policy
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .error()
    {
        if !cancellation.is_cancelled() {
            stop!(traversal::Stop::Resource(error));
        }
        return;
    }
    if !batch.is_empty() && !publish(Progress::Batch(batch)) {
        return;
    }
    if cancellation.is_cancelled() {
        return;
    }
    publish(Progress::Finished {
        capped: false,
        skipped,
    });
}

/// Complete the trailing native line without reading or allocating beyond
/// its existing cap. BufRead supplies bulk slices for both CR and LF endings.
fn complete_native_line(
    reader: &mut impl BufRead,
    bytes: &mut Vec<u8>,
    start: usize,
    cancellation: &CancellationToken,
) -> std::io::Result<bool> {
    while bytes.len() - start <= MAX_LINE_BYTES {
        if cancellation.is_cancelled() {
            return Ok(true);
        }
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if available.is_empty() {
            return Ok(true);
        }
        let take = memchr::memchr2(b'\r', b'\n', available)
            .map_or(available.len(), |index| index + 1)
            .min(MAX_LINE_BYTES + 1 - (bytes.len() - start));
        let ended = matches!(available[take - 1], b'\r' | b'\n');
        bytes.extend_from_slice(&available[..take]);
        reader.consume(take);
        if ended {
            break;
        }
    }
    Ok(false)
}

/// A block may already end in a paired terminator. Alternating runs pair
/// from their first byte, so only an odd suffix can pair with the next block.
fn unpaired_ending(bytes: &[u8]) -> Option<u8> {
    let ending = *bytes.last()?;
    if !matches!(ending, b'\r' | b'\n') {
        return None;
    }
    let mut previous = ending;
    let mut count = 1;
    for &byte in bytes[..bytes.len() - 1].iter().rev() {
        if !matches!(byte, b'\r' | b'\n') || byte == previous {
            break;
        }
        count += 1;
        previous = byte;
    }
    (count % 2 == 1).then_some(ending)
}

/// Match the editor's LineIter: CR, LF, CRLF and LFCR, with byte columns
/// measured from the start of each line's content.
fn native_lines(mut text: &str) -> impl Iterator<Item = &str> {
    std::iter::from_fn(move || {
        if text.is_empty() {
            return None;
        }
        let end = memchr::memchr2(b'\r', b'\n', text.as_bytes()).unwrap_or(text.len());
        let mut next = (end + 1).min(text.len());
        if next < text.len()
            && matches!(text.as_bytes()[next], b'\r' | b'\n')
            && text.as_bytes()[next] != text.as_bytes()[end]
        {
            next += 1;
        }
        let line = &text[..end];
        text = &text[next..];
        Some(line)
    })
}

fn snippet(text: &str, range: std::ops::Range<usize>) -> (String, std::ops::Range<usize>) {
    let mut start = range.start.saturating_sub(120);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let mut end = (range.end + 200).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    // A regex can select an enormous line. Keep snippets bounded even then.
    end = end.min(start + 500);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let prefix = if start > 0 { "…" } else { "" };
    let suffix = if end < text.len() { "…" } else { "" };
    let highlight = prefix.len() + range.start.saturating_sub(start)
        ..prefix.len() + range.end.min(end).saturating_sub(start);
    (format!("{prefix}{}{suffix}", &text[start..end]), highlight)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn run(root: &Path, options: Options) -> Vec<Progress> {
        let mut events = Vec::new();
        search(root, &options, &CancellationToken::default(), |event| {
            events.push(event);
            true
        });
        events
    }
    fn matches(events: Vec<Progress>) -> Vec<Match> {
        events
            .into_iter()
            .flat_map(|event| match event {
                Progress::Batch(batch) => batch,
                _ => Vec::new(),
            })
            .collect()
    }
    fn options(query: &str) -> Options {
        Options {
            query: query.into(),
            ..Default::default()
        }
    }
    #[test]
    fn ignore_hidden_globs_and_options() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::write(fixture.path().join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir(fixture.path().join("ignored")).unwrap();
        std::fs::write(fixture.path().join("ignored/cache.rs"), "needle").unwrap();
        std::fs::write(fixture.path().join("main.rs"), "Needle needle needles\n").unwrap();
        std::fs::write(fixture.path().join("notes.txt"), "needle").unwrap();
        std::fs::write(fixture.path().join(".hidden"), "needle").unwrap();
        assert_eq!(matches(run(fixture.path(), options("needle"))).len(), 4);
        let mut opts = options("needle");
        opts.include_ignored = true;
        opts.include_hidden = true;
        assert_eq!(matches(run(fixture.path(), opts.clone())).len(), 6);
        let mut include_default = options("needle");
        include_default.include = "*.rs".into();
        assert_eq!(matches(run(fixture.path(), include_default)).len(), 3);
        opts.include = "*.rs".into();
        opts.exclude = "ignored/**".into();
        opts.case_sensitive = true;
        opts.whole_word = true;
        let result = matches(run(fixture.path(), opts));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].column, 7);
        assert_eq!(result[0].length, 6);
        assert_eq!(result[0].path, Path::new("main.rs"));
    }
    #[test]
    fn directory_globs_filter_descendants_without_losing_their_scope() {
        let fixture = tempfile::tempdir().unwrap();
        for path in [
            "src/main.rs",
            "src/child/deep.rs",
            "nested/src/lib.rs",
            "generated/root.rs",
            "nested/generated/nested.rs",
            "docs/readme.txt",
            "src/ignored/hidden.rs",
        ] {
            let path = fixture.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "needle").unwrap();
        }
        std::fs::write(fixture.path().join(".ignore"), "src/ignored/\n").unwrap();
        let paths = |include: &str, exclude: &str, include_ignored| {
            let mut opts = options("needle");
            opts.include = include.into();
            opts.exclude = exclude.into();
            opts.include_ignored = include_ignored;
            let mut found = matches(run(fixture.path(), opts))
                .into_iter()
                .map(|matched| matched.path)
                .collect::<Vec<_>>();
            found.sort();
            found
        };
        assert_eq!(
            paths("src/", "", false),
            [
                PathBuf::from("nested/src/lib.rs"),
                PathBuf::from("src/child/deep.rs"),
                PathBuf::from("src/main.rs")
            ]
        );
        assert_eq!(
            paths("/src/", "", false),
            [
                PathBuf::from("src/child/deep.rs"),
                PathBuf::from("src/main.rs")
            ]
        );
        assert_eq!(
            paths("nested/src/", "", false),
            [PathBuf::from("nested/src/lib.rs")]
        );
        assert_eq!(
            paths("*.rs", "generated/", false),
            [
                PathBuf::from("nested/src/lib.rs"),
                PathBuf::from("src/child/deep.rs"),
                PathBuf::from("src/main.rs")
            ]
        );
        assert_eq!(
            paths("*.rs", "generated", false),
            paths("*.rs", "generated/", false)
        );
        assert_eq!(
            paths("src/", "/src/", false),
            [PathBuf::from("nested/src/lib.rs")]
        );
        assert_eq!(paths("src/", "", true).len(), 4);
        assert_eq!(
            paths("src/**", "", false),
            [
                PathBuf::from("src/child/deep.rs"),
                PathBuf::from("src/main.rs")
            ]
        );
        assert_eq!(
            paths("src/**/", "", false),
            [PathBuf::from("src/child/deep.rs")]
        );
        assert_eq!(
            paths("/src/**/", "", false),
            [PathBuf::from("src/child/deep.rs")]
        );
        assert_eq!(paths("**/", "generated/", false).len(), 4);
        assert_eq!(paths("src/,src/child", "", false), paths("src/", "", false));
        for path in [
            "looks_like.rs/notes.txt",
            "looks_like.rs/child.rs",
            "src/looks_like.rs/notes.txt",
        ] {
            let path = fixture.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "needle").unwrap();
        }
        assert!(
            paths("*.rs", "", false)
                .iter()
                .all(|path| path.extension().is_some_and(|extension| extension == "rs"))
        );
        assert_eq!(paths("src/*.rs", "", false), [PathBuf::from("src/main.rs")]);
        assert!(paths("*.rs", "", false).contains(&PathBuf::from("looks_like.rs/child.rs")));
    }

    #[test]
    fn special_ignore_files_end_the_worker_without_blocking_or_leaking_queries() {
        use std::os::unix::ffi::OsStrExt;
        for name in [
            ".ignore",
            ".gitignore",
            "nested/.ignore",
            ".git/info/exclude",
        ] {
            let fixture = tempfile::tempdir().unwrap();
            let path = fixture.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            // SAFETY: the temporary pathname is a valid NUL-terminated string.
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            std::fs::write(fixture.path().join("match.txt"), "safe_marker").unwrap();
            let root = fixture.path().to_path_buf();
            let (sender, receiver) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                sender.send(run(&root, options("safe_marker"))).unwrap()
            });
            let events = receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("an ignore FIFO must never strand a search worker");
            worker.join().unwrap();
            assert!(events.iter().any(|event| matches!(event, Progress::Error(error) if error.contains("regular file"))), "{events:?}");
            let mut next = options("safe_marker");
            next.include_ignored = true;
            assert_eq!(
                matches(run(fixture.path(), next)).len(),
                1,
                "a new query can explicitly bypass unsupported ignore metadata"
            );
        }
    }

    #[test]
    fn descriptor_ignore_policy_preserves_precedence_nested_repositories_and_linked_excludes() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(fixture.path().join("sub")).unwrap();
        std::fs::create_dir_all(fixture.path().join("nested/.git")).unwrap();
        std::fs::write(fixture.path().join(".gitignore"), "*.txt\n").unwrap();
        std::fs::write(fixture.path().join(".ignore"), "!keep.txt\n").unwrap();
        std::fs::write(fixture.path().join("sub/.gitignore"), "!child.txt\n").unwrap();
        for path in [
            "keep.txt",
            "drop.txt",
            "sub/child.txt",
            "nested/included.txt",
        ] {
            std::fs::write(fixture.path().join(path), "marker").unwrap();
        }
        let mut found = matches(run(fixture.path(), options("marker")))
            .into_iter()
            .map(|matched| matched.path)
            .collect::<Vec<_>>();
        found.sort();
        assert_eq!(
            found,
            vec![
                PathBuf::from("keep.txt"),
                "nested/included.txt".into(),
                "sub/child.txt".into()
            ]
        );
        let metadata = fixture.path().join("metadata");
        std::fs::create_dir_all(metadata.join("worktrees/linked")).unwrap();
        std::fs::create_dir_all(metadata.join("info")).unwrap();
        std::fs::write(metadata.join("info/exclude"), "excluded.rs\n").unwrap();
        std::fs::write(metadata.join("worktrees/linked/commondir"), "../..\n").unwrap();
        let checkout = fixture.path().join("linked");
        std::fs::create_dir(&checkout).unwrap();
        std::fs::write(
            checkout.join(".git"),
            format!("gitdir: {}\n", metadata.join("worktrees/linked").display()),
        )
        .unwrap();
        std::fs::write(checkout.join("excluded.rs"), "marker").unwrap();
        std::fs::write(checkout.join("visible.rs"), "marker").unwrap();
        assert_eq!(
            matches(run(&checkout, options("marker")))
                .into_iter()
                .map(|matched| matched.path)
                .collect::<Vec<_>>(),
            vec![PathBuf::from("visible.rs")]
        );
    }

    #[test]
    fn whole_word_handles_punctuation_and_regex_alternate_retry() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::write(fixture.path().join("source"), "foo() food() foobar\n").unwrap();
        let mut opts = options("foo()");
        opts.whole_word = true;
        let result = matches(run(fixture.path(), opts));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].length, 5);
        let mut opts = options("foo|foobar");
        opts.regex = true;
        opts.whole_word = true;
        let result = matches(run(fixture.path(), opts));
        assert_eq!(result.len(), 2);
        assert_eq!(result[1].length, 6);
    }

    #[test]
    fn large_search_streams_first_results_before_completion() {
        let fixture = tempfile::tempdir().unwrap();
        for index in 0..3_000 {
            std::fs::write(
                fixture.path().join(format!("{index}.txt")),
                "unique_stream_marker\n",
            )
            .unwrap();
        }
        let started = Instant::now();
        let mut first_batch = None;
        let mut delivered = 0;
        search(
            fixture.path(),
            &options("unique_stream_marker"),
            &CancellationToken::default(),
            |event| {
                if let Progress::Batch(batch) = event {
                    first_batch.get_or_insert(started.elapsed());
                    delivered += batch.len();
                }
                true
            },
        );
        eprintln!(
            "search_first_batch_ms={} full_capped_ms={} results={delivered}",
            first_batch.unwrap().as_millis(),
            started.elapsed().as_millis()
        );
        assert_eq!(delivered, RESULT_LIMIT);
    }

    #[test]
    fn deep_directory_search_preserves_ignore_glob_and_symlink_membership() {
        let fixture = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("outside.txt"), "depth_marker\n").unwrap();
        std::fs::write(fixture.path().join(".gitignore"), "*.ignored\n").unwrap();
        std::fs::write(fixture.path().join(".ignore"), "!keep.ignored\n").unwrap();
        std::fs::write(fixture.path().join("scoped.txt"), "depth_marker\n").unwrap();
        let mut expected = vec![PathBuf::from("scoped.txt")];
        let mut relative = PathBuf::new();
        // Exceed WalkDir's old ten-open-directory threshold, with siblings at
        // every ancestor. Membership must not depend on filesystem order.
        for depth in 0..=16 {
            let directory = fixture.path().join(&relative);
            std::fs::create_dir_all(&directory).unwrap();
            for sibling in 0..24 {
                std::fs::write(directory.join(format!("sibling-{sibling}.txt")), "other\n")
                    .unwrap();
            }
            for name in [
                "keep.ignored",
                "drop.ignored",
                "glob.skip.txt",
                ".hidden.txt",
            ] {
                std::fs::write(directory.join(name), "depth_marker\n").unwrap();
            }
            expected.push(relative.join("keep.ignored"));
            if depth < 16 {
                relative.push("deeper");
            }
        }
        let deepest = fixture.path().join(&relative);
        // These local rules must not leak into the root sibling on unwind.
        std::fs::write(deepest.join(".ignore"), "scoped.txt\n").unwrap();
        std::fs::write(deepest.join("scoped.txt"), "depth_marker\n").unwrap();
        std::os::unix::fs::symlink(outside.path(), deepest.join("linked-directory")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("outside.txt"),
            deepest.join("linked-file.txt"),
        )
        .unwrap();
        let mut opts = options("depth_marker");
        opts.include = "*.txt,*.ignored".into();
        opts.exclude = "*.skip.txt".into();
        let events = run(fixture.path(), opts.clone());
        assert!(matches!(
            events.last(),
            Some(Progress::Finished {
                capped: false,
                skipped: 0
            })
        ));
        let mut found = matches(events)
            .into_iter()
            .map(|matched| matched.path)
            .collect::<Vec<_>>();
        found.sort();
        expected.sort();
        assert_eq!(found, expected);

        opts.include_ignored = true;
        opts.include_hidden = true;
        let all = matches(run(fixture.path(), opts));
        assert_eq!(all.len(), 17 * 3 + 2);
        assert!(
            all.iter()
                .any(|matched| matched.path == relative.join("scoped.txt"))
        );
        assert!(
            all.iter()
                .any(|matched| matched.path == Path::new(".hidden.txt"))
        );
        assert!(all.iter().all(|matched| {
            !matched.path.starts_with(&relative.join("linked-directory"))
                && matched.path != relative.join("linked-file.txt")
                && !matched.path.ends_with("glob.skip.txt")
        }));
    }

    #[test]
    fn deep_wide_search_stops_on_first_delivery_cancellation_or_refusal() {
        let fixture = tempfile::tempdir().unwrap();
        let mut relative = PathBuf::new();
        for _ in 0..16 {
            let directory = fixture.path().join(&relative);
            std::fs::create_dir_all(&directory).unwrap();
            for sibling in 0..32 {
                std::fs::write(directory.join(format!("sibling-{sibling}.txt")), "other\n")
                    .unwrap();
            }
            relative.push("deeper");
        }
        let deepest = fixture.path().join(&relative);
        std::fs::create_dir_all(&deepest).unwrap();
        let target = relative.join("matches.txt");
        std::fs::write(
            fixture.path().join(&target),
            "λ marker\n".repeat(BATCH_SIZE * 3),
        )
        .unwrap();
        for cancel_token in [false, true] {
            let token = CancellationToken::default();
            let mut events = Vec::new();
            search(fixture.path(), &options("marker"), &token, |event| {
                assert!(matches!(&event, Progress::Batch(_)), "{event:?}");
                events.push(event);
                if cancel_token {
                    token.cancel();
                }
                cancel_token
            });
            assert_eq!(token.is_cancelled(), cancel_token);
            assert_eq!(
                events.len(),
                1,
                "stopped delivery cannot finish or publish again"
            );
            let delivered = matches(events);
            assert!(!delivered.is_empty() && delivered.len() <= BATCH_SIZE);
            for matched in delivered {
                assert_eq!(matched.path, target);
                assert_eq!((matched.column, matched.length), (3, 6));
                assert!(matched.line_identity.matches("λ marker"));
            }
        }
    }

    #[test]
    fn deep_ignore_error_terminates_without_a_finished_event() {
        let fixture = tempfile::tempdir().unwrap();
        let mut directory = fixture.path().to_path_buf();
        for _ in 0..16 {
            directory.push("deeper");
        }
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join(".ignore"), b"invalid \xff").unwrap();
        std::fs::write(directory.join("matches.txt"), "marker\n").unwrap();
        let events = run(fixture.path(), options("marker"));
        assert!(
            matches!(events.as_slice(), [Progress::Error(error)] if error.contains("not UTF-8"))
        );
    }

    #[test]
    fn regex_errors_and_binary_files() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::write(fixture.path().join("binary"), b"needle\0needle").unwrap();
        let mut opts = options("[");
        opts.regex = true;
        assert!(matches!(
            run(fixture.path(), opts).as_slice(),
            [Progress::Error(_)]
        ));
        assert!(matches(run(fixture.path(), options("needle"))).is_empty());
    }
    #[test]
    fn content_search_never_follows_links_outside_the_selected_root() {
        let fixture = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "external_only_marker").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), fixture.path().join("link"))
            .unwrap();
        std::os::unix::fs::symlink(outside.path(), fixture.path().join("linked-directory"))
            .unwrap();
        assert!(matches(run(fixture.path(), options("external_only_marker"))).is_empty());
    }

    #[test]
    #[ignore = "explicit local repository latency probe"]
    fn repository_first_result_latency() {
        let root =
            std::env::var_os("CAGENTS_SEARCH_BENCH_ROOT").expect("set CAGENTS_SEARCH_BENCH_ROOT");
        let started = Instant::now();
        let mut first = None;
        let mut count = 0;
        search(
            Path::new(&root),
            &options("fn source_control_snapshot_task"),
            &CancellationToken::default(),
            |event| {
                if let Progress::Batch(batch) = event {
                    first.get_or_insert(started.elapsed());
                    count += batch.len();
                }
                true
            },
        );
        eprintln!(
            "repository_first_batch_ms={} repository_complete_ms={} results={count}",
            first.expect("repository marker must match").as_millis(),
            started.elapsed().as_millis()
        );
        assert!(count > 0);
        assert!(
            first.unwrap() < Duration::from_millis(125),
            "the worker must leave room for the 75ms UI debounce within the 200ms first-result budget: {first:?}"
        );
    }

    #[test]
    fn literal_block_prefilter_keeps_line_offsets_and_valid_prefix_before_invalid_data() {
        let fixture = tempfile::tempdir().unwrap();
        let prefix = "not a match\n".repeat(10_000);
        let mut text = prefix.into_bytes();
        text.extend_from_slice(b"NEEDLE\r\n");
        text.extend_from_slice("more text\n".repeat(10_000).as_bytes());
        text.extend_from_slice(b"needle\ninvalid\0needle\nneedle\n");
        std::fs::write(fixture.path().join("large.txt"), text).unwrap();
        let mut opts = options("needle");
        opts.whole_word = true;
        let events = run(fixture.path(), opts);
        let found = matches(events.clone());
        assert_eq!(
            found.iter().map(|found| found.line).collect::<Vec<_>>(),
            vec![10_000, 20_001]
        );
        assert!(
            found
                .iter()
                .all(|found| found.column == 0 && found.length == 6)
        );
        assert!(matches!(
            events.last(),
            Some(Progress::Finished { skipped: 1, .. })
        ));
    }

    #[test]
    fn search_locations_select_native_editor_matches_for_every_line_ending() {
        use crate::{model::CardId, repository_widget::RepositoryRuntime};
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("notes.txt");
        let padding = "x".repeat(SEARCH_BLOCK_BYTES - 1);
        let cases = [
            "first\rneedle".to_owned(),
            "first\n\rneedle".to_owned(),
            "first\r\nneedle".to_owned(),
            "first\nneedle".to_owned(),
            "no hit\rno hit\n\ré needle\rneedle".to_owned(),
            format!("{padding}\r\nneedle"),
            format!("{padding}\n\rneedle"),
            format!("{padding}\r\n\rneedle"),
            format!("{}\r\nneedle", "no hit\r".repeat(12_000)),
        ];
        for text in cases {
            std::fs::write(&path, &text).unwrap();
            let found = matches(run(fixture.path(), options("needle")));
            assert_eq!(found.len(), text.matches("needle").count());
            let card = CardId::new();
            let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, &text);
            for matched in found {
                drop(runtime.open_search_result(card, &matched));
                assert_eq!(runtime.selected_text().as_deref(), Some("needle"));
            }
        }
    }

    #[test]
    fn bulk_reader_completes_cross_block_utf8_lines_and_skips_one_invalid_suffix() {
        let fixture = tempfile::tempdir().unwrap();
        let mut text = vec![b'x'; SEARCH_BLOCK_BYTES - 2];
        text.extend_from_slice("é needle\r\n".as_bytes());
        text.extend_from_slice(b"valid needle\n");
        text.extend(std::iter::repeat_n(b'x', MAX_LINE_BYTES + 1));
        text.extend_from_slice(b"needle\n");
        std::fs::write(fixture.path().join("cross-block"), text).unwrap();
        let events = run(fixture.path(), options("needle"));
        let found = matches(events.clone());
        assert_eq!(
            found
                .iter()
                .map(|found| (found.line, found.column))
                .collect::<Vec<_>>(),
            vec![(0, SEARCH_BLOCK_BYTES + 1), (1, 6)]
        );
        assert!(matches!(
            events.last(),
            Some(Progress::Finished { skipped: 1, .. })
        ));
        std::fs::write(
            fixture.path().join("cross-block"),
            b"needle\nvalid\ninvalid\xffneedle\nneedle\n",
        )
        .unwrap();
        let events = run(fixture.path(), options("needle"));
        assert_eq!(matches(events.clone()).len(), 1);
        assert!(matches!(
            events.last(),
            Some(Progress::Finished { skipped: 1, .. })
        ));
    }

    #[test]
    fn caps_results_and_cancels_during_delivery() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::write(
            fixture.path().join("many"),
            "needle\n".repeat(RESULT_LIMIT + 1),
        )
        .unwrap();
        let events = run(fixture.path(), options("needle"));
        assert!(matches!(
            events.last(),
            Some(Progress::Finished { capped: true, .. })
        ));
        assert_eq!(matches(events).len(), RESULT_LIMIT);
        let token = CancellationToken::default();
        let mut count = 0;
        search(fixture.path(), &options("needle"), &token, |event| {
            if let Progress::Batch(batch) = event {
                count += batch.len();
                token.cancel();
            }
            true
        });
        assert!(count > 0 && count < RESULT_LIMIT);
        let mut called = false;
        search(fixture.path(), &options("needle"), &token, |_| {
            called = true;
            true
        });
        assert!(!called);
    }

    #[test]
    fn cancellation_from_the_final_batch_suppresses_finished_progress() {
        for count in [1, RESULT_LIMIT + 1] {
            let fixture = tempfile::tempdir().unwrap();
            std::fs::write(fixture.path().join("matches"), "needle\n".repeat(count)).unwrap();
            let token = CancellationToken::default();
            let expected = count.min(RESULT_LIMIT);
            let mut delivered = 0;
            let mut events = Vec::new();
            search(fixture.path(), &options("needle"), &token, |event| {
                if let Progress::Batch(batch) = &event {
                    delivered += batch.len();
                    if delivered == expected {
                        token.cancel();
                    }
                }
                events.push(event);
                true
            });
            assert_eq!(delivered, expected);
            assert!(token.is_cancelled());
            assert!(
                events
                    .iter()
                    .all(|event| matches!(event, Progress::Batch(_))),
                "a cancelled final batch cannot finish a query: {events:?}"
            );
        }
    }
}
