use std::{
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use super::{ProviderInstallMethod, ProviderStatus};
use crate::{account::probe, agent::Provider};

impl ProviderStatus {
    pub fn detect(provider: Provider, search_path: Option<&OsStr>) -> Self {
        let base_dirs = directories::BaseDirs::new();
        detect(
            provider,
            search_path,
            base_dirs.as_ref().map(directories::BaseDirs::home_dir),
            base_dirs.as_ref().map(directories::BaseDirs::data_dir),
            &|name| std::env::var_os(name),
        )
    }
}

fn detect(
    provider: Provider,
    search_path: Option<&OsStr>,
    home: Option<&Path>,
    data: Option<&Path>,
    environment: &impl Fn(&str) -> Option<OsString>,
) -> ProviderStatus {
    let executable = find_provider_executable(provider, search_path);
    let resolved_executable = executable
        .as_deref()
        .and_then(|path| path.canonicalize().ok());
    let install_method = match (executable.as_deref(), resolved_executable.as_deref()) {
        (Some(entry), Some(resolved)) => {
            classify(provider, entry, resolved, home, data, environment)
        }
        (Some(_), None) => ProviderInstallMethod::Unknown,
        (None, _) => ProviderInstallMethod::Missing,
    };
    ProviderStatus {
        provider,
        executable,
        resolved_executable,
        version: None,
        install_method,
        last_checked_at: None,
        last_result: None,
        error: None,
    }
}

fn find_provider_executable(provider: Provider, search_path: Option<&OsStr>) -> Option<PathBuf> {
    let entry = probe::find_executable(provider.executable(), search_path)?;
    if provider == Provider::Muse
        && let Some(program) = entry.to_str()
    {
        let path = search_path.and_then(OsStr::to_str).unwrap_or_default();
        let upstream = provider.subcommand_program(program, Path::new("/"), path);
        if Path::new(&upstream).is_absolute() {
            return Some(upstream.into());
        }
        return probe::find_executable(&upstream, search_path);
    }
    Some(entry)
}

fn classify(
    provider: Provider,
    entry: &Path,
    resolved: &Path,
    home: Option<&Path>,
    data: Option<&Path>,
    environment: &impl Fn(&str) -> Option<OsString>,
) -> ProviderInstallMethod {
    let paths = executable_paths(entry, resolved);
    if let Some(method) = managed_method(&paths, home, data, environment) {
        return method;
    }
    if !owned_by_user(resolved) {
        return ProviderInstallMethod::System;
    }

    if let Some(prefix) = npm_prefix(provider, resolved) {
        return if owned_by_user(prefix) {
            ProviderInstallMethod::Npm {
                prefix: prefix.to_owned(),
            }
        } else {
            ProviderInstallMethod::System
        };
    }

    let local_bin = home.and_then(|home| home.join(".local/bin").canonicalize().ok());
    let Some(bin) = local_bin.filter(|bin| owned_by_user(bin)) else {
        return ProviderInstallMethod::Unknown;
    };
    let local_entry = bin.join(provider.executable());
    match provider {
        Provider::Claude
            if local_entry.canonicalize().ok().as_deref() == Some(resolved)
                && (data.is_some_and(|data| claude_version(data, resolved))
                    || home.is_some_and(|home| {
                        claude_version(&home.join(".local/share"), resolved)
                    })) =>
        {
            ProviderInstallMethod::Native
        }
        Provider::Muse
            if (resolved == local_entry || resolved == bin.join("muse-upstream"))
                && is_muse_launcher(resolved) =>
        {
            ProviderInstallMethod::Native
        }
        _ if resolved == local_entry && elf_binary(resolved) => ProviderInstallMethod::Standalone,
        _ => ProviderInstallMethod::Unknown,
    }
}

/// A PATH entry can point to a manager shim which itself points to the
/// manager's executable. Keep the intermediate hop for classification.
fn executable_paths(entry: &Path, resolved: &Path) -> Vec<PathBuf> {
    let mut paths = vec![entry.to_owned()];
    let mut current = entry.to_owned();
    for _ in 0..40 {
        let Some(parent) = current.parent() else {
            break;
        };
        let Some(name) = current.file_name() else {
            break;
        };
        if let Ok(parent) = parent.canonicalize() {
            let physical = parent.join(name);
            if physical != current {
                paths.push(physical.clone());
                current = physical;
            }
        }
        let Ok(target) = fs::read_link(&current) else {
            break;
        };
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent.join(target);
        paths.push(current.clone());
    }
    paths.push(resolved.to_owned());
    paths
}

fn managed_method(
    paths: &[PathBuf],
    home: Option<&Path>,
    data: Option<&Path>,
    environment: &impl Fn(&str) -> Option<OsString>,
) -> Option<ProviderInstallMethod> {
    if paths.first().is_some_and(|path| system_path(path)) {
        return Some(ProviderInstallMethod::System);
    }

    let under = |root: &Path| paths.iter().any(|path| path.starts_with(root));
    for (manager, variable, home_path, data_path) in [
        ("mise", "MISE_DATA_DIR", ".local/share/mise", Some("mise")),
        ("mise", "RTX_DATA_DIR", ".local/share/rtx", Some("rtx")),
        ("asdf", "ASDF_DATA_DIR", ".asdf", None),
        ("nvm", "NVM_DIR", ".nvm", Some("nvm")),
        ("fnm", "FNM_DIR", ".local/share/fnm", Some("fnm")),
        (
            "fnm",
            "FNM_MULTISHELL_PATH",
            ".local/state/fnm_multishells",
            None,
        ),
        ("Volta", "VOLTA_HOME", ".volta", None),
        ("nodenv", "NODENV_ROOT", ".nodenv", None),
        ("pnpm", "PNPM_HOME", ".local/share/pnpm", Some("pnpm")),
        ("Bun", "BUN_INSTALL", ".bun", None),
        ("pipx", "PIPX_HOME", ".local/share/pipx", Some("pipx")),
        (
            "uv",
            "UV_TOOL_DIR",
            ".local/share/uv/tools",
            Some("uv/tools"),
        ),
    ] {
        if environment(variable)
            .map(PathBuf::from)
            .is_some_and(|root| root.is_absolute() && under(&root))
            || home.is_some_and(|home| under(&home.join(home_path)))
            || data
                .zip(data_path)
                .is_some_and(|(data, path)| under(&data.join(path)))
        {
            return Some(ProviderInstallMethod::VersionManager(manager.into()));
        }
    }
    if under(Path::new("/nix/store")) || home.is_some_and(|home| under(&home.join(".nix-profile")))
    {
        return Some(ProviderInstallMethod::VersionManager("Nix".into()));
    }
    if paths
        .iter()
        .any(|path| path.components().any(|part| part.as_os_str() == "Cellar"))
    {
        return Some(ProviderInstallMethod::VersionManager("Homebrew".into()));
    }
    if paths.iter().any(|path| {
        // A shell can contribute a Node executable to PATH without passing
        // on the version manager's environment variables.
        path.ancestors().any(|ancestor| {
            ancestor.ends_with("versions/node") || ancestor.ends_with("installs/node")
        })
    }) {
        return Some(ProviderInstallMethod::VersionManager(
            "Node version manager".into(),
        ));
    }
    paths
        .iter()
        .any(|path| system_path(path))
        .then_some(ProviderInstallMethod::System)
}

fn system_path(path: &Path) -> bool {
    [
        "/usr", "/bin", "/sbin", "/lib", "/lib64", "/opt", "/snap", "/var/lib",
    ]
    .iter()
    .any(|root| path.starts_with(root))
}

fn owned_by_user(path: &Path) -> bool {
    // SAFETY: geteuid takes no arguments and has no preconditions.
    let uid = unsafe { libc::geteuid() };
    fs::metadata(path).is_ok_and(|metadata| metadata.uid() == uid)
}

fn npm_prefix(provider: Provider, resolved: &Path) -> Option<&Path> {
    for modules in resolved
        .ancestors()
        .filter(|path| path.ends_with("lib/node_modules"))
    {
        let package = resolved.strip_prefix(modules).ok()?;
        let known = match provider {
            Provider::Codex => [
                "@openai/codex",
                "@openai/codex-linux-x64",
                "@openai/codex-linux-arm64",
            ]
            .iter()
            .any(|name| package.starts_with(name)),
            Provider::Claude => package.starts_with("@anthropic-ai/claude-code"),
            Provider::Omp => package.starts_with("@oh-my-pi/pi-coding-agent"),
            Provider::Antigravity | Provider::Muse => false,
        };
        if known {
            return modules.parent()?.parent();
        }
    }
    None
}

fn claude_version(data: &Path, resolved: &Path) -> bool {
    let Ok(versions) = data.join("claude/versions").canonicalize() else {
        return false;
    };
    resolved.parent() == Some(versions.as_path())
}

fn elf_binary(path: &Path) -> bool {
    let mut magic = [0; 4];
    File::open(path)
        .and_then(|mut file| file.read_exact(&mut magic))
        .is_ok()
        && magic == *b"\x7fELF"
}

/// Muse's installer deliberately installs a shell launcher. Other wrappers
/// with the same command name belong to the user and stay unmanaged.
pub fn is_muse_launcher(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut source = String::new();
    file.take(64 * 1024).read_to_string(&mut source).is_ok()
        && source.starts_with("#!")
        && source.contains("launcher_url=\"https://api.meta.ai/muse-launcher.sh\"")
        && source.contains(".muse-version")
        && source.contains("muse-bin-")
        && source.contains("MUSE_LAUNCHER_INSTALL")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::{
        ffi::OsStringExt,
        fs::{PermissionsExt, symlink},
    };

    struct Fixture {
        root: tempfile::TempDir,
        bin: PathBuf,
        data: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let bin = root.path().join(".local/bin");
            let data = root.path().join(".local/share");
            fs::create_dir_all(&bin).unwrap();
            Self { root, bin, data }
        }

        fn file(&self, path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> PathBuf {
            let path = self.root.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        fn status(&self, provider: Provider) -> ProviderStatus {
            detect(
                provider,
                Some(self.bin.as_os_str()),
                Some(self.root.path()),
                Some(&self.data),
                &|_| None,
            )
        }
    }

    #[test]
    fn detection_records_both_paths_for_claude_native() {
        let fixture = Fixture::new();
        let binary = fixture.file(fixture.data.join("claude/versions/2.1.317"), b"\x7fELFfake");
        let entry = fixture.bin.join("claude");
        symlink(&binary, &entry).unwrap();

        let status = fixture.status(Provider::Claude);
        assert_eq!(status.install_method, ProviderInstallMethod::Native);
        assert_eq!(status.executable, Some(entry));
        assert_eq!(
            status.resolved_executable,
            Some(binary.canonicalize().unwrap())
        );
    }

    #[test]
    fn detects_npm_prefix_from_wrapper_top_level_and_nested_codex_binary() {
        let fixture = Fixture::new();
        let prefix = fixture.data.join("cagents-tools");
        let entry = fixture.bin.join("codex");
        for target in [
            "@openai/codex/bin/codex.js",
            "@openai/codex-linux-x64/vendor/x86_64-unknown-linux-musl/bin/codex",
            "@openai/codex/node_modules/@openai/codex-linux-x64/vendor/x86_64-unknown-linux-musl/bin/codex",
        ] {
            let binary = fixture.file(prefix.join("lib/node_modules").join(target), b"fake");
            symlink(&binary, &entry).unwrap();
            let status = fixture.status(Provider::Codex);
            assert_eq!(
                status.install_method,
                ProviderInstallMethod::Npm {
                    prefix: prefix.canonicalize().unwrap()
                }
            );
            assert_eq!(
                status.resolved_executable,
                Some(binary.canonicalize().unwrap())
            );
            fs::remove_file(&entry).unwrap();
        }
    }

    #[test]
    fn status_round_trips_non_utf8_paths() {
        let fixture = Fixture::new();
        let prefix = fixture.data.join(OsString::from_vec(b"npm-\xff".to_vec()));
        let entry = fixture.bin.join("codex");
        let binary = fixture.file(
            prefix.join("lib/node_modules/@openai/codex/bin/codex.js"),
            b"fake",
        );
        symlink(binary, entry).unwrap();
        let status = fixture.status(Provider::Codex);
        assert_eq!(
            status.install_method,
            ProviderInstallMethod::Npm {
                prefix: prefix.canonicalize().unwrap()
            }
        );
        assert_eq!(
            postcard::from_bytes::<ProviderStatus>(&postcard::to_allocvec(&status).unwrap())
                .unwrap(),
            status
        );
        assert_eq!(
            serde_json::from_value::<ProviderStatus>(serde_json::to_value(&status).unwrap())
                .unwrap(),
            status
        );
    }

    #[test]
    fn intermediary_mise_shim_and_manager_owned_npm_are_managed() {
        let fixture = Fixture::new();
        let entry = fixture.bin.join("codex");
        let shim = fixture.data.join("mise/shims/codex");
        let mise = fixture.file(".local/bin/mise", b"\x7fELFfake");
        fs::create_dir_all(shim.parent().unwrap()).unwrap();
        symlink(mise, &shim).unwrap();
        symlink(&shim, &entry).unwrap();
        assert_eq!(
            fixture.status(Provider::Codex).install_method,
            ProviderInstallMethod::VersionManager("mise".into())
        );

        fs::remove_file(&entry).unwrap();
        let codex = fixture.file(
            ".nvm/versions/node/v22/lib/node_modules/@openai/codex/bin/codex.js",
            b"fake",
        );
        symlink(codex, &entry).unwrap();
        assert_eq!(
            fixture.status(Provider::Codex).install_method,
            ProviderInstallMethod::VersionManager("nvm".into())
        );
    }

    #[test]
    fn system_links_and_custom_mise_paths_are_managed() {
        let paths =
            |first: &str, resolved: &str| vec![PathBuf::from(first), PathBuf::from(resolved)];
        for pair in [
            paths("/usr/local/bin/claude", "/home/user/claude"),
            paths("/home/user/bin/codex", "/usr/bin/codex"),
        ] {
            assert_eq!(
                managed_method(&pair, None, None, &|_| None),
                Some(ProviderInstallMethod::System)
            );
        }
        let pair = paths("/somewhere/mise/shims/codex", "/usr/bin/mise");
        let env = |name: &str| (name == "MISE_DATA_DIR").then(|| OsString::from("/somewhere/mise"));
        assert_eq!(
            managed_method(&pair, None, None, &env),
            Some(ProviderInstallMethod::VersionManager("mise".into()))
        );
    }

    #[test]
    fn distinguishes_local_binaries_muse_launcher_and_custom_wrappers() {
        let fixture = Fixture::new();
        for provider in [Provider::Omp, Provider::Antigravity] {
            assert_eq!(
                fixture.status(provider).install_method,
                ProviderInstallMethod::Missing
            );
            fixture.file(fixture.bin.join(provider.executable()), b"\x7fELFfake");
            assert_eq!(
                fixture.status(provider).install_method,
                ProviderInstallMethod::Standalone
            );
        }

        let muse = fixture.bin.join("muse");
        fixture.file(&muse, b"#!/bin/sh\nexec muse-upstream \"$@\"\n");
        assert_eq!(
            fixture.status(Provider::Muse).install_method,
            ProviderInstallMethod::Unknown
        );

        let launcher = b"#!/bin/bash\nlauncher_url=\"https://api.meta.ai/muse-launcher.sh\"\nstamp=.muse-version\nbinary=muse-bin-1.2.3\ninstall=$MUSE_LAUNCHER_INSTALL\n";
        let upstream = fixture.file(fixture.bin.join("muse-upstream"), launcher);
        let status = fixture.status(Provider::Muse);
        assert_eq!(status.install_method, ProviderInstallMethod::Native);
        assert_eq!(status.executable, Some(upstream));
    }

    #[tokio::test]
    async fn version_requires_successful_exit_and_disables_muse_auto_update() {
        let fixture = Fixture::new();
        let entry = fixture.bin.join("muse");
        fixture.file(&entry, b"#!/bin/sh\ntest \"$1\" = --version && test \"$MUSE_NO_AUTO_UPDATE\" = 1 || exit 1\nprintf 'muse v1.2.3-beta.2\\n'\n");
        assert_eq!(
            super::super::read_version(Provider::Muse, &entry, None)
                .await
                .unwrap(),
            "1.2.3-beta.2"
        );
        fixture.file(&entry, b"#!/bin/sh\nprintf 'muse 1.2.3\\n'\nexit 2\n");
        assert!(
            super::super::read_version(Provider::Muse, &entry, None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn version_accepts_provider_formats() {
        let fixture = Fixture::new();
        for (provider, display, release) in [
            (Provider::Claude, "2.1.317 (Claude Code)", "2.1.317"),
            (Provider::Codex, "codex-cli 0.172.0", "0.172.0"),
            (Provider::Omp, "omp/18.12.6", "18.12.6"),
            (Provider::Antigravity, "1.2.3", "1.2.3"),
        ] {
            let entry = fixture.file(
                fixture.bin.join(provider.executable()),
                format!("#!/bin/sh\nprintf '%s\\n' '{display}'\n"),
            );
            assert_eq!(
                super::super::read_version(provider, &entry, None)
                    .await
                    .unwrap(),
                release,
            );
        }
    }
}
