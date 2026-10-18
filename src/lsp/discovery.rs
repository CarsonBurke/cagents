//! Bounded filesystem discovery, off the update thread. Unknown workspace
//! membership falls back to the nearest project rather than absorbing it.
//!
//! Supports Cargo member globs/exclusions, in-root path dependencies and explicit
//! package.workspace links within the opened worktree; npm workspace arrays,
//! Yarn packages objects, pnpm scalar packages lists (including quoted patterns
//! and exclusions), and JSONC TypeScript references. This is conservative root
//! selection, not full package-manager configuration parity. YAML aliases and
//! anchors, excessive structure, and uncertain manifests keep projects separate.
use super::*;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::OnceLock;

pub(super) const MAX_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_PROBES: usize = 512;
pub(super) const MAX_PENDING: usize = 256;
const MAX_WORKERS: usize = 2;
pub(super) const BUSY_REASON: &str = "language-server root discovery is busy; retry this document";

pub(super) fn worker_slots() -> Arc<tokio::sync::Semaphore> {
    static SLOTS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    Arc::clone(SLOTS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(MAX_WORKERS))))
}
pub(super) fn admission_slots() -> Arc<tokio::sync::Semaphore> {
    static SLOTS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    Arc::clone(SLOTS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(MAX_PENDING))))
}

struct Budget<'a> {
    cancelled: &'a AtomicBool,
    probes: usize,
    bytes: usize,
    root: PathBuf,
}
impl Budget<'_> {
    fn step(&mut self) -> Option<()> {
        if self.cancelled.load(Ordering::Acquire) || self.probes >= MAX_PROBES {
            return None;
        }
        self.probes += 1;
        Some(())
    }
    fn directory(&mut self, path: &Path) -> Option<PathBuf> {
        self.step()?;
        let path = path.canonicalize().ok()?;
        path.starts_with(&self.root).then_some(path)
    }
    fn marker(&mut self, path: &Path) -> bool {
        let Some(()) = self.step() else {
            return false;
        };
        // Presence and read eligibility are different. A FIFO, broken link or
        // unreadable descriptor is an uncertain project boundary, not absence.
        !matches!(std::fs::symlink_metadata(path), Err(error) if error.kind() == io::ErrorKind::NotFound)
    }
    fn read(&mut self, path: &Path) -> Option<Vec<u8>> {
        self.step()?;
        let path = path.canonicalize().ok()?;
        if !path.starts_with(&self.root) {
            return None;
        }
        // Open before fstat with NONBLOCK: a concurrent replacement by a FIFO
        // cannot block between a path-level type check and the actual open.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() || metadata.len() > MAX_MANIFEST_BYTES as u64 {
            return None;
        }
        if self.bytes + MAX_MANIFEST_BYTES > MAX_BYTES {
            return None;
        }
        let mut bytes = Vec::new();
        file.take((MAX_MANIFEST_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .ok()?;
        self.bytes += bytes.len();
        if bytes.len() > MAX_MANIFEST_BYTES || self.cancelled.load(Ordering::Acquire) {
            return None;
        }
        Some(bytes)
    }
    fn cargo(&mut self, directory: &Path) -> Option<toml::Value> {
        toml::from_str(std::str::from_utf8(&self.read(&directory.join("Cargo.toml"))?).ok()?).ok()
    }
}

