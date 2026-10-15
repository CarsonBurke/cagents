//! Bounded, cancellable worktree content search. The blocking worker publishes
//! small batches; neither traversal nor matching runs on the UI thread.
use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use ignore::{WalkBuilder, overrides::OverrideBuilder};
use regex::RegexBuilder;

use crate::process::CancellationToken;

pub const RESULT_LIMIT: usize = 2_000;
const BATCH_SIZE: usize = 64;
const BATCH_DELAY: Duration = Duration::from_millis(25);
const MAX_LINE_BYTES: usize = 1024 * 1024;

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
    /// Zero-based line and UTF-8 byte offsets, shared with editor locations.
    pub line: usize,
    pub column: usize,
    pub length: usize,
    pub snippet: String,
    /// Match range inside the displayed (possibly shortened) snippet.
    pub highlight: std::ops::Range<usize>,
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
pub fn search(
    root: &Path,
    options: &Options,
    cancellation: &CancellationToken,
    mut publish: impl FnMut(Progress) -> bool,
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
    let mut overrides = OverrideBuilder::new(root);
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
            if let Err(error) = overrides.add(&pattern) {
                publish(Progress::Error(format!("Invalid file glob: {error}")));
                return;
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
    let guarded_root = match crate::fs_safety::Root::open(root) {
        Ok(root) => root,
        Err(error) => {
            publish(Progress::Error(format!(
                "Could not search this folder: {error}"
            )));
            return;
        }
    };
    let mut walker = WalkBuilder::new(root);
    walker
        .hidden(!options.include_hidden)
        .git_ignore(!options.include_ignored)
        .git_global(!options.include_ignored)
        .git_exclude(!options.include_ignored)
        .ignore(!options.include_ignored)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|entry| entry.file_name() != ".git");
    let mut batch = Vec::with_capacity(BATCH_SIZE);
    let mut total = 0;
    let mut skipped = 0;
    let mut last_batch = Instant::now();
    for entry in walker.build() {
        if cancellation.is_cancelled() {
            return;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        // Apply include/exclude patterns after the ignore-aware walk. Putting
        // whitelist overrides on WalkBuilder would silently re-include ignored
        // files whenever an include glob matched them.
        if overrides.matched(entry.path(), false).is_ignore() {
            continue;
        }
        let Ok(path) = entry.path().strip_prefix(root) else {
            skipped += 1;
            continue;
        };
        let Ok(file) = crate::fs_safety::open_beneath(&guarded_root, path) else {
            skipped += 1;
            continue;
        };
        if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
            skipped += 1;
            continue;
        }
        let path = path.to_path_buf();
        let mut reader = BufReader::new(file);
        let mut bytes = Vec::new();
        let mut line = 0;
        loop {
            if cancellation.is_cancelled() {
                return;
            }
            bytes.clear();
            // `take` bounds even a single multi-megabyte line. The remainder
            // is not useful text search; report the file as skipped instead.
            let read = std::io::Read::take(&mut reader, (MAX_LINE_BYTES + 1) as u64)
                .read_until(b'\n', &mut bytes);
            match read {
                Ok(0) => break,
                Err(_) => {
                    skipped += 1;
                    break;
                }
                _ => {}
            }
            if bytes.len() > MAX_LINE_BYTES || bytes.contains(&0) {
                skipped += 1;
                break;
            }
            let Ok(text) = std::str::from_utf8(&bytes) else {
                skipped += 1;
                break;
            };
            let text = text.trim_end_matches(['\r', '\n']);
            for matched in regex.find_iter(text) {
                if cancellation.is_cancelled() {
                    return;
                }
                if total == RESULT_LIMIT {
                    if !batch.is_empty() && !publish(Progress::Batch(std::mem::take(&mut batch))) {
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
        if !batch.is_empty() && last_batch.elapsed() >= BATCH_DELAY {
            if !publish(Progress::Batch(std::mem::take(&mut batch))) {
                return;
            }
            last_batch = Instant::now();
        }
    }
    if cancellation.is_cancelled() {
        return;
    }
    if !batch.is_empty() && !publish(Progress::Batch(batch)) {
        return;
    }
    publish(Progress::Finished {
        capped: false,
        skipped,
    });
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
}
