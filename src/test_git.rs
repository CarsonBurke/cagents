//! Owned bounded Git fixtures, including read-only product calls in the child.
use crate::process::{
    CancellationToken, CommandSpec, ProcessExecutor, ProcessOutput, SystemProcessExecutor,
};
use std::time::Duration;

pub(crate) fn isolated_command(mut command: CommandSpec) -> CommandSpec {
    for (name, _) in
        std::env::vars_os().filter(|(name, _)| name.as_encoded_bytes().starts_with(b"GIT_"))
    {
        command = command.env_remove(name);
    }
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
}

pub(crate) fn run(command: CommandSpec) -> ProcessOutput {
    let output = SystemProcessExecutor
        .execute(
            &command
                .timeout(Duration::from_secs(30))
                .output_limits(64 * 1024, 64 * 1024),
            &CancellationToken::default(),
        )
        .expect("owned bounded fixture child");
    assert!(
        output.success,
        "fixture child failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

pub(crate) fn isolated_test(test: &str) -> bool {
    const CHILD: &str = "CAGENTS_TEST_ISOLATED_GIT";
    if std::env::var_os(CHILD).as_deref() == Some(std::ffi::OsStr::new(test)) {
        for (name, value) in
            std::env::vars_os().filter(|(name, _)| name.as_encoded_bytes().starts_with(b"GIT_"))
        {
            let expected = match name.to_str() {
                Some("GIT_CONFIG_GLOBAL" | "GIT_CONFIG_SYSTEM") => "/dev/null",
                Some("GIT_CONFIG_NOSYSTEM") => "1",
                _ => panic!("unexpected Git environment in isolated fixture: {name:?}"),
            };
            assert_eq!(value, std::ffi::OsStr::new(expected));
        }
        return true;
    }
    let command = CommandSpec::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(CHILD, test);
    let output = run(isolated_command(command));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("test result: ok. 1 passed;"),
        "the owned child must execute exactly its named fixture"
    );
    false
}
