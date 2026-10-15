//! Transfer exact semantic-retirement proof before consuming helper cleanup.
//! This proves one actor retired, never the absence of other session writers.
use super::Registry;
use crate::{account::scope::LaunchAccountScope, chat, daemon::holder_registry::Record};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const MAX_RECEIPT_BYTES: usize = 256 * 1024;

/// A durable roster includes helpers which have never emitted a semantic event.
/// Unknown scope remains explicit and cannot be treated as another account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::host) struct NativeActorOwnership {
    actor: Uuid,
    owner: Record,
    source: chat::Session,
    scope: Result<LaunchAccountScope, String>,
}
impl NativeActorOwnership {
    pub(in crate::host) fn actor(&self) -> Uuid { self.actor }
    pub(in crate::host) fn owner(&self) -> &Record { &self.owner }
    pub(in crate::host) fn source(&self) -> &chat::Session { &self.source }
    pub(in crate::host) fn scope(&self) -> Result<&LaunchAccountScope, &str> { self.scope.as_ref().map_err(String::as_str) }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::host) struct NativeActorRetirement {
    actor: Uuid,
    owner: Record,
    source: chat::Session,
    scope: Result<LaunchAccountScope, String>,
    confirmed_session: Option<String>,
}

impl NativeActorRetirement {
    pub(in crate::host) fn actor(&self) -> Uuid { self.actor }
    pub(in crate::host) fn owner(&self) -> &Record { &self.owner }
    pub(in crate::host) fn source(&self) -> &chat::Session { &self.source }
    pub(in crate::host) fn scope(&self) -> Result<&LaunchAccountScope, &str> { self.scope.as_ref().map_err(String::as_str) }
    pub(in crate::host) fn confirmed_session(&self) -> Option<&str> { self.confirmed_session.as_deref() }

    fn validate(&self) -> Result<(), String> {
        if self.actor.is_nil()
            || self.owner.kind != crate::daemon::holder_registry::Kind::Native
            || !crate::daemon::holder_registry::record_application_matches(&self.owner)
            || self.owner.terminal != self.source.id
            || !self.source.proof.as_ref().is_some_and(|proof| proof.is_complete() && proof.boot_id == self.owner.boot)
            || self.source.launch.provider != crate::agent::Provider::Claude
            || self.scope.as_ref().is_ok_and(|scope| scope.provider != self.source.launch.provider || !scope.canonical_home.is_absolute())
            || !matches!(self.source.native_continuity, chat::NativeContinuity::RetainedClaudeActor { protocol: 1 })
            || !self.source.has_valid_native_continuity()
            || self.confirmed_session.as_ref().is_some_and(|id| uuid::Uuid::parse_str(id).is_err())
            || self.source.launch.session_id.as_ref().is_some_and(|id| Some(id) != self.confirmed_session.as_ref())
        {
            return Err("Retained actor retirement has invalid exact source or physical account identity".into());
        }
        Ok(())
    }
}

fn same_scope(left:&Result<LaunchAccountScope,String>,right:&Result<LaunchAccountScope,String>) -> bool {
    match (left,right) {
        (Ok(old),Ok(new)) => old.provider==new.provider && old.canonical_home==new.canonical_home && old.device==new.device && old.inode==new.inode,
        (Err(old),Err(new)) => old==new,
        _ => false,
    }
}

pub(super) fn migrate(db: &Connection) -> Result<(), String> {
    let tx = rusqlite::Transaction::new_unchecked(db, TransactionBehavior::Immediate).map_err(|e| e.to_string())?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS native_actor_ownerships(actor TEXT PRIMARY KEY, ownership TEXT NOT NULL) STRICT; CREATE TABLE IF NOT EXISTS native_actor_retirements(actor TEXT PRIMARY KEY REFERENCES native_actor_ownerships(actor), receipt TEXT NOT NULL) STRICT; PRAGMA user_version = 16;").map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())
}

