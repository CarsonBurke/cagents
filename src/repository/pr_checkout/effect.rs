use super::*;

struct PinnedDirectories {
    roots: [fs_safety::Root; 3],
    files: [Arc<fs::File>; 3],
    protection_files: Vec<Arc<fs::File>>,
}

impl PinnedDirectories {
    fn open(plan: &ExistingPrCheckoutPlan) -> Result<Self, String> {
        let identities = [&plan.worktree, &plan.git_dir, &plan.common_dir];
        let mut roots = Vec::new();
        let mut files = Vec::new();
        for identity in identities {
            let root = fs_safety::Root::open(&identity.path).map_err(|error| error.to_string())?;
            let metadata = root.metadata().map_err(|error| error.to_string())?;
            if (metadata.dev(), metadata.ino()) != (identity.device, identity.inode) {
                return Err(
                    "The selected directory was replaced; refresh the checkout confirmation".into(),
                );
            }
            files.push(Arc::new(
                root.retained_directory()
                    .map_err(|error| error.to_string())?,
            ));
            roots.push(root);
        }
        Ok(Self {
            roots: roots.try_into().unwrap(),
            files: files.try_into().unwrap(),
            protection_files: Vec::new(),
        })
    }

    fn path(&self, index: usize) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.files[index].as_raw_fd()))
    }

    fn check(&self, plan: &ExistingPrCheckoutPlan) -> Result<(), String> {
        for (index, identity) in [&plan.worktree, &plan.git_dir, &plan.common_dir]
            .into_iter()
            .enumerate()
        {
            if DirectoryIdentity::read(&identity.path)? != *identity {
                return Err(
                    "A selected directory moved or was replaced; refresh the checkout confirmation"
                        .into(),
                );
            }
            let held = self.roots[index]
                .metadata()
                .map_err(|error| error.to_string())?;
            if (held.dev(), held.ino()) != (identity.device, identity.inode) {
                return Err("The retained checkout identity changed".into());
            }
        }
        Ok(())
    }

    fn command<E: ProcessExecutor>(
        &self,
        service: &RepositoryService<E>,
        read: bool,
    ) -> Result<CommandSpec, String> {
        let arguments = [
            OsString::from(format!("--git-dir={}", self.path(1).display())),
            OsString::from(format!("--work-tree={}", self.path(0).display())),
        ];
        let mut command = (if read {
            service.git_read(&self.path(0), arguments)
        } else {
            service.git_mutation(&self.path(0), arguments)
        })
        .env("GIT_COMMON_DIR", self.path(2));
        for file in self.files.iter().chain(&self.protection_files) {
            command = command
                .inherit_file(file.clone())
                .map_err(|error| error.to_string())?;
        }
        Ok(command)
    }
}