/// TypeScript configuration permits comments and trailing commas. Preserve
/// quoted strings byte-for-byte while normalizing those two JSON extensions.
pub(super) fn workspace_manifest(mut bytes: Vec<u8>, jsonc: bool) -> Option<serde_json::Value> {
    if jsonc {
        let mut index = 0;
        let mut quoted = false;
        let mut escaped = false;
        while index < bytes.len() {
            let byte = bytes[index];
            if quoted {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    quoted = false;
                }
                index += 1;
            } else if byte == b'"' {
                quoted = true;
                index += 1;
            } else if bytes.get(index..index + 2) == Some(b"//") {
                while index < bytes.len() && bytes[index] != b'\n' {
                    bytes[index] = b' ';
                    index += 1;
                }
            } else if bytes.get(index..index + 2) == Some(b"/*") {
                bytes[index..index + 2].fill(b' ');
                index += 2;
                loop {
                    if bytes.get(index..index + 2) == Some(b"*/") {
                        bytes[index..index + 2].fill(b' ');
                        index += 2;
                        break;
                    }
                    let byte = bytes.get_mut(index)?;
                    if *byte != b'\n' {
                        *byte = b' ';
                    }
                    index += 1;
                }
            } else {
                index += 1;
            }
        }
        quoted = false;
        escaped = false;
        for index in 0..bytes.len() {
            let byte = bytes[index];
            if quoted {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    quoted = false;
                }
            } else if byte == b'"' {
                quoted = true;
            } else if byte == b','
                && bytes[index + 1..]
                    .iter()
                    .find(|byte| !byte.is_ascii_whitespace())
                    .is_some_and(|byte| matches!(byte, b']' | b'}'))
            {
                bytes[index] = b' ';
            }
        }
    }
    serde_json::from_slice(&bytes).ok()
}

fn patterns(value: Option<&toml::Value>) -> Option<Vec<String>> {
    value?
        .as_array()?
        .iter()
        .map(|entry| entry.as_str().map(str::to_owned))
        .collect()
}

fn relative_directory(root: &Path, path: &Path) -> PathBuf {
    let root: Vec<_> = root.components().collect();
    let path: Vec<_> = path.components().collect();
    let common = root
        .iter()
        .zip(&path)
        .take_while(|(left, right)| left == right)
        .count();
    let mut relative = PathBuf::new();
    for _ in common..root.len() {
        relative.push("..");
    }
    for component in &path[common..] {
        relative.push(component.as_os_str());
    }
    relative
}

fn matches(pattern: &str, relative: &Path) -> bool {
    let normalized: PathBuf = Path::new(pattern).components().collect();
    let Some(pattern) = normalized.to_str() else {
        return false;
    };
    glob::Pattern::new(pattern).is_ok_and(|pattern| {
        pattern.matches_path_with(
            relative,
            glob::MatchOptions {
                case_sensitive: true,
                require_literal_separator: true,
                require_literal_leading_dot: false,
            },
        )
    })
}

/// Expand member globs ourselves so a single glob iterator cannot scan an
/// unbounded tree before yielding its next match. Directory entries, graph
/// edges and manifest reads all consume the same finite discovery budget.
fn expand(root: &Path, pattern: &str, budget: &mut Budget<'_>) -> Option<Vec<PathBuf>> {
    let parts: Vec<_> = Path::new(pattern).components().collect();
    let mut queue = VecDeque::from([(root.to_path_buf(), 0)]);
    let mut found = Vec::new();
    while let Some((path, index)) = queue.pop_front() {
        budget.step()?;
        if index == parts.len() {
            if let Some(path) = budget.directory(&path) {
                found.push(path);
            }
            continue;
        }
        let part = match parts[index] {
            std::path::Component::CurDir => {
                queue.push_back((path, index + 1));
                continue;
            }
            std::path::Component::ParentDir => {
                if let Some(path) = budget.directory(&path.join("..")) {
                    queue.push_back((path, index + 1));
                }
                continue;
            }
            std::path::Component::Normal(part) => part.to_str()?,
            _ => return None,
        };
        if !part.contains(['*', '?', '[']) {
            queue.push_back((path.join(part), index + 1));
            continue;
        }
        let pattern = glob::Pattern::new(part).ok()?;
        if part == "**" {
            queue.push_back((path.clone(), index + 1));
        }
        for entry in std::fs::read_dir(&path).ok()? {
            budget.step()?;
            let entry = entry.ok()?;
            // Do not recursively follow symlinks; an explicit member path is
            // still canonicalized and checked at its leaf.
            if !entry.file_type().ok()?.is_dir() {
                continue;
            }
            if part == "**" {
                queue.push_back((entry.path(), index));
            } else if pattern.matches_path(Path::new(&entry.file_name())) {
                queue.push_back((entry.path(), index + 1));
            }
        }
    }
    Some(found)
}

