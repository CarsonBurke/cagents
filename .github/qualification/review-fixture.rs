//! Private saved utility data for actual native review-tooltip qualification.
//! No actor, proof, authenticated viewer, provider, or review mutation is created.
use cagents::{
    model::*,
    persistence::{SnapshotBase, StateStore},
    repository::{CancellationToken, RepositoryService},
    session::SessionRuntime,
};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

fn private_directory(
    root: &Path,
    path: &Path,
    owner: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    if !path.starts_with(root) || fs::canonicalize(path)? != path {
        return Err("Directory escapes the canonical private root".into());
    }
    let mut current = path;
    loop {
        let metadata = fs::symlink_metadata(current)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() || metadata.uid() != owner {
            return Err("Private directory ownership or type changed".into());
        }
        if current == root {
            return Ok(());
        }
        current = current.parent().ok_or("Private directory has no parent")?;
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var_os("CAGENTS_AGENT_AUTHORITY").is_some() {
        return Err("Inherited caller authority; fixture refused without changes".into());
    }
    let root = PathBuf::from(std::env::args_os().nth(1).ok_or("Private root required")?);
    if fs::canonicalize(&root)? != root || !root.starts_with("/tmp") {
        return Err("Canonical private temporary root required".into());
    }
    let owner = fs::metadata("/proc/self")?.uid();
    private_directory(&root, &root, owner)?;
    if fs::metadata(&root)?.mode() & 0o7777 != 0o700 {
        return Err("Private root must have mode0700".into());
    }
    for key in [
        "HOME",
        "XDG_DATA_HOME",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
        "XDG_RUNTIME_DIR",
        "TMPDIR",
    ] {
        let path = PathBuf::from(std::env::var_os(key).ok_or("Private environment required")?);
        private_directory(&root, &path, owner)?;
        if fs::metadata(&path)?.mode() & 0o7777 != 0o700 {
            return Err("Private environment directories must have mode0700".into());
        }
    }
    let checkout = root.join("project");
    private_directory(&root, &checkout.join(".git"), owner)?;
    let discovered =
        RepositoryService::system("git").discover(&checkout, &CancellationToken::new())?;
    let repository = discovered.common_dir;
    private_directory(&root, &repository, owner)?;
    if repository != checkout.join(".git")
        || discovered.git_dir != repository
        || discovered.worktree_root.as_ref() != Some(&checkout)
        || discovered.is_bare
    {
        return Err("The private checkout's discovered Git identity changed".into());
    }
    let database = SessionRuntime::default_database_path()?;
    let data = PathBuf::from(std::env::var_os("XDG_DATA_HOME").ok_or("Private data required")?);
    if !database.starts_with(&data) || database == data || fs::symlink_metadata(&database).is_ok() {
        return Err("Refusing to overwrite an existing saved workspace".into());
    }
    // StateStore creates the missing suffix. Validate every existing ancestor
    // first, so an existing symlink cannot route that creation outside data.
    let mut parent = database.parent().ok_or("Database parent missing")?;
    while !parent.try_exists()? {
        if fs::symlink_metadata(parent).is_ok() {
            return Err("Database ancestor is a dangling symlink".into());
        }
        parent = parent.parent().ok_or("Database escaped private data")?;
    }
    private_directory(&root, parent, owner)?;
    if !parent.starts_with(&data) {
        return Err("Database ancestor escaped private data".into());
    }
    let project: ProjectId = "00000000-0000-4000-8000-000000001933".parse()?;
    let channel: ChannelId = "00000000-0000-4000-8000-000000002933".parse()?;
    let card: CardId = "00000000-0000-4000-8000-000000003933".parse()?;
    let workspace = WorkspaceState {
        project_order: vec![project],
        cards: vec![CardState {
            id: card,
            generation: 0,
            title: "Private review fixture".into(),
            width_fraction: 1.0,
            kind: CardKind::PullRequests(PullRequestView {
                repository: repository.clone(),
                selected_number: None,
                file_panel_width: Some(320),
                file_panel_collapsed: Some(false),
            }),
            existence: Existence::Open,
            exposure: Exposure::Visible,
            residency: Residency::Cold,
        }],
        channels: vec![ProjectChannel {
            id: channel,
            project_id: project,
            title: "Private review fixture".into(),
            title_source: Default::default(),
            ordinal: 0,
            shared: false,
            cards: vec![card],
            focused: Some(card),
        }],
        focused: Some(card),
        active_channel: Some(channel),
        ..Default::default()
    };
    let store = StateStore::open(&database, cagents::session::current_boot_id()?)?;
    store.save_project(&ProjectState {
        id: project,
        generation: 0,
        name: "Private review fixture".into(),
        repository: repository.clone(),
        kind: ProjectKind::Git,
        existence: Existence::Open,
    })?;
    store.save_workspace(
        &workspace,
        SnapshotBase {
            workspace: 0,
            cards: &BTreeMap::new(),
        },
    )?;
    // create_new atomically refuses existing regular files and symlinks.
    for (name, contents) in [
        ("review-workspace-id", workspace.id.to_string()),
        (
            "review-repository-path",
            repository.to_string_lossy().into_owned(),
        ),
        (
            "review-database-path",
            database.to_string_lossy().into_owned(),
        ),
    ] {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(root.join(name))?;
        file.write_all(contents.as_bytes())?;
    }
    Ok(())
}
