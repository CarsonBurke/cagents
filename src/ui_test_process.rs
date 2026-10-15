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

fn run(test:&str, child_key:&str, value:&str, deadline:Duration, limits:(usize,usize))->Result<ProcessOutput,ProcessFailure> {
    let spec=CommandSpec::new(std::env::current_exe().expect("current test executable"))
        .args([test,"--exact","--nocapture","--test-threads=1"])
        .env(child_key,value).timeout(deadline).output_limits(limits.0,limits.1);
    SystemProcessExecutor.execute(&spec,&CancellationToken::new())
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
    if std::env::var_os(child_key).as_deref()==Some(std::ffi::OsStr::new("1")) {return false;}
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
        match std::env::var(CHILD).ok().as_deref() {
            Some("hang")=>{
                println!("bounded UI fixture entered");std::io::stdout().flush().unwrap();
                loop {std::thread::park();}
            }
            Some("cap")=>{
                let bytes=[b'x';4096];let mut stdout=std::io::stdout().lock();
                loop {stdout.write_all(&bytes).unwrap();stdout.flush().unwrap();}
            }
            _=>{}
        }
    }
    #[test]
    fn current_executable_hang_obeys_deadline_and_does_not_claim_family_proof() {
        let start=std::time::Instant::now();
        let error=run(FIXTURE,CHILD,"hang",Duration::from_secs(2),(4096,4096)).unwrap_err();
        assert_eq!(error.kind,ProcessFailureKind::TimedOut);
        assert!(String::from_utf8_lossy(&error.stdout).contains("bounded UI fixture entered"));
        assert!(error.stdout.len()<=4096 && error.stderr.len()<=4096);
        assert!(start.elapsed()<Duration::from_secs(10));
        // Strict outer B0 separately proves every actual descendant settled.
    }
    #[test]
    fn current_executable_output_is_capped_before_collection() {
        let error=run(FIXTURE,CHILD,"cap",Duration::from_secs(5),(16384,4096)).unwrap_err();
        assert_eq!(error.kind,ProcessFailureKind::OutputLimit);
        assert!(error.stdout.len()<=16384 && error.stderr.len()<=4096);
    }
    #[test]
    fn an_empty_filter_success_is_not_a_named_fixture_pass() {
        let missing="ui_test_process::tests::this_test_does_not_exist";
        let output=run(missing,CHILD,"empty",Duration::from_secs(5),(4096,4096)).unwrap();
        assert!(output.success);assert!(!exact_pass(&output,missing));
    }
}