fn dependency_paths(
    manifest: &toml::Value,
    workspace: &toml::Value,
    directory: &Path,
    root: &Path,
) -> Vec<PathBuf> {
    fn collect(
        table: &toml::Value,
        workspace: &toml::Value,
        directory: &Path,
        root: &Path,
        paths: &mut Vec<PathBuf>,
    ) {
        for kind in ["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(dependencies) = table.get(kind).and_then(toml::Value::as_table) {
                for (name, dependency) in dependencies {
                    if let Some(path) = dependency.get("path").and_then(toml::Value::as_str) {
                        paths.push(directory.join(path));
                    } else if dependency.get("workspace").and_then(toml::Value::as_bool)
                        == Some(true)
                    {
                        // Workspace dependency paths are relative to its root,
                        // resolved separately by the caller below.
                        if let Some(path) = workspace
                            .get("dependencies")
                            .and_then(|deps| deps.get(name))
                            .and_then(|dep| dep.get("path"))
                            .and_then(toml::Value::as_str)
                        {
                            paths.push(root.join(path));
                        }
                    }
                }
            }
        }
    }
    let mut paths = Vec::new();
    collect(manifest, workspace, directory, root, &mut paths);
    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        for target in targets.values() {
            collect(target, workspace, directory, root, &mut paths);
        }
    }
    paths
}

fn cargo_member(
    root: &Path,
    package: &Path,
    manifest: &toml::Value,
    budget: &mut Budget<'_>,
) -> Option<bool> {
    if package == root {
        return Some(true);
    }
    let workspace = manifest.get("workspace")?.as_table()?;
    let exclude = match workspace.get("exclude") {
        Some(value) => patterns(Some(value))?,
        None => Vec::new(),
    };
    let excluded = |path: &Path| {
        let relative = relative_directory(root, path);
        exclude.iter().any(|pattern| matches(pattern, &relative))
    };
    if excluded(package) {
        return Some(false);
    }
    let members = match workspace.get("members") {
        Some(value) => patterns(Some(value))?,
        None => Vec::new(),
    };
    let relative = relative_directory(root, package);
    if members.iter().any(|pattern| matches(pattern, &relative)) {
        return Some(true);
    }
    // Cargo implicitly includes in-root path dependencies, including their
    // transitive dependencies. Never run Cargo or traverse outside the worktree.
    let mut queue = VecDeque::new();
    if manifest.get("package").is_some() {
        queue.push_back(root.to_path_buf());
    }
    for member in &members {
        queue.extend(expand(root, member, budget)?);
    }
    let mut visited = HashSet::new();
    let workspace_value = manifest.get("workspace")?;
    while let Some(directory) = queue.pop_front() {
        budget.step()?;
        if excluded(&directory) || !visited.insert(directory.clone()) {
            continue;
        }
        if directory == package {
            return Some(true);
        }
        let member = if directory == root {
            manifest.clone()
        } else {
            budget.cargo(&directory)?
        };
        if directory != root && member.get("workspace").is_some() {
            continue;
        }
        for path in dependency_paths(&member, workspace_value, &directory, root) {
            if let Some(path) = budget.directory(&path) {
                if path.starts_with(root) {
                    queue.push_back(path);
                }
            }
        }
    }
    Some(false)
}

