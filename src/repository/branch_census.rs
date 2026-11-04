//! Whole-repository reads that stay cheap on very large checkouts: every
//! branch tip in one process, ahead/behind counts only for the refs a caller
//! asks about, and a worktree status that never descends into an untracked or
//! ignored tree such as `node_modules`.
use super::*;

/// One ref as `for-each-ref` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefTip {
    /// The full ref name, such as `refs/heads/fix/login`.
    pub refname: String,
    pub oid: String,
    pub committed_at_unix_ms: Option<u64>,
    /// The worktree that has this branch checked out.
    pub worktree: Option<PathBuf>,
}

/// Ahead/behind counts of one ref against a base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Divergence {
    pub ahead: u32,
    pub behind: u32,
}

const TIP_FORMAT: &str = "%(refname)%00%(objectname)%00%(committerdate:unix)%00%(worktreepath)%00";
const TIP_FIELDS: usize = 4;

/// `for-each-ref` ends every record with a newline that its fields cannot
/// separate from a newline inside a worktree path, so each format ends in a
/// NUL and records are split on the NUL-newline pair, which no path contains.
fn records(output: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = output;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let (record, remainder) = match rest.windows(2).position(|pair| pair == b"\0\n") {
            Some(end) => (&rest[..end], &rest[end + 2..]),
            None => (rest, &[][..]),
        };
        rest = remainder;
        Some(record)
    })
}

pub fn parse_ref_tips(output: &[u8]) -> Result<Vec<RefTip>, GitParseError> {
    const FORMAT: &str = "git for-each-ref";
    let mut tips = Vec::new();
    for (index, record) in records(output).enumerate() {
        let fields = record.split(|byte| *byte == 0).collect::<Vec<_>>();
        let [refname, oid, committed, worktree] = fields[..] else {
            return Err(malformed(
                FORMAT,
                index,
                format!("expected {TIP_FIELDS} fields, found {}", fields.len()),
            ));
        };
        let refname = std::str::from_utf8(refname)
            .map_err(|_| malformed(FORMAT, index, "ref name is not UTF-8"))?;
        if refname.is_empty() {
            return Err(malformed(FORMAT, index, "empty ref name"));
        }
        let oid = Oid::parse_object(String::from_utf8_lossy(oid).into_owned())
            .map_err(|error| malformed(FORMAT, index, error.to_string()))?;
        let committed_at_unix_ms = std::str::from_utf8(committed)
            .ok()
            .and_then(|seconds| seconds.parse::<u64>().ok())
            .map(|seconds| seconds.saturating_mul(1000));
        let worktree = (!worktree.is_empty()).then(|| bytes_path(worktree));
        tips.push(RefTip {
            refname: refname.to_owned(),
            oid: oid.as_str().to_owned(),
            committed_at_unix_ms,
            worktree,
        });
    }
    Ok(tips)
}

/// Parses `%(refname)%00%(ahead-behind:BASE)%00` records.
pub fn parse_divergence(output: &[u8]) -> Result<BTreeMap<String, Divergence>, GitParseError> {
    const FORMAT: &str = "git for-each-ref ahead-behind";
    let mut counts = BTreeMap::new();
    for (index, record) in records(output).enumerate() {
        let Some((refname, counts_field)) = record
            .iter()
            .position(|byte| *byte == 0)
            .map(|split| (&record[..split], &record[split + 1..]))
        else {
            return Err(malformed(FORMAT, index, "missing ahead-behind field"));
        };
        let refname = std::str::from_utf8(refname)
            .map_err(|_| malformed(FORMAT, index, "ref name is not UTF-8"))?;
        let divergence = parse_counts(counts_field)
            .ok_or_else(|| malformed(FORMAT, index, "ahead-behind is not two counts"))?;
        counts.insert(refname.to_owned(), divergence);
    }
    Ok(counts)
}

