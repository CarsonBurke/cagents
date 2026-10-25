//! Short-lived checkout selections belong to one host and one authenticated
//! caller. They carry observations and retained pins, never execution permits.
pub(super) mod capture;

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use crate::{
    channel_work::{Change, CheckoutContext, Home, Mutation, Source, Target},
    channel_work_api::Caller,
};
use capture::{CapturedCheckout, CheckoutFacts, CheckoutIdentity};
use uuid::Uuid;

const MAX_SELECTIONS: usize = 64;
const MAX_INTERVALS: usize = 64;
const MAX_INSPECTIONS: usize = 4;
const SELECTION_LIFETIME: Duration = Duration::from_secs(5 * 60);

/// Comparison data constructed after the service authenticates its caller.
/// This type cannot be deserialized from a request.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct Owner {
    pub(super) source: Source,
    pub(super) caller: Option<Caller>,
}

impl Owner {
    fn validate(&self) -> Result<(), String> {
        match (&self.source, &self.caller) {
            (Source::User, None) => Ok(()),
            (Source::Session(source), Some(caller))
                if source.session == caller.session
                    && source.run == caller.run
                    && !source.session.as_uuid().is_nil()
                    && !source.lineage.0.as_uuid().is_nil()
                    && !caller.card.as_uuid().is_nil() =>
            {
                Ok(())
            }
            _ => Err("The checkout selection has no matching authenticated caller".into()),
        }
    }
}

/// Safe response metadata. The nonce grants no authority without its original
/// home and caller, and is not a file descriptor or a filesystem capability.
#[derive(Clone)]
pub(super) struct Candidate {
    pub(super) selection: Uuid,
    pub(super) home: Home,
    pub(super) context: CheckoutContext,
}

struct Selection {
    candidate: Candidate,
    owner: Owner,
    pin: Arc<CapturedCheckout>,
    created: Instant,
    operation: Option<Operation>,
}

struct Operation {
    mutation: Mutation,
    active: Arc<AtomicBool>,
}

#[derive(Debug)]
pub(super) enum BeginError {
    Refused(String),
    InFlight,
}

/// One original attempt owns its reader/commit interval. A duplicate may check
/// its durable receipt, but cannot start another attempt before this one ends.
pub(super) struct Attempt {
    pub(super) prepared: Prepared,
    active: Arc<AtomicBool>,
}
impl Drop for Attempt {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}

struct Interval {
    registry: Uuid,
    identity: CheckoutIdentity,
    facts: CheckoutFacts,
    context: CheckoutContext,
    observed: Instant,
    // Retaining the original objects prevents a released inode from becoming
    // a new checkout that happens to match an old cached device/inode pair.
    _pin: Arc<CapturedCheckout>,
    touched: Instant,
}

/// Clone under the short host lock; perform Git/FS revalidation after releasing
/// that lock. The canonical write separately rechecks home/source/CAS.
pub(super) struct Prepared {
    pub(super) context: CheckoutContext,
    pub(super) pin: Arc<CapturedCheckout>,
}

