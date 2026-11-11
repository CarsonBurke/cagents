//! Read-only qualification for replacing a console view around its live actor.
//! No helper is attached and no old Registry is opened for schema admission.
use super::*;
use crate::{
    agent::Provider,
    agent_execution::{Agent, HostedSession, Surface},
    daemon::holder_registry::{self, Kind, Record},
    host::model::NativeActorOwnership,
    model::AgentId,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, config::DbConfig};
use std::os::unix::fs::MetadataExt;

const EVIDENCE_LIMIT: usize = 256 * 1024;
const UNQUALIFIED: &str = "The accepted console's retained ownership could not be confirmed";

pub(super) fn qualify(
    source: &Session,
    matched_cli: &Path,
    proof: &HostSessionProof,
) -> Result<(), &'static str> {
    let terminal = source.terminal.as_ref().ok_or(UNQUALIFIED)?;
    let agent = crate::agent_execution::console_agent(&terminal.launch).ok_or(UNQUALIFIED)?;
    let store = crate::instance::project_dirs()
        .ok_or(UNQUALIFIED)?
        .data_dir()
        .join("workspace-model.sqlite3");
    let (binding, ownership) = read_binding(&store, agent, source).map_err(|_| UNQUALIFIED)?;
    let record = |kind| {
        holder_registry::directory(kind)
            .and_then(|directory| {
                holder_registry::recovery_candidate(
                    &holder_registry::socket(&directory, source.id),
                    kind,
                )
            })
            .ok()
            .flatten()
            .ok_or(UNQUALIFIED)
    };
    qualify_binding(
        source,
        matched_cli,
        proof,
        &binding,
        &ownership,
        &record(Kind::Native)?,
        &record(Kind::Terminal)?,
    )
}

