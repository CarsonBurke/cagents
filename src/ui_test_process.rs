//! Bounded current-test-executable isolation for process-global UI widget state.
//!
//! Requires the reviewed process executor's original Child/pidfd ownership,
//! deadlines and bounded output. It proves leader/group cleanup only. Every
//! actual run still needs the strict outer build queue's separate whole-family
//! ECHILD/quiescence receipt and retained original parent tool completion.
use crate::process::{CancellationToken, CommandSpec, ProcessExecutor, ProcessOutput,
    ProcessFailure, SystemProcessExecutor};
use std::time::Duration;

const STDOUT_LIMIT:usize=256*1024;
const STDERR_LIMIT:usize=64*1024;
const DEADLINE:Duration=Duration::from_secs(30);
const PARENT_PID:&str="CAGENTS_UI_TEST_ORIGINAL_PARENT_PID";
const PARENT_START:&str="CAGENTS_UI_TEST_ORIGINAL_PARENT_START";
fn start_ticks(pid:u32)->String {
    let stat=std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("original parent identity");
    stat.rsplit_once(')').expect("process stat command boundary").1.split_whitespace()
        .nth(19).expect("process start ticks").to_owned()
}
fn arm_owner_loss() {
    let expected:u32=std::env::var(PARENT_PID).expect("private launch parent").parse().unwrap();
    let expected_start=std::env::var(PARENT_START).expect("private launch parent start");
    #[cfg(target_os="linux")]
    {
        // SAFETY: changes only this exact private test child's own parent-death
        // disposition. The following parent/start check closes pre-arm loss;
        // no numeric PID is used to send a signal to another process.
        assert_eq!(unsafe {libc::prctl(libc::PR_SET_PDEATHSIG,libc::SIGKILL,0,0,0)},0);
        assert_eq!(unsafe {libc::getppid()} as u32,expected,"original spawning parent already changed");
        assert_eq!(start_ticks(expected),expected_start,"original parent identity changed");
    }
    #[cfg(not(target_os="linux"))]
    panic!("private UI fixture owner-loss protection requires Linux");
}

