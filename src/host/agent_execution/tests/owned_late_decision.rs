//! An exact private test controller owns worker descendants; the ordinary
//! libtest process never changes its process-wide subreaper policy.
use std::{
    fs, io::{self, Read},
    os::unix::{fs::{OpenOptionsExt, PermissionsExt}, process::CommandExt},
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

const ROLE: &str = "CAGENTS_LATE_DECISION_FIXTURE_ROLE";
const ROOT: &str = "CAGENTS_EXECUTION_FIXTURE";
const LOG_LIMIT: u64 = 262144;

fn log_file(root:&Path,name:&str)->fs::File {
    fs::OpenOptions::new().create_new(true).write(true).mode(0o600).open(root.join(name)).unwrap()
}
fn read_log(root:&Path,name:&str)->String {
    let mut bytes=Vec::new();
    fs::File::open(root.join(name)).unwrap().take(LOG_LIMIT+1).read_to_end(&mut bytes).unwrap();
    assert!(bytes.len() as u64<=LOG_LIMIT,"private fixture log exceeded its cap");
    String::from_utf8_lossy(&bytes).into_owned()
}
fn receipt(root:&Path,name:&str,value:&str) {
    use std::io::Write;
    let mut file=fs::OpenOptions::new().create_new(true).write(true).mode(0o600).open(root.join(name)).unwrap();
    file.write_all(value.as_bytes()).unwrap();
}
fn command(name:&str,root:&Path,role:&str)->Command {
    let mut command=Command::new(std::env::current_exe().unwrap());
    command.args(["--exact",&format!("host::agent_execution::tests::{name}"),"--nocapture","--test-threads=1"])
        .env_clear().env(ROLE,role).env(ROOT,root)
        .env("CAGENTS_EXECUTION_SERVICE_TEST",name)
        .env("HOME",root.join("home"))
        .env("XDG_DATA_HOME",root.join("data"))
        .env("XDG_CONFIG_HOME",root.join("config"))
        .env("XDG_CACHE_HOME",root.join("cache"))
        .env("TMPDIR",root.join("tmp"))
        .env("XDG_RUNTIME_DIR",root.join("run"))
        .env("PATH",format!("{}:/usr/bin:/bin",root.join("bin").display()))
        .stdin(Stdio::null());
    // SAFETY: this child-only pre-exec callback performs only setrlimit. It
    // bounds every private regular log, with overflow failing the child.
    unsafe {command.pre_exec(|| {
        let limit=libc::rlimit{rlim_cur:LOG_LIMIT,rlim_max:LOG_LIMIT};
        if libc::setrlimit(libc::RLIMIT_FSIZE,&limit)==0 {Ok(())}else{Err(io::Error::last_os_error())}
    });}
    command
}

/// Only this owner waits for the original controller Child; an unreaped child
/// retains its PID, and no global waitpid consumer exists in the parent.
struct ControllerChild {
    child:Child,
    reaped:bool,
    cleanup_deadline:Option<Instant>,
}
impl ControllerChild {
    fn wait_until(&mut self,deadline:Instant)->io::Result<Option<ExitStatus>> {
        loop {
            if let Some(status)=self.child.try_wait()? {self.reaped=true;return Ok(Some(status));}
            if Instant::now()>=deadline {return Ok(None);}
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn finish(&mut self)->io::Result<()> {
        if self.reaped{return Ok(());}
        let deadline=*self.cleanup_deadline.get_or_insert_with(||Instant::now()+Duration::from_secs(3));
        if let Some(_)=self.child.try_wait()? {self.reaped=true;return Ok(());}
        // This is our still-unreaped original Child, not a discovered PID or
        // borrowed process group. A timeout is failure, never family proof.
        self.child.kill()?;
        self.wait_until(deadline)?.ok_or_else(||io::Error::other("original controller did not settle"))?;
        Ok(())
    }
}
impl Drop for ControllerChild {
    fn drop(&mut self) {
        if let Err(error)=self.finish() {
            eprintln!("late-decision controller cleanup FAILED: {error}");
            if !thread::panicking(){panic!("{error}");}
        }
    }
}

struct Family {
    settled: bool,
    cleanup_deadline: Option<Instant>,
}
impl Family {
    fn identity(pid: i32) -> std::io::Result<(u32, String)> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let fields: Vec<_> = stat
            .rsplit_once(')')
            .ok_or_else(|| std::io::Error::other("invalid child stat"))?
            .1
            .split_whitespace()
            .collect();
        Ok((
            fields
                .get(1)
                .ok_or_else(|| std::io::Error::other("missing child parent"))?
                .parse()
                .map_err(std::io::Error::other)?,
            fields
                .get(19)
                .ok_or_else(|| std::io::Error::other("missing child start"))?
                .to_string(),
        ))
    }
    fn kill_direct(pid: i32) -> std::io::Result<()> {
        use std::os::fd::{AsRawFd, FromRawFd};
        let before = match Self::identity(pid) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if before.0 != std::process::id() {
            return Err(std::io::Error::other("fixture child parent changed"));
        }
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if raw < 0 {
            let error = std::io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::ESRCH) {
                Ok(())
            } else {
                Err(error)
            };
        }
        // SAFETY: pidfd_open returned one newly owned descriptor.
        let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw as i32) };
        match Self::identity(pid) {
            Ok(after) if after == before => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Ok(_) => {
                return Err(std::io::Error::other(
                    "fixture child identity changed after pinning",
                ));
            }
            Err(error) => return Err(error),
        }
        // SAFETY: the pidfd pins the start/parent-checked direct child.
        let sent = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if sent < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        Ok(())
    }
    fn finish(&mut self) -> Result<(), String> {
        if self.settled {
            return Ok(());
        }
        let deadline = *self
            .cleanup_deadline
            .get_or_insert_with(|| Instant::now() + Duration::from_secs(5));
        loop {
            loop {
                if Instant::now() >= deadline {
                    return Err("fixture family did not settle before its deadline".into());
                }
                let mut status = 0;
                let reaped = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
                if reaped > 0 {
                    eprintln!(
                        "late-decision fixture reaped child {reaped}: wait status {status}"
                    );
                    continue;
                }
                if reaped == 0 {
                    break;
                }
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ECHILD) {
                    self.settled = true;
                    eprintln!("late-decision fixture family settled: kernel ECHILD");
                    return Ok(());
                }
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(format!("fixture waitpid failed: {error}"));
                }
            }
            if Instant::now() >= deadline {
                return Err("fixture family did not settle before its deadline".into());
            }
            let mut children = std::collections::BTreeSet::new();
            for task in fs::read_dir("/proc/self/task")
                .map_err(|error| format!("fixture task census failed: {error}"))?
            {
                let task = task.map_err(|error| error.to_string())?;
                let members = match fs::read_to_string(task.path().join("children")) {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        return Err(format!("fixture child census failed: {error}"));
                    }
                };
                for member in members.split_whitespace() {
                    children
                        .insert(member.parse::<i32>().map_err(|error| error.to_string())?);
                }
            }
            for child in children {
                Self::kill_direct(child).map_err(|error| {
                    format!("cannot retire exact fixture child {child}: {error}")
                })?;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Family {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            eprintln!("late-decision fixture cleanup FAILED: {error}");
            if !std::thread::panicking() {
                panic!("{error}");
            }
        }
    }
}