/// Build a Git lock below an already-open administrative directory. Names in
/// `.git` are not trusted to remain beneath it through a second path lookup.
pub(super) fn lock_at(root: &fs_safety::Root, relative: &Path) -> Result<GitFileLock, String> {
    let (parent, mut name) = match fs_safety::open_parent_beneath(root, relative) {
        Ok(parent) => parent,
        Err(error) if error.is_not_found() && relative.starts_with("refs/heads") => {
            // Packed branches need not have any loose-ref parent directory.
            // Build only the validated branch namespace, one retained parent
            // at a time, never following a symlink or replacing an entry.
            let mut current = root.try_clone().map_err(|error| error.to_string())?;
            let directory = relative.parent().ok_or("Branch reference has no parent")?;
            for component in directory.components() {
                let std::path::Component::Normal(name) = component else {
                    return Err("Invalid branch reference namespace".into());
                };
                let child = Path::new(name).join(".cagents-parent-probe");
                match fs_safety::open_parent_beneath(&current, &child) {
                    Ok((next, _)) => current = next,
                    Err(error) if error.is_not_found() => {
                        use std::os::unix::ffi::OsStrExt;
                        let name = std::ffi::CString::new(name.as_bytes())
                            .map_err(|error| error.to_string())?;
                        let held = current
                            .retained_directory()
                            .map_err(|error| error.to_string())?;
                        // SAFETY: held owns the parent descriptor, name is one
                        // validated NUL-terminated component; mode respects
                        // the caller's umask, like an ordinary Git directory.
                        if unsafe { libc::mkdirat(held.as_raw_fd(), name.as_ptr(), 0o777) } != 0 {
                            let error = io::Error::last_os_error();
                            if error.kind() != io::ErrorKind::AlreadyExists {
                                return Err(error.to_string());
                            }
                        }
                        current = fs_safety::open_parent_beneath(&current, &child)
                            .map_err(|error| error.to_string())?
                            .0;
                    }
                    Err(error) => return Err(error.to_string()),
                }
            }
            let name = relative
                .file_name()
                .ok_or("Branch reference has no name")?
                .to_owned();
            (current, name)
        }
        Err(error) => return Err(error.to_string()),
    };
    name.push(".lock");
    let (file, temporary) =
        fs_safety::create_temporary_in(&parent, 0o600).map_err(|error| error.to_string())?;
    if let Err(error) = fs_safety::rename_new_within(&parent, &temporary, &name) {
        let _ = fs_safety::unlink_within(&parent, &temporary);
        return Err(format!(
            "Git operation is busy; the existing {} was preserved: {error}",
            Path::new(&name).display()
        ));
    }
    let version = GitFileLock::version(&file.metadata().map_err(|error| error.to_string())?);
    Ok(GitFileLock {
        parent,
        name,
        file,
        version,
        active: true,
    })
}

#[cfg(test)]
thread_local! {
    pub(super) static RECONCILIATION_TEST_HOOK: std::cell::RefCell<Option<Box<dyn FnMut(&str)>>> = const { std::cell::RefCell::new(None) };
}

fn test_phase(_phase: &str) {
    #[cfg(test)]
    RECONCILIATION_TEST_HOOK.with_borrow_mut(|hook| {
        if let Some(hook) = hook {
            hook(_phase);
        }
    });
}

fn unchanged_source(
    source: &fs::File,
    original: &fs::Metadata,
    root: &fs_safety::Root,
) -> Result<(), String> {
    let named = fs_safety::metadata_beneath(root, Path::new("config"))
        .map_err(|error| error.to_string())?;
    if GitFileLock::version(&source.metadata().map_err(|error| error.to_string())?)
        != GitFileLock::version(original)
        || GitFileLock::version(&named) != GitFileLock::version(original)
    {
        return Err("Clone configuration changed during tracking repair; it was preserved".into());
    }
    Ok(())
}

