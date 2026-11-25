use super::{
    LineageBirth, ScanError, SurvivingProcess, SurvivorEvidence, SurvivorLink, TerminalSession,
    UnreadableProcess, process_error, settle,
};
use crate::process::{inspect, table};
use std::{collections::BTreeMap, io};

pub(super) fn scan(
    session: &TerminalSession,
    owner: u32,
    leave: &[UnreadableProcess],
) -> Result<Option<SurvivingProcess>, ScanError> {
    let uid = crate::daemon::current_uid();
    let pids = table::list(uid).map_err(|error| ScanError::Incomplete(process_error(error)))?;
    scan_pids(session, owner, leave, uid, pids, || {
        crate::lineage_birth::instance_birth(session)
    })
}

/// Compare each record's Mach start ticks on both sides of KERN_PROCARGS2.
/// A reused or changing PID cannot turn an unreadable scan into proof of
/// absence. The caller keeps its existing admission fence around the scan.
fn scan_pids(
    session: &TerminalSession,
    owner: u32,
    leave: &[UnreadableProcess],
    uid: u32,
    pids: Vec<u32>,
    lineage: impl FnOnce() -> Option<LineageBirth>,
) -> Result<Option<SurvivingProcess>, ScanError> {
    let boot = crate::daemon::read_boot_id()
        .map_err(|error| ScanError::Incomplete(process_error(error)))?;
    let evidence = SurvivorEvidence::new(session);
    let mut incomplete = None;
    let mut records = BTreeMap::new();
    for pid in pids {
        if pid == std::process::id() {
            continue;
        }
        match live_record(pid, uid) {
            Ok(Some(record)) => {
                records.insert(pid, record);
            }
            Ok(None) => {}
            Err(error) => {
                incomplete.get_or_insert(error);
            }
        }
    }

    let mut unreadable = Vec::new();
    for (&pid, before) in &records {
        if owned_by_current_daemon(before, owner, uid, &records) {
            continue;
        }
        let (after, invocation) = match settle(|| observe(pid, uid, before, &evidence)) {
            Ok(Some((after, invocation))) => (after, Some(invocation)),
            Ok(None) => continue,
            Err(ObservationError::Incomplete(error)) => {
                incomplete.get_or_insert(error);
                continue;
            }
            Err(ObservationError::Invocation(_, error)) if evidence.exact_identity.is_some() => {
                // A process older than the lineage can independently open
                // the conversation. Birth and Leave cannot rule that out.
                incomplete.get_or_insert(error);
                continue;
            }
            Err(ObservationError::Invocation(after, _)) => (after, None),
        };
        let name = process_name(&after);
        if let Some(invocation) = invocation {
            if invocation.conversation {
                return Ok(Some(SurvivingProcess {
                    pid,
                    name,
                    link: SurvivorLink::Conversation,
                    reason: format!(
                        "agent session still has a live process ({pid}); automatic recovery would duplicate its writer"
                    ),
                }));
            }
            if invocation.marked {
                return Ok(Some(SurvivingProcess {
                    pid,
                    name,
                    link: SurvivorLink::Lineage,
                    reason: format!(
                        "terminal {} still has a live process ({pid}); preserving it",
                        session.id
                    ),
                }));
            }
            if invocation.environment_visible {
                continue;
            }
            // XNU can return an invocation with the environment hidden
            // by SIP. An empty environment does not prove no marker.
        }
        let process = UnreadableProcess {
            pid,
            start: after.start,
            boot: Some(boot.clone()),
            name,
            cgroup: None,
        };
        if !leave.iter().any(|left| left.same_process(&process)) {
            unreadable.push(process);
        }
    }

    if let Some(error) = incomplete {
        return Err(ScanError::Incomplete(error));
    }
    // Read the boot's lineage facts after the observations so all earlier
    // launches are represented. There is no Darwin cgroup exclusion.
    if !unreadable.is_empty()
        && let Some(birth) = lineage()
    {
        unreadable.retain(|process| !process.outside_lineage(&birth, None));
    }
    if unreadable.is_empty() {
        Ok(None)
    } else {
        Err(ScanError::Unreadable(unreadable))
    }
}

struct Invocation {
    conversation: bool,
    marked: bool,
    environment_visible: bool,
}

fn read_invocation(record: &[u8], evidence: &SurvivorEvidence) -> Option<Invocation> {
    // The ordinary parser ends the environment at the first empty entry.
    // Search the returned bytes for exact markers even when argv is empty.
    let marked = evidence.carries_marker(record.get(4..)?.split(|byte| *byte == 0));
    let Some(arguments) = inspect::parse_arguments(record).filter(|arguments| {
        arguments
            .arguments
            .first()
            .is_some_and(|arg| !arg.is_empty())
    }) else {
        return marked.then_some(Invocation {
            conversation: false,
            marked,
            environment_visible: false,
        });
    };
    Some(Invocation {
        conversation: evidence.names_conversation(arguments.arguments.iter().map(Vec::as_slice)),
        marked,
        environment_visible: !arguments.environment.is_empty(),
    })
}