/// Two whitespace-separated counts, `AHEAD BEHIND`.
fn parse_counts(field: &[u8]) -> Option<Divergence> {
    let text = std::str::from_utf8(field).ok()?;
    let mut counts = text.split_whitespace().map(|count| count.parse::<u32>());
    let (Some(Ok(ahead)), Some(Ok(behind)), None) = (counts.next(), counts.next(), counts.next())
    else {
        return None;
    };
    Some(Divergence { ahead, behind })
}

#[cfg(unix)]
fn bytes_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn bytes_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

/// Keeps each argv far below `ARG_MAX`; refs are listed as patterns.
fn ref_batches(refs: &[String]) -> impl Iterator<Item = &[String]> {
    const BUDGET: usize = 32 * 1024;
    let mut remaining = refs;
    std::iter::from_fn(move || {
        if remaining.is_empty() {
            return None;
        }
        let mut used = 0;
        let count = remaining
            .iter()
            .take_while(|refname| {
                used += refname.len() + 1;
                used <= BUDGET
            })
            .count()
            .max(1);
        let (batch, rest) = remaining.split_at(count);
        remaining = rest;
        Some(batch)
    })
}

impl<E: ProcessExecutor> RepositoryService<E> {
    /// The remote-tracking default branch, such as `refs/remotes/origin/main`:
    /// what `refs/remotes/<remote>/HEAD` points at, preferring `upstream`
    /// (a fork's parent, as `gh repo fork` names it) and then `origin`, then a
    /// remote `main` or `master`, then a local one. Reads refs only; it never
    /// contacts a remote.
    pub fn default_branch_ref(
        &self,
        repository: &Path,
        cancellation: &CancellationToken,
    ) -> Result<Option<String>, RepositoryError> {
        let command = self.git_read(
            repository,
            [
                "for-each-ref",
                "--format=%(refname)%00%(symref)%00",
                "refs/remotes/*/HEAD",
                "refs/remotes/origin/main",
                "refs/remotes/origin/master",
                "refs/heads/main",
                "refs/heads/master",
            ],
        );
        let output = self.run_checked(&command, cancellation)?;
        let refs = records(&output.stdout)
            .filter_map(|record| {
                let mut fields = record.split(|byte| *byte == 0);
                let refname = std::str::from_utf8(fields.next()?).ok()?;
                let target = std::str::from_utf8(fields.next().unwrap_or_default()).ok()?;
                Some((refname.to_owned(), target.to_owned()))
            })
            .collect::<Vec<_>>();
        let symbolic = |remote: Option<&str>| {
            refs.iter()
                .filter(|(refname, target)| {
                    refname.ends_with("/HEAD")
                        && target.starts_with("refs/")
                        && remote.is_none_or(|remote| {
                            refname.as_str() == format!("refs/remotes/{remote}/HEAD")
                        })
                })
                .map(|(_, target)| target.clone())
                .next()
        };
        let exists = |name: &str| refs.iter().any(|(refname, _)| refname == name);
        Ok(symbolic(Some("upstream"))
            .or_else(|| symbolic(Some("origin")))
            .or_else(|| symbolic(None))
            .or_else(|| {
                [
                    "refs/remotes/origin/main",
                    "refs/remotes/origin/master",
                    "refs/heads/main",
                    "refs/heads/master",
                ]
                .into_iter()
                .find(|name| exists(name))
                .map(str::to_owned)
            }))
    }

    /// Every ref matching `patterns` in one process, with its commit time,
    /// committer and checked-out worktree.
    pub fn ref_tips(
        &self,
        repository: &Path,
        patterns: &[&str],
        cancellation: &CancellationToken,
    ) -> Result<Vec<RefTip>, RepositoryError> {
        let command = self.git_read(
            repository,
            ["for-each-ref", &format!("--format={TIP_FORMAT}")]
                .into_iter()
                .chain(patterns.iter().copied())
                .map(OsString::from),
        );
        let output = self.run_checked(&command, cancellation)?;
        invalid_output(parse_ref_tips(&output.stdout), output)
    }

