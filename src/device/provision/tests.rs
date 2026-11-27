use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};

struct Fake {
    version: &'static str,
    glibc: Option<Vec<u32>>,
}

impl Build for Fake {
    fn version(&self) -> &str {
        self.version
    }
    fn required_glibc(&self) -> io::Result<Option<(Vec<u32>, String)>> {
        Ok(self
            .glibc
            .clone()
            .map(|version| (version, "cagents-ui".into())))
    }
}

const OURS: Fake = Fake {
    version: "0.2.4+local.new",
    glibc: None,
};

fn device(executable: &str) -> Device {
    Device {
        name: "build-host".into(),
        host: "build-host".into(),
        executable: executable.into(),
    }
}

/// A device whose install.sh installation serves `current`.
fn installed(current: &str) -> Remote {
    Remote {
        os: "Linux".into(),
        arch: "x86_64".into(),
        glibc: Some(vec![2, 41]),
        home: "/home/user".into(),
        resolved: Some(format!(
            "/home/user/.local/share/cagents-install/versions/{current}/bin/cagents"
        )),
        off_path: false,
        default_prefix: "/home/user/.local/share/cagents-install".into(),
        installation: Some(Installation {
            prefix: "/home/user/.local/share/cagents-install".into(),
            marked: true,
            current: Some(current.into()),
            has_wanted: false,
            wanted_staged_beside: None,
        }),
        protocol: Some(PROTOCOL_VERSION - 2),
    }
}

fn older() -> Daemon {
    Daemon::Speaks(PROTOCOL_VERSION - 2)
}

fn action(remote: &Remote, daemon: &Daemon) -> Action {
    plan(&device("cagents"), &OURS, remote, daemon)
        .unwrap()
        .action
}

fn refusal(local: &Fake, remote: &Remote, daemon: &Daemon) -> String {
    plan(&device("cagents"), local, remote, daemon)
        .unwrap_err()
        .to_string()
}

#[test]
fn an_older_installed_device_gets_this_build_staged_then_handed_over() {
    assert_eq!(
        action(&installed("0.2.4+local.old"), &older()),
        Action::Install {
            prefix: "/home/user/.local/share/cagents-install".into(),
            upgrade: true,
        }
    );
    // An unreachable daemon is the device's own update's to judge.
    assert!(matches!(
        action(
            &installed("0.2.4+local.old"),
            &Daemon::Unknown("reset".into())
        ),
        Action::Install { upgrade: true, .. }
    ));
}

#[test]
fn a_build_already_on_the_device_is_not_sent_again() {
    let mut staged = installed("0.2.4+local.old");
    let installation = staged.installation.as_mut().unwrap();
    installation.has_wanted = true;
    installation.wanted_staged_beside = Some("0.2.4+local.old".into());
    assert_eq!(
        action(&staged, &older()),
        Action::Upgrade {
            cli: "/home/user/.local/share/cagents-install/versions/0.2.4+local.new/bin/cagents"
                .into()
        }
    );
    // Staged beside another version, it is not this update's to finish.
    staged.installation.as_mut().unwrap().wanted_staged_beside = Some("0.2.4+local.other".into());
    assert!(matches!(action(&staged, &older()), Action::Install { .. }));
    // Installed and served, with the daemon left on the build before.
    assert_eq!(
        action(&installed(OURS.version), &older()),
        Action::Upgrade {
            cli: "/home/user/.local/share/cagents-install/current/bin/cagents".into()
        }
    );
}

#[test]
fn a_compatible_device_is_left_alone() {
    let remote = installed("0.2.4+local.old");
    assert_eq!(action(&remote, &Daemon::Compatible), Action::Nothing);
    let mut stopped = remote;
    stopped.protocol = Some(PROTOCOL_VERSION);
    assert_eq!(action(&stopped, &Daemon::Stopped), Action::Nothing);
    // Sending the build again cannot help reach a daemon this computer could
    // not; the note says what might.
    stopped.off_path = true;
    let unreachable = plan(
        &device("cagents"),
        &OURS,
        &stopped,
        &Daemon::Unknown("not found".into()),
    )
    .unwrap();
    assert_eq!(unreachable.action, Action::Nothing);
    assert!(
        unreachable
            .note
            .unwrap()
            .contains("/home/user/.local/bin/cagents")
    );
    stopped.off_path = false;
    stopped.protocol = Some(PROTOCOL_VERSION - 1);
    assert!(matches!(
        action(&stopped, &Daemon::Stopped),
        Action::Install { .. }
    ));
}