enum ObservationError {
    Incomplete(String),
    Invocation(table::Record, String),
}

fn observe(
    pid: u32,
    uid: u32,
    captured: &table::Record,
    evidence: &SurvivorEvidence,
) -> Result<Option<(table::Record, Invocation)>, ObservationError> {
    let identity_changed = || {
        ObservationError::Incomplete(format!(
            "cannot confirm process identity during recovery ({pid})"
        ))
    };
    let Some(before) = live_record(pid, uid).map_err(ObservationError::Incomplete)? else {
        return Ok(None);
    };
    if !same_lifetime(captured, &before) {
        return Err(identity_changed());
    }
    let invocation = inspect::argument_record(pid)
        .map_err(process_error)
        .and_then(|record| {
            read_invocation(&record, evidence)
                .ok_or_else(|| format!("cannot inspect process invocation ({pid})"))
        });
    let Some(after) = live_record(pid, uid).map_err(ObservationError::Incomplete)? else {
        return Ok(None);
    };
    if !same_lifetime(&before, &after)
        || before.session != after.session
        || before.bsd.pbsi_comm != after.bsd.pbsi_comm
    {
        return Err(identity_changed());
    }
    match invocation {
        Ok(invocation) => Ok(Some((after, invocation))),
        Err(error) => Err(ObservationError::Invocation(after, error)),
    }
}

fn live_record(pid: u32, uid: u32) -> Result<Option<table::Record>, String> {
    match table::record(pid as libc::pid_t) {
        Ok(record) if record.bsd.pbsi_uid == uid && record.bsd.pbsi_status != libc::SZOMB => {
            Ok(Some(record))
        }
        Ok(_) => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(process_error(error)),
    }
}

fn same_lifetime(before: &table::Record, after: &table::Record) -> bool {
    before.start.is_some() && before.start == after.start
}

/// Own work already belongs to the authenticated current daemon. Check every
/// edge against the original lifetime and its current parent before skipping
/// it. A detached orphan is inspected on its own, even if a former ancestor
/// belonged to this daemon.
fn owned_by_current_daemon<'a>(
    mut member: &'a table::Record,
    owner: u32,
    uid: u32,
    records: &'a BTreeMap<u32, table::Record>,
) -> bool {
    if owner != std::process::id() && !records.contains_key(&owner) {
        return false;
    }
    let leaf = member;
    let still_current = |before: &table::Record| {
        live_record(before.bsd.pbsi_pid, uid).is_ok_and(|after| {
            after.is_some_and(|after| {
                same_lifetime(before, &after) && before.bsd.pbsi_ppid == after.bsd.pbsi_ppid
            })
        })
    };
    let mut chain = Vec::new();
    for _ in 0..records.len() {
        chain.push(member);
        if member.bsd.pbsi_pid == owner
            || (owner == std::process::id() && member.bsd.pbsi_ppid == owner)
        {
            return chain.into_iter().rev().all(still_current) && still_current(leaf);
        }
        let Some(parent) = records.get(&member.bsd.pbsi_ppid) else {
            return false;
        };
        if !matches!((parent.start, member.start), (Some(parent), Some(child)) if parent <= child) {
            return false;
        }
        member = parent;
    }
    false
}