    /// Ahead/behind of each of `refs` (full ref names) against `base`, in as
    /// few processes as the argument budget allows. Refs that do not exist are
    /// absent from the result.
    pub fn divergence(
        &self,
        repository: &Path,
        base: &str,
        refs: &[String],
        cancellation: &CancellationToken,
    ) -> Result<BTreeMap<String, Divergence>, RepositoryError> {
        let mut counts = BTreeMap::new();
        let format = format!("--format=%(refname)%00%(ahead-behind:{base})%00");
        for batch in ref_batches(refs) {
            let command = self.git_read(
                repository,
                ["for-each-ref".into(), OsString::from(&format)]
                    .into_iter()
                    .chain(batch.iter().map(OsString::from)),
            );
            let output = self.run_checked(&command, cancellation)?;
            // A pattern also matches refs below it (`refs/heads/fix` matches
            // `refs/heads/fix/a`); keep exactly the refs asked about.
            let parsed = invalid_output(parse_divergence(&output.stdout), output)?;
            counts.extend(
                parsed
                    .into_iter()
                    .filter(|(refname, _)| batch.contains(refname)),
            );
        }
        Ok(counts)
    }

    /// Ahead/behind of a single revision, such as a detached worktree's head.
    pub fn revision_divergence(
        &self,
        repository: &Path,
        base: &str,
        revision: &str,
        cancellation: &CancellationToken,
    ) -> Result<Divergence, RepositoryError> {
        let command = self
            .git_read(
                repository,
                [
                    "rev-list".into(),
                    "--left-right".into(),
                    "--count".into(),
                    OsString::from(format!("{base}...{revision}")),
                    "--".into(),
                ],
            )
            .env("GIT_NO_LAZY_FETCH", "1");
        let output = self.run_checked(&command, cancellation)?;
        // rev-list prints the base side first: BEHIND AHEAD.
        let counts = parse_counts(output.stdout.trim_ascii()).ok_or_else(|| {
            RepositoryError::InvalidGitOutput {
                error: malformed("git rev-list --count", 0, "expected two counts"),
                process: output.clone(),
            }
        })?;
        Ok(Divergence {
            ahead: counts.behind,
            behind: counts.ahead,
        })
    }

    /// Whether `ancestor` is reachable from `descendant`. A commit that is not
    /// present locally is an error, never an answer.
    pub fn is_ancestor(
        &self,
        repository: &Path,
        ancestor: &str,
        descendant: &str,
        cancellation: &CancellationToken,
    ) -> Result<bool, RepositoryError> {
        let command = self
            .git_read(
                repository,
                [
                    "merge-base",
                    "--is-ancestor",
                    "--end-of-options",
                    ancestor,
                    descendant,
                ],
            )
            .env("GIT_NO_LAZY_FETCH", "1");
        let output = self.run(&command, cancellation)?;
        match output.exit_code {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(git_failure(&command, output).into()),
        }
    }

