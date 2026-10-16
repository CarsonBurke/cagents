//! Window custody of the producer's existing recovery authority. No new
//! reservation, reconstructed filesystem operation, or Drop I/O occurs here.
use crate::recovery_authority::RecoveryAuthority;
use std::{collections::BTreeSet, path::{Path, PathBuf}};
use cosmic::{Element, iced::Task, widget};

#[derive(Default)]
pub struct State {
    authority: Option<RecoveryAuthority>,
    scopes: BTreeSet<PathBuf>,
    unknown_scope: bool,
    pending: Option<RecoveryAuthority>,
    message: String,
}

#[derive(Debug, Clone)]
pub enum Action {
    Retry(RecoveryAuthority),
    Finished { authority: RecoveryAuthority, result: Result<(), String> },
}

impl State {
    /// Call before logical routing, including stale or missing pane returns.
    pub fn retain(&mut self, scope: Option<PathBuf>, authority: RecoveryAuthority) {
        if authority.is_resolved() { return; }
        if !self.is_pending() {
            self.authority = None;
            self.scopes.clear();
            self.unknown_scope = false;
        }
        self.authority = Some(match self.authority.take() {
            Some(existing) => existing.combine(authority),
            None => authority,
        });
        match scope {
            Some(scope) if self.scopes.len() < 32 || self.scopes.contains(&scope) => { self.scopes.insert(scope); }
            _ => self.unknown_scope = true,
        }
        self.message = format!("A file operation needs recovery. {}", self.authority.as_ref().unwrap().description());
    }
    pub fn is_pending(&self) -> bool {
        self.authority.as_ref().is_some_and(|authority| !authority.is_resolved())
    }
    pub fn blocks(&self, scope: &Path) -> bool {
        self.is_pending() && (self.unknown_scope || self.scopes.contains(scope))
    }
    pub fn update(&mut self, action: Action) -> Task<Action> {
        match action {
            Action::Retry(authority) => {
                if self.authority.as_ref() != Some(&authority) || self.pending.is_some() || authority.is_resolved() { return Task::none(); }
                self.pending = Some(authority.clone());
                cosmic::task::future(async move {
                    let worker = authority.clone();
                    let result = tokio::task::spawn_blocking(move || worker.retry()).await
                        .map_err(|error| error.to_string()).and_then(|result| result);
                    Action::Finished { authority, result }
                })
            }
            Action::Finished { authority, result } => {
                if self.pending.as_ref() != Some(&authority) { return Task::none(); }
                self.pending = None;
                if !self.is_pending() {
                    self.authority = None;
                    self.scopes.clear();
                    self.unknown_scope = false;
                    self.message.clear();
                } else {
                    self.message = match result {
                        Ok(()) => "Another retained operation still needs recovery.".into(),
                        Err(error) => format!("Recovery is still pending; original files were kept. {error}").chars().take(4096).collect(),
                    };
                }
                Task::none()
            }
        }
    }
    pub fn view(&self) -> Element<'_, Action> {
        let Some(authority) = self.authority.as_ref().filter(|_| self.is_pending()) else {
            return widget::Space::new().height(cosmic::iced::Length::Shrink).into();
        };
        widget::container(widget::row::with_children(vec![
            crate::design::body(&self.message).width(cosmic::iced::Length::Fill).into(),
            crate::ui_controls::button::secondary("Retry file recovery")
                .on_press_maybe(self.pending.is_none().then(|| Action::Retry(authority.clone()))).into(),
        ]).spacing(crate::design::space::S)).padding(crate::design::space::M)
            .width(cosmic::iced::Length::Fill).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery_authority::RecoverySlot;
    use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
    fn leaf(ready: Arc<AtomicBool>) -> RecoveryAuthority {
        RecoverySlot::reserve().unwrap().retain("original fixture".into(), move || {
            if ready.load(Ordering::Acquire) { Ok(()) } else { Err("original artifact remains".into()) }
        })
    }
    async fn finish(state: &mut State, task: Task<Action>) {
        use cosmic::iced::futures::StreamExt;
        if let Some(mut stream) = cosmic::iced::runtime::task::into_stream(task) {
            while let Some(cosmic::iced::runtime::Action::Output(action)) = stream.next().await { drop(state.update(action)); }
        }
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