fn process_name(record: &table::Record) -> Option<String> {
    let bytes = record
        .bsd
        .pbsi_comm
        .iter()
        .take_while(|&&byte| byte != 0)
        .map(|&byte| byte as u8)
        .collect::<Vec<_>>();
    (!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BootId, LaunchRecipe, TerminalContinuity, TerminalId};
    use std::{path::Path, process::Stdio};

    fn source() -> TerminalSession {
        let launch = LaunchRecipe::shell(std::env::current_dir().unwrap()).unwrap();
        TerminalSession {
            id: TerminalId::new(),
            recovery_origin: None,
            predecessor: None,
            owned_codex_listener: None,
            launch: launch.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe: launch,
                last_exit: None,
            },
        }
    }

    struct Child(std::process::Child);

    impl Child {
        fn start(command: &mut std::process::Command) -> Self {
            Self(
                command
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .spawn()
                    .unwrap(),
            )
        }

        fn pid(&self) -> u32 {
            self.0.id()
        }
    }

    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn signed_cat(directory: &Path, restricted: bool) -> std::path::PathBuf {
        let cat = directory.join("recovery-cat");
        std::fs::copy("/bin/cat", &cat).unwrap();
        let mut sign = std::process::Command::new("/usr/bin/codesign");
        sign.args(["--force", "--sign", "-"]);
        if restricted {
            sign.args(["--options", "restrict"]);
        }
        let signed = sign.arg(&cat).output().unwrap();
        assert!(
            signed.status.success(),
            "codesign: {}",
            String::from_utf8_lossy(&signed.stderr)
        );
        cat
    }

    fn scan_child(
        source: &TerminalSession,
        child: &Child,
        leave: &[UnreadableProcess],
        birth: Option<LineageBirth>,
    ) -> Result<Option<SurvivingProcess>, ScanError> {
        scan_pids(
            source,
            u32::MAX,
            leave,
            crate::daemon::current_uid(),
            vec![child.pid()],
            || birth,
        )
    }

    #[test]
    fn an_empty_environment_entry_does_not_hide_a_later_lineage_marker() {
        let evidence = SurvivorEvidence::new(&source());
        let mut record = 1_i32.to_ne_bytes().to_vec();
        record.extend_from_slice(b"/bin/cat\0\0cat\0EXAMPLE=1\0\0");
        record.extend_from_slice(evidence.markers[0].as_bytes());
        record.push(0);
        let invocation = read_invocation(&record, &evidence).unwrap();
        assert!(invocation.environment_visible);
        assert!(invocation.marked);

        let mut record = 0_i32.to_ne_bytes().to_vec();
        record.extend_from_slice(b"/bin/cat\0\0");
        record.extend_from_slice(evidence.markers[0].as_bytes());
        record.push(0);
        assert!(read_invocation(&record, &evidence).unwrap().marked);
    }

    #[test]
    fn exact_writer_still_blocks_when_argv_is_visible_and_environment_is_hidden() {
        let mut source = source();
        let conversation = uuid::Uuid::new_v4().to_string();
        source.launch.program = "codex".into();
        source.launch.arguments = vec!["resume".into(), conversation.clone()];
        assert!(SurvivorEvidence::new(&source).exact_identity.is_some());
        // XNU reports the Apple platform shell's invocation even when it
        // hides the initial environment. Stdin keeps this lifetime live.
        let child = Child::start(std::process::Command::new("/bin/sh").args([
            "-c",
            "IFS= read -r barrier",
            "fixture",
            &conversation,
        ]));
        let survivor = super::scan(&source, u32::MAX, &[]).unwrap().unwrap();
        assert_eq!(survivor.pid, child.pid());
        assert_eq!(survivor.link, SurvivorLink::Conversation);
        assert_eq!(
            scan_child(&source, &child, &[], Some(LineageBirth::Unborn))
                .unwrap()
                .unwrap()
                .link,
            SurvivorLink::Conversation
        );
    }

    #[test]
    fn a_marked_process_in_a_detached_session_blocks_recovery() {
        use std::os::unix::process::CommandExt;

        let directory = tempfile::tempdir().unwrap();
        let mut source = source();
        let origin = TerminalId::new();
        source.recovery_origin = Some(origin);
        let mut command = std::process::Command::new(signed_cat(directory.path(), false));
        command.env("CAGENTS_RECOVERY_ORIGIN", origin.to_string());
        // SAFETY: setsid takes no pointers and is async-signal-safe in this
        // child. It gives the fixture an independent POSIX session.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = Child::start(&mut command);
        assert_eq!(
            live_record(child.pid(), crate::daemon::current_uid())
                .unwrap()
                .unwrap()
                .session,
            Some(child.pid() as libc::pid_t)
        );
        let survivor = super::scan(&source, u32::MAX, &[]).unwrap().unwrap();
        assert_eq!(survivor.pid, child.pid());
        assert_eq!(survivor.link, SurvivorLink::Lineage);
    }

    #[test]
    fn hidden_markers_remain_unknown_and_only_the_current_lifetime_can_be_left() {
        let directory = tempfile::tempdir().unwrap();
        let source = source();
        let mut child = Child::start(
            std::process::Command::new(signed_cat(directory.path(), true))
                .env("CAGENTS_TERMINAL_ID", source.id.to_string()),
        );
        let record = inspect::argument_record(child.pid()).unwrap();
        let invocation = read_invocation(&record, &SurvivorEvidence::new(&source)).unwrap();
        // Kernels with less restrictive visibility must still block on the
        // positive marker. On a SIP-protected Mac exercise the hidden case.
        if invocation.marked {
            assert_eq!(
                scan_child(&source, &child, &[], None)
                    .unwrap()
                    .unwrap()
                    .link,
                SurvivorLink::Lineage
            );
            return;
        }
        assert!(!invocation.environment_visible);
        let ScanError::Unreadable(processes) = scan_child(&source, &child, &[], None).unwrap_err()
        else {
            panic!("the process with hidden markers must require a decision");
        };
        let [process] = processes.as_slice() else {
            panic!("only the inspected lifetime should be returned");
        };
        assert_eq!(process.pid, child.pid());
        assert!(process.start.is_some());
        assert_eq!(
            scan_child(&source, &child, std::slice::from_ref(process), None),
            Ok(None)
        );
        assert!(child.0.try_wait().unwrap().is_none());

        let mut stale_start = process.clone();
        stale_start.start = stale_start.start.map(|start| start.wrapping_add(1));
        let mut stale_boot = process.clone();
        stale_boot.boot = Some(BootId::new("prior-boot"));
        for stale in [stale_start, stale_boot] {
            assert_eq!(
                scan_child(&source, &child, &[stale], None),
                Err(ScanError::Unreadable(processes.clone()))
            );
        }
        assert_eq!(
            scan_child(&source, &child, &[], Some(LineageBirth::Unborn)),
            Ok(None)
        );
    }
}