    /// One worktree's head and changed paths, as cheaply as Git can say: no
    /// rename detection, no upstream walk, and an untracked directory is
    /// reported once rather than file by file. Ignored trees are never read.
    pub fn activity_status(
        &self,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<StatusSnapshot, RepositoryError> {
        let command = self.git_read(
            worktree,
            [
                "status",
                "--porcelain=v2",
                "--branch",
                "--no-ahead-behind",
                "--untracked-files=normal",
                "--no-renames",
                "--ignore-submodules=dirty",
                "-z",
            ],
        );
        let output = self.run_checked(&command, cancellation)?;
        invalid_output(parse_status(&output.stdout), output)
    }

    /// The committer time of each of `oids`, such as detached worktree heads,
    /// in one process. Commits that are not present locally fail the read.
    pub fn commit_times(
        &self,
        repository: &Path,
        oids: &[String],
        cancellation: &CancellationToken,
    ) -> Result<BTreeMap<String, u64>, RepositoryError> {
        if oids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let command = self
            .git_read(
                repository,
                ["log", "--no-walk=unsorted", "--format=%H%x00%ct"]
                    .into_iter()
                    .map(OsString::from)
                    .chain(oids.iter().map(OsString::from))
                    .chain(["--".into()]),
            )
            .env("GIT_NO_LAZY_FETCH", "1");
        let output = self.run_checked(&command, cancellation)?;
        Ok(output
            .stdout
            .split(|byte| *byte == b'\n')
            .filter_map(|line| {
                let (oid, seconds) = std::str::from_utf8(line).ok()?.split_once('\0')?;
                let seconds = seconds.parse::<u64>().ok()?;
                Some((oid.to_owned(), seconds.saturating_mul(1000)))
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    const A: &str = "0123456789012345678901234567890123456789";
    const B: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn ref_tips_keep_worktree_paths_with_newlines_and_tolerate_missing_fields() {
        let output = format!(
            "refs/heads/main\0{A}\01700000000\0/repo\0\n\
             refs/heads/odd\0{B}\0\0/repo/new\nline\0\n"
        );
        let tips = parse_ref_tips(output.as_bytes()).unwrap();
        assert_eq!(tips.len(), 2);
        assert_eq!(tips[0].refname, "refs/heads/main");
        assert_eq!(tips[0].committed_at_unix_ms, Some(1_700_000_000_000));
        assert_eq!(tips[0].worktree, Some(PathBuf::from("/repo")));
        assert_eq!(tips[1].committed_at_unix_ms, None);
        assert_eq!(tips[1].worktree, Some(PathBuf::from("/repo/new\nline")));
        assert!(parse_ref_tips(b"").unwrap().is_empty());
    }

    #[test]
    fn malformed_ref_tips_are_refused() {
        assert!(parse_ref_tips(b"refs/heads/main\0nothex\0\0\0\0\n").is_err());
        assert!(parse_ref_tips(format!("refs/heads/main\0{A}\0\n").as_bytes()).is_err());
    }

    #[test]
    fn divergence_counts_parse_per_ref() {
        let counts = parse_divergence(b"refs/heads/a\x009 77\0\nrefs/heads/b\x000 0\0\n").unwrap();
        assert_eq!(
            counts["refs/heads/a"],
            Divergence {
                ahead: 9,
                behind: 77
            }
        );
        assert_eq!(
            counts["refs/heads/b"],
            Divergence {
                ahead: 0,
                behind: 0
            }
        );
        assert!(parse_divergence(b"refs/heads/a\x00nine\0\n").is_err());
    }

    #[test]
    fn ref_batches_cover_every_ref_in_order() {
        let refs = (0..5000)
            .map(|index| format!("refs/heads/branch-{index:05}"))
            .collect::<Vec<_>>();
        let batches = ref_batches(&refs).collect::<Vec<_>>();
        assert!(batches.len() > 1);
        assert_eq!(batches.concat(), refs);
    }

    /// Exercises the real Git commands against a throwaway repository with a
    /// linked worktree, a remote-tracking default branch and an ignored tree.
    #[test]
    fn census_reads_tips_divergence_status_and_the_default_branch() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        let git = |arguments: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(["-c", "user.name=Test", "-c", "user.email=me@example.com"])
                .args([
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "init.defaultBranch=main",
                ])
                .args(arguments)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .output()
                .unwrap();
            assert!(output.status.success(), "git {arguments:?}: {output:?}");
            String::from_utf8(output.stdout).unwrap()
        };
        git(&["init", "-q"]);
        std::fs::write(root.join("README"), "one\n").unwrap();
        std::fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "one"]);
        git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git(&[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ]);
        let linked = directory.path().join("linked");
        git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            linked.to_str().unwrap(),
        ]);
        std::fs::write(root.join("README"), "two\n").unwrap();
        git(&["commit", "-q", "-am", "two"]);
        std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        std::fs::write(root.join("node_modules/pkg/index.js"), "x").unwrap();
        std::fs::create_dir_all(root.join("scratch/deep")).unwrap();
        std::fs::write(root.join("scratch/deep/a"), "a").unwrap();
        std::fs::write(root.join("scratch/deep/b"), "b").unwrap();
        std::fs::write(root.join("README"), "three\n").unwrap();

