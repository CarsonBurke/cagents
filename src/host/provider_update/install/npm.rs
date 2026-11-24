use std::{
    fs,
    io::{self, Read},
    os::unix::fs::{MetadataExt, symlink},
    path::{Path, PathBuf},
};

use semver::Version;
use serde::Deserialize;
use tokio::process::Command;

use super::{
    CommandContext, EntryStamp, NPM_IN_USE, Preparation, PreparedUpdate, Replacement,
    SelectedInstall, fs_error, npm_running_in, run_update, stage_in,
};
use crate::account::{
    probe::find_executable,
    provider_install::{ProviderStatus, read_version},
};

const REGISTRY: &str = "https://registry.npmjs.org/";
const PACKAGE: &str = "@openai/codex";
const SCOPE_DIRECTORY: &str = "lib/node_modules/@openai";
const WRAPPER: &str = "bin/codex.js";
const NESTED_SCOPE: &str = "node_modules/@openai";

struct Platform {
    alias: String,
    tag: &'static str,
    triple: &'static str,
}

impl Platform {
    fn current() -> Option<Self> {
        let (tag, triple) = match std::env::consts::ARCH {
            "x86_64" => ("linux-x64", "x86_64-unknown-linux-musl"),
            "aarch64" => ("linux-arm64", "aarch64-unknown-linux-musl"),
            _ => return None,
        };
        Some(Self {
            alias: format!("codex-{tag}"),
            tag,
            triple,
        })
    }

    fn binary(&self) -> PathBuf {
        Path::new("vendor").join(self.triple).join("bin/codex")
    }

    fn legacy_binary(&self) -> PathBuf {
        Path::new("vendor").join(self.triple).join("codex")
    }
}