fn repair_tracking<E: ProcessExecutor>(
    service: &RepositoryService<E>,
    plan: &ExistingPrCheckoutPlan,
    pinned: &PinnedDirectories,
    private: &Path,
    cancellation: &CancellationToken,
    changed: &mut bool,
) -> Result<(), String> {
    // Status reads suppress optional index locks. Every native writer must
    // acquire at least one of these before changing the confirmed branch,
    // checkout, or clone-local configuration.
    let _head = lock_at(&pinned.roots[1], Path::new("HEAD"))?;
    let _branch = lock_at(&pinned.roots[2], Path::new(&plan.request.branch.full_ref()))?;
    let _index = lock_at(&pinned.roots[1], Path::new("index"))?;
    let mut config = lock_at(&pinned.roots[2], Path::new("config"))?;
    pinned.check(plan)?;
    if service
        .recheck_existing_pr_checkout(plan, cancellation)
        .map_err(|error| error.to_string())?
        != *plan
    {
        return Err(
            "The checkout or its configuration changed after confirmation; refresh it".into(),
        );
    }
    let mut source = fs_safety::open_beneath(&pinned.roots[2], Path::new("config"))
        .map_err(|error| error.to_string())?;
    let original = source.metadata().map_err(|error| error.to_string())?;
    if !original.is_file() || original.nlink() != 1 {
        return Err("The clone-local configuration is not a single regular file; reconcile its upstream explicitly".into());
    }
    test_phase("after_source_metadata");
    let bytes =
        fs_safety::read_bounded(&mut source, CONFIG_LIMIT).map_err(|error| error.to_string())?;
    if bytes.len() as u64 > CONFIG_LIMIT {
        return Err("Clone configuration is too large; it was preserved".into());
    }
    // Freeze metadata beside the captured bytes inside the private0700
    // operation directory. The externally named config.lock stays0600.
    // Never copy live source ACL/ownership after validating this snapshot.
    let metadata_snapshot =
        fs::File::create(private.join("config.metadata")).map_err(|error| error.to_string())?;
    metadata_snapshot
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| error.to_string())?;
    fs_safety::copy_ownership(&source, &metadata_snapshot).map_err(|error| error.to_string())?;
    fs_safety::copy_access_acl(&source, &metadata_snapshot).map_err(|error| error.to_string())?;
    let uncopied = fs_safety::copy_extended_attributes(&source, &metadata_snapshot);
    if !uncopied.is_empty() {
        return Err(format!(
            "Configuration metadata could not be captured: {}",
            uncopied[0]
        ));
    }
    test_phase("before_source_validation");
    unchanged_source(&source, &original, &pinned.roots[2])?;
    metadata_snapshot
        .set_permissions(original.permissions())
        .map_err(|error| error.to_string())?;
    let staged = private.join("config.staged");
    fs::write(&staged, bytes).map_err(|error| error.to_string())?;
    for (key, value) in [
        (
            format!("branch.{}.remote", plan.request.branch.as_str()),
            plan.request.remote.clone(),
        ),
        (
            format!("branch.{}.merge", plan.request.branch.as_str()),
            plan.request.head_branch.full_ref(),
        ),
    ] {
        let command = pinned.command(service, false)?.args([
            OsString::from("config"),
            "--file".into(),
            staged.as_os_str().to_owned(),
            "--add".into(),
            key.into(),
            value.into(),
        ]);
        service
            .run_checked(&command, cancellation)
            .map_err(|error| error.reported_detail())?;
    }
    let mut staged = fs::File::open(&staged).map_err(|error| error.to_string())?;
    let bytes =
        fs_safety::read_bounded(&mut staged, CONFIG_LIMIT).map_err(|error| error.to_string())?;
    if bytes.len() as u64 > CONFIG_LIMIT {
        return Err("Updated clone configuration is too large; the original was preserved".into());
    }
    config.check()?;
    // Recheck the source before making the lock visible under its old mode.
    // All transferred metadata comes from the coherent private snapshot.
    unchanged_source(&source, &original, &pinned.roots[2])?;
    fs_safety::copy_ownership(&metadata_snapshot, &config.file)
        .map_err(|error| error.to_string())?;
    fs_safety::copy_access_acl(&metadata_snapshot, &config.file)
        .map_err(|error| error.to_string())?;
    let uncopied = fs_safety::copy_extended_attributes(&metadata_snapshot, &config.file);
    if !uncopied.is_empty() {
        return Err(format!(
            "Configuration metadata could not be preserved: {}",
            uncopied[0]
        ));
    }
    config
        .file
        .set_permissions(original.permissions())
        .map_err(|error| error.to_string())?;
    config
        .file
        .write_all(&bytes)
        .and_then(|()| config.file.sync_all())
        .map_err(|error| error.to_string())?;
    // Only this operation wrote the retained lock. Adopt its new version,
    // then verify both the held fd and its directory entry before publishing.
    config.version =
        GitFileLock::version(&config.file.metadata().map_err(|error| error.to_string())?);
    config.check()?;
    pinned.check(plan)?;
    unchanged_source(&source, &original, &pinned.roots[2])?;
    if service
        .recheck_existing_pr_checkout(plan, cancellation)
        .map_err(|error| error.to_string())?
        != *plan
    {
        return Err(
            "The checkout or effective configuration changed before publication; refresh it".into(),
        );
    }
    *changed = true;
    fs_safety::rename_within(&config.parent, &config.name, OsStr::new("config"))
        .map_err(|error| error.to_string())?;
    config.active = false;
    fs_safety::sync_directory(&config.parent).map_err(|error| {
        format!("Tracking was published, but configuration durability is unknown: {error}")
    })
}

