//! CLI-only behavior: reads can refuse startup even when a launchable sibling exists.
use serde_json::Value;
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::{Command, Output, Stdio}};

struct Fixture { root: tempfile::TempDir, cli: PathBuf }
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for path in ["bin", "home", "run", "data", "config", "cache"] { fs::create_dir(root.path().join(path)).unwrap(); }
        fs::set_permissions(root.path().join("run"), fs::Permissions::from_mode(0o700)).unwrap();
        let cli = root.path().join("bin/cagents");
        if fs::hard_link(env!("CARGO_BIN_EXE_cagents"), &cli).is_err() { fs::copy(env!("CARGO_BIN_EXE_cagents"), &cli).unwrap(); }
        let daemon = root.path().join("bin/cagentsd");
        fs::write(&daemon, "#!/bin/sh\nprintf invoked > \"$HOME/../spawned\"\nexit 72\n").unwrap();
        fs::set_permissions(daemon, fs::Permissions::from_mode(0o700)).unwrap();
        Self { root, cli }
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.cli);
        command.env_clear().env("HOME", self.root.path().join("home"))
            .env("XDG_RUNTIME_DIR", self.root.path().join("run"))
            .env("XDG_DATA_HOME", self.root.path().join("data"))
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_CACHE_HOME", self.root.path().join("cache"))
            .env("PATH", "/usr/bin:/bin").current_dir(self.root.path()).stdin(Stdio::null());
        command.arg("--json").args(args);
        command
    }
    fn output(&self, args: &[&str], environment_policy: bool) -> Output {
        let mut command = self.command(args);
        if environment_policy { command.env("CAGENTS_NO_START", "1"); }
        command.output().unwrap()
    }
}

#[test]
fn every_read_can_skip_daemon_start_through_flag_or_environment() {
    let fixture = Fixture::new();
    for environment in [false, true] {
        for args in [vec!["agent", "list"], vec!["automation", "list"], vec!["channel", "list"],
            vec!["terminal", "list"], vec!["project", "list"], vec!["sessions"], vec!["whoami"], vec!["account", "info"]] {
            let mut arguments = args;
            if !environment { arguments.push("--no-start"); }
            let output = fixture.output(&arguments, environment);
            assert_eq!(output.status.code(), Some(1), "{arguments:?}: {output:?}");
            let value: Value = serde_json::from_slice(&output.stderr).unwrap();
            assert_eq!(value["error"]["kind"], "daemon_not_running", "{value}");
            assert!(!fixture.root.path().join("spawned").exists(), "daemon was spawned: {arguments:?}");
        }
    }
}

#[test]
fn stopping_an_absent_daemon_twice_succeeds_and_has_a_stable_shape() {
    let fixture = Fixture::new();
    for _ in 0..2 {
        let output = fixture.output(&["daemon", "stop"], false);
        assert!(output.status.success(), "{output:?}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["data"], serde_json::json!({"state":"already_stopped"}));
        assert!(!fixture.root.path().join("spawned").exists());
    }
}

#[test]
fn an_agent_token_cannot_remove_an_account_or_start_services() {
    let fixture = Fixture::new();
    for args in [vec!["account", "remove", "claude/system"], vec!["--device", "unconfigured-remote", "account", "remove", "claude/system"]] {
        let output = fixture.command(&args).env("CAGENTS_AGENT_AUTHORITY", "fixture-token").output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        let value: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(value["error"]["kind"], "account_authority");
        assert!(!fixture.root.path().join("spawned").exists());
    }
}

#[test]
fn generated_fish_script_completes_nested_agent_spawn() {
    let fixture = Fixture::new();
    let output = fixture.output(&["completions", "fish"], false);
    assert!(output.status.success(), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let script = fixture.root.path().join("cagents.fish");
    fs::write(&script, value["data"]["script"].as_str().unwrap()).unwrap();
    let result = Command::new("fish").env_clear().env("HOME", fixture.root.path().join("home"))
        .env("PATH", format!("{}:/usr/bin:/bin", fixture.root.path().join("bin").display()))
        .args(["--no-config", "-c", "source $argv[1]; complete -C 'cagents agent sp'", "--"])
        .arg(script).output();
    let output = match result { Ok(output) => output, Err(error) if error.kind() == std::io::ErrorKind::NotFound => return, Err(error) => panic!("{error}") };
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).lines().any(|line| line.starts_with("spawn\t")), "{output:?}");
    assert!(!fixture.root.path().join("spawned").exists());
}

#[test]
fn settings_can_be_read_and_changed_without_any_daemon() {
    let fixture = Fixture::new();
    let changed = fixture.output(&["settings", "set", "terminal_font_size", "15", "--no-start"], false);
    assert!(changed.status.success(), "{changed:?}");
    let changed: Value = serde_json::from_slice(&changed.stdout).unwrap();
    assert_eq!(changed["data"]["key"], "terminal_font_size");
    assert_eq!(changed["data"]["value"], 15.0);
    let loaded = fixture.output(&["settings", "get", "terminal_font_size"], false);
    assert!(loaded.status.success(), "{loaded:?}");
    let loaded: Value = serde_json::from_slice(&loaded.stdout).unwrap();
    assert_eq!(loaded["data"]["value"], 15.0);
    let path = fixture.output(&["settings", "path"], false);
    assert!(path.status.success(), "{path:?}");
    let path: Value = serde_json::from_slice(&path.stdout).unwrap();
    assert_eq!(path["data"]["path"], changed["data"]["path"]);
    assert!(!fixture.root.path().join("spawned").exists());
}