        let service = RepositoryService::system("git");
        let cancellation = CancellationToken::new();
        assert_eq!(
            service
                .default_branch_ref(&root, &cancellation)
                .unwrap()
                .as_deref(),
            Some("refs/remotes/origin/main")
        );
        let tips = service
            .ref_tips(&root, &["refs/heads"], &cancellation)
            .unwrap();
        let main = tips
            .iter()
            .find(|tip| tip.refname == "refs/heads/main")
            .unwrap();
        assert!(main.committed_at_unix_ms.is_some());
        assert_eq!(
            main.worktree
                .as_deref()
                .map(fs::canonicalize)
                .transpose()
                .unwrap(),
            Some(fs::canonicalize(&root).unwrap())
        );
        assert!(main.committed_at_unix_ms.is_some());
        let feature = tips
            .iter()
            .find(|tip| tip.refname == "refs/heads/feature")
            .unwrap();
        assert_eq!(
            feature
                .worktree
                .as_deref()
                .map(fs::canonicalize)
                .transpose()
                .unwrap(),
            Some(fs::canonicalize(&linked).unwrap())
        );

        let counts = service
            .divergence(
                &root,
                "refs/remotes/origin/main",
                &["refs/heads/main".into(), "refs/heads/missing".into()],
                &cancellation,
            )
            .unwrap();
        assert_eq!(
            counts.get("refs/heads/main"),
            Some(&Divergence {
                ahead: 1,
                behind: 0
            })
        );
        assert!(!counts.contains_key("refs/heads/missing"));
        // The per-ref fallback for Git older than 2.41 agrees.
        assert_eq!(
            service
                .revision_divergence(
                    &root,
                    "refs/remotes/origin/main",
                    "refs/heads/main",
                    &cancellation
                )
                .unwrap(),
            counts["refs/heads/main"]
        );
        assert_eq!(
            service
                .revision_divergence(&root, "refs/remotes/origin/main", &main.oid, &cancellation)
                .unwrap(),
            Divergence {
                ahead: 1,
                behind: 0
            }
        );
        assert!(
            service
                .is_ancestor(&root, "refs/remotes/origin/main", "HEAD", &cancellation)
                .unwrap()
        );
        assert!(
            !service
                .is_ancestor(&root, "HEAD", "refs/remotes/origin/main", &cancellation)
                .unwrap()
        );
        assert!(
            service
                .is_ancestor(&root, B, "HEAD", &cancellation)
                .is_err(),
            "a missing commit is not an answer"
        );
        assert_eq!(
            service
                .commit_times(
                    &root,
                    &[main.oid.clone(), feature.oid.clone()],
                    &cancellation
                )
                .unwrap(),
            BTreeMap::from([
                (main.oid.clone(), main.committed_at_unix_ms.unwrap()),
                (feature.oid.clone(), feature.committed_at_unix_ms.unwrap()),
            ])
        );

        let status = service.activity_status(&root, &cancellation).unwrap();
        let paths = status
            .entries
            .iter()
            .map(|entry| entry.path.as_path().to_owned())
            .collect::<BTreeSet<_>>();
        // The untracked directory is one entry and the ignored tree is absent.
        assert_eq!(
            paths,
            BTreeSet::from([PathBuf::from("README"), PathBuf::from("scratch/")])
        );

        // A fork compares with its parent, not with itself.
        git(&["update-ref", "refs/remotes/upstream/main", "HEAD"]);
        git(&[
            "symbolic-ref",
            "refs/remotes/upstream/HEAD",
            "refs/remotes/upstream/main",
        ]);
        assert_eq!(
            service
                .default_branch_ref(&root, &cancellation)
                .unwrap()
                .as_deref(),
            Some("refs/remotes/upstream/main")
        );
    }
}