/// The full loader clones anchored trees. Refuse aliases/anchors and bound
/// structural work before materialization, including unrelated configuration
/// keys. Complexity is uncertain membership and falls back to the project.
fn yaml_packages(bytes: &[u8], cancelled: &AtomicBool) -> Option<Vec<String>> {
    use yaml_rust::scanner::{Scanner, Token, TokenType};
    const MAX_TOKENS: usize = 4096;
    const MAX_DEPTH: usize = 32;
    let text = std::str::from_utf8(bytes).ok()?;
    let mut scanner = Scanner::new(text.chars());
    let mut depth = 0usize;
    for (count, Token(_, token)) in scanner.by_ref().enumerate() {
        if count >= MAX_TOKENS || cancelled.load(Ordering::Acquire) {
            return None;
        }
        match token {
            TokenType::Alias(_) | TokenType::Anchor(_) => return None,
            TokenType::BlockSequenceStart
            | TokenType::BlockMappingStart
            | TokenType::FlowSequenceStart
            | TokenType::FlowMappingStart => {
                depth += 1;
                if depth > MAX_DEPTH {
                    return None;
                }
            }
            TokenType::BlockEnd | TokenType::FlowSequenceEnd | TokenType::FlowMappingEnd => {
                depth = depth.checked_sub(1)?;
            }
            _ => {}
        }
    }
    if scanner.get_error().is_some() || depth != 0 {
        return None;
    }
    let yaml = yaml_rust::YamlLoader::load_from_str(text).ok()?;
    if yaml.len() != 1 {
        return None;
    }
    yaml.first()?["packages"]
        .as_vec()?
        .iter()
        .map(|value| value.as_str().map(str::to_owned))
        .collect()
}

enum JavascriptWorkspace {
    Package(bool),
    References(bool),
}