pub(super) fn run(name:&str,body:impl FnOnce()) {
    match std::env::var(ROLE).unwrap_or_default().as_str() {
        "worker"=>{
            body();
            let root=std::path::PathBuf::from(std::env::var_os(ROOT).expect("private worker root"));
            receipt(&root,"worker-completed",name);
        }
        "controller"=>{
            let root=std::path::PathBuf::from(std::env::var_os(ROOT).expect("private controller root"));
            // SAFETY: only this exact single-test subprocess becomes a subreaper.
            assert_eq!(unsafe{libc::prctl(libc::PR_SET_CHILD_SUBREAPER,1,0,0,0)},0);
            let mut family=Family{settled:false,cleanup_deadline:None};
            let mut worker=command(name,&root,"worker")
                .stdout(log_file(&root,"worker.stdout"))
                .stderr(log_file(&root,"worker.stderr"))
                .spawn().unwrap();
            let deadline=Instant::now()+Duration::from_secs(45);
            let outcome=loop {
                if let Some(status)=worker.try_wait().unwrap(){break Some(status);}
                if Instant::now()>=deadline {Family::kill_direct(worker.id() as i32).unwrap();break None;}
                thread::sleep(Duration::from_millis(10));
            };
            family.finish().expect("actual worker/provider family must reach ECHILD");
            let stdout=read_log(&root,"worker.stdout");let stderr=read_log(&root,"worker.stderr");
            assert!(outcome.is_some_and(|status|status.success()),"exact worker failed/timed out: {outcome:?}\n{stdout}\n{stderr}");
            assert_eq!(fs::read_to_string(root.join("worker-completed")).unwrap(),name,"the requested actual body must finish");
            assert_eq!(stdout.matches("running 1 test").count(),1,"{stdout}");
            assert_eq!(stdout.matches("test result: ok. 1 passed; 0 failed;").count(),1,"{stdout}");
            assert!(stdout.contains(&format!("test host::agent_execution::tests::{name} ...")),"exact named test must execute: {stdout}");
            receipt(&root,"worker-status",&format!("{outcome:?}"));
            receipt(&root,"controller-settled",name);
        }
        ""=>{
            let directory=tempfile::tempdir().unwrap();let root=directory.path();
            fs::set_permissions(root,fs::Permissions::from_mode(0o700)).unwrap();
            for path in ["bin","home/.claude","home/.codex","data","config","cache","tmp","run","project"] {
                fs::create_dir_all(root.join(path)).unwrap();
            }
            super::install_claude(&root.join("bin/claude"));
            let child=command(name,root,"controller")
                .stdout(log_file(root,"controller.stdout"))
                .stderr(log_file(root,"controller.stderr"))
                .spawn().unwrap();
            let mut controller=ControllerChild{child,reaped:false,cleanup_deadline:None};
            let status=controller.wait_until(Instant::now()+Duration::from_secs(60)).unwrap();
            if status.is_none() {
                controller.finish().expect("exact original controller must settle after timeout");
                panic!("private controller timed out; worker-family settlement is UNKNOWN, never passing evidence");
            }
            let stdout=read_log(root,"controller.stdout");let stderr=read_log(root,"controller.stderr");
            // Preserve complete capped child logs in the durable Cargo receipt,
            // while escaping nested libtest text so its results are not counted
            // as additional parent executions by the outer queue parser.
            let worker_stdout=root.join("worker.stdout").exists().then(||read_log(root,"worker.stdout"));
            let worker_stderr=root.join("worker.stderr").exists().then(||read_log(root,"worker.stderr"));
            eprintln!("late-decision fixture evidence: {}",serde_json::json!({
                "case":name,"controller_pid":controller.child.id(),"controller_status":format!("{status:?}"),
                "controller_stdout":stdout,"controller_stderr":stderr,
                "worker_stdout":worker_stdout,"worker_stderr":worker_stderr,
                "worker_status":fs::read_to_string(root.join("worker-status")).ok(),
                "settled_receipt":fs::read_to_string(root.join("controller-settled")).ok(),
            }));
            assert!(status.unwrap().success(),"private controller failed\n{stdout}\n{stderr}");
            assert_eq!(fs::read_to_string(root.join("controller-settled")).unwrap(),name,"exact ECHILD receipt required");
            assert_eq!(stdout.matches("running 1 test").count(),1,"{stdout}");
            assert_eq!(stdout.matches("test result: ok. 1 passed; 0 failed;").count(),1,"{stdout}");
            assert!(stdout.contains(&format!("test host::agent_execution::tests::{name} ...")),"{stdout}");
        }
        other=>panic!("unknown late-decision fixture role {other}"),
    }
}