pub(super) async fn prepare(
    status: &ProviderStatus,
    prefix: &Path,
    context: &CommandContext<'_>,
) -> Result<Preparation, String> {
    let Some(platform) = Platform::current() else {
        return Ok(Preparation::Skipped(
            "Codex has no npm package for this architecture".into(),
        ));
    };
    let installation = SelectedInstall::capture(status, context.search_path)?;
    let scope = prefix.join(SCOPE_DIRECTORY);
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    if scope.canonicalize().map_err(fs_error)? != scope
        || fs::metadata(&scope).map_err(fs_error)?.uid() != uid
    {
        return Err("Codex's npm package directory is not owned by this user".into());
    }
    let relative_target = installation
        .resolved
        .strip_prefix(&scope)
        .map_err(|_| "Codex is outside its detected npm prefix")?;
    let wrapper = scope.join("codex");
    let platform_top = scope.join(&platform.alias);
    let platform_nested = wrapper.join(NESTED_SCOPE).join(&platform.alias);
    let targets = [
        wrapper.as_path(),
        platform_top.as_path(),
        platform_nested.as_path(),
    ];
    let stamps = package_stamps(targets)?;

    let npm = find_executable("npm", context.search_path)
        .ok_or("npm was not found on the provider search path")?;
    let stage = stage_in(&scope)?;
    let mut view = command(context, &npm, stage.path());
    view.args([
        "--fetch-timeout=15000",
        "--fetch-retry-mintimeout=1000",
        "view",
        "@openai/codex@latest",
        "version",
        "--json",
    ]);
    let output = run_update(view).await?;
    let latest: Version = serde_json::from_slice::<String>(&output)
        .ok()
        .and_then(|version| Version::parse(&version).ok())
        .ok_or("npm did not report a Codex release version")?;

    let installed = status
        .version
        .as_deref()
        .and_then(|version| Version::parse(version).ok());
    let wrapper_version = package_version(&wrapper, None)?;
    let top_version = package_version(&platform_top, Some(platform.tag))?;
    let nested_version = package_version(&platform_nested, Some(platform.tag))?;
    if let Some(installed) = &installed
        && installed > &latest
    {
        installation.check(context.search_path)?;
        return Ok(Preparation::Current(installed.to_string()));
    }
    if [
        wrapper_version.as_ref(),
        top_version.as_ref(),
        nested_version.as_ref(),
    ]
    .into_iter()
    .flatten()
    .any(|version| version > &latest)
    {
        return Ok(Preparation::Skipped(
            "This npm prefix already contains a Codex release newer than the latest stable release"
                .into(),
        ));
    }

    let launcher = wrapper.join(WRAPPER);
    let top_binary = platform_top.join(platform.binary());
    let replace_top = top_version.is_some()
        && (top_version.as_ref() != Some(&latest)
            || verify(&top_binary, &latest, context, status).await.is_err());

    // An npm install may hoist its platform package beside the wrapper. A
    // current wrapper can keep using that path while we repair the alias.
    let hoisted = nested_version.is_none() && top_version.is_some();
    let keep_wrapper = if wrapper_version.as_ref() != Some(&latest) {
        false
    } else {
        match (nested_version.as_ref(), top_version.as_ref()) {
            (None, Some(version)) => verify(&launcher, version, context, status).await.is_ok(),
            (Some(version), _) if version == &latest => {
                verify(
                    &platform_nested.join(platform.binary()),
                    &latest,
                    context,
                    status,
                )
                .await
                .is_ok()
                    && verify(&launcher, &latest, context, status).await.is_ok()
            }
            _ => false,
        }
    };
    if keep_wrapper && !replace_top && installed.as_ref() == Some(&latest) {
        installation.check(context.search_path)?;
        return Ok(Preparation::Current(latest.to_string()));
    }
    let keep_wrapper = keep_wrapper && replace_top;
    let affected_roots: Vec<_> = [
        (!keep_wrapper).then_some(wrapper.as_path()),
        replace_top.then_some(platform_top.as_path()),
    ]
    .into_iter()
    .flatten()
    .collect();
    if npm_running_in(&affected_roots)? {
        return Ok(Preparation::Deferred(NPM_IN_USE.into()));
    }

    let mut install = command(context, &npm, stage.path());
    install
        .args(["install", "--include=optional"])
        .arg(format!("{PACKAGE}@{latest}"));
    run_update(install).await?;

    let staged_scope = stage.path().join(SCOPE_DIRECTORY);
    let staged_wrapper = staged_scope.join("codex");
    let staged_top = staged_scope.join(&platform.alias);
    let staged_nested = staged_wrapper.join(NESTED_SCOPE).join(&platform.alias);
    if package_version(&staged_wrapper, None)?.as_ref() != Some(&latest) {
        return Err("npm did not stage the requested Codex wrapper release".into());
    }

    // npm can install the optional platform package beside the wrapper or
    // inside it. Give the wrapper its own verified dependency so it can be
    // published as a unit and can never select the previous top-level copy.
    if !staged_nested.exists() && staged_top.is_dir() {
        let nested_scope = staged_wrapper.join(NESTED_SCOPE);
        fs::create_dir_all(nested_scope).map_err(fs_error)?;
        fs::rename(&staged_top, &staged_nested).map_err(fs_error)?;
    }
    if package_version(&staged_nested, Some(platform.tag))?.as_ref() != Some(&latest) {
        return Err("npm did not stage the matching Codex platform package".into());
    }
    let staged_native = staged_nested.join(platform.binary());
    verify(&staged_native, &latest, context, status).await?;
    let staged_launcher = staged_wrapper.join(WRAPPER);
    verify(&staged_launcher, &latest, context, status).await?;

    // Keep direct links to older package layouts resolving through the
    // vendor directory without copying the new binary to a different location.
    preserve_legacy_binary(&platform_nested, &staged_nested, &platform)?;

    if replace_top {
        if !staged_top.exists() {
            let mut copy = context.command(Path::new("/usr/bin/cp"));
            copy.args(["-al", "--"])
                .arg(&staged_nested)
                .arg(&staged_top);
            run_update(copy).await?;
        }
        if package_version(&staged_top, Some(platform.tag))?.as_ref() != Some(&latest) {
            return Err("npm did not stage the matching top-level Codex platform package".into());
        }
        verify(
            &staged_top.join(platform.binary()),
            &latest,
            context,
            status,
        )
        .await?;
        preserve_legacy_binary(&platform_top, &staged_top, &platform)?;
    }

    let executable = if relative_target.starts_with(&platform.alias) && !replace_top {
        installation.resolved.clone()
    } else {
        staged_scope.join(relative_target)
    };
    let expected_version = verify(&executable, &latest, context, status).await?;
    let mut replacements = Vec::new();
    if replace_top {
        replacements.push(Replacement::new(staged_top, platform_top.clone())?);
    }
    // If the launcher runs against a hoisted dependency, keep it in place
    // only when it is identical to the verified wrapper of this release.
    let keep_wrapper = keep_wrapper && (!hoisted || same_launcher(&launcher, &staged_launcher));
    if keep_wrapper {
        tokio::fs::remove_dir_all(&staged_wrapper)
            .await
            .map_err(fs_error)?;
    } else {
        replacements.push(Replacement::new(staged_wrapper, wrapper.clone())?);
    }
    installation.check(context.search_path)?;
    if package_stamps(targets)? != stamps {
        return Err("Codex's npm packages changed during the update check".into());
    }
    Ok(Preparation::Ready(PreparedUpdate::staged(
        installation,
        stage,
        replacements,
        expected_version,
    )))
}