fn transaction<E: ProcessExecutor>(
    service: &RepositoryService<E>,
    plan: &ExistingPrCheckoutPlan,
    pinned: &PinnedDirectories,
    private: &Path,
    lease: &Arc<fs::File>,
    probe: bool,
    cancellation: &CancellationToken,
) -> Result<(), String> {
    pinned.check(plan)?;
    let previous_command = pinned.command(service, true)?.args([
        "rev-parse",
        "--path-format=absolute",
        "--git-path",
        "hooks/reference-transaction",
    ]);
    let previous = service
        .run_checked(&previous_command, cancellation)
        .map_err(|error| error.reported_detail())?;
    let previous =
        parse_path_line(&previous.stdout, "reference_hook").map_err(|error| error.to_string())?;
    let hook = private.join("reference-transaction");
    if !hook.exists() {
        fs::write(&hook, include_bytes!("transaction.sh")).map_err(|error| error.to_string())?;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
        fs::write(
            private.join("submodule-status"),
            include_bytes!("submodule_status.sh"),
        )
        .map_err(|error| error.to_string())?;
        fs::write(
            private.join("check-new-paths"),
            include_bytes!("check_new_paths.sh"),
        )
        .map_err(|error| error.to_string())?;
    }
    fs::write(private.join("config-expected"), &plan.config).map_err(|error| error.to_string())?;
    if probe && private.join("probe").exists() {
        fs::remove_file(private.join("probe")).map_err(|error| error.to_string())?;
    }
    let zero = "0".repeat(plan.current_head.as_str().len());
    let new = if probe {
        plan.current_head.as_str()
    } else {
        plan.request.expected_head.as_str()
    };
    let update_remote = !probe && plan.request.tracking == PrCheckoutTracking::RemoteBranch;
    let remote_ref = format!(
        "refs/remotes/{}/{}",
        plan.request.remote,
        plan.request.head_branch.as_str()
    );
    let remote_old = plan
        .remote_tracking
        .as_ref()
        .map_or(zero.as_str(), Oid::as_str);
    let mut input = format!("start\nupdate HEAD {new} {}\n", plan.current_head.as_str());
    if update_remote {
        input.push_str(&format!(
            "option no-deref\nupdate {remote_ref} {new} {remote_old}\n"
        ));
    }
    input.push_str(if probe {
        "prepare\nabort\n"
    } else {
        "prepare\ncommit\n"
    });
    let mut hooks = OsString::from("core.hooksPath=");
    hooks.push(private);
    let original = std::env::var_os("GIT_CONFIG_PARAMETERS").unwrap_or_default();
    let mut policy_config = original.clone();
    if !policy_config.is_empty() {
        policy_config.push(" ");
    }
    policy_config.push("'core.fsmonitor=false'");
    let command = pinned
        .command(service, false)?
        .args([
            OsString::from("-c"),
            hooks,
            "update-ref".into(),
            "--stdin".into(),
        ])
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env(
            "CAGENTS_PR_GIT",
            complete_inventory::resolved_git_executable(&service.git)?,
        )
        .env("CAGENTS_PR_PRIVATE", private)
        .env("CAGENTS_PR_MODE", if probe { "probe" } else { "effect" })
        .env("CAGENTS_PR_WORKTREE", &plan.worktree.path)
        .env("CAGENTS_PR_GITDIR", &plan.git_dir.path)
        .env("CAGENTS_PR_COMMON", &plan.common_dir.path)
        .env("CAGENTS_PR_WORKTREE_FD", pinned.path(0))
        .env("CAGENTS_PR_GITDIR_FD", pinned.path(1))
        .env("CAGENTS_PR_COMMON_FD", pinned.path(2))
        .env(
            "CAGENTS_PR_WORKTREE_ID",
            format!("{}:{}", plan.worktree.device, plan.worktree.inode),
        )
        .env(
            "CAGENTS_PR_GITDIR_ID",
            format!("{}:{}", plan.git_dir.device, plan.git_dir.inode),
        )
        .env(
            "CAGENTS_PR_COMMON_ID",
            format!("{}:{}", plan.common_dir.device, plan.common_dir.inode),
        )
        .env("CAGENTS_PR_BRANCH", plan.request.branch.full_ref())
        .env(
            "CAGENTS_PR_EXPECTED_UPSTREAM",
            if plan.tracking.remote.is_empty() {
                String::new()
            } else {
                format!(
                    "refs/remotes/{}/{}",
                    plan.request.remote,
                    plan.request.head_branch.as_str()
                )
            },
        )
        .env("CAGENTS_PR_OLD", plan.current_head.as_str())
        .env("CAGENTS_PR_NEW", new)
        .env("CAGENTS_PR_REMOTE_REF", remote_ref)
        .env(
            "CAGENTS_PR_UPDATE_REMOTE",
            if update_remote { "1" } else { "0" },
        )
        .env("CAGENTS_PR_REMOTE_OLD", remote_old)
        .env("CAGENTS_PR_PREVIOUS_HOOK", previous)
        .env("CAGENTS_PR_ORIGINAL_CONFIG", original)
        .env("CAGENTS_PR_PREVIOUS_CONFIG", policy_config)
        .stdin(input.into_bytes())
        .inherit_file(lease.clone())
        .map_err(|error| error.to_string())?;
    let result = service
        .run_checked(&command, cancellation)
        .map_err(|error| error.reported_detail());
    drop(command);
    result?;
    if fs::read(private.join(if probe { "probe" } else { "effect-prepared" }))
        .map_err(|error| error.to_string())?
        != b"prepared"
    {
        return Err("Git did not retain its prepared transaction proof; inspect the retained checkout operation".into());
    }
    if !probe
        && (fs::read(private.join("ref-committed")).map_err(|error| error.to_string())?
            != b"committed"
            || private.join("policy-failed").exists())
    {
        return Err(
            "The reference transaction or repository policy did not settle successfully".into(),
        );
    }
    Ok(())
}