#[test]
fn a_device_without_cagents_gets_a_first_install() {
    let mut bare = installed("unused");
    bare.resolved = None;
    bare.installation = None;
    bare.protocol = None;
    let first = plan(&device("cagents"), &OURS, &bare, &Daemon::Stopped).unwrap();
    assert_eq!(
        first.action,
        Action::Install {
            prefix: "/home/user/.local/share/cagents-install".into(),
            upgrade: false,
        }
    );
    assert!(
        first
            .note
            .unwrap()
            .contains("/home/user/.local/bin/cagents")
    );
    // A device already configured with the installer's link needs no note.
    let linked = plan(
        &device("/home/user/.local/bin/cagents"),
        &OURS,
        &bare,
        &Daemon::Stopped,
    )
    .unwrap();
    assert_eq!(linked.note, None);
}

#[test]
fn a_mismatch_says_which_side_to_update() {
    let message = |actual| {
        crate::device::Incompatible {
            device: "build-host".into(),
            actual,
        }
        .to_string()
    };
    let older = message(PROTOCOL_VERSION - 1);
    assert!(
        older.contains("`cagents device update build-host`"),
        "{older}"
    );
    let newer = message(PROTOCOL_VERSION + 1);
    assert!(newer.contains("Update cagents on this computer"), "{newer}");
    assert!(!newer.contains("device update"), "{newer}");
}

#[test]
fn what_the_device_cannot_run_or_should_not_get_is_refused_before_sending() {
    let newer = refusal(
        &OURS,
        &installed("0.2.5"),
        &Daemon::Speaks(PROTOCOL_VERSION + 1),
    );
    assert!(newer.contains("has a newer cagents"), "{newer}");
    assert!(newer.contains("never downgraded"), "{newer}");

    let mut arm = installed("0.2.4+local.old");
    arm.arch = "aarch64".into();
    assert!(refusal(&OURS, &arm, &older()).contains("Linux aarch64"));

    let needs = Fake {
        glibc: Some(vec![2, 44]),
        ..OURS
    };
    let glibc = refusal(&needs, &installed("0.2.4+local.old"), &older());
    assert!(
        glibc.contains("needs glibc 2.44 (cagents-ui), but build-host has glibc 2.41"),
        "{glibc}"
    );
    let mut musl = installed("0.2.4+local.old");
    musl.glibc = None;
    assert!(refusal(&OURS, &musl, &older()).contains("GNU C library"));

    let mut package = installed("0.2.4+local.old");
    package.resolved = Some("/usr/bin/cagents".into());
    package.installation = None;
    let unmanaged = refusal(&OURS, &package, &older());
    assert!(
        unmanaged.contains("(/usr/bin/cagents) was not installed with install.sh"),
        "{unmanaged}"
    );
    // Neither a stopped daemon nor an unreachable one hides a newer build.
    let mut newer_cli = installed("0.2.4+local.later");
    newer_cli.protocol = Some(PROTOCOL_VERSION + 1);
    for daemon in [
        Daemon::Stopped,
        Daemon::Unknown("reset".into()),
        Daemon::Speaks(PROTOCOL_VERSION - 1),
    ] {
        assert!(
            refusal(&OURS, &newer_cli, &daemon).contains("never downgraded"),
            "{daemon:?}"
        );
    }
}