fn command(context: &CommandContext<'_>, npm: &Path, stage: &Path) -> Command {
    let mut command = context.command(npm);
    for (key, _) in std::env::vars_os() {
        if key
            .to_string_lossy()
            .to_ascii_lowercase()
            .starts_with("npm_config_")
        {
            command.env_remove(key);
        }
    }
    command
        .env_remove("NODE_OPTIONS")
        .env_remove("NODE_PATH")
        .current_dir(stage)
        .arg("--global")
        .arg("--prefix")
        .arg(stage)
        .arg("--userconfig")
        .arg(stage.join("npm-userconfig"))
        .arg("--globalconfig")
        .arg(stage.join("npm-globalconfig"))
        .arg("--cache")
        .arg(stage.join("npm-cache"))
        .arg(format!("--registry={REGISTRY}"))
        .args([
            "--ignore-scripts",
            "--no-audit",
            "--no-fund",
            "--no-update-notifier",
            "--fetch-retries=1",
        ]);
    command
}

fn package_stamps(paths: [&Path; 3]) -> Result<[Option<EntryStamp>; 3], String> {
    let [wrapper, top, nested] = paths;
    Ok([
        EntryStamp::read(wrapper).map_err(fs_error)?,
        EntryStamp::read(top).map_err(fs_error)?,
        EntryStamp::read(nested).map_err(fs_error)?,
    ])
}

#[derive(Deserialize)]
struct PackageManifest {
    name: String,
    version: String,
}

fn package_version(root: &Path, tag: Option<&str>) -> Result<Option<Version>, String> {
    match fs::symlink_metadata(root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Ok(metadata) if metadata.is_dir() && root.canonicalize().map_err(fs_error)? == root => (),
        _ => {
            return Err(format!(
                "Unexpected npm package directory at {}",
                root.display()
            ));
        }
    }
    let file = root.join("package.json");
    if !fs::symlink_metadata(&file).map_err(fs_error)?.is_file() {
        return Err(format!(
            "Unexpected npm package manifest at {}",
            file.display()
        ));
    }
    let mut content = Vec::new();
    fs::File::open(&file)
        .map_err(fs_error)?
        .take(64 * 1024)
        .read_to_end(&mut content)
        .map_err(fs_error)?;
    let manifest: PackageManifest = serde_json::from_slice(&content)
        .map_err(|_| format!("Invalid npm package manifest at {}", file.display()))?;
    let version = match tag {
        Some(tag) => manifest
            .version
            .strip_suffix(tag)
            .and_then(|version| version.strip_suffix('-')),
        None => Some(manifest.version.as_str()),
    };
    if manifest.name != PACKAGE {
        return Err(format!("Unexpected npm package name at {}", file.display()));
    }
    version
        .and_then(|version| Version::parse(version).ok())
        .map(Some)
        .ok_or_else(|| format!("Invalid Codex package version at {}", file.display()))
}

