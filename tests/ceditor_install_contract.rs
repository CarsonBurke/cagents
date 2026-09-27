#![cfg(target_os = "linux")]

use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

fn executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn standalone_editor_switches_a_complete_private_set_without_replacing_cagents() {
    let root = tempfile::tempdir().unwrap();
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
if [ "$1" = metadata ]; then printf '{"target_directory":"%s"}\n' "$CED_TEST_TARGET"; fi
"#,
    );
    let install = || {
        Command::new("sh")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/install-ceditor"))
            .arg("--prefix")
            .arg(&prefix)
            .env("HOME", root.path())
            .env("PATH", format!("{}:/usr/bin:/bin", mock.display()))
            .env("CED_TEST_TARGET", &target)
            .env("CED_TEST_LOG", root.path().join("builds"))
            .env_remove("CARGO_BUILD_TARGET")
            .output()
            .unwrap()
    };
    for name in ["ceditor", "cagents", "cagentsd"] {
        executable(&target.join("release").join(name), &format!("first-{name}"));
    }
    let first = install();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let served = prefix.join("bin/ceditor");
    let old = served.canonicalize().unwrap();
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
    // A missing final companion must not publish the partly copied new build.
    fs::remove_file(target.join("release/cagentsd")).unwrap();
    assert!(!install().status.success());
    assert_eq!(served.canonicalize().unwrap(), old);
    assert_eq!(fs::read_to_string(&served).unwrap(), "first-ceditor");
    executable(&target.join("release/cagentsd"), "second-cagentsd");
    assert!(install().status.success());
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
    assert!(builds.contains("build --locked --release --no-default-features --bin cagents\n"));
    assert!(builds.contains(
        "build --locked --release --no-default-features --features host --bin cagentsd\n"
    ));
    assert!(builds.contains("build --locked --release --package ceditor\n"));
}