fn javascript_workspace(
    directory: &Path,
    package: &Path,
    budget: &mut Budget<'_>,
) -> Option<JavascriptWorkspace> {
    let pnpm = directory.join("pnpm-workspace.yaml");
    let package_json = directory.join("package.json");
    let tsconfig = directory.join("tsconfig.json");
    let mut member_patterns = None;
    if budget.marker(&pnpm) {
        let parsed = budget
            .read(&pnpm)
            .and_then(|bytes| yaml_packages(&bytes, budget.cancelled));
        let Some(patterns) = parsed else {
            return Some(JavascriptWorkspace::Package(false));
        };
        member_patterns = Some(patterns);
    }
    if member_patterns.is_none() && budget.marker(&package_json) {
        let Some(manifest) = budget
            .read(&package_json)
            .and_then(|bytes| workspace_manifest(bytes, false))
        else {
            return Some(JavascriptWorkspace::Package(false));
        };
        if let Some(workspaces) = manifest.get("workspaces") {
            let entries = workspaces
                .as_array()
                .or_else(|| workspaces.get("packages")?.as_array());
            let Some(patterns) = entries.and_then(|entries| {
                entries
                    .iter()
                    .map(|entry| entry.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
            }) else {
                return Some(JavascriptWorkspace::Package(false));
            };
            member_patterns = Some(patterns);
        }
    }
    if let Some(patterns) = member_patterns {
        if directory == package {
            return Some(JavascriptWorkspace::Package(true));
        }
        let relative = package.strip_prefix(directory).ok()?;
        let included = patterns
            .iter()
            .filter(|pattern| !pattern.starts_with('!'))
            .any(|pattern| matches(pattern, relative));
        let excluded = patterns
            .iter()
            .filter_map(|pattern| pattern.strip_prefix('!'))
            .any(|pattern| matches(pattern, relative));
        return Some(JavascriptWorkspace::Package(included && !excluded));
    }
    if !budget.marker(&tsconfig) {
        return None;
    }
    let Some(manifest) = budget
        .read(&tsconfig)
        .and_then(|bytes| workspace_manifest(bytes, true))
    else {
        return Some(JavascriptWorkspace::Package(false));
    };
    let references = manifest.get("references")?;
    let Some(references) = references.as_array() else {
        return Some(JavascriptWorkspace::Package(false));
    };
    Some(JavascriptWorkspace::References(
        directory == package
            || references.iter().any(|entry| {
                let Some(path) = entry.get("path").and_then(serde_json::Value::as_str) else {
                    return false;
                };
                let mut path = directory.join(path);
                if path
                    .extension()
                    .is_some_and(|extension| extension == "json")
                {
                    path.pop();
                }
                budget
                    .directory(&path)
                    .is_some_and(|path| package.starts_with(path))
            }),
    ))
}

pub(super) fn discover(
    path: &Path,
    worktree: &Path,
    markers: &[String],
    cancelled: &AtomicBool,
) -> Option<PathBuf> {
    if cancelled.load(Ordering::Acquire) {
        return None;
    }
    let canonical_root = match worktree.canonicalize() {
        Ok(root) => root,
        Err(_) => return Some(worktree.to_owned()),
    };
    if markers.is_empty() {
        return Some(canonical_root);
    }
    let mut budget = Budget {
        cancelled,
        probes: 0,
        bytes: 0,
        root: canonical_root,
    };
    let rust = path
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"));
    let javascript = path
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            ["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"]
                .iter()
                .any(|kind| extension.eq_ignore_ascii_case(kind))
        });
    let mut nearest = None;
    let mut package = None;
    let mut reference_root = None;
    let mut current = path.parent();
    while let Some(directory) = current {
        let Some(canonical) = budget.directory(directory) else {
            break;
        };
        if nearest.is_none() {
            for marker in markers {
                if budget.marker(&directory.join(marker)) {
                    nearest = Some(canonical.clone());
                    break;
                }
            }
        }
        if rust {
            if budget.marker(&directory.join("Cargo.toml")) {
                nearest.get_or_insert_with(|| canonical.clone());
                if package.is_none() {
                    package = Some(canonical.clone());
                }
                // A present but unreadable/invalid manifest is an uncertain
                // project boundary, never permission to join an outer one.
                let Some(manifest) = budget.cargo(directory) else {
                    break;
                };
                if manifest.get("workspace").is_some() {
                    let belongs = cargo_member(
                        &canonical,
                        package.as_deref().unwrap_or(&canonical),
                        &manifest,
                        &mut budget,
                    );
                    // A nested workspace is a boundary even when malformed or
                    // over budget. Do not continue to an unrelated outer root.
                    if belongs == Some(true) {
                        return Some(canonical.clone());
                    }
                    break;
                }
                if let Some(explicit) = manifest
                    .get("package")
                    .and_then(|package| package.get("workspace"))
                    .and_then(toml::Value::as_str)
                {
                    if let Some(root) = budget.directory(&directory.join(explicit)) {
                        if let Some(workspace) = budget.cargo(&root) {
                            if cargo_member(&root, &canonical, &workspace, &mut budget)
                                == Some(true)
                            {
                                return Some(root);
                            }
                        }
                    }
                    break;
                }
            }
        } else if javascript {
            if package.is_none() && budget.marker(&directory.join("package.json")) {
                package = Some(canonical.clone());
            }
            match javascript_workspace(
                &canonical,
                package.as_deref().unwrap_or(&canonical),
                &mut budget,
            ) {
                Some(JavascriptWorkspace::Package(true)) => return Some(canonical.clone()),
                Some(JavascriptWorkspace::Package(false)) => break,
                // Package membership outranks a leaf solution's own app/node
                // references. Keep the nearest solution as a fallback while
                // looking for a proven npm/pnpm workspace membership above it.
                Some(JavascriptWorkspace::References(true)) => {
                    if reference_root
                        .as_ref()
                        .is_none_or(|root| Some(root) == package.as_ref())
                    {
                        reference_root = Some(canonical.clone());
                    }
                }
                Some(JavascriptWorkspace::References(false)) | None => {}
            }
        }
        if canonical == budget.root {
            break;
        }
        current = directory.parent();
    }
    if cancelled.load(Ordering::Acquire) {
        return None;
    }
    Some(reference_root.or(nearest).unwrap_or(budget.root))
}

#[cfg(test)]
pub(super) fn discover_root(path: &Path, worktree: &Path, markers: &[String]) -> PathBuf {
    discover(path, worktree, markers, &AtomicBool::new(false))
        .unwrap_or_else(|| worktree.to_owned())
}