fn same_launcher(installed: &Path, staged: &Path) -> bool {
    const MAX_LAUNCHER_BYTES: u64 = 64 * 1024;
    let read = |path: &Path| -> io::Result<Vec<u8>> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_file() || metadata.len() > MAX_LAUNCHER_BYTES {
            return Err(io::Error::other("Unexpected Codex npm launcher"));
        }
        let mut bytes = Vec::new();
        fs::File::open(path)?
            .take(MAX_LAUNCHER_BYTES + 1)
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    matches!((read(installed), read(staged)), (Ok(installed), Ok(staged)) if installed == staged)
}

fn preserve_legacy_binary(live: &Path, staged: &Path, platform: &Platform) -> Result<(), String> {
    let relative = platform.legacy_binary();
    let staged_legacy = staged.join(&relative);
    if live.join(relative).exists() && !staged_legacy.exists() {
        symlink("bin/codex", staged_legacy).map_err(fs_error)?;
    }
    Ok(())
}

async fn verify(
    executable: &Path,
    version: &Version,
    context: &CommandContext<'_>,
    status: &ProviderStatus,
) -> Result<String, String> {
    let actual = read_version(status.provider, executable, context.search_path).await?;
    if !Version::parse(&actual).is_ok_and(|actual| &actual == version) {
        return Err(format!(
            "Staged Codex executable reports {actual}; expected {version}"
        ));
    }
    Ok(actual)
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, os::unix::fs::PermissionsExt};

    use tempfile::TempDir;

    use super::super::Applied;
    use super::*;
    use crate::{account::provider_install::ProviderInstallMethod, agent::Provider};

    struct Fixture {
        _root: TempDir,
        prefix: PathBuf,
        path: OsString,
        status: ProviderStatus,
    }

    impl Fixture {
        fn new(platform: &Platform, direct: bool) -> Self {
            let root = tempfile::tempdir().unwrap();
            let prefix = root.path().join("custom-prefix");
            let primary_bin = root.path().join("local-bin");
            fs::create_dir(&primary_bin).unwrap();
            let wrapper = prefix.join(SCOPE_DIRECTORY).join("codex");
            write_package(&wrapper, "2.0.0");
            let nested = wrapper.join(NESTED_SCOPE).join(&platform.alias);
            write_package(&nested, &format!("2.0.0-{}", platform.tag));
            write_executable(
                &nested.join(platform.binary()),
                "#!/bin/sh\necho codex-cli 2.0.0\n",
            );
            write_wrapper(&wrapper, platform);
            let top = prefix.join(SCOPE_DIRECTORY).join(&platform.alias);
            write_package(&top, &format!("1.0.0-{}", platform.tag));
            write_executable(
                &top.join(platform.binary()),
                "#!/bin/sh\necho codex-cli 1.0.0\n",
            );
            let installed = if direct {
                top.join(platform.binary())
            } else {
                wrapper.join(WRAPPER)
            };
            let executable = primary_bin.join("codex");
            symlink(&installed, &executable).unwrap();
            let path =
                std::env::join_paths([primary_bin.as_path(), Path::new("/usr/bin")]).unwrap();
            let status = ProviderStatus {
                provider: Provider::Codex,
                executable: Some(executable),
                resolved_executable: Some(installed),
                install_method: ProviderInstallMethod::Npm {
                    prefix: prefix.clone(),
                },
                version: Some(if direct { "1.0.0" } else { "2.0.0" }.into()),
                last_checked_at: None,
                last_result: None,
                error: None,
            };
            Self {
                _root: root,
                prefix,
                path,
                status,
            }
        }

        fn fake_npm(&self, platform: &Platform, candidate_version: &str) {
            let bin = self.status.executable.as_deref().unwrap().parent().unwrap();
            let script = format!(
                r#"#!/bin/sh
set -eu
prefix=""
registry=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --prefix) prefix="$2"; shift 2 ;;
        --registry=*) registry="${{1#*=}}"; shift ;;
        view|install) action="$1"; shift; break ;;
        *) shift ;;
    esac