/// Keep accepted-operation ownership until every holder has closed the original
/// flock description. Cancellation requests do not mean detached children exited.
/// A flock cannot attribute the holder: an unrelated fork can inherit this
/// CLOEXEC descriptor until exec. Unknown holders still require a refusal.
fn settle_lease(observer: &fs::File) -> bool {
    let mut observed_holder = false;
    loop {
        // SAFETY: observer is an independent, owned open description of the
        // original lease inode, retained before any subprocess can replace it.
        if unsafe { libc::flock(observer.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return observed_holder;
        }
        if !observed_holder {
            test_phase("retained_lease_observed");
            observed_holder = true;
        }
        // A noncooperating policy child deliberately leaves this operation
        // unsettled. Do not clear UI busy/close guards or directory protection.
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn fetch_captured_head<E: ProcessExecutor>(
    service: &RepositoryService<E>,
    plan: &ExistingPrCheckoutPlan,
    pinned: &PinnedDirectories,
    private: &Path,
    private_ref: &str,
    lease: &Arc<fs::File>,
    cancellation: &CancellationToken,
) -> Result<(), String> {
    let count = match std::env::var_os("GIT_CONFIG_COUNT") {
        None => 0,
        Some(value) => value
            .to_str()
            .and_then(|value| value.parse::<u32>().ok())
            .ok_or("Git environment configuration count is invalid")?,
    };
    if count >= i32::MAX as u32 {
        return Err("Git environment configuration count cannot be extended safely".into());
    }
    let alias = format!("cagents-pr-fetch-{}", Uuid::new_v4().simple());
    let endpoint = plan
        .remote
        .fetch_url
        .as_ref()
        .ok_or("PR remote has no fetch URL")?;
    // A synthetic remote carries the captured RAW endpoint in environment
    // configuration. Git applies its usual URL rewrite once, while neither a
    // credential-bearing URL nor a rewritten secret enters process argv.
    let fetch = pinned
        .command(service, false)?
        .args([
            OsString::from("fetch"),
            "--quiet".into(),
            "--no-tags".into(),
            "--no-recurse-submodules".into(),
            "--no-write-fetch-head".into(),
            "--refmap=".into(),
            "--end-of-options".into(),
            alias.clone().into(),
            format!("+{}:{private_ref}", plan.request.head_branch.full_ref()).into(),
        ])
        .env("GIT_CONFIG_COUNT", (count + 1).to_string())
        .env(
            format!("GIT_CONFIG_KEY_{count}"),
            format!("remote.{alias}.url"),
        )
        .env(format!("GIT_CONFIG_VALUE_{count}"), endpoint)
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .inherit_file(lease.clone())
        .map_err(|error| error.to_string())?;
    // Generic repository failure logging includes both argv and stderr.
    // Transport stderr may repeat credentials even with a safe alias, so
    // capture it privately and return only typed status metadata to the UI.
    let outcome = service.executor.execute(&fetch, cancellation);
    drop(fetch);
    let (stdout, stderr, detail) = match outcome {
        Ok(output) if output.success => return Ok(()),
        Ok(output) => (
            output.stdout,
            output.stderr,
            format!(
                "Fetching the reviewed PR head failed (status {:?}, signal {:?})",
                output.exit_code, output.signal
            ),
        ),
        Err(failure) => (
            failure.stdout,
            failure.stderr,
            format!(
                "Fetching the reviewed PR head failed ({:?}, status {:?}, signal {:?})",
                failure.kind, failure.exit_code, failure.signal
            ),
        ),
    };
    fs::write(private.join("fetch.stdout"), stdout)
        .map_err(|_| "Private fetch diagnostics could not be retained".to_owned())?;
    fs::write(private.join("fetch.stderr"), stderr)
        .map_err(|_| "Private fetch diagnostics could not be retained".to_owned())?;
    Err(format!(
        "{detail}; transport diagnostics are retained in the private operation directory"
    ))
}

pub(super) fn reconcile<E: ProcessExecutor>(
    service: &RepositoryService<E>,
    plan: &ExistingPrCheckoutPlan,
    cancellation: &CancellationToken,
) -> Result<VerifiedPrCheckout, PrCheckoutFailure> {
    let refuse = |detail: String| PrCheckoutFailure::refused(plan.path(), detail);
    let fresh = service.recheck_existing_pr_checkout(plan, cancellation)?;
    if fresh != *plan {
        return Err(refuse(
            "The checkout changed after confirmation; refresh and review its new state".into(),
        ));
    }
    let mut pinned = PinnedDirectories::open(plan).map_err(refuse)?;
    pinned.check(plan).map_err(refuse)?;
    let verified = || VerifiedPrCheckout {
        disposition: plan.disposition,
        path: plan.path().to_owned(),
        branch: plan.request.branch.clone(),
        head: if matches!(
            plan.disposition,
            PrCheckoutDisposition::OpenPreservingLocal
                | PrCheckoutDisposition::RepairTrackingPreservingLocal
        ) {
            plan.current_head.clone()
        } else {
            plan.request.expected_head.clone()
        },
        upstream: (plan.request.tracking == PrCheckoutTracking::RemoteBranch).then(|| {
            format!(
                "refs/remotes/{}/{}",
                plan.request.remote,
                plan.request.head_branch.as_str()
            )
        }),
    };
    let _protection = crate::worktree_cleanup::protect_operation(plan.path())
        .map_err(|error| refuse(error.to_string()))?;
    pinned.protection_files = _protection
        .retained_child_files()
        .map_err(|error| refuse(error.to_string()))?;
    let private = tempfile::Builder::new()
        .prefix("cagents-pr-checkout-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(pinned.path(2))
        .map_err(|error| refuse(error.to_string()))?;
    let private_name = private
        .path()
        .file_name()
        .ok_or_else(|| refuse("Checkout operation has no directory name".into()))?
        .to_owned();
    let lease_path = private.path().join("lease");
    let lease = Arc::new(
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&lease_path)
            .map_err(|error| refuse(error.to_string()))?,
    );
    fs::set_permissions(&lease_path, fs::Permissions::from_mode(0o600))
        .map_err(|error| refuse(error.to_string()))?;
    let lease_observer = fs::File::open(&lease_path).map_err(|error| refuse(error.to_string()))?;
    // SAFETY: this live owned descriptor retains the operation's flock.
    if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(refuse(io::Error::last_os_error().to_string()));
    }
    let private_ref = format!("refs/cagents/pr-checkout/{}/head", Uuid::new_v4());
    let mut changed = false;
    let mut expected_final = plan.clone();
    let attempt = (|| -> Result<(), String> {
        // Effect-free capability proof: no repository policy or checkout runs.
        transaction(
            service,
            plan,
            &pinned,
            private.path(),
            &lease,
            true,
            cancellation,
        )?;
        if matches!(
            plan.disposition,
            PrCheckoutDisposition::AlreadyCurrent | PrCheckoutDisposition::OpenPreservingLocal
        ) {
            return Ok(());
        }
        if plan.disposition == PrCheckoutDisposition::RepairTrackingPreservingLocal {
            repair_tracking(
                service,
                plan,
                &pinned,
                private.path(),
                cancellation,
                &mut changed,
            )?;
            let current = service
                .recheck_existing_pr_checkout(plan, cancellation)
                .map_err(|error| error.to_string())?;
            if current.disposition != PrCheckoutDisposition::OpenPreservingLocal
                || current.worktree != plan.worktree
                || current.git_dir != plan.git_dir
                || current.common_dir != plan.common_dir
                || current.current_head != plan.current_head
                || current.remote != plan.remote
                || current.remote_tracking != plan.remote_tracking
                || current.dirty != plan.dirty
            {
                return Err("The local checkout changed during tracking-only repair; inspect the retained operation before opening".into());
            }
            transaction(
                service,
                &current,
                &pinned,
                private.path(),
                &lease,
                true,
                cancellation,
            )?;
            expected_final = current;
            return Ok(());
        }
        fetch_captured_head(
            service,
            plan,
            &pinned,
            private.path(),
            &private_ref,
            &lease,
            cancellation,
        )?;
        let head = pinned.command(service, true)?.args([
            "rev-parse",
            "--verify",
            "--end-of-options",
            &private_ref,
        ]);
        let head = service
            .run_checked(&head, cancellation)
            .map_err(|error| error.reported_detail())?;
        if parse_utf8_line(&head.stdout, "fetched_pr_head").map_err(|error| error.to_string())?
            != plan.request.expected_head.as_str()
        {
            return Err(
                "The fetched pull-request head changed; refresh the review before checking it out"
                    .into(),
            );
        }
        pinned.check(plan)?;
        if service
            .recheck_existing_pr_checkout(plan, cancellation)
            .map_err(|error| error.to_string())?
            != *plan
        {
            return Err(
                "The checkout or its configuration changed during fetch; refresh it".into(),
            );
        }
        if plan.current_head != plan.request.expected_head {
            let ancestor = pinned
                .command(service, true)?
                .args([
                    "merge-base",
                    "--is-ancestor",
                    plan.current_head.as_str(),
                    plan.request.expected_head.as_str(),
                ])
                .env("GIT_NO_REPLACE_OBJECTS", "1");
            service.run_checked(&ancestor, cancellation).map_err(|_| {
                "The local PR branch is ahead or diverged; its commits were preserved".to_owned()
            })?;
        }
        let repair = matches!(
            plan.disposition,
            PrCheckoutDisposition::RepairTracking | PrCheckoutDisposition::FastForwardAndRepair
        );
        if repair {
            repair_tracking(
                service,
                plan,
                &pinned,
                private.path(),
                cancellation,
                &mut changed,
            )?;
        }
        let settled = service
            .recheck_existing_pr_checkout(plan, cancellation)
            .map_err(|error| error.to_string())?;
        // Repair intentionally changes only tracking configuration. Every
        // physical identity and the exact original commit must still match.
        if settled.worktree != plan.worktree
            || settled.git_dir != plan.git_dir
            || settled.common_dir != plan.common_dir
            || settled.current_head != plan.current_head
            || settled.remote != plan.remote
            || settled.remote_tracking != plan.remote_tracking
            || settled.dirty != plan.dirty
        {
            return Err("The checkout changed while its upstream was configured; inspect the retained operation".into());
        }
        transaction(
            service,
            &settled,
            &pinned,
            private.path(),
            &lease,
            false,
            cancellation,
        )?;
        changed = true;
        // Cleanup is also a Git mutation that can invoke repository policy.
        // Keep its descendants inside the operation lease and perform final
        // checkout verification only after it has settled.
        let cleanup = pinned
            .command(service, false)?
            .args([
                "update-ref",
                "-d",
                &private_ref,
                plan.request.expected_head.as_str(),
            ])
            .inherit_file(lease.clone())
            .map_err(|error| error.to_string())?;
        service
            .run_checked(&cleanup, cancellation)
            .map_err(|error| {
                format!("Checkout updated, but private-ref cleanup was refused: {error}")
            })?;
        drop(cleanup);
        let current = service
            .recheck_existing_pr_checkout(plan, cancellation)
            .map_err(|error| error.to_string())?;
        pinned.check(plan)?;
        if current.disposition != PrCheckoutDisposition::AlreadyCurrent
            || current.worktree != plan.worktree
            || current.git_dir != plan.git_dir
            || current.common_dir != plan.common_dir
            || current.dirty != plan.dirty
        {
            return Err("The checkout changed before its final verification".into());
        }
        // Bind final effective tracking and HEAD inside a fresh effect-free
        // native reference transaction, while its HEAD/branch locks are held.
        transaction(
            service,
            &current,
            &pinned,
            private.path(),
            &lease,
            true,
            cancellation,
        )?;
        expected_final = current;
        Ok(())
    })();
    let retained_directory = fs::read_link(pinned.path(2))
        .unwrap_or_else(|_| plan.common_dir.path.clone())
        .join(private_name);
    drop(lease);
    test_phase("before_lease_observation");
    let observed_holder = settle_lease(&lease_observer);
    let lease_settlement = if observed_holder {
        Err("The operation lease remained held after its command and has now been released. Its holder could not be identified; a policy or Git descendant, or an unrelated child before exec, may have inherited it. Refresh and inspect the checkout before continuing".to_owned())
    } else {
        Ok(())
    };
    let state = || -> String {
        match service.recheck_existing_pr_checkout(plan, &CancellationToken::default()) {
            Ok(current) => format!(
                "Current branch {} at {}; upstream state {:?}",
                current.request.branch.as_str(),
                current.current_head,
                current.disposition
            ),
            Err(error) => format!("Current checkout could not be verified: {error}"),
        }
    };
    let effects = changed
        || private.path().join("checkout-started").exists()
        || private.path().join("ref-committed").exists();
    if let Err(detail) = attempt.and(lease_settlement) {
        let current = state();
        let detail = format!(
            "{detail}. {current}. Operation evidence: {}",
            retained_directory.display()
        );
        let _retained_path = private.keep();
        return Err(PrCheckoutFailure {
            kind: if effects || observed_holder {
                PrCheckoutFailureKind::PartialOrUnknown
            } else {
                PrCheckoutFailureKind::Refused
            },
            detail,
            path: plan.path().to_owned(),
            retained_ref: Some(private_ref),
        });
    }
    // The prior native proof may precede the last owned policy child's
    // effects. Re-read only after all inherited leases have truly retired,
    // including children that exited before the first observer flock.
    let final_state = service.recheck_existing_pr_checkout(plan, &CancellationToken::default());
    if !matches!(&final_state, Ok(current) if current == &expected_final) {
        let detail = match final_state {
            Ok(_) => "The checkout changed after its last policy hook; refresh it before opening"
                .to_owned(),
            Err(error) => {
                format!("The checkout could not be verified after its last policy hook: {error}")
            }
        };
        let detail = format!(
            "{detail}. Operation evidence: {}",
            retained_directory.display()
        );
        let _retained_path = private.keep();
        return Err(PrCheckoutFailure {
            kind: PrCheckoutFailureKind::PartialOrUnknown,
            detail,
            path: plan.path().to_owned(),
            retained_ref: Some(private_ref),
        });
    }
    if let Err(detail) = pinned.check(plan) {
        let detail = format!(
            "{detail}. {}. Operation evidence: {}",
            state(),
            retained_directory.display()
        );
        let _retained_path = private.keep();
        return Err(PrCheckoutFailure {
            kind: PrCheckoutFailureKind::PartialOrUnknown,
            detail,
            path: plan.path().to_owned(),
            retained_ref: Some(private_ref),
        });
    }
    Ok(verified())
}
