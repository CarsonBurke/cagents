//! Native, thread-scoped goals. This leaf performs no I/O and never sends a
//! prompt. Adapters retain actor authority and validate decoded provider data
//! before publishing it through this state.
use serde::{Deserialize, Serialize};

pub const MAX_OBJECTIVE_CHARS: usize = 4_000;
pub const MAX_THREAD_ID_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    Active,
    Paused,
    Blocked,
    UsageLimited,
    BudgetLimited,
    Complete,
}

/// Provider fields retain their native spelling and signed integer ABI.
/// A budget is an observation, never a default or an implicit mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Goal {
    pub thread_id: String,
    pub objective: String,
    pub status: Status,
    pub token_budget: Option<i64>,
    pub tokens_used: i64,
    pub time_used_seconds: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Goal {
    pub fn validate(&self, thread_id: &str) -> Result<(), Error> {
        validate_thread(thread_id)?;
        if self.thread_id != thread_id {
            return Err(Error::ThreadMismatch);
        }
        validated_objective(&self.objective)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Operation {
    Get,
    SetObjective(String),
    Pause,
    Resume,
    Clear,
}

impl Operation {
    /// Direct/serialized commands must pass the same check as slash input.
    pub fn validate(&self) -> Result<(), Error> {
        if let Self::SetObjective(objective) = self {
            validated_objective(objective)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidObjective,
    InvalidThread,
    ThreadMismatch,
    UnexpectedArguments,
    Busy,
    Unsupported,
    InvalidReply,
    Exhausted,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidObjective => "A goal needs 1–4,000 characters of text without NUL bytes",
            Self::InvalidThread => "The original goal thread is unavailable",
            Self::ThreadMismatch => "The goal belongs to a different thread",
            Self::UnexpectedArguments => {
                "Use /goal set to set an objective beginning with a goal command"
            }
            Self::Busy => "The previous goal request is still awaiting its result",
            Self::Unsupported => "Native goals are unavailable for this conversation",
            Self::InvalidReply => "The goal reply did not match the original request",
            Self::Exhausted => "The goal request sequence is unavailable",
        })
    }
}

impl std::error::Error for Error {}

pub fn validated_objective(objective: &str) -> Result<&str, Error> {
    if objective.trim().is_empty()
        || objective.contains('\0')
        || objective.chars().take(MAX_OBJECTIVE_CHARS + 1).count() > MAX_OBJECTIVE_CHARS
    {
        Err(Error::InvalidObjective)
    } else {
        Ok(objective)
    }
}

fn validate_thread(thread: &str) -> Result<(), Error> {
    if thread.trim().is_empty()
        || thread == "latest"
        || thread.len() > MAX_THREAD_ID_BYTES
        || thread.chars().any(char::is_control)
    {
        Err(Error::InvalidThread)
    } else {
        Ok(())
    }
}

/// `None` is ordinary input. `Some(Err(_))` is a reserved goal command that
/// must be shown as an error, never retried as a generic provider prompt.
/// `set` disambiguates objectives equal to a command such as `pause`.
pub fn parse(input: &str) -> Option<Result<Operation, Error>> {
    let rest = input.trim_start().strip_prefix("/goal")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim();
    if rest.is_empty() {
        return Some(Ok(Operation::Get));
    }
    let (word, arguments) = rest
        .split_once(char::is_whitespace)
        .map_or((rest, ""), |(word, arguments)| (word, arguments.trim()));
    let command = match word {
        "view" | "status" => Some(Operation::Get),
        "pause" => Some(Operation::Pause),
        "resume" => Some(Operation::Resume),
        "clear" => Some(Operation::Clear),
        "set" => {
            return Some(
                validated_objective(arguments).map(|text| Operation::SetObjective(text.into())),
            );
        }
        _ => None,
    };
    if let Some(command) = command {
        Some(if arguments.is_empty() {
            Ok(command)
        } else {
            Err(Error::UnexpectedArguments)
        })
    } else {
        Some(validated_objective(rest).map(|text| Operation::SetObjective(text.into())))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Support {
    #[default]
    Unknown,
    Supported,
    Unsupported,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ReadState {
    #[default]
    Unknown,
    Known(Option<Goal>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Read(Option<Goal>),
    Updated(Goal),
    Cleared(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    Unsupported,
    Unavailable,
}

/// Opaque physical request identity. Only its matching completion releases the
/// slot, including when a newer observation has made its result stale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    id: u64,
    thread_id: String,
    observation: u64,
    operation: Operation,
}

impl Request {
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }
    pub fn operation(&self) -> &Operation {
        &self.operation
    }
}

/// At most one outstanding operation and one bounded confirmed observation.
/// Epochs order local observations, not provider timestamps. This state neither
/// proves actor admission nor automatically retries, reads, or resumes a goal.
#[derive(Debug, Clone)]
pub struct State {
    thread_id: String,
    observation: u64,
    next: u64,
    support: Support,
    read: ReadState,
    uncertain: bool,
    pending: Option<Request>,
}

impl State {
    pub fn new(thread_id: String) -> Result<Self, Error> {
        validate_thread(&thread_id)?;
        Ok(Self {
            thread_id,
            observation: 0,
            next: 0,
            support: Support::Unknown,
            read: ReadState::Unknown,
            uncertain: false,
            pending: None,
        })
    }

    pub fn support(&self) -> Support {
        self.support
    }
    pub fn read(&self) -> &ReadState {
        &self.read
    }
    pub fn uncertain(&self) -> bool {
        self.uncertain
    }
    pub fn pending(&self) -> Option<&Request> {
        self.pending.as_ref()
    }

    /// A changed owner/thread invalidates observations without pretending an
    /// already-issued request stopped. Actor teardown remains the owner's job.
    pub fn rebind(&mut self, thread_id: String) -> Result<(), Error> {
        validate_thread(&thread_id)?;
        let observation = self.observation.checked_add(1).ok_or(Error::Exhausted)?;
        self.thread_id = thread_id;
        self.observation = observation;
        self.support = Support::Unknown;
        self.read = ReadState::Unknown;
        self.uncertain = self.pending.is_some();
        Ok(())
    }

    pub fn begin(&mut self, operation: Operation) -> Result<Request, Error> {
        operation.validate()?;
        if self.pending.is_some() {
            return Err(Error::Busy);
        }
        if self.support == Support::Unsupported {
            return Err(Error::Unsupported);
        }
        let id = self.next.checked_add(1).ok_or(Error::Exhausted)?;
        let request = Request {
            id,
            thread_id: self.thread_id.clone(),
            observation: self.observation,
            operation,
        };
        self.next = id;
        self.pending = Some(request.clone());
        Ok(request)
    }

    /// Apply an exact-thread updated/cleared notification. A notification does
    /// not release a physical request, even if it arrived before its response.
    pub fn observe(&mut self, thread_id: &str, goal: Option<Goal>) -> Result<(), Error> {
        if thread_id != self.thread_id {
            return Err(Error::ThreadMismatch);
        }
        if let Some(goal) = &goal {
            goal.validate(thread_id)?;
        }
        let observation = self.observation.checked_add(1).ok_or(Error::Exhausted)?;
        self.observation = observation;
        self.support = Support::Supported;
        self.read = ReadState::Known(goal);
        self.uncertain = false;
        Ok(())
    }

    /// True means a current completion was handled, not that a goal or its
    /// objective succeeded. Lost/invalid replies never cause an automatic retry.
    pub fn complete(
        &mut self,
        request: &Request,
        result: Result<Reply, Failure>,
    ) -> Result<bool, Error> {
        if self.pending.as_ref() != Some(request) {
            return Ok(false);
        }
        self.pending = None;
        if request.thread_id != self.thread_id || request.observation != self.observation {
            return Ok(false);
        }
        let reply = match result {
            Err(Failure::Unsupported) => {
                self.support = Support::Unsupported;
                self.read = ReadState::Unknown;
                self.uncertain = false;
                return Ok(true);
            }
            Err(Failure::Unavailable) => {
                self.uncertain = true;
                return Ok(true);
            }
            Ok(reply) => reply,
        };
        let goal = match (&request.operation, reply) {
            (Operation::Get, Reply::Read(goal)) => goal,
            (operation, Reply::Updated(goal)) => {
                let matches = match operation {
                    Operation::SetObjective(objective) => goal.objective == *objective,
                    Operation::Pause => goal.status == Status::Paused,
                    Operation::Resume => goal.status == Status::Active,
                    _ => false,
                };
                if !matches {
                    self.uncertain = true;
                    return Err(Error::InvalidReply);
                }
                Some(goal)
            }
            (Operation::Clear, Reply::Cleared(true)) => None,
            (Operation::Clear, Reply::Cleared(false)) => {
                // A refusal to clear is not an observation of absence.
                self.support = Support::Supported;
                self.uncertain = true;
                return Ok(true);
            }
            _ => {
                self.uncertain = true;
                return Err(Error::InvalidReply);
            }
        };
        if let Some(goal) = &goal
            && let Err(error) = goal.validate(&self.thread_id)
        {
            self.uncertain = true;
            return Err(error);
        }
        self.support = Support::Supported;
        self.read = ReadState::Known(goal);
        self.uncertain = false;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goal(thread: &str, objective: &str, status: Status) -> Goal {
        Goal {
            thread_id: thread.into(),
            objective: objective.into(),
            status,
            token_budget: None,
            tokens_used: 123,
            time_used_seconds: 7,
            created_at: 50,
            updated_at: 60,
        }
    }

    #[test]
    fn slash_commands_are_reserved_and_objectives_count_unicode_characters() {
        for input in ["/goal", " /goal \n", "/goal view", "/goal status"] {
            assert_eq!(parse(input), Some(Ok(Operation::Get)));
        }
        for (word, command) in [
            ("pause", Operation::Pause),
            ("resume", Operation::Resume),
            ("clear", Operation::Clear),
        ] {
            assert_eq!(parse(&format!("/goal {word}")), Some(Ok(command)));
            assert_eq!(
                parse(&format!("/goal set {word}")),
                Some(Ok(Operation::SetObjective(word.into())))
            );
            assert_eq!(
                parse(&format!("/goal {word} something")),
                Some(Err(Error::UnexpectedArguments))
            );
        }
        for word in ["pause", "resume", "clear", "status", "view", "set"] {
            assert_eq!(
                parse(&format!("/goal set {word}")),
                Some(Ok(Operation::SetObjective(word.into())))
            );
        }
        for input in ["/goals", "/goalkeeper", "ordinary text", "/goal/path"] {
            assert_eq!(parse(input), None);
        }
        assert_eq!(parse("/goal set \t"), Some(Err(Error::InvalidObjective)));
        assert_eq!(parse("/goal a\0b"), Some(Err(Error::InvalidObjective)));
        let allowed = "🦀".repeat(MAX_OBJECTIVE_CHARS);
        assert_eq!(
            parse(&format!("/goal {allowed}")),
            Some(Ok(Operation::SetObjective(allowed.clone())))
        );
        assert_eq!(
            parse(&format!("/goal {allowed}🦀")),
            Some(Err(Error::InvalidObjective))
        );
        assert!(Operation::SetObjective(" ".into()).validate().is_err());
        assert!(Operation::SetObjective(allowed).validate().is_ok());
    }

    #[test]
    fn native_json_preserves_status_fields_and_never_supplies_a_budget() {
        for (status, spelling) in [
            (Status::Active, "active"),
            (Status::Paused, "paused"),
            (Status::Blocked, "blocked"),
            (Status::UsageLimited, "usageLimited"),
            (Status::BudgetLimited, "budgetLimited"),
            (Status::Complete, "complete"),
        ] {
            let original = goal("thread-a", "finish", status);
            let mut value = serde_json::to_value(&original).unwrap();
            assert_eq!(value["status"], spelling);
            assert_eq!(value["threadId"], "thread-a");
            assert_eq!(value["tokensUsed"], 123);
            assert!(value["tokenBudget"].is_null());
            value.as_object_mut().unwrap().remove("tokenBudget");
            let decoded: Goal = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(decoded, original);
            assert!(decoded.validate("thread-a").is_ok());
            value["status"] = "futureStatus".into();
            assert!(serde_json::from_value::<Goal>(value).is_err());
        }
        let mut value = serde_json::to_value(goal("thread-a", "finish", Status::Active)).unwrap();
        value.as_object_mut().unwrap().remove("tokensUsed");
        assert!(serde_json::from_value::<Goal>(value).is_err());
    }

    #[test]
    fn notification_clear_fences_an_old_get_but_releases_only_its_actual_slot() {
        let mut state = State::new("thread-a".into()).unwrap();
        let old = state.begin(Operation::Get).unwrap();
        state.observe("thread-a", None).unwrap();
        assert_eq!(state.begin(Operation::Resume), Err(Error::Busy));
        assert!(
            !state
                .complete(
                    &old,
                    Ok(Reply::Read(Some(goal("thread-a", "old", Status::Active))))
                )
                .unwrap()
        );
        assert_eq!(state.read(), &ReadState::Known(None));
        let current = state.begin(Operation::Get).unwrap();
        assert!(!state.complete(&old, Err(Failure::Unavailable)).unwrap());
        assert_eq!(state.pending(), Some(&current));
        assert!(
            state
                .complete(
                    &current,
                    Ok(Reply::Read(Some(goal("thread-a", "new", Status::Paused))))
                )
                .unwrap()
        );
        assert_eq!(
            state.read(),
            &ReadState::Known(Some(goal("thread-a", "new", Status::Paused)))
        );
    }

    #[test]
    fn newer_notifications_win_without_comparing_provider_timestamps() {
        let mut state = State::new("thread-a".into()).unwrap();
        let request = state.begin(Operation::SetObjective("old".into())).unwrap();
        let mut latest = goal("thread-a", "new", Status::Blocked);
        latest.updated_at = 1;
        state.observe("thread-a", Some(latest.clone())).unwrap();
        let mut older = goal("thread-a", "old", Status::Active);
        older.updated_at = i64::MAX;
        assert!(!state.complete(&request, Ok(Reply::Updated(older))).unwrap());
        assert_eq!(state.read(), &ReadState::Known(Some(latest)));
        assert!(!state.uncertain());
        assert_eq!(state.support(), Support::Supported);
    }

    #[test]
    fn rebinding_preserves_one_physical_operation_and_refuses_foreign_observations() {
        let mut state = State::new("thread-a".into()).unwrap();
        let old = state.begin(Operation::Pause).unwrap();
        state.rebind("thread-b".into()).unwrap();
        for _ in 0..64 {
            assert_eq!(state.begin(Operation::Get), Err(Error::Busy));
        }
        assert_eq!(state.observe("thread-a", None), Err(Error::ThreadMismatch));
        assert_eq!(
            state.observe(
                "thread-b",
                Some(goal("thread-a", "foreign", Status::Active))
            ),
            Err(Error::ThreadMismatch)
        );
        assert_eq!(state.pending(), Some(&old));
        assert!(!state.complete(&old, Err(Failure::Unsupported)).unwrap());
        assert_eq!(state.support(), Support::Unknown);
        assert_eq!(state.read(), &ReadState::Unknown);
        let current = state.begin(Operation::Get).unwrap();
        assert!(state.complete(&current, Ok(Reply::Read(None))).unwrap());
        assert!(!state.uncertain());
    }

    #[test]
    fn lost_mutation_and_false_clear_do_not_confirm_absence_or_retry() {
        let mut state = State::new("thread-a".into()).unwrap();
        let original = goal("thread-a", "retain", Status::Paused);
        state.observe("thread-a", Some(original.clone())).unwrap();
        let request = state.begin(Operation::Resume).unwrap();
        assert!(state.complete(&request, Err(Failure::Unavailable)).unwrap());
        assert!(state.pending().is_none() && state.uncertain());
        assert_eq!(state.read(), &ReadState::Known(Some(original.clone())));
        let clear = state.begin(Operation::Clear).unwrap();
        assert!(state.complete(&clear, Ok(Reply::Cleared(false))).unwrap());
        assert!(state.pending().is_none() && state.uncertain());
        assert_eq!(state.read(), &ReadState::Known(Some(original)));
        let read = state.begin(Operation::Get).unwrap();
        assert!(state.complete(&read, Ok(Reply::Read(None))).unwrap());
        assert_eq!(state.read(), &ReadState::Known(None));
        assert!(!state.uncertain());
    }

    #[test]
    fn incompatible_or_unbound_replies_cannot_publish_a_goal() {
        for reply in [
            Reply::Updated(goal("thread-b", "wanted", Status::Active)),
            Reply::Updated(goal("thread-a", "different", Status::Active)),
            Reply::Read(None),
            Reply::Cleared(true),
        ] {
            let mut state = State::new("thread-a".into()).unwrap();
            let request = state
                .begin(Operation::SetObjective("wanted".into()))
                .unwrap();
            assert!(state.complete(&request, Ok(reply)).is_err());
            assert_eq!(state.read(), &ReadState::Unknown);
            assert!(state.pending().is_none() && state.uncertain());
        }
        let mut state = State::new("thread-a".into()).unwrap();
        let request = state.begin(Operation::Get).unwrap();
        assert!(state.complete(&request, Err(Failure::Unsupported)).unwrap());
        assert_eq!(state.support(), Support::Unsupported);
        assert_eq!(
            state.begin(Operation::SetObjective("never a prompt".into())),
            Err(Error::Unsupported)
        );
        assert!(state.pending().is_none());
        assert!(state.begin(Operation::SetObjective("\0".into())).is_err());
    }

    #[test]
    fn direct_wire_construction_cannot_bypass_bounds_or_wrap_request_identity() {
        for thread in ["", " ", "latest", "thread\0other"] {
            assert!(matches!(
                State::new(thread.into()),
                Err(Error::InvalidThread)
            ));
        }
        let mut state = State::new("thread-a".into()).unwrap();
        let oversized = "🦀".repeat(MAX_OBJECTIVE_CHARS + 1);
        let direct: Operation = serde_json::from_value(serde_json::json!({
            "SetObjective": oversized
        }))
        .unwrap();
        assert_eq!(state.begin(direct), Err(Error::InvalidObjective));
        assert!(state.pending().is_none());
        let request = state.begin(Operation::Get).unwrap();
        assert_eq!(
            request.id(),
            1,
            "refused input consumes no request identity"
        );
        let invalid = goal("thread-a", " ", Status::Active);
        assert_eq!(
            state.observe("thread-a", Some(invalid.clone())),
            Err(Error::InvalidObjective)
        );
        assert_eq!(state.pending(), Some(&request));
        assert_eq!(
            state.complete(&request, Ok(Reply::Read(Some(invalid)))),
            Err(Error::InvalidObjective)
        );
        assert_eq!(state.read(), &ReadState::Unknown);
        assert!(state.uncertain());
        state.next = u64::MAX;
        assert_eq!(state.begin(Operation::Get), Err(Error::Exhausted));
        assert!(state.pending().is_none());
        state.observation = u64::MAX;
        assert_eq!(state.observe("thread-a", None), Err(Error::Exhausted));
        assert_eq!(state.rebind("thread-b".into()), Err(Error::Exhausted));
        assert_eq!(state.thread_id, "thread-a");
        assert_eq!(state.read(), &ReadState::Unknown);
    }
}