done
[ "$registry" = "{REGISTRY}" ]
case "$prefix" in */.cagents-provider-update-*) ;; *) exit 1 ;; esac
if [ "$action" = view ]; then
    [ "$1" = "@openai/codex@latest" ]
    echo '"2.0.0"'
    exit 0
fi
[ "$1" = "--include=optional" ]
[ "$2" = "@openai/codex@2.0.0" ]
scope="$prefix/{SCOPE_DIRECTORY}"
wrapper="$scope/codex"
platform="$scope/{alias}"
mkdir -p "$wrapper/bin" "$platform/{binary_parent}"
printf '%s\n' '{{"name":"@openai/codex","version":"2.0.0"}}' >"$wrapper/package.json"
printf '%s\n' '{{"name":"@openai/codex","version":"2.0.0-{tag}"}}' >"$platform/package.json"
cat >"$wrapper/{WRAPPER}" <<'WRAPPER_SH'
{wrapper_script}WRAPPER_SH
printf '#!/bin/sh\necho codex-cli %s\n' '{candidate_version}' >"$platform/{binary}"
chmod +x "$wrapper/{WRAPPER}" "$platform/{binary}"
"#,
                alias = platform.alias,
                binary_parent = platform.binary().parent().unwrap().display(),
                binary = platform.binary().display(),
                tag = platform.tag,
                wrapper_script = wrapper_script(platform),
            );
            write_executable(&bin.join("npm"), &script);
        }

        async fn prepare(&self) -> Result<Preparation, String> {
            prepare(
                &self.status,
                &self.prefix,
                &CommandContext {
                    search_path: Some(&self.path),
                },
            )
            .await
        }

        async fn read_live(&self) -> String {
            read_version(
                Provider::Codex,
                self.status.executable.as_deref().unwrap(),
                Some(&self.path),
            )
            .await
            .unwrap()
        }
    }

    fn write_package(package: &Path, version: &str) {
        fs::create_dir_all(package).unwrap();
        fs::write(
            package.join("package.json"),
            serde_json::to_vec(&serde_json::json!({ "name": PACKAGE, "version": version }))
                .unwrap(),
        )
        .unwrap();
    }

    fn write_executable(path: &Path, script: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn wrapper_script(platform: &Platform) -> String {
        format!(
            "#!/bin/sh\nroot=$(dirname \"$(readlink -f \"$0\")\")/..\nplatform=\"$root/{NESTED_SCOPE}/{alias}/{binary}\"\n[ -x \"$platform\" ] || platform=\"$root/../{alias}/{binary}\"\nexec \"$platform\" \"$@\"\n",
            alias = platform.alias,
            binary = platform.binary().display(),
        )
    }

    fn write_wrapper(package: &Path, platform: &Platform) {
        write_executable(&package.join(WRAPPER), &wrapper_script(platform));
    }

    #[tokio::test]
    async fn updates_stale_top_level_platform_even_when_wrapper_was_already_current() {
        let Some(platform) = Platform::current() else {
            return;
        };
        let fixture = Fixture::new(&platform, true);
        fixture.fake_npm(&platform, "2.0.0");
        let wrapper = fixture.prefix.join(SCOPE_DIRECTORY).join("codex");
        let wrapper_inode = fs::metadata(&wrapper).unwrap().ino();

        let prepared = fixture.prepare().await.unwrap();
        let Preparation::Ready(update) = prepared else {
            panic!("expected an update")
        };
        assert_eq!(fixture.read_live().await, "1.0.0");
        let result = update.apply(Some(&fixture.path), || false).await.unwrap();
        assert!(matches!(result, Applied::Updated(version) if version == "2.0.0"));
        assert_eq!(fixture.read_live().await, "2.0.0");

        assert_eq!(fs::metadata(&wrapper).unwrap().ino(), wrapper_inode);
        let nested = wrapper.join(NESTED_SCOPE).join(&platform.alias);
        assert!(nested.join(platform.binary()).is_file());
        assert_eq!(
            read_version(Provider::Codex, &wrapper.join(WRAPPER), Some(&fixture.path))
                .await
                .unwrap(),
            "2.0.0"
        );
    }

    #[tokio::test]
    async fn repairs_the_top_level_alias_when_the_selected_wrapper_is_current() {
        let Some(platform) = Platform::current() else {
            return;
        };
        let fixture = Fixture::new(&platform, false);
        fixture.fake_npm(&platform, "2.0.0");

        let prepared = fixture.prepare().await.unwrap();
        let Preparation::Ready(update) = prepared else {
            panic!("expected a repair")
        };
        assert_eq!(fixture.read_live().await, "2.0.0");
        let result = update.apply(Some(&fixture.path), || false).await.unwrap();
        assert!(matches!(result, Applied::Updated(version) if version == "2.0.0"));

        let top = fixture
            .prefix
            .join(SCOPE_DIRECTORY)
            .join(&platform.alias)
            .join(platform.binary());
        assert_eq!(
            read_version(Provider::Codex, &top, Some(&fixture.path))
                .await
                .unwrap(),
            "2.0.0"
        );
    }

    #[tokio::test]
    async fn keeps_a_current_wrapper_that_resolves_a_stale_hoisted_platform() {
        let Some(platform) = Platform::current() else {
            return;
        };
        let mut fixture = Fixture::new(&platform, false);
        fixture.fake_npm(&platform, "2.0.0");
        let wrapper = fixture.prefix.join(SCOPE_DIRECTORY).join("codex");
        let wrapper_inode = fs::metadata(&wrapper).unwrap().ino();
        fs::remove_dir_all(wrapper.join("node_modules")).unwrap();
        fixture.status.version = Some("1.0.0".into());

        assert_eq!(fixture.read_live().await, "1.0.0");
        let prepared = fixture.prepare().await.unwrap();
        let Preparation::Ready(update) = prepared else {
            panic!("expected a repair")
        };
        let result = update.apply(Some(&fixture.path), || false).await.unwrap();
        assert!(matches!(result, Applied::Updated(version) if version == "2.0.0"));
        assert_eq!(fixture.read_live().await, "2.0.0");
        assert_eq!(fs::metadata(&wrapper).unwrap().ino(), wrapper_inode);
        assert!(!wrapper.join("node_modules").exists());
    }

    #[tokio::test]
    async fn replaces_a_wrapper_whose_nested_binary_disagrees_with_its_manifest() {
        let Some(platform) = Platform::current() else {
            return;
        };
        let fixture = Fixture::new(&platform, true);
        fixture.fake_npm(&platform, "2.0.0");
        let wrapper = fixture.prefix.join(SCOPE_DIRECTORY).join("codex");
        let wrapper_inode = fs::metadata(&wrapper).unwrap().ino();
        let binary = wrapper
            .join(NESTED_SCOPE)
            .join(&platform.alias)
            .join(platform.binary());
        write_executable(&binary, "#!/bin/sh\necho codex-cli 1.0.0\n");

        let prepared = fixture.prepare().await.unwrap();
        let Preparation::Ready(update) = prepared else {
            panic!("expected a repair")
        };
        let result = update.apply(Some(&fixture.path), || false).await.unwrap();
        assert!(matches!(result, Applied::Updated(version) if version == "2.0.0"));
        assert_ne!(fs::metadata(&wrapper).unwrap().ino(), wrapper_inode);
        assert_eq!(
            read_version(Provider::Codex, &binary, Some(&fixture.path))
                .await
                .unwrap(),
            "2.0.0"
        );
    }

    #[tokio::test]
    async fn rejects_unverified_package_before_touching_live_install() {
        let Some(platform) = Platform::current() else {
            return;
        };
        let fixture = Fixture::new(&platform, true);
        fixture.fake_npm(&platform, "1.0.0");

        let result = fixture.prepare().await;
        assert!(matches!(result, Err(error) if error.contains("expected 2.0.0")));
        assert_eq!(fixture.read_live().await, "1.0.0");
    }
}
