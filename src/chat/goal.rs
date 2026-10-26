//! Native, thread-scoped goals. This leaf performs no I/O and never sends a
//! prompt. Adapters retain actor authority and validate decoded provider data
//! before publishing it through this state.
use serde::{Deserialize, Serialize};

pub const MAX_OBJECTIVE_CHARS: usize = 4_000;
pub const MAX_THREAD_ID_BYTES: usize = 1_024;

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
    StaleObservation,
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
            Self::StaleObservation => "The goal changed; check its current state before acting",
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
    let input = input.trim_start();
    let (command, rest) = input.split_once(char::is_whitespace).unwrap_or((input, ""));
    if !command.eq_ignore_ascii_case("/goal") {
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
        word if word.eq_ignore_ascii_case("view") || word.eq_ignore_ascii_case("status") => {
            Some(Operation::Get)
        }
        word if word.eq_ignore_ascii_case("pause") => Some(Operation::Pause),
        word if word.eq_ignore_ascii_case("resume") => Some(Operation::Resume),
        word if word.eq_ignore_ascii_case("clear") => Some(Operation::Clear),
        word if word.eq_ignore_ascii_case("set") => {
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Support {
    #[default]
    Unknown,
    Supported,
    Unsupported,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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

/// A bounded UI projection. Physical request IDs and admission tokens stay in
/// the actor. The revision is local ordering, never a provider timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub thread_id: String,
    pub revision: u64,
    pub support: Support,
    pub read: ReadState,
    pub pending: bool,
    pub uncertain: bool,
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

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            thread_id: self.thread_id.clone(),
            revision: self.observation,
            support: self.support,
            read: self.read.clone(),
            pending: self.pending.is_some(),
            uncertain: self.uncertain,
        }
    }

    /// Recheck a control captured by a UI publication. The actor's owner must
    /// additionally admit its original account, directory and connection.
    pub fn begin_for(
        &mut self,
        thread_id: &str,
        revision: u64,
        operation: Operation,
    ) -> Result<Request, Error> {
        if thread_id != self.thread_id {
            return Err(Error::ThreadMismatch);
        }
        if revision != self.observation {
            return Err(Error::StaleObservation);
        }
        self.begin(operation)
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
        let observation = self.observation.checked_add(1).ok_or(Error::Exhausted)?;
        let request = Request {
            id,
            thread_id: self.thread_id.clone(),
            observation,
            operation,
        };
        self.next = id;
        self.observation = observation;
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
    /// Call only after the physical RPC resolves or its connection is proved
    /// closed. A presentation/acknowledgement timeout alone cannot release it.
    pub fn complete(
        &mut self,
        request: &Request,
        result: Result<Reply, Failure>,
    ) -> Result<bool, Error> {
        if self.pending.as_ref() != Some(request) {
            return Ok(false);
        }
        let current =
            request.thread_id == self.thread_id && request.observation == self.observation;
        self.pending = None;
        // A response or refusal changes the publication too. A queued control
        // from before this request must not act on the newly observed goal,
        // even when no provider notification accompanied the response.
        let Some(next) = self.observation.checked_add(1) else {
            self.uncertain = true;
            return Err(Error::Exhausted);
        };
        self.observation = next;
        if !current {
            return Ok(false);
        }
        self.uncertain = true;
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
            (
                Operation::SetObjective(_) | Operation::Pause | Operation::Resume,
                Reply::Updated(goal),
            ) => Some(goal),
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
    fn queued_controls_cannot_cross_a_response_only_goal_replacement() {
        let mut state = State::new("original-thread".into()).unwrap();
        state
            .observe(
                "original-thread",
                Some(goal("original-thread", "old objective", Status::Paused)),
            )
            .unwrap();
        let old = state.snapshot();
        let request = state
            .begin_for(
                &old.thread_id,
                old.revision,
                Operation::SetObjective("replacement".into()),
            )
            .unwrap();
        assert_eq!(
            state.begin_for(&old.thread_id, old.revision, Operation::Clear),
            Err(Error::StaleObservation)
        );
        assert_eq!(state.pending(), Some(&request));
        let replacement = goal("original-thread", "replacement", Status::Active);
        state
            .complete(&request, Ok(Reply::Updated(replacement.clone())))
            .unwrap();
        assert_eq!(
            state.begin_for(&old.thread_id, old.revision, Operation::Clear),
            Err(Error::StaleObservation)
        );
        let current = state.snapshot();
        assert_eq!(current.read, ReadState::Known(Some(replacement)));
        assert_eq!(
            state.begin_for("another-thread", current.revision, Operation::Clear),
            Err(Error::ThreadMismatch)
        );
        assert!(state.pending().is_none());
        let clear = state
            .begin_for(&current.thread_id, current.revision, Operation::Clear)
            .unwrap();
        state.complete(&clear, Ok(Reply::Cleared(true))).unwrap();
        assert_eq!(state.read(), &ReadState::Known(None));
    }

    #[test]
    fn projected_pending_state_roundtrips_without_exposing_a_physical_request() {
        let mut state = State::new("original-thread".into()).unwrap();
        let before = state.snapshot();
        let request = state
            .begin_for(&before.thread_id, before.revision, Operation::Get)
            .unwrap();
        let pending = state.snapshot();
        assert!(pending.pending && pending.revision > before.revision);
        let json = serde_json::to_value(&pending).unwrap();
        assert_eq!(json.as_object().unwrap().len(), 6);
        assert!(json.get("operation").is_none() && json.get("request").is_none());
        assert_eq!(serde_json::from_value::<Snapshot>(json).unwrap(), pending);
        let bytes = postcard::to_allocvec(&pending).unwrap();
        assert_eq!(postcard::from_bytes::<Snapshot>(&bytes).unwrap(), pending);
        state.complete(&request, Ok(Reply::Read(None))).unwrap();
        let confirmed = state.snapshot();
        assert!(!confirmed.pending && confirmed.revision > pending.revision);
        assert_eq!(confirmed.read, ReadState::Known(None));
        assert_eq!(
            state.begin_for(&pending.thread_id, pending.revision, Operation::Resume),
            Err(Error::StaleObservation)
        );
    }

    #[test]
    fn stale_physical_completion_retires_its_publication_without_replacing_newer_goal() {
        let mut state = State::new("original-thread".into()).unwrap();
        let request = state.begin(Operation::Get).unwrap();
        let newest = goal("original-thread", "newer notification", Status::Active);
        state
            .observe("original-thread", Some(newest.clone()))
            .unwrap();
        let held = state.snapshot();
        assert!(held.pending);
        assert!(!state.complete(&request, Ok(Reply::Read(None))).unwrap());
        let settled = state.snapshot();
        assert!(!settled.pending && settled.revision > held.revision);
        assert_eq!(settled.read, ReadState::Known(Some(newest)));
        assert!(!settled.uncertain);
        assert_eq!(
            state.begin_for(&held.thread_id, held.revision, Operation::Clear),
            Err(Error::StaleObservation)
        );
        let before_duplicate = state.snapshot();
        assert!(!state.complete(&request, Ok(Reply::Read(None))).unwrap());
        assert_eq!(state.snapshot(), before_duplicate);

        let mut exhausted = State::new("original-thread".into()).unwrap();
        exhausted.observation = u64::MAX - 1;
        let final_request = exhausted.begin(Operation::Get).unwrap();
        assert_eq!(
            exhausted.complete(&final_request, Ok(Reply::Read(None))),
            Err(Error::Exhausted)
        );
        assert!(exhausted.pending().is_none() && exhausted.uncertain());
        assert_eq!(exhausted.begin(Operation::Get), Err(Error::Exhausted));
    }

    #[test]
    fn slash_commands_are_reserved_and_objectives_count_unicode_characters() {
        for input in ["/goal", " /goal \n", "/goal view", "/goal status"] {
            assert_eq!(parse(input), Some(Ok(Operation::Get)));
        }
        assert_eq!(parse("/GOAL PaUsE"), Some(Ok(Operation::Pause)));
        assert_eq!(parse("/GoAl RESUME"), Some(Ok(Operation::Resume)));
        assert_eq!(parse("/GOAL VIEW"), Some(Ok(Operation::Get)));
        assert_eq!(
            parse("/GoAl SeT PaUsE"),
            Some(Ok(Operation::SetObjective("PaUsE".into())))
        );
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
        for input in [
            "/goals",
            "/GOALS",
            "/goalkeeper",
            "ordinary text",
            "/goal/path",
        ] {
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
    fn mutation_replies_preserve_provider_truth_without_claiming_the_requested_action() {
        let mut state = State::new("thread-a".into()).unwrap();
        state
            .observe("thread-a", Some(goal("thread-a", "finish", Status::Paused)))
            .unwrap();
        let request = state.begin(Operation::Resume).unwrap();
        // Native progress can finish before the response, without an earlier
        // notification. An Active-only acknowledgement test would lose truth.
        let completed = goal("thread-a", "finish", Status::Complete);
        assert!(
            state
                .complete(&request, Ok(Reply::Updated(completed.clone())))
                .unwrap()
        );
        assert_eq!(state.read(), &ReadState::Known(Some(completed)));
        assert!(!state.uncertain());
        let request = state
            .begin(Operation::SetObjective("new objective".into()))
            .unwrap();
        // Another native client can replace the objective. This exact-thread
        // observation is not proof our requested objective won that race.
        let current = goal("thread-a", "other native objective", Status::Blocked);
        assert!(
            state
                .complete(&request, Ok(Reply::Updated(current.clone())))
                .unwrap()
        );
        assert_eq!(state.read(), &ReadState::Known(Some(current)));
        assert!(state.pending().is_none() && !state.uncertain());
    }

    #[test]
    fn direct_wire_construction_cannot_bypass_bounds_or_wrap_request_identity() {
        for thread in ["", " ", "latest", "thread\0other"] {
            assert!(matches!(
                State::new(thread.into()),
                Err(Error::InvalidThread)
            ));
        }
        assert!(State::new("a".repeat(MAX_THREAD_ID_BYTES)).is_ok());
        assert!(matches!(
            State::new("a".repeat(MAX_THREAD_ID_BYTES + 1)),
            Err(Error::InvalidThread)
        ));
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