#[test]
fn the_device_shell_reports_its_installation() {
    let home = tempfile::tempdir().unwrap();
    let prefix = home.path().join(".local/share/cagents-install");
    let wanted = "0.2.4+local.new";
    for version in ["0.2.4+local.old", wanted] {
        fs::create_dir_all(prefix.join("versions").join(version).join("bin")).unwrap();
    }
    fs::write(
        prefix.join(".cagents-install-root"),
        "cagents install root\n",
    )
    .unwrap();
    symlink("versions/0.2.4+local.old", prefix.join("current")).unwrap();
    fs::write(
        prefix.join("versions").join(wanted).join(STAGED_MARKER),
        "0.2.4+local.old\n",
    )
    .unwrap();
    let cli = prefix.join("versions/0.2.4+local.old/bin/cagents");
    fs::write(
        &cli,
        "#!/bin/sh\n[ \"$*\" = 'daemon protocol-version' ] && echo 208\n",
    )
    .unwrap();
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o755)).unwrap();
    let bin = home.path().join(".local/bin");
    fs::create_dir_all(&bin).unwrap();
    symlink(prefix.join("current/bin/cagents"), bin.join("cagents")).unwrap();

    let probe = |executable: &str, path: &str| {
        let mut child = Command::new("sh")
            .arg("-s")
            .env_clear()
            .env("HOME", home.path())
            .env("PATH", path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        write!(
            child.stdin.take().unwrap(),
            "exe={}\nwanted={}\nmarker={STAGED_MARKER}\n{PROBE}",
            quote(executable),
            quote(wanted)
        )
        .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        Remote::parse(&String::from_utf8(output.stdout).unwrap())
    };
    let system = "/usr/bin:/bin";
    let remote = probe("cagents", &format!("{}:{system}", bin.display()));
    let canonical = prefix.canonicalize().unwrap();
    assert_eq!(remote.os, "Linux");
    assert_eq!(
        remote.resolved.as_deref(),
        Some(
            canonical
                .join("versions/0.2.4+local.old/bin/cagents")
                .to_str()
                .unwrap()
        )
    );
    assert!(!remote.off_path);
    assert_eq!(remote.protocol, Some(208));
    assert_eq!(
        remote.installation,
        Some(Installation {
            prefix: canonical.to_str().unwrap().into(),
            marked: true,
            current: Some("0.2.4+local.old".into()),
            has_wanted: true,
            wanted_staged_beside: Some("0.2.4+local.old".into()),
        })
    );
    // SSH commands often lack ~/.local/bin; the installer's tree is found anyway.
    let off_path = probe("cagents", system);
    assert!(off_path.off_path);
    assert_eq!(off_path.installation, remote.installation);
    // So is a configured executable that is not there.
    let moved = probe("/opt/cagents/bin/cagents", system);
    assert!(moved.off_path);
    assert_eq!(moved.installation, remote.installation);
    // Nothing installed at all.
    fs::remove_file(prefix.join("current")).unwrap();
    let missing = probe("/nonexistent/cagents", system);
    assert_eq!((missing.resolved, missing.installation), (None, None));
}

fn listed(archive: &[u8]) -> Vec<(String, u32, Vec<u8>)> {
    let mut entries = Vec::new();
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let name = entry.path().unwrap().to_string_lossy().into_owned();
        let mode = entry.header().mode().unwrap();
        let mut content = Vec::new();
        entry.read_to_end(&mut content).unwrap();
        entries.push((name, mode, content));
    }
    entries
}