fn run(test:&str, child_key:&str, value:&str, deadline:Duration, limits:(usize,usize))->Result<ProcessOutput,ProcessFailure> {
    use std::os::unix::fs::{PermissionsExt,OpenOptionsExt};
    use std::io::Write as _;
    let directory=tempfile::Builder::new().prefix("cagents-ui-fixture-").tempdir().expect("private fixture directory");
    std::fs::set_permissions(directory.path(),std::fs::Permissions::from_mode(0o700)).unwrap();
    for name in ["home","config","data","cache","runtime","tmp"] {
        let path=directory.path().join(name);std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(path,std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let spec=CommandSpec::new(std::env::current_exe().expect("current test executable"))
        .args([test,"--exact","--nocapture","--test-threads=1"])
        .cwd(directory.path()).env("HOME",directory.path().join("home"))
        .env("XDG_CONFIG_HOME",directory.path().join("config"))
        .env("XDG_DATA_HOME",directory.path().join("data"))
        .env("XDG_CACHE_HOME",directory.path().join("cache"))
        .env("XDG_RUNTIME_DIR",directory.path().join("runtime"))
        .env("TMPDIR",directory.path().join("tmp"))
        .env(PARENT_PID,std::process::id().to_string()).env(PARENT_START,start_ticks(std::process::id()))
        .env(child_key,value).timeout(deadline).output_limits(limits.0,limits.1);
    let result=SystemProcessExecutor.execute(&spec,&CancellationToken::new());
    // Preserve bounded logs and the private test state for any failed run.
    // No environment values are logged. Outer B0 still owns family settlement.
    let failed=match &result {Ok(output)=>!exact_pass(output,test),Err(_)=>true};
    if failed {
        let (stdout,stderr)=match &result {Ok(output)=>(&output.stdout,&output.stderr),Err(error)=>(&error.stdout,&error.stderr)};
        for (name,bytes) in [("stdout.log",stdout),("stderr.log",stderr)] {
            let mut file=std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600)
                .open(directory.path().join(name)).unwrap();
            file.write_all(bytes).unwrap();
        }
        let retained=directory.keep();
        eprintln!("bounded UI fixture failure evidence retained at {}",retained.display());
    }
    result
}
fn exact_pass(output:&ProcessOutput, test:&str)->bool {
    if !output.success || output.exit_code!=Some(0) || output.signal.is_some() {return false;}
    let text=String::from_utf8_lossy(&output.stdout);
    let named=format!("test {test} ... ok");
    text.lines().filter(|line|*line=="running 1 test").count()==1
        && text.lines().filter(|line|*line==named).count()==1
        && text.lines().filter(|line|line.starts_with("test result:")).count()==1
        && text.lines().any(|line|line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; "))
}
/// Returns true in the supervising test, false inside its exact isolated case.
/// The caller returns immediately on true; the actual child body uses the same
/// named libtest entry. Only this current executable is ever spawned.
pub(crate) fn isolated(test:&str, child_key:&str)->bool {
    if std::env::var_os(child_key).as_deref()==Some(std::ffi::OsStr::new("1")) {arm_owner_loss();return false;}
    let output=run(test,child_key,"1",DEADLINE,(STDOUT_LIMIT,STDERR_LIMIT)).unwrap_or_else(|failure| {
        panic!("isolated UI test {test} failed: {failure}\n{}\n{}",
            String::from_utf8_lossy(&failure.stdout),String::from_utf8_lossy(&failure.stderr))
    });
    assert!(output.stdout.len()<=STDOUT_LIMIT && output.stderr.len()<=STDERR_LIMIT);
    assert!(exact_pass(&output,test),"isolated UI test did not execute exactly {test}:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::ProcessFailureKind;
    use std::io::Write as _;
    const CHILD:&str="CAGENTS_TEST_BOUNDED_UI_HELPER_NEGATIVE";
    const FIXTURE:&str="ui_test_process::tests::child_fixture";
    #[test]
    fn child_fixture() {
        if std::env::var_os(CHILD).is_some() {arm_owner_loss();}
        match std::env::var(CHILD).ok().as_deref() {
            Some("hang")=>{
                println!("bounded UI fixture entered");std::io::stdout().flush().unwrap();
                let end=std::time::Instant::now()+Duration::from_secs(8);
                while std::time::Instant::now()<end {std::thread::park_timeout(Duration::from_millis(20));}
            }
            Some("cap")=>{
                let bytes=[b'x';4096];let mut stdout=std::io::stdout().lock();
                loop {stdout.write_all(&bytes).unwrap();stdout.flush().unwrap();}
            }
            Some("private")=>{
                use std::os::unix::fs::PermissionsExt;
                let root=std::env::current_dir().unwrap();
                assert_eq!(std::fs::metadata(&root).unwrap().permissions().mode()&0o777,0o700);
                for (key,name) in [("HOME","home"),("XDG_CONFIG_HOME","config"),("XDG_DATA_HOME","data"),
                    ("XDG_CACHE_HOME","cache"),("XDG_RUNTIME_DIR","runtime"),("TMPDIR","tmp")] {
                    let path=std::path::PathBuf::from(std::env::var_os(key).unwrap());
                    assert_eq!(path,root.join(name));
                    assert_eq!(std::fs::metadata(path).unwrap().permissions().mode()&0o777,0o700);
                }
            }
            _=>{}
        }
    }
    #[test]
    fn current_executable_hang_obeys_deadline_and_does_not_claim_family_proof() {
        let start=std::time::Instant::now();
        let error=run(FIXTURE,CHILD,"hang",Duration::from_secs(2),(4096,4096)).unwrap_err();
        assert_eq!(error.kind,ProcessFailureKind::TimedOut);assert!(error.started);
        assert!(String::from_utf8_lossy(&error.stdout).contains("bounded UI fixture entered"));
        assert!(error.stdout.len()<=4096 && error.stderr.len()<=4096);
        assert!(start.elapsed()<Duration::from_secs(10));
        // Strict outer B0 separately proves every actual descendant settled.
    }
    #[test]
    fn current_executable_output_is_capped_before_collection() {
        let error=run(FIXTURE,CHILD,"cap",Duration::from_secs(5),(16384,4096)).unwrap_err();
        assert_eq!(error.kind,ProcessFailureKind::OutputLimit);assert!(error.started);
        assert!(error.stdout.len()<=16384 && error.stderr.len()<=4096);
    }
    #[test]
    fn current_executable_has_private_home_xdg_and_working_directory() {
        let output=run(FIXTURE,CHILD,"private",Duration::from_secs(5),(4096,4096)).unwrap();
        assert!(exact_pass(&output,FIXTURE));
    }
    #[test]
    fn an_empty_filter_success_is_not_a_named_fixture_pass() {
        let missing="ui_test_process::tests::this_test_does_not_exist";
        let output=run(missing,CHILD,"empty",Duration::from_secs(5),(4096,4096)).unwrap();
        assert!(output.success);assert!(!exact_pass(&output,missing));
    }
}