/// Bound Git readers per host without retaining a host lock across I/O.
pub(super) struct Inspection {
    active: Arc<AtomicUsize>,
}
impl Drop for Inspection {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Stored directly in HostState, so host retirement drops every owned pin.
/// There is no global cache, background timer, pathname-based actor fallback,
/// or persistence of a transient selection into the work ledger.
#[derive(Default)]
pub(in crate::host) struct Selections {
    selections: Vec<Selection>,
    intervals: Vec<Interval>,
    inspections: Arc<AtomicUsize>,
}

impl Selections {
    pub(super) fn inspect(&self) -> Result<Inspection, String> {
        self.inspections
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_INSPECTIONS).then(|| active + 1)
            })
            .map_err(|_| "Checkout inspections are busy; try again shortly".to_owned())?;
        Ok(Inspection {
            active: Arc::clone(&self.inspections),
        })
    }

    fn expire(&mut self, now: Instant) {
        self.selections.retain(|entry| {
            entry
                .operation
                .as_ref()
                .is_some_and(|operation| operation.active.load(Ordering::Acquire))
                || now
                    .checked_duration_since(entry.created)
                    .is_some_and(|elapsed| elapsed < SELECTION_LIFETIME)
        });
        let pending = self
            .selections
            .iter()
            .map(|entry| entry.candidate.context.id)
            .collect::<std::collections::HashSet<_>>();
        self.intervals.retain(|entry| {
            pending.contains(&entry.context.id)
                || now
                    .checked_duration_since(entry.touched)
                    .is_some_and(|elapsed| elapsed < SELECTION_LIFETIME)
        });
    }

    pub(super) fn issue(
        &mut self,
        home: Home,
        owner: Owner,
        pin: Arc<CapturedCheckout>,
        now: Instant,
    ) -> Result<Candidate, String> {
        home.validate().map_err(|error| error.to_string())?;
        owner.validate()?;
        self.expire(now);
        if self.selections.len() == MAX_SELECTIONS {
            return Err(
                "Too many checkout selections are pending; finish one or select again later".into(),
            );
        }
        let identity = pin.identity();
        let facts = pin.facts();
        let observed = pin.observed_at();
        let existing = self
            .intervals
            .iter()
            .position(|entry| entry.registry == home.registry && entry.identity == identity);
        let context = if let Some(index) = existing {
            let interval = &self.intervals[index];
            if observed < interval.observed
                || (observed == interval.observed && interval.facts != facts)
            {
                return Err(
                    "A newer checkout inspection completed first; inspect the checkout again"
                        .into(),
                );
            }
            let generation = if interval.facts == facts {
                interval.context.generation
            } else {
                interval
                    .context
                    .generation
                    .checked_add(1)
                    .ok_or("The checkout observation generation is exhausted")?
            };
            CheckoutContext {
                id: interval.context.id,
                registry: home.registry,
                repository: facts.repository.clone(),
                path: facts.path.clone(),
                branch: facts.branch.clone(),
                head: facts.head.clone(),
                generation,
                dirty: facts.dirty,
            }
        } else {
            CheckoutContext {
                id: Uuid::new_v4(),
                registry: home.registry,
                repository: facts.repository.clone(),
                path: facts.path.clone(),
                branch: facts.branch.clone(),
                head: facts.head.clone(),
                generation: 1,
                dirty: facts.dirty,
            }
        };
        context.validate().map_err(|error| error.to_string())?;
        if existing.is_none() && self.intervals.len() == MAX_INTERVALS {
            let pending = self
                .selections
                .iter()
                .map(|entry| entry.candidate.context.id)
                .collect::<std::collections::HashSet<_>>();
            let evict = self
                .intervals
                .iter()
                .enumerate()
                .filter(|(_, entry)| !pending.contains(&entry.context.id))
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(index, _)| index)
                .ok_or("The checkout selection cache is busy; finish a pending selection first")?;
            self.intervals.remove(evict);
        }
        let interval = Interval {
            registry: home.registry,
            identity,
            facts,
            context: context.clone(),
            observed,
            _pin: Arc::clone(&pin),
            touched: now,
        };
        if let Some(index) = existing {
            self.intervals[index] = interval;
        } else {
            self.intervals.push(interval);
        }
        let candidate = Candidate {
            selection: Uuid::new_v4(),
            home,
            context,
        };
        self.selections.push(Selection {
            candidate: candidate.clone(),
            owner,
            pin,
            created: now,
            operation: None,
        });
        Ok(candidate)
    }

    pub(super) fn lookup(
        &mut self,
        home: Home,
        owner: &Owner,
        selection: Uuid,
        now: Instant,
    ) -> Result<Prepared, String> {
        home.validate().map_err(|error| error.to_string())?;
        owner.validate()?;
        self.expire(now);
        let found = self
            .selections
            .iter()
            .find(|entry| {
                entry.candidate.selection == selection
                    && entry.candidate.home == home
                    && &entry.owner == owner
            })
            .ok_or(
                "This checkout selection expired or belongs to another caller; select it again",
            )?;
        Ok(Prepared {
            context: found.candidate.context.clone(),
            pin: Arc::clone(&found.pin),
        })
    }

    pub(super) fn consume(&mut self, home: Home, owner: &Owner, selection: Uuid) {
        self.selections.retain(|entry| {
            entry.candidate.selection != selection
                || entry.candidate.home != home
                || &entry.owner != owner
        });
    }

    /// Bind before slow reads, so an identical request's failure cannot be
    /// confused with the outcome of an original attempt that is still running.
    pub(super) fn begin_operation(
        &mut self,
        home: Home,
        owner: &Owner,
        selection: Uuid,
        mutation: &Mutation,
        now: Instant,
    ) -> Result<Attempt, BeginError> {
        let prepared = self
            .lookup(home, owner, selection, now)
            .map_err(BeginError::Refused)?;
        if mutation.home != home
            || !matches!(&mutation.change, Change::Claim { target: Target::Local(id), .. } if *id == prepared.context.id)
            || mutation
                .canonicalized()
                .map_err(|error| BeginError::Refused(error.to_string()))?
                != *mutation
        {
            return Err(BeginError::Refused(
                "The request does not match the selected local work identity".into(),
            ));
        }
        let selected = self
            .selections
            .iter_mut()
            .find(|entry| entry.candidate.selection == selection)
            .ok_or_else(|| {
                BeginError::Refused("The checkout selection is no longer available".into())
            })?;
        if let Some(original) = &selected.operation {
            if &original.mutation != mutation {
                return Err(BeginError::Refused("This checkout selection already belongs to another tracking request; inspect again".into()));
            }
        } else {
            selected.operation = Some(Operation {
                mutation: mutation.clone(),
                active: Arc::new(AtomicBool::new(false)),
            });
        }
        let operation = selected.operation.as_ref().expect("operation just bound");
        operation
            .active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| BeginError::InFlight)?;
        Ok(Attempt {
            prepared,
            active: Arc::clone(&operation.active),
        })
    }

    pub(super) fn confirm_operation(
        &mut self,
        home: Home,
        owner: &Owner,
        selection: Uuid,
        attempt: &Attempt,
        now: Instant,
    ) -> Result<(), String> {
        self.lookup(home, owner, selection, now)?;
        let selected = self
            .selections
            .iter()
            .find(|entry| entry.candidate.selection == selection)
            .ok_or("The checkout selection is no longer available")?;
        if !now
            .checked_duration_since(selected.created)
            .is_some_and(|age| age < SELECTION_LIFETIME)
            || !selected
                .operation
                .as_ref()
                .is_some_and(|operation| Arc::ptr_eq(&operation.active, &attempt.active))
        {
            return Err("The checkout selection expired; inspect again".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