#[test]
fn the_archive_is_the_installed_tree_in_release_layout_without_local_markers() {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(root.path().join("share/doc/cagents")).unwrap();
    fs::write(bin.join("cagents"), "cli").unwrap();
    fs::set_permissions(bin.join("cagents"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(bin.join(".cagents-ui-lease-v1"), "").unwrap();
    fs::write(root.path().join("share/doc/cagents/LICENSE"), "license").unwrap();
    fs::write(root.path().join(STAGED_MARKER), "0.2.4+local.old\n").unwrap();
    fs::write(root.path().join(".deferred-owner.json"), "{}").unwrap();

    let mut archive = Vec::new();
    let mut read = Vec::new();
    pack(root.path(), "cagents-0.2.4", &mut archive, &mut |bytes| {
        read.push(bytes)
    })
    .unwrap();
    let entries = listed(&archive);
    let names: Vec<_> = entries.iter().map(|(name, ..)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "cagents-0.2.4/",
            "cagents-0.2.4/bin/",
            "cagents-0.2.4/bin/.cagents-ui-lease-v1",
            "cagents-0.2.4/bin/cagents",
            "cagents-0.2.4/share/",
            "cagents-0.2.4/share/doc/",
            "cagents-0.2.4/share/doc/cagents/",
            "cagents-0.2.4/share/doc/cagents/LICENSE",
        ]
    );
    let cli = &entries[3];
    assert_eq!(
        (cli.1 & 0o777, cli.2.as_slice()),
        (0o755, b"cli".as_slice())
    );
    assert_eq!(read.last(), Some(&tree_size(root.path()).unwrap()));
    assert_eq!(tree_size(root.path()).unwrap(), 10);

    // A link in the tree is not shipped as one.
    symlink("/etc/passwd", bin.join("cagentsd")).unwrap();
    assert!(
        pack(root.path(), "cagents-0.2.4", &mut Vec::new(), &mut |_| ())
            .unwrap_err()
            .to_string()
            .contains("not a regular file")
    );
}

/// The installer's own archive checks, taken from the install.sh this build
/// sends, so a change on either side fails here rather than on a device.
#[test]
fn the_installer_accepts_the_archive() {
    let root = tempfile::tempdir().unwrap();
    for (path, mode) in [
        ("bin/cagents", 0o755),
        ("bin/cagents-ui", 0o755),
        ("bin/cagentsd", 0o755),
        ("bin/cagents-uninstall", 0o755),
        ("bin/.cagents-ui-lease-v1", 0o644),
        ("share/applications/dev.cagents.Cagents.desktop", 0o644),
        ("share/autostart/dev.cagents.Cagents.Daemon.desktop", 0o644),
        (
            "share/icons/hicolor/scalable/apps/dev.cagents.Cagents.svg",
            0o644,
        ),
        (
            "share/icons/hicolor/128x128/apps/dev.cagents.Cagents.png",
            0o644,
        ),
        ("share/metainfo/dev.cagents.Cagents.metainfo.xml", 0o644),
        ("share/doc/cagents/LICENSE", 0o644),
        (STAGED_MARKER, 0o644),
    ] {
        let path = root.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "x").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    }
    let top = "cagents-0.2.4+local.new";
    let archive = root.path().join("cagents.tar.gz");
    let mut packed = Vec::new();
    // Packed before the archive exists, so the tree does not contain it.
    pack(root.path(), top, &mut packed, &mut |_| ()).unwrap();
    fs::write(&archive, packed).unwrap();

    let between = |start: &str, end: &str| {
        let from = INSTALLER.find(start).expect(start);
        let to = from + INSTALLER[from..].find(end).expect(end) + end.len();
        &INSTALLER[from..to]
    };
    let entry_types = between(
        "awk '\n    {\n        type = substr($1, 1, 1)",
        "' \"$ARCHIVE_VERBOSE\"",
    );
    let members = between("validate_version_members() {\n", "\n}\n");
    let check = format!(
        "set -e\nARCHIVE_VERBOSE=/dev/stdin\nPREPARED_MARKER=p LOCAL_CANDIDATE_MARKER={STAGED_MARKER} LOCAL_SOURCE_MARKER=s RETAINED_MARKER=r\n{members}\nLC_ALL=C tar -tvzf \"$1\" | {entry_types}\nLC_ALL=C tar -tzf \"$1\" | validate_version_members {top} 0\n",
        entry_types = entry_types.trim_end_matches(" \"$ARCHIVE_VERBOSE\"")
    );
    let output = Command::new("sh")
        .args(["-c", &check, "sh"])
        .arg(&archive)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "install.sh would refuse the archive: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn glibc_requirements_come_from_an_executables_version_needs() {
    let versions = glibc_versions(&std::env::current_exe().unwrap()).unwrap();
    assert!(
        versions.iter().any(|version| version.first() == Some(&2)),
        "{versions:?}"
    );
    let script = tempfile::NamedTempFile::new().unwrap();
    fs::write(script.path(), "#!/bin/sh\n").unwrap();
    assert!(glibc_versions(script.path()).unwrap().is_empty());
    assert_eq!(parse_version("2.41"), Some(vec![2, 41]));
    assert!(vec![2, 9] < vec![2, 41]);
}
