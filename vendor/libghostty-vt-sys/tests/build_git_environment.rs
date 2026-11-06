use super::*;
use std::{
    fs,
    process::{Output, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const CASE: &str = "CAGENTS_GHOSTTY_GIT_FIXTURE";
const SELECTOR: &str =
    "build_git_environment::default_git_commands_ignore_inherited_repository_routing";

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn output(mut command: Command, root: &Path) -> Output {
    // Fixture configuration only: never remove repository routing here. The
    // actual production constructors must remove those inherited variables.
    command
        .env("GIT_CONFIG_GLOBAL", root.join("no-global-config"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT")
        .env("GIT_TEMPLATE_DIR", root.join("empty-template"))
        .env("GIT_AUTHOR_NAME", "fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@localhost")
        .env("GIT_COMMITTER_NAME", "fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@localhost")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("original fixture subprocess timed out: {command:?}");
        }
        thread::sleep(Duration::from_millis(5));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git(root: &Path, repository: &Path, args: &[&str]) -> String {
    let mut command = ghostty_git_command();
    command.arg("-C").arg(repository).args(args);
    String::from_utf8(output(command, root).stdout)
        .unwrap()
        .trim_end()
        .to_owned()
}

#[test]
fn default_git_commands_ignore_inherited_repository_routing() {
    if let Some(root) = env::var_os(CASE) {
        let root = PathBuf::from(root);
        let source = root.join("source");
        let old = fs::read_to_string(root.join("old-head")).unwrap();
        output(ghostty_clone_command(root.join("origin"), &source), &root);
        output(ghostty_checkout_command(&source, &old), &root);
        assert_eq!(git(&root, &source, &["rev-parse", "HEAD"]), old);
        assert_eq!(fs::read(source.join("revision")).unwrap(), b"old\n");

        let patch = root.join("fixture.patch");
        fs::write(
            &patch,
            b"--- a/revision\n+++ b/revision\n@@ -1 +1 @@\n-old\n+patched\n",
        )
        .unwrap();
        output(
            ghostty_patch_command_at(&source, &patch, false, true),
            &root,
        );
        assert_eq!(fs::read(source.join("revision")).unwrap(), b"old\n");
        output(
            ghostty_patch_command_at(&source, &patch, false, false),
            &root,
        );
        output(ghostty_patch_command_at(&source, &patch, true, true), &root);
        assert_eq!(fs::read(source.join("revision")).unwrap(), b"patched\n");
        return;
    }

    let root = env::temp_dir().join(format!(
        "cagents-ghostty-git-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let _scratch = Scratch(root.clone());
    fs::create_dir(root.join("empty-template")).unwrap();
    let origin = root.join("origin");
    let caller = root.join("caller");
    let mut init = ghostty_git_command();
    init.args(["init", "--initial-branch=main"]).arg(&origin);
    output(init, &root);
    fs::write(origin.join("revision"), b"old\n").unwrap();
    git(&root, &origin, &["add", "revision"]);
    git(&root, &origin, &["commit", "-m", "old"]);
    let old = git(&root, &origin, &["rev-parse", "HEAD"]);
    fs::write(root.join("old-head"), old).unwrap();
    fs::write(origin.join("revision"), b"new\n").unwrap();
    git(&root, &origin, &["commit", "-am", "new"]);
    let new = git(&root, &origin, &["rev-parse", "HEAD"]);
    output(ghostty_clone_command(&origin, &caller), &root);
    output(ghostty_checkout_command(&caller, &new), &root);
    let index = fs::read(caller.join(".git/index")).unwrap();

    // Only this self-exec child inherits poisoned routing. The test process,
    // other Rust tests and every real repository retain their environment.
    let mut child = Command::new(env::current_exe().unwrap());
    child
        .args(["--exact", SELECTOR, "--nocapture", "--test-threads=1"])
        .current_dir(&root)
        .env(CASE, &root)
        .env("GIT_DIR", caller.join(".git"))
        .env("GIT_WORK_TREE", &caller)
        .env("GIT_COMMON_DIR", caller.join(".git"))
        .env("GIT_INDEX_FILE", caller.join(".git/index"))
        .env("GIT_OBJECT_DIRECTORY", caller.join(".git/objects"))
        .env(
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            caller.join(".git/objects"),
        )
        .env("GIT_CEILING_DIRECTORIES", &caller)
        .env("GIT_DISCOVERY_ACROSS_FILESYSTEM", "1");
    let observed = output(child, &root);
    assert!(String::from_utf8_lossy(&observed.stdout).contains("1 passed; 0 failed"));
    assert_eq!(git(&root, &caller, &["rev-parse", "HEAD"]), new);
    assert_eq!(fs::read(caller.join("revision")).unwrap(), b"new\n");
    assert_eq!(fs::read(caller.join(".git/index")).unwrap(), index);
    assert_eq!(
        fs::read(root.join("source/revision")).unwrap(),
        b"patched\n"
    );
}