impl Registry {
    /// Publish after authenticating installation/adoption and before exposing
    /// a handle that accepts input. This is a known-owner census, not launch
    /// authority; only that exact helper's authenticated route may refresh it.
    pub(in crate::host) fn register_native_actor_ownership(
        &mut self,
        actor: Uuid,
        owner: &Record,
        source: &chat::Session,
        scope: Result<&LaunchAccountScope, &str>,
    ) -> Result<(), String> {
        if actor.is_nil() || owner.kind != crate::daemon::holder_registry::Kind::Native
            || !crate::daemon::holder_registry::record_application_matches(owner)
            || owner.terminal != source.id
            || source.launch.provider != crate::agent::Provider::Claude
            || !source.proof.as_ref().is_some_and(|proof| proof.is_complete() && proof.boot_id == owner.boot)
            || !matches!(source.native_continuity, chat::NativeContinuity::RetainedClaudeActor { protocol: 1 })
            || !source.has_valid_native_continuity()
        { return Err("Retained actor census has invalid authenticated ownership".into()); }
        let scope = match scope {
            Ok(scope) if scope.provider == source.launch.provider => { scope.revalidate()?; Ok(scope.clone()) },
            Ok(_) => return Err("Retained actor census account provider changed".into()),
            Err(reason) => Err(reason.chars().take(4096).collect()),
        };
        let ownership = NativeActorOwnership { actor, owner: owner.clone(), source: source.clone(), scope };
        let encoded = serde_json::to_string(&ownership).map_err(|e|e.to_string())?;
        if encoded.len()>MAX_RECEIPT_BYTES { return Err("Retained actor census exceeds its metadata bound".into()); }
        let tx=self.db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|e|e.to_string())?;
        let previous: Option<String> = tx.query_row("SELECT ownership FROM native_actor_ownerships WHERE actor=?1",[actor.to_string()],|row|row.get(0)).optional().map_err(|e|e.to_string())?;
        if let Some(previous) = previous {
            if previous.len()>MAX_RECEIPT_BYTES { return Err("Stored actor census exceeds its metadata bound".into()); }
            let previous:NativeActorOwnership=serde_json::from_str(&previous).map_err(|e|e.to_string())?;
            if previous.actor!=actor || previous.owner!=*owner || previous.source.id!=source.id {
                return Err("Retained actor census changed its exact helper".into());
            }
            if let (Ok(old),Ok(new))=(&previous.scope,&ownership.scope) {
                if old.provider!=new.provider || old.device!=new.device || old.inode!=new.inode || old.canonical_home!=new.canonical_home {
                    return Err("Retained actor census changed its captured physical account".into());
                }
            }
            let retired:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM native_actor_retirements WHERE actor=?1)",[actor.to_string()],|row|row.get(0)).map_err(|e|e.to_string())?;
            if retired { return Err("A finally retired actor cannot be registered as a new writer".into()); }
        }
        tx.execute("INSERT INTO native_actor_ownerships(actor,ownership) VALUES (?1,?2) ON CONFLICT(actor) DO UPDATE SET ownership=excluded.ownership",params![actor.to_string(),encoded]).map_err(|e|e.to_string())?;
        tx.commit().map_err(|e|e.to_string())
    }

    /// Bounded keyset pages, including retired owners. Under the caller's
    /// account reservation, walk until an empty page; truncation is never a
    /// complete census. Unregistered/legacy discovery remains a separate error.
    pub(in crate::host) fn native_actor_ownerships(&self, after:Option<Uuid>, limit:usize) -> Result<Vec<NativeActorOwnership>,String> {
        if !(1..=128).contains(&limit) { return Err("Retained actor census page must contain between one and 128 rows".into()); }
        let after=after.map(|id|id.to_string()).unwrap_or_default();
        let mut statement=self.db.prepare("SELECT actor,ownership FROM native_actor_ownerships WHERE actor>?1 ORDER BY actor LIMIT ?2").map_err(|e|e.to_string())?;
        let rows=statement.query_map(params![after,limit as i64],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?))).map_err(|e|e.to_string())?;
        rows.map(|row| {
            let (actor,encoded)=row.map_err(|e|e.to_string())?;
            if encoded.len()>MAX_RECEIPT_BYTES {return Err("Stored actor census exceeds its metadata bound".into());}
            let ownership:NativeActorOwnership=serde_json::from_str(&encoded).map_err(|e|e.to_string())?;
            if ownership.actor.to_string()!=actor || ownership.actor.is_nil() { return Err("Stored actor census key changed its identity".into()); }
            Ok(ownership)
        }).collect()
    }

    /// Call only after the authenticated handle's finish_retained_close proves
    /// semantic/loss ACK, exact helper disappearance and discovery retirement.
    /// A completed provider, Stop ACK or an absent socket is never sufficient.
    pub(in crate::host) fn commit_native_actor_retirement(
        &mut self,
        actor: Uuid,
        owner: &Record,
        source: &chat::Session,
        scope: Result<&LaunchAccountScope, &str>,
        confirmed_session: Option<&str>,
    ) -> Result<(), String> {
        let scope = scope.map(Clone::clone).map_err(|reason| reason.chars().take(4096).collect::<String>());
        let receipt = NativeActorRetirement { actor, owner: owner.clone(), source: source.clone(), scope, confirmed_session: confirmed_session.map(str::to_owned) };
        receipt.validate()?;
        // Preserve the original captured physical tuple even if its pathname
        // was externally removed/replaced after startup. This records process
        // retirement, not current read/delete authority. The deletion permit
        // separately validates its selected current target under reservation.
        let encoded = serde_json::to_string(&receipt).map_err(|e| e.to_string())?;
        if encoded.len() > MAX_RECEIPT_BYTES { return Err("Retained actor retirement exceeds its metadata bound".into()); }
        let tx = self.db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|e|e.to_string())?;
        let known:Option<String>=tx.query_row("SELECT ownership FROM native_actor_ownerships WHERE actor=?1",[actor.to_string()],|row|row.get(0)).optional().map_err(|e|e.to_string())?;
        let known:NativeActorOwnership=serde_json::from_str(&known.ok_or("The retiring actor has no authenticated durable census registration")?).map_err(|e|e.to_string())?;
        if known.actor!=actor || known.owner!=*owner || !same_scope(&known.scope,&receipt.scope) {
            return Err("The retirement proof changed its registered helper or physical account".into());
        }
        let existing: Option<String> = tx.query_row("SELECT receipt FROM native_actor_retirements WHERE actor=?1",[actor.to_string()],|row|row.get(0)).optional().map_err(|e|e.to_string())?;
        if let Some(existing) = existing {
            if existing != encoded { return Err("Retained actor retirement changed an already committed exact proof".into()); }
        } else {
            tx.execute("INSERT INTO native_actor_retirements(actor,receipt) VALUES (?1,?2)",params![actor.to_string(),encoded]).map_err(|e|e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())
    }

    /// Evidence for this exact actor only. The caller still needs a mutation
    /// reservation and a fresh complete account/session ownership census.
    pub(in crate::host) fn native_actor_retirement(&self, actor: Uuid) -> Result<Option<NativeActorRetirement>, String> {
        let encoded: Option<String> = self.db.query_row("SELECT receipt FROM native_actor_retirements WHERE actor=?1",[actor.to_string()],|row|row.get(0)).optional().map_err(|e|e.to_string())?;
        encoded.map(|encoded| {
            if encoded.len() > MAX_RECEIPT_BYTES { return Err("Stored actor retirement exceeds its metadata bound".into()); }
            let receipt: NativeActorRetirement = serde_json::from_str(&encoded).map_err(|e| e.to_string())?;
            receipt.validate()?;
            if receipt.actor != actor { return Err("Stored actor retirement belongs to another actor".into()); }
            Ok(receipt)
        }).transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn fixture(root:&std::path::Path)->(Uuid,Record,chat::Session,LaunchAccountScope) {
        let actor=Uuid::new_v4();
        let proof=crate::model::HostSessionProof { host_session_id:"retirement".into(),boot_id:"boot".into(),host_incarnation:"incarnation".into() };
        let id=crate::model::TerminalId::new();
        let source=chat::Session { id,launch:chat::Launch {provider:crate::agent::Provider::Claude,session_id:None,recipe:crate::model::LaunchRecipe {program:"claude".into(),arguments:vec![],working_directory:root.into(),environment:Default::default(),unset_environment:Default::default()}},proof:Some(proof.clone()),terminal:None,owned_codex_listener:None,native_continuity:chat::NativeContinuity::RetainedClaudeActor {protocol:1}};
        let owner=Record {version:4,build:None,kind:crate::daemon::holder_registry::Kind::Native,application:crate::instance::APPLICATION_ID.into(),boot:proof.boot_id,terminal:id,holder:crate::daemon::holder_registry::Process {pid:42,start:1},root:crate::daemon::holder_registry::Process {pid:43,start:2}};
        let metadata=std::fs::metadata(root).unwrap();
        let scope=LaunchAccountScope { provider:crate::agent::Provider::Claude,canonical_home:root.canonicalize().unwrap(),device:metadata.dev(),inode:metadata.ino(),registered_id:None };
        (actor,owner,source,scope)
    }

    #[test]
    fn no_event_and_unknown_scope_owners_remain_in_complete_paged_census() {
        let directory=tempfile::tempdir_in("/var/tmp").unwrap();
        let mut registry=Registry::at(&directory.path().join("registry")).unwrap();
        let (first,owner,source,scope)=fixture(directory.path());
        registry.register_native_actor_ownership(first,&owner,&source,Ok(&scope)).unwrap();
        let (unknown,other,other_source,_)=fixture(directory.path());
        registry.register_native_actor_ownership(unknown,&other,&other_source,Err("original captured home unavailable")).unwrap();
        assert_eq!(registry.db.query_row("SELECT COUNT(*) FROM native_actor_cursors",[],|row|row.get::<_,i64>(0)).unwrap(),0,"census must not depend on any input/outcome having occurred");
        assert!(registry.native_actor_retirement(first).unwrap().is_none(),"registration is never semantic retirement");
        let mut after=None;
        let mut seen=Vec::new();
        loop {
            let page=registry.native_actor_ownerships(after,1).unwrap();
            if page.is_empty() {break;}
            after=Some(page[0].actor());seen.extend(page);
        }
        assert_eq!(seen.len(),2);
        assert!(seen.iter().find(|row|row.actor()==unknown).unwrap().scope().is_err());
        assert!(registry.native_actor_ownerships(None,0).is_err());
        assert!(registry.native_actor_ownerships(None,129).is_err());
        assert!(registry.commit_native_actor_retirement(unknown,&other,&other_source,Ok(&scope),None).is_err(),"unknown physical scope cannot certify the requested account");
        registry.commit_native_actor_retirement(unknown,&other,&other_source,Err("original captured home unavailable"),None).unwrap();
        let retired=registry.native_actor_retirement(unknown).unwrap().unwrap();
        assert!(retired.scope().is_err(),"actor retirement does not invent a physical account");
        assert!(retired.confirmed_session().is_none(),"actor retirement does not invent a named conversation");
        assert_eq!(retired.owner(),&other);
        assert!(registry.commit_native_actor_retirement(unknown,&other,&other_source,Ok(&scope),None).is_err(),"a finally retired unknown owner cannot acquire a later caller's account identity");
    }

    #[test]
    fn exact_retirement_survives_reopen_without_inventing_an_unnamed_session() {
        let directory=tempfile::tempdir_in("/var/tmp").unwrap();
        let path=directory.path().join("registry");
        let mut registry=Registry::at(&path).unwrap();
        let (actor,owner,source,scope)=fixture(directory.path());
        assert!(registry.commit_native_actor_retirement(actor,&owner,&source,Ok(&scope),None).is_err(),"unregistered owners are not a complete census");
        registry.register_native_actor_ownership(actor,&owner,&source,Ok(&scope)).unwrap();
        registry.commit_native_actor_retirement(actor,&owner,&source,Ok(&scope),None).unwrap();
        registry.commit_native_actor_retirement(actor,&owner,&source,Ok(&scope),None).unwrap();
        assert!(registry.register_native_actor_ownership(actor,&owner,&source,Ok(&scope)).is_err(),"retired UUID cannot become a new writer");
        drop(registry);
        let mut registry=Registry::at(&path).unwrap();
        let receipt=registry.native_actor_retirement(actor).unwrap().unwrap();
        assert_eq!(receipt.actor(),actor);assert_eq!(receipt.owner(),&owner);assert_eq!(receipt.source(),&source);assert_eq!(receipt.scope().unwrap(),&scope);assert!(receipt.confirmed_session().is_none());
        assert!(registry.commit_native_actor_retirement(actor,&owner,&source,Ok(&scope),Some("11111111-1111-4111-8111-111111111111")).is_err(),"later callers cannot invent a named history for an already unnamed retirement");
        let mut wrong=scope.clone();wrong.inode+=1;
        assert!(registry.commit_native_actor_retirement(actor,&owner,&source,Ok(&wrong),None).is_err());
        let mut wrong=owner.clone();wrong.holder.start+=1;
        assert!(registry.commit_native_actor_retirement(actor,&wrong,&source,Ok(&scope),None).is_err());
    }

    #[test]
    fn removed_or_replaced_home_does_not_erase_proved_retirement_of_original_scope() {
        let directory=tempfile::tempdir_in("/var/tmp").unwrap();
        let home=directory.path().join("original-home");
        std::fs::create_dir(&home).unwrap();
        let mut registry=Registry::at(&directory.path().join("registry")).unwrap();
        let (actor,owner,source,scope)=fixture(&home);
        registry.register_native_actor_ownership(actor,&owner,&source,Ok(&scope)).unwrap();
        let _old_home=std::fs::File::open(&home).unwrap();
        std::fs::remove_dir(&home).unwrap();
        assert!(scope.revalidate().is_err());
        registry.commit_native_actor_retirement(actor,&owner,&source,Ok(&scope),None).unwrap();
        std::fs::create_dir(&home).unwrap();
        let (_,_,_,replacement)=fixture(&home);
        assert_ne!(replacement.inode,scope.inode);
        assert!(scope.revalidate().is_err());
        let receipt=registry.native_actor_retirement(actor).unwrap().unwrap();
        assert_eq!(receipt.scope().unwrap(),&scope);
        assert!(registry.commit_native_actor_retirement(actor,&owner,&source,Ok(&replacement),None).is_err());
        registry.commit_native_actor_retirement(actor,&owner,&source,Ok(&scope),None).unwrap();
    }
}
