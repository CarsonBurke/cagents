#![cfg(target_os = "linux")]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

fn executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn entries(path: &Path) -> Vec<PathBuf> {
    let mut entries = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

#[test]
fn standalone_editor_switches_a_complete_private_set_without_replacing_cagents() {
    let root = tempfile::Builder::new()
        .prefix("ceditor-install \"quoted\" \\")
        .tempdir()
        .unwrap();
    let target = root.path().join("target");
    let mock = root.path().join("tools");
    let prefix = root.path().join("prefix");
    fs::create_dir_all(target.join("release")).unwrap();
    fs::create_dir_all(&mock).unwrap();
    fs::create_dir_all(prefix.join("bin")).unwrap();
    fs::write(prefix.join("bin/cagents"), "other installed CLI").unwrap();
    fs::write(prefix.join("bin/cagentsd"), "other installed daemon").unwrap();
    executable(
        &mock.join("cargo"),
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$CED_TEST_LOG"
name=ceditor
while [ "$#" -gt 0 ]; do
    if [ "$1" = --bin ]; then shift; name=$1; fi
    shift
done
case $name in
    cagents) printf '%s\n' "$CED_TEST_CLI_ARTIFACT" ;;
    cagentsd) printf '%s\n' "$CED_TEST_DAEMON_ARTIFACT" ;;
    ceditor) printf '%s\n' "$CED_TEST_EDITOR_ARTIFACT" ;;
    *) exit 64 ;;
esac
"#,
    );
    executable(
        &mock.join("install"),
        r#"#!/bin/sh
destination=
for argument do destination=$argument; done
case "$destination" in
    "$CED_TEST_PREFIX"/lib/ceditor/releases/build.*/bin/cagentsd)
        if [ "$CED_TEST_FAIL_COPY" = 1 ]; then
            staged_bin=${destination%/*}
            [ "$(cat "$staged_bin/ceditor")" = second-ceditor ] || exit 74
            [ "$(cat "$staged_bin/cagents")" = second-cagents ] || exit 75
            printf '%s\n' "$staged_bin" > "$CED_TEST_COPY_FAILURE"
            exit 73
        fi
        ;;
esac
exec /usr/bin/install "$@"
"#,
    );
    for tool in ["gtk-update-icon-cache", "update-desktop-database"] {
        executable(&mock.join(tool), "#!/bin/sh\nexit 0\n");
    }
    let artifact = |name: &str| {
        serde_json::json!({
            "reason": "compiler-artifact",
            "target": {"name": name, "kind": ["bin"]},
            "executable": target.join("release").join(name),
        })
        .to_string()
    };
    let failure_marker = root.path().join("copy-failure");
    let install = |fail_copy: bool| {
        Command::new("sh")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/install-ceditor"))
            .arg("--prefix")
            .arg(&prefix)
            .env("HOME", root.path())
            .env("PATH", format!("{}:/usr/bin:/bin", mock.display()))
            .env("CED_TEST_LOG", root.path().join("builds"))
            .env("CED_TEST_CLI_ARTIFACT", artifact("cagents"))
            .env("CED_TEST_DAEMON_ARTIFACT", artifact("cagentsd"))
            .env("CED_TEST_EDITOR_ARTIFACT", artifact("ceditor"))
            .env("CED_TEST_PREFIX", &prefix)
            .env("CED_TEST_COPY_FAILURE", &failure_marker)
            .env("CED_TEST_FAIL_COPY", if fail_copy { "1" } else { "0" })
            .env_remove("CARGO_BUILD_TARGET")
            .output()
            .unwrap()
    };
    for name in ["ceditor", "cagents", "cagentsd"] {
        executable(&target.join("release").join(name), &format!("first-{name}"));
    }
    let first = install(false);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let served = prefix.join("bin/ceditor");
    let old = served.canonicalize().unwrap();
    let private = prefix.join("lib/ceditor");
    let current = private.join("current");
    let releases = private.join("releases");
    let original_public_link = fs::read_link(&served).unwrap();
    let original_current_link = fs::read_link(&current).unwrap();
    let original_releases = entries(&releases);
    let original_private_entries = entries(&private);
    let original_public_entries = entries(&prefix.join("bin"));
    let assert_original_publication = || {
        assert_eq!(fs::read_link(&served).unwrap(), original_public_link);
        assert_eq!(fs::read_link(&current).unwrap(), original_current_link);
        assert_eq!(served.canonicalize().unwrap(), old);
        for name in ["ceditor", "cagents", "cagentsd"] {
            assert_eq!(
                fs::read_to_string(old.with_file_name(name)).unwrap(),
                format!("first-{name}")
            );
        }
        assert_eq!(
            fs::read_to_string(old.with_file_name(".ceditor-companions")).unwrap(),
            "ceditor companions v1\n"
        );
        assert_eq!(entries(&releases), original_releases);
        assert_eq!(entries(&private), original_private_entries);
        assert_eq!(entries(&prefix.join("bin")), original_public_entries);
    };
    assert_eq!(
        fs::read_to_string(old.with_file_name(".ceditor-companions")).unwrap(),
        "ceditor companions v1\n"
    );
    for name in ["ceditor", "cagents", "cagentsd"] {
        assert_eq!(
            fs::read_to_string(old.with_file_name(name)).unwrap(),
            format!("first-{name}")
        );
        executable(
            &target.join("release").join(name),
            &format!("second-{name}"),
        );
    }
    // Artifact preflight must preserve the original publication before staging.
    fs::remove_file(target.join("release/cagentsd")).unwrap();
    assert!(!install(false).status.success());
    assert_original_publication();
    executable(&target.join("release/cagentsd"), "second-cagentsd");

    // This separate fault reaches staging only after two real binary copies.
    assert!(!failure_marker.exists());
    let failed = install(true);
    assert_eq!(failed.status.code(), Some(73));
    let failed_bin = fs::read_to_string(&failure_marker).unwrap();
    let failed_bin = Path::new(failed_bin.strip_suffix('\n').unwrap());
    let failed_release = failed_bin.parent().unwrap();
    assert_eq!(failed_release.parent(), Some(releases.as_path()));
    assert!(
        !failed_release.exists(),
        "the partial release must be removed"
    );
    assert_original_publication();

    assert!(install(false).status.success());
    let new = served.canonicalize().unwrap();
    assert_ne!(new, old);
    assert!(
        old.exists(),
        "running old windows retain their immutable companions"
    );
    for name in ["ceditor", "cagents", "cagentsd"] {
        assert_eq!(
            fs::read_to_string(new.with_file_name(name)).unwrap(),
            format!("second-{name}")
        );
    }
    assert_eq!(
        fs::read_to_string(prefix.join("bin/cagents")).unwrap(),
        "other installed CLI"
    );
    assert_eq!(
        fs::read_to_string(prefix.join("bin/cagentsd")).unwrap(),
        "other installed daemon"
    );
    let builds = fs::read_to_string(root.path().join("builds")).unwrap();
    assert!(builds.contains("build --locked --release --no-default-features --bin cagents --message-format=json-render-diagnostics\n"));
    assert!(builds.contains(
        "build --locked --release --no-default-features --features host --bin cagentsd --message-format=json-render-diagnostics\n"
    ));
    assert!(builds.contains(
        "build --locked --release --package ceditor --message-format=json-render-diagnostics\n"
    ));
}