fn read_binding(
    path: &Path,
    agent: AgentId,
    source: &Session,
) -> Result<(Agent, NativeActorOwnership), String> {
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|error| error.to_string())?;
    db.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)
        .map_err(|error| error.to_string())?;
    db.busy_timeout(Duration::from_secs(2))
        .map_err(|error| error.to_string())?;
    let transaction = db
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    let record: String = transaction
        .query_row(
            "SELECT substr(record,1,?2) FROM agents WHERE id=?1 AND closed=0",
            rusqlite::params![agent.to_string(), EVIDENCE_LIMIT + 1],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?
        .ok_or(UNQUALIFIED)?;
    if record.len() > EVIDENCE_LIMIT {
        return Err(UNQUALIFIED.into());
    }
    let binding = serde_json::from_str(&record).map_err(|error| error.to_string())?;
    let owners: usize = transaction
        .query_row(
            "SELECT count(*) FROM (SELECT 1 FROM native_actor_ownerships LIMIT ?1)",
            [MAX_CENSUS_ROWS + 1],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if owners > MAX_CENSUS_ROWS {
        return Err(UNQUALIFIED.into());
    }
    let mut statement = transaction
        .prepare(
            "SELECT actor,substr(ownership,1,?2) FROM native_actor_ownerships o \
             WHERE CASE WHEN length(ownership)<=?2 THEN json_extract(ownership,'$.source.id') END=?1 \
             AND NOT EXISTS(SELECT 1 FROM native_actor_retirements r WHERE r.actor=o.actor) \
             LIMIT 2",
        )
        .map_err(|error| error.to_string())?;
    let records = statement
        .query_map(
            rusqlite::params![source.id.to_string(), EVIDENCE_LIMIT + 1],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    let [(actor, record)] = records.as_slice() else {
        return Err(UNQUALIFIED.into());
    };
    if record.len() > EVIDENCE_LIMIT {
        return Err(UNQUALIFIED.into());
    }
    let ownership: NativeActorOwnership =
        serde_json::from_str(record).map_err(|error| error.to_string())?;
    if actor != &ownership.actor().to_string() {
        return Err(UNQUALIFIED.into());
    }
    Ok((binding, ownership))
}

fn qualify_binding(
    source: &Session,
    matched_cli: &Path,
    proof: &HostSessionProof,
    agent: &Agent,
    ownership: &NativeActorOwnership,
    native: &Record,
    console: &Record,
) -> Result<(), &'static str> {
    let terminal = source.terminal.as_ref().ok_or(UNQUALIFIED)?;
    let id = crate::agent_execution::console_agent(&terminal.launch).ok_or(UNQUALIFIED)?;
    if !proof.is_complete()
        || source.proof.as_ref() != Some(proof)
        || source.launch.provider != Provider::Claude
        || !matches!(
            source.native_continuity,
            crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 }
        )
        || !source.has_valid_native_continuity()
        || source.id != terminal.id
        || terminal.continuity.live_proof().as_ref() != Some(proof)
        || terminal.owned_codex_listener.is_some()
        || agent.id != id
        || agent.closed
        || agent.spec.surface != Surface::Terminal
        || agent.spec.provider != Provider::Claude
        || agent.console.as_ref().is_none_or(|saved| {
            saved.id != terminal.id
                || saved.launch != terminal.launch
                || saved.continuity.live_proof().as_ref() != Some(proof)
        })
        || !agent
            .session
            .as_ref()
            .is_some_and(|bound| bound.same_process(&HostedSession::Chat(source.clone())))
        || ownership.is_pending()
        || ownership.actor().is_nil()
        || !crate::agent_execution::same_chat_process(ownership.source(), source)
        || ownership.owner() != native
        || !matches!(native.version, 2 | 3 | 4)
        || native.kind != Kind::Native
        || native.terminal != source.id
        || native.boot != proof.boot_id
        || !holder_registry::record_application_matches(native)
        || !native.build.as_ref().is_some_and(|build| {
            build
                .capabilities
                .iter()
                .any(|capability| capability == holder_registry::RETAINED_CLAUDE_CAPABILITY)
        })
        || !matches!(console.version, 3 | 4 | 5)
        || console.kind != Kind::Terminal
        || console.terminal != source.id
        || console.boot != proof.boot_id
        || !(console.application == crate::instance::APPLICATION_ID
            || holder_registry::production_role_pair(
                &console.application,
                crate::instance::APPLICATION_ID,
            ))
        || [native, console]
            .iter()
            .any(|record| !record.holder.is_current() || !record.root.is_current())
    {
        return Err(UNQUALIFIED);
    }
    let scope = ownership.scope().map_err(|_| UNQUALIFIED)?;
    let recipe = &source.launch.recipe;
    let selected = |name: &str| {
        (!recipe
            .unset_environment
            .iter()
            .any(|removed| removed == name))
        .then(|| recipe.environment.get(name))
        .flatten()
        .filter(|value| !value.is_empty())
    };
    let home = selected("CLAUDE_CONFIG_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| selected("HOME").map(|home| Path::new(home).join(".claude")))
        .ok_or(UNQUALIFIED)?;
    if scope.provider != Provider::Claude
        || scope.revalidate().is_err()
        || source
            .launch
            .recipe
            .working_directory
            .join(home)
            .canonicalize()
            .ok()
            .as_ref()
            != Some(&scope.canonical_home)
    {
        return Err(UNQUALIFIED);
    }
    let issued = Path::new(&terminal.launch.program);
    let executable = |path: &Path| {
        std::fs::metadata(path)
            .ok()
            .filter(|metadata| metadata.is_file() && metadata.mode() & 0o111 != 0)
    };
    let Some(selected) = executable(issued) else {
        return Err(UNQUALIFIED);
    };
    let Some(running) = executable(
        &Path::new("/proc")
            .join(console.root.pid.to_string())
            .join("exe"),
    ) else {
        return Err(UNQUALIFIED);
    };
    if !issued.is_absolute()
        || issued.file_name() != Some(crate::companion::Product::Cli.file_name().as_os_str())
        || !matched_cli.is_absolute()
        || executable(matched_cli).is_none()
        || selected.dev() != running.dev()
        || selected.ino() != running.ino()
        || !console.root.is_current()
    {
        return Err(UNQUALIFIED);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        account::scope::LaunchAccountScope,
        agent_execution::{Origin, Spec, Target},
        daemon::holder_registry::{BuildIdentity, Process},
        model::{ChannelId, LaunchRecipe, TerminalContinuity},
    };

    struct Fixture {
        root: tempfile::TempDir,
        source: Session,
        agent: Agent,
        ownership: NativeActorOwnership,
        native: Record,
        console: Record,
        matched: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let home = root.path().join("private-account");
            std::fs::create_dir(&home).unwrap();
            let metadata = std::fs::metadata(&home).unwrap();
            let scope = LaunchAccountScope {
                provider: Provider::Claude,
                canonical_home: home.clone(),
                device: metadata.dev(),
                inode: metadata.ino(),
                registered_id: None,
                purge_ids: Vec::new(),
            };
            let matched = root.path().join("cagents");
            // Inspect this test's executable without launching a provider or
            // opening an account, daemon, or credential file.
            std::os::unix::fs::symlink(std::env::current_exe().unwrap(), &matched).unwrap();
            let mut source = super::super::tests::session();
            source.native_continuity =
                crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
            source.proof.as_mut().unwrap().boot_id = crate::daemon::read_boot_id().unwrap();
            source.launch.recipe.working_directory = root.path().to_owned();
            source
                .launch
                .recipe
                .environment
                .insert("CLAUDE_CONFIG_DIR".into(), home.display().to_string());
            let id = AgentId::new();
            let proof = source.proof.as_ref().unwrap();
            let recipe = LaunchRecipe {
                program: matched.display().to_string(),
                arguments: vec!["agent".into(), "console".into(), id.to_string()],
                working_directory: root.path().to_owned(),
                environment: Default::default(),
                unset_environment: Default::default(),
            };
            let mut terminal =
                crate::terminal_ipc::TerminalDescriptor::relaunchable(source.id, recipe, None)
                    .session;
            terminal.continuity = TerminalContinuity::Live {
                host_session_id: proof.host_session_id.clone(),
                boot_id: proof.boot_id.clone(),
                host_incarnation: proof.host_incarnation.clone(),
            };
            let issued = source.clone();
            source.terminal = Some(terminal.clone());
            let agent = Agent {
                id,
                name: Some("fixture-console".into()),
                origin: Origin::User,
                spec: Spec {
                    provider: Provider::Claude,
                    account: None,
                    target: Target {
                        repository: root.path().to_owned(),
                        directory: root.path().to_owned(),
                        placement: None,
                    },
                    surface: Surface::Terminal,
                    model: None,
                    permission_mode: None,
                    reasoning_effort: None,
                },
                placement: crate::agent_execution::Placement::Channel(ChannelId::new()),
                session: Some(HostedSession::Chat(issued.clone())),
                console: Some(terminal),
                created_ms: 1,
                closed: false,
            };
            let process = Process::read(std::process::id() as i32).unwrap();
            let native = Record {
                version: 4,
                build: Some(BuildIdentity {
                    version: "fixture".into(),
                    commit: None,
                    capabilities: vec![holder_registry::RETAINED_CLAUDE_CAPABILITY.into()],
                }),
                kind: Kind::Native,
                application: crate::instance::APPLICATION_ID.into(),
                boot: source.proof.as_ref().unwrap().boot_id.clone(),
                terminal: source.id,
                holder: process.clone(),
                root: process,
            };
            let console = Record {
                kind: Kind::Terminal,
                build: None,
                ..native.clone()
            };
            let ownership = serde_json::from_value(serde_json::json!({
                "actor": uuid::Uuid::new_v4(), "owner": native, "source": issued, "scope": {"Ok": scope}
            })).unwrap();
            Self {
                root,
                source,
                agent,
                ownership,
                native,
                console,
                matched,
            }
        }

        fn qualify(&self) -> Result<(), &'static str> {
            qualify_binding(
                &self.source,
                &self.matched,
                self.source.proof.as_ref().unwrap(),
                &self.agent,
                &self.ownership,
                &self.native,
                &self.console,
            )
        }
    }

    #[test]
    fn retained_console_pair_qualifies_without_requiring_the_old_cli_path() {
        let fixture = Fixture::new();
        fixture.qualify().unwrap();
        let new_cli = fixture.root.path().join("new-cagents");
        std::os::unix::fs::symlink(std::env::current_exe().unwrap(), &new_cli).unwrap();
        qualify_binding(
            &fixture.source,
            &new_cli,
            fixture.source.proof.as_ref().unwrap(),
            &fixture.agent,
            &fixture.ownership,
            &fixture.native,
            &fixture.console,
        )
        .unwrap();
    }

    #[test]
    fn malformed_or_unowned_consoles_require_stop_review() {
        for change in [
            "provider",
            "protocol",
            "pair",
            "agent",
            "surface",
            "closed",
            "launch",
            "owner",
            "process",
            "executable",
            "destination",
            "account",
        ] {
            let mut fixture = Fixture::new();
            match change {
                "provider" => fixture.source.launch.provider = Provider::Codex,
                "protocol" => {
                    fixture.source.native_continuity =
                        crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 1 }
                }
                "pair" => fixture.source.terminal = None,
                "agent" => fixture.agent.id = AgentId::new(),
                "surface" => fixture.agent.spec.surface = Surface::Chat,
                "closed" => fixture.agent.closed = true,
                "launch" => fixture
                    .agent
                    .console
                    .as_mut()
                    .unwrap()
                    .launch
                    .arguments
                    .push("private-argument".into()),
                "owner" => fixture.native.root.start += 1,
                "process" => fixture.console.root.start += 1,
                "executable" => {
                    fixture.source.terminal.as_mut().unwrap().launch.program =
                        "/private/account/not-cagents".into()
                }
                "destination" => fixture.matched = fixture.root.path().join("missing"),
                "account" => {
                    let home = fixture.root.path().join("private-account");
                    std::fs::rename(&home, fixture.root.path().join("previous-account")).unwrap();
                    std::fs::create_dir(home).unwrap();
                }
                _ => unreachable!(),
            }
            assert_eq!(fixture.qualify(), Err(UNQUALIFIED), "{change}");
            assert!(!UNQUALIFIED.contains("private"));
        }
    }

    #[test]
    fn old_registry_console_evidence_is_read_without_migration_or_writes() {
        let fixture = Fixture::new();
        let path = fixture.root.path().join("workspace-model.sqlite3");
        {
            let db = Connection::open(&path).unwrap();
            db.execute_batch("PRAGMA user_version=22; CREATE TABLE agents(id TEXT PRIMARY KEY,closed INTEGER,record TEXT); CREATE TABLE native_actor_ownerships(actor TEXT PRIMARY KEY,ownership TEXT); CREATE TABLE native_actor_retirements(actor TEXT PRIMARY KEY);").unwrap();
            db.execute(
                "INSERT INTO agents VALUES(?1,0,?2)",
                rusqlite::params![
                    fixture.agent.id.to_string(),
                    serde_json::to_string(&fixture.agent).unwrap()
                ],
            )
            .unwrap();
            db.execute(
                "INSERT INTO native_actor_ownerships VALUES(?1,?2)",
                rusqlite::params![
                    fixture.ownership.actor().to_string(),
                    serde_json::to_string(&fixture.ownership).unwrap()
                ],
            )
            .unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        let (agent, ownership) = read_binding(&path, fixture.agent.id, &fixture.source).unwrap();
        assert_eq!(agent, fixture.agent);
        assert_eq!(ownership, fixture.ownership);
        assert_eq!(before, std::fs::read(&path).unwrap());
        let db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert_eq!(
            db.pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            22
        );
        db.close().unwrap();
    }
}
