//! Window custody of the producer's existing recovery authority. No new
//! reservation, reconstructed filesystem operation, or Drop I/O occurs here.
use crate::recovery_authority::RecoveryAuthority;
use cosmic::{Element, iced::Task, widget};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

pub type Shared = Arc<Mutex<State>>;

#[derive(Default)]
pub struct State {
    operations: BTreeMap<uuid::Uuid, PendingOperation>,
    authority: Option<RecoveryAuthority>,
    scopes: BTreeSet<PathBuf>,
    unknown_scope: bool,
    pending: Option<RetryReceipt>,
    message: String,
}

struct PendingOperation {
    scope: PathBuf,
    started: bool,
    interrupted: bool,
    // An unknown worker retains the SAME original read root, not a pathname
    // recapture. This clone adds no descriptor and grants no cleanup action.
    _root: Arc<crate::fs_safety::Root>,
}

/// The window installs this plain custody record before returning a lazy Task.
/// The actual blocking worker keeps it through typed result retention. Drop
/// only records interruption; it cannot discard an unknown filesystem result.
pub struct OperationTicket {
    state: Shared,
    id: Option<uuid::Uuid>,
}
pub fn begin_operation(
    state: &Shared,
    scope: PathBuf,
    root: Arc<crate::fs_safety::Root>,
) -> Result<OperationTicket, String> {
    let mut store = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if store.operations.len() >= 32 || store.blocks(&scope) {
        return Err(
            "Resolve or finish earlier file operations before changing files in this worktree."
                .into(),
        );
    }
    let id = uuid::Uuid::new_v4();
    store.operations.insert(
        id,
        PendingOperation {
            scope,
            started: false,
            interrupted: false,
            _root: root,
        },
    );
    Ok(OperationTicket {
        state: state.clone(),
        id: Some(id),
    })
}
impl OperationTicket {
    pub(crate) fn start(&self) {
        if let Some(id) = self.id {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(operation) = state.operations.get_mut(&id) {
                operation.started = true;
            }
        }
    }
    pub(crate) fn finish(mut self, authority: Option<RecoveryAuthority>) {
        let Some(id) = self.id.take() else {
            return;
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(operation) = state.operations.remove(&id)
            && let Some(authority) = authority
        {
            state.retain(Some(operation.scope), authority);
        }
    }
}
impl Drop for OperationTicket {
    fn drop(&mut self) {
        let Some(id) = self.id else {
            return;
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(operation) = state.operations.get_mut(&id) {
            if operation.started {
                operation.interrupted = true;
            } else {
                state.operations.remove(&id);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum Action {
    Retry(RecoveryAuthority),
    Finished {
        receipt: RetryReceipt,
        result: Result<(), String>,
    },
}

#[derive(Debug, Clone)]
pub struct RetryReceipt {
    authority: RecoveryAuthority,
    done: Arc<AtomicBool>,
}
struct RetryGuard(Arc<AtomicBool>);
impl Drop for RetryGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl State {
    fn retry_running(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|receipt| !receipt.done.load(Ordering::Acquire))
    }
    /// Call before logical routing, including stale or missing pane returns.
    pub fn retain(&mut self, scope: Option<PathBuf>, authority: RecoveryAuthority) {
        if authority.is_resolved() {
            return;
        }
        if !self.authority_pending() {
            self.authority = None;
            self.scopes.clear();
            self.unknown_scope = false;
        }
        self.authority = Some(match self.authority.take() {
            Some(existing) => existing.combine(authority),
            None => authority,
        });
        match scope {
            Some(scope) if self.scopes.len() < 32 || self.scopes.contains(&scope) => {
                self.scopes.insert(scope);
            }
            _ => self.unknown_scope = true,
        }
        self.message = format!(
            "A file operation needs recovery. {}",
            self.authority.as_ref().unwrap().description()
        );
    }
    fn authority_pending(&self) -> bool {
        self.authority
            .as_ref()
            .is_some_and(|authority| !authority.is_resolved())
    }
    pub fn is_pending(&self) -> bool {
        self.authority_pending() || !self.operations.is_empty()
    }
    pub fn blocks(&self, scope: &Path) -> bool {
        (self.authority_pending() && (self.unknown_scope || self.scopes.contains(scope)))
            || self
                .operations
                .values()
                .any(|operation| operation.scope == scope)
    }
    pub fn update(&mut self, action: Action) -> Task<Action> {
        match action {
            Action::Retry(authority) => {
                if self.authority.as_ref() != Some(&authority)
                    || self.retry_running()
                    || authority.is_resolved()
                {
                    return Task::none();
                }
                let receipt = RetryReceipt {
                    authority,
                    done: Arc::new(AtomicBool::new(false)),
                };
                let guard = RetryGuard(receipt.done.clone());
                self.pending = Some(receipt.clone());
                cosmic::task::future(async move {
                    let worker = receipt.authority.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        let _guard = guard;
                        worker.retry()
                    })
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result);
                    Action::Finished { receipt, result }
                })
            }
            Action::Finished { receipt, result } => {
                if !self
                    .pending
                    .as_ref()
                    .is_some_and(|pending| Arc::ptr_eq(&pending.done, &receipt.done))
                {
                    return Task::none();
                }
                self.pending = None;
                if !self.authority_pending() {
                    self.authority = None;
                    self.scopes.clear();
                    self.unknown_scope = false;
                    self.message.clear();
                } else {
                    self.message = match result {
                        Ok(()) => "Another retained operation still needs recovery.".into(),
                        Err(error) => {
                            format!("Recovery is still pending; original files were kept. {error}")
                                .chars()
                                .take(4096)
                                .collect()
                        }
                    };
                }
                Task::none()
            }
        }
    }
    pub fn view(&self) -> Element<'static, Action> {
        let authority = self.authority.as_ref().filter(|_| self.authority_pending());
        if authority.is_none() && self.operations.is_empty() {
            return widget::Space::new()
                .height(cosmic::iced::Length::Shrink)
                .into();
        }
        let message = if authority.is_some() {
            self.message.clone()
        } else if self
            .operations
            .values()
            .any(|operation| operation.interrupted)
        {
            "A file operation did not report a result. Closing is blocked because its disk state is unknown.".into()
        } else {
            "Finishing file operations…".into()
        };
        let mut controls = vec![
            crate::design::body(message)
                .width(cosmic::iced::Length::Fill)
                .into(),
        ];
        if let Some(authority) = authority {
            controls.push(
                crate::ui_controls::button::secondary("Retry file recovery")
                    .on_press_maybe(
                        (!self.retry_running()).then(|| Action::Retry(authority.clone())),
                    )
                    .into(),
            );
        }
        widget::container(widget::row::with_children(controls).spacing(crate::design::space::S))
            .padding(crate::design::space::M)
            .width(cosmic::iced::Length::Fill)
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery_authority::RecoverySlot;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    fn leaf(ready: Arc<AtomicBool>) -> RecoveryAuthority {
        RecoverySlot::reserve()
            .unwrap()
            .retain("original fixture".into(), move || {
                if ready.load(Ordering::Acquire) {
                    Ok(())
                } else {
                    Err("original artifact remains".into())
                }
            })
    }
    async fn finish(state: &mut State, task: Task<Action>) {
        use cosmic::iced::futures::StreamExt;
        if let Some(mut stream) = cosmic::iced::runtime::task::into_stream(task) {
            while let Some(cosmic::iced::runtime::Action::Output(action)) = stream.next().await {
                drop(state.update(action));
            }
        }
    }
    #[test]
    fn window_custody_precedes_effects_and_typed_result_delivery() {
        let state = Arc::new(Mutex::new(State::default()));
        let directory = tempfile::tempdir().unwrap();
        let root = Arc::new(crate::fs_safety::Root::open(directory.path()).unwrap());
        let ticket = begin_operation(&state, "/a".into(), root.clone()).unwrap();
        assert!(state.lock().unwrap().is_pending());
        drop(ticket);
        assert!(!state.lock().unwrap().is_pending());
        let ticket = begin_operation(&state, "/a".into(), root.clone()).unwrap();
        ticket.start();
        let ready = Arc::new(AtomicBool::new(false));
        let authority = leaf(ready);
        ticket.finish(Some(authority.clone()));
        // No message was delivered. The exact original is already in custody.
        assert_eq!(state.lock().unwrap().authority.as_ref(), Some(&authority));
        assert!(state.lock().unwrap().blocks(Path::new("/a")));
        let interrupted = begin_operation(&state, "/b".into(), root.clone()).unwrap();
        interrupted.start();
        drop(interrupted);
        assert!(state.lock().unwrap().blocks(Path::new("/b")));
    }
    #[tokio::test]
    async fn recovery_retains_old_receipt_when_new_completion_arrives_during_retry() {
        let a_ready = Arc::new(AtomicBool::new(false));
        let a = leaf(a_ready.clone());
        let b_ready = Arc::new(AtomicBool::new(false));
        let b = leaf(b_ready.clone());
        let mut state = State::default();
        state.retain(Some("/a".into()), a.clone());
        let task = state.update(Action::Retry(a.clone()));
        state.retain(Some("/b".into()), b.clone());
        a_ready.store(true, Ordering::Release);
        finish(&mut state, task).await;
        assert!(a.is_resolved());
        assert!(!b.is_resolved());
        assert!(state.blocks(Path::new("/b")));
        b_ready.store(true, Ordering::Release);
        let task = state.update(Action::Retry(state.authority.clone().unwrap()));
        finish(&mut state, task).await;
        assert!(!state.is_pending());
    }
    #[tokio::test]
    async fn dropping_unpolled_retry_keeps_custody_and_allows_an_explicit_new_attempt() {
        let ready = Arc::new(AtomicBool::new(true));
        let authority = leaf(ready);
        let mut state = State::default();
        state.retain(None, authority.clone());
        let task = state.update(Action::Retry(authority.clone()));
        assert!(state.retry_running());
        drop(task);
        assert!(!state.retry_running());
        assert!(state.is_pending());
        let task = state.update(Action::Retry(authority));
        finish(&mut state, task).await;
        assert!(!state.is_pending());
    }
    #[tokio::test]
    async fn failed_retry_keeps_original_and_unknown_scope_blocks_all_until_resolution() {
        let ready = Arc::new(AtomicBool::new(false));
        let authority = leaf(ready.clone());
        let mut state = State::default();
        state.retain(None, authority.clone());
        let task = state.update(Action::Retry(authority.clone()));
        finish(&mut state, task).await;
        assert!(state.blocks(Path::new("/unrelated")));
        assert!(!authority.is_resolved());
        ready.store(true, Ordering::Release);
        let task = state.update(Action::Retry(authority.clone()));
        finish(&mut state, task).await;
        assert!(!state.is_pending());
    }
}
