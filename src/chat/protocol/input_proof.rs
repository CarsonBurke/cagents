//! Exact live accepted-input joins, independent of optional settings capture.
//! Original input metadata and historical outcomes share this provider proof.
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};

pub(crate) const MAX_PENDING: usize = 64;
const MAX_RETIRED: usize = 128;
pub(crate) const MAX_ID_BYTES: usize = 512;
const MAX_ALIASES: usize = 64;
const MAX_ALIAS_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AcceptedPromptProof {
    pub command_id: String,
    pub session_id: String,
    pub user_id: String,
    pub turn_id: String,
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct User {
    session: String,
    turn: String,
    id: String,
    aliases: Vec<String>,
}

struct Pending {
    command: String,
    expected_session: Option<String>,
    accepted: Option<(Option<String>, String)>,
    user: Option<User>,
}

#[derive(Default)]
struct Joins {
    pending: BTreeMap<String, Pending>,
    retired: VecDeque<String>,
}

pub(crate) fn valid_id(id: &str) -> bool {
    !id.trim().is_empty() && id.len() <= MAX_ID_BYTES && !id.chars().any(char::is_control)
}

fn aliases(values: &[String]) -> Option<Vec<String>> {
    if values.len() > MAX_ALIASES
        || values.iter().map(String::len).sum::<usize>() > MAX_ALIAS_BYTES
        || values.iter().any(|value| !valid_id(value))
    {
        return None;
    }
    let mut result = Vec::new();
    for value in values {
        if !result.contains(value) {
            result.push(value.clone());
        }
    }
    Some(result)
}

impl Joins {
    fn submit(&mut self, key: &str, command: &str, expected: Option<&str>) -> Result<(), String> {
        if !valid_id(key) || !valid_id(command) || expected.is_some_and(|id| !valid_id(id)) {
            return Err("Invalid original-command proof identity".into());
        }
        if self.pending.len() >= MAX_PENDING {
            return Err("Too many pending original-command proofs".into());
        }
        if self.pending.contains_key(key)
            || self.retired.iter().any(|id| id == key)
            || self
                .pending
                .values()
                .any(|pending| pending.command == command)
        {
            return Err("Original-command proof identity was already submitted".into());
        }
        self.pending.insert(
            key.into(),
            Pending {
                command: command.into(),
                expected_session: expected.map(str::to_owned),
                accepted: None,
                user: None,
            },
        );
        Ok(())
    }

    fn retire(&mut self, key: &str) {
        if self.pending.remove(key).is_some() {
            self.retired.push_back(key.into());
            while self.retired.len() > MAX_RETIRED {
                self.retired.pop_front();
            }
        }
    }

    fn cancel(&mut self, command: &str) {
        let key = self
            .pending
            .iter()
            .find_map(|(key, pending)| (pending.command == command).then(|| key.clone()));
        if let Some(key) = key {
            self.retire(&key);
        }
    }

    fn accepted(
        &mut self,
        key: &str,
        session: Option<&str>,
        turn: &str,
    ) -> Option<AcceptedPromptProof> {
        if !valid_id(turn) || session.is_some_and(|id| !valid_id(id)) {
            return None;
        }
        let pending = self.pending.get_mut(key)?;
        if session
            .zip(pending.expected_session.as_deref())
            .is_some_and(|(a, b)| a != b)
        {
            return None;
        }
        let accepted = (session.map(str::to_owned), turn.to_owned());
        if let Some(previous) = &pending.accepted {
            if previous.1 != accepted.1
                || previous
                    .0
                    .as_deref()
                    .zip(session)
                    .is_some_and(|(a, b)| a != b)
            {
                self.retire(key);
                return None;
            }
        }
        // A later exact session confirmation may strengthen the same ACK.
        if pending
            .accepted
            .as_ref()
            .is_none_or(|(session, _)| session.is_none())
        {
            pending.accepted = Some(accepted);
        }
        self.finish(key)
    }

    fn user(&mut self, key: &str, mut user: User) -> Option<AcceptedPromptProof> {
        let pending = self.pending.get_mut(key)?;
        if pending
            .expected_session
            .as_deref()
            .is_some_and(|expected| expected != user.session)
        {
            return None;
        }
        if let Some(previous) = &pending.user {
            if (
                previous.session.as_str(),
                previous.turn.as_str(),
                previous.id.as_str(),
            ) != (user.session.as_str(), user.turn.as_str(), user.id.as_str())
            {
                self.retire(key);
                return None;
            }
            let mut combined = previous.aliases.clone();
            combined.extend(user.aliases);
            user.aliases = aliases(&combined)?;
        }
        pending.user = Some(user);
        self.finish(key)
    }

    fn finish(&mut self, key: &str) -> Option<AcceptedPromptProof> {
        let pending = self.pending.get(key)?;
        let (session, turn) = pending.accepted.as_ref()?;
        let user = pending.user.as_ref()?;
        if turn != &user.turn
            || session
                .as_deref()
                .is_some_and(|session| session != user.session)
        {
            return None;
        }
        let proof = AcceptedPromptProof {
            command_id: pending.command.clone(),
            session_id: user.session.clone(),
            user_id: user.id.clone(),
            turn_id: user.turn.clone(),
            aliases: user.aliases.clone(),
        };
        self.retire(key);
        Some(proof)
    }
}

#[derive(Default)]
pub(crate) struct ClaudeProofs {
    joins: Joins,
}

impl ClaudeProofs {
    pub fn submit(
        &mut self,
        command: &str,
        uuid: &str,
        expected_session: Option<&str>,
    ) -> Result<(), String> {
        self.joins.submit(uuid, command, expected_session)
    }

    pub fn cancel(&mut self, command: &str) {
        self.joins.cancel(command);
    }

    /// The optional context must itself come from a provider-confirmed session,
    /// never launch argv or submit's expected session. A user frame supplies
    /// its own authority when context was not confirmed before the first prompt.
    pub fn observe(
        &mut self,
        frame: &Value,
        parsed_aliases: &[String],
        confirmed_session: Option<&str>,
    ) -> Option<AcceptedPromptProof> {
        let frame_session = frame["session_id"].as_str();
        if frame_session
            .zip(confirmed_session)
            .is_some_and(|(a, b)| a != b)
        {
            return None;
        }
        let session = frame_session.or(confirmed_session);
        match frame["type"].as_str()? {
            "command_lifecycle" => {
                let key = frame["command_uuid"].as_str()?;
                match frame["state"].as_str()? {
                    "started" | "completed" => self.joins.accepted(key, session, key),
                    "cancelled" | "discarded" | "refused" => {
                        // A started command may later be cancelled because its
                        // turn failed. Its delayed exact user echo still joins.
                        if self
                            .joins
                            .pending
                            .get(key)
                            .is_some_and(|pending| pending.accepted.is_none())
                        {
                            self.joins.retire(key);
                        }
                        None
                    }
                    _ => None,
                }
            }
            "user" => {
                if !frame["parent_tool_use_id"].is_null()
                    || frame["isSynthetic"].as_bool() == Some(true)
                    || frame["message"]["role"] != "user"
                {
                    return None;
                }
                let key = frame["uuid"].as_str()?;
                let session = session.filter(|id| valid_id(id))?;
                self.joins.user(
                    key,
                    User {
                        session: session.into(),
                        turn: key.into(),
                        id: key.into(),
                        aliases: aliases(parsed_aliases)?,
                    },
                )
            }
            _ => None,
        }
    }
}

#[derive(Default)]
pub(crate) struct CodexProofs {
    joins: Joins,
    rpc: BTreeMap<String, String>,
}

impl CodexProofs {
    /// Register the exact turn/start request ID and clientUserMessageId written
    /// by the adapter. Queue-add success alone is not turn/start acceptance.
    pub fn submit(
        &mut self,
        command: &str,
        client_id: &str,
        rpc_id: &str,
        expected_session: Option<&str>,
    ) -> Result<(), String> {
        if !valid_id(rpc_id) || self.rpc.contains_key(rpc_id) {
            return Err("Invalid or reused original-command RPC identity".into());
        }
        self.joins.submit(client_id, command, expected_session)?;
        self.rpc.insert(rpc_id.into(), client_id.into());
        Ok(())
    }

    fn prune(&mut self) {
        self.rpc
            .retain(|_, key| self.joins.pending.contains_key(key));
    }

    pub fn cancel(&mut self, command: &str) {
        self.joins.cancel(command);
        self.prune();
    }

    /// A successful correlated turn/start response proves the turn, not the
    /// user identity. Only the actual UserMessage clientId completes the join.
    pub fn acknowledge(
        &mut self,
        frame: &Value,
        confirmed_session: Option<&str>,
    ) -> Option<AcceptedPromptProof> {
        let rpc_id = frame["id"].as_str()?;
        let key = self.rpc.get(rpc_id)?.clone();
        if frame.get("error").is_some_and(|error| !error.is_null()) {
            self.joins.retire(&key);
            self.prune();
            return None;
        }
        let turn = frame["result"]["turn"]["id"].as_str()?;
        let proof = self.joins.accepted(&key, confirmed_session, turn);
        self.prune();
        proof
    }

    /// The caller supplies the actual enclosing provider thread/turn and the
    /// aliases returned by parsing this native item. No suffix is reconstructed.
    pub fn user(
        &mut self,
        session: &str,
        turn: &str,
        item: &Value,
        parsed_aliases: &[String],
    ) -> Option<AcceptedPromptProof> {
        if item["type"] != "userMessage" || !valid_id(session) || !valid_id(turn) {
            return None;
        }
        let key = item["clientId"].as_str().filter(|id| valid_id(id))?;
        let id = item["id"].as_str().filter(|id| valid_id(id))?;
        let proof = self.joins.user(
            key,
            User {
                session: session.into(),
                turn: turn.into(),
                id: id.into(),
                aliases: aliases(parsed_aliases)?,
            },
        );
        self.prune();
        proof
    }
}

