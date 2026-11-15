//! A definitive seed refusal waits for an explicit offer, not another snapshot.
use super::*;

pub(in crate::app) const RETRY_ID: &str = "shared-views.retry-seeds";

impl Cagents {
    pub(in crate::app) fn has_refused_view_seeds(&self) -> bool {
        !self.shared_views.seed_failures.is_empty()
    }

    pub(in crate::app) fn can_retry_view_seeds(&self) -> bool {
        self.has_refused_view_seeds()
            && !self
                .shared_views
                .seed_failures
                .keys()
                .any(|scope| self.shared_views.seeding.contains(scope))
    }

    pub(in crate::app) fn forget_view_seed(&mut self, scope: &Scope) {
        self.shared_views.seed_failures.remove(scope);
        self.shared_views.seeding.remove(scope);
        self.shared_views.seed_attempts.remove(scope);
    }

    pub(in crate::app) fn forget_project_view_seeds(&mut self, repository: &Path) {
        let belongs = |scope: &Scope| matches!(scope, Scope::Worktree { repository: owner, .. } if owner == repository);
        self.shared_views
            .seed_failures
            .retain(|scope, _| !belongs(scope));
        self.shared_views.seeding.retain(|scope| !belongs(scope));
        self.shared_views
            .seed_attempts
            .retain(|scope, _| !belongs(scope));
    }

    pub(in crate::app) fn refuse_view_seed(
        &mut self,
        scope: Scope,
        attempt: uuid::Uuid,
        error: String,
    ) -> bool {
        if self.shared_views.seed_attempts.get(&scope) != Some(&attempt) {
            return false;
        }
        self.shared_views.seed_attempts.remove(&scope);
        if !self.shared_views.seeding.remove(&scope) {
            return false;
        }
        self.shared_views.seed_failures.insert(scope, error);
        true
    }

    pub(in crate::app) fn retry_view_seeds(&mut self) {
        if !self.can_retry_view_seeds() {
            return;
        }
        // Open publication only during this synchronous explicit offer. Keep
        // the reasons until receipts settle, so in-flight retries keep a notice.
        let failed = std::mem::take(&mut self.shared_views.seed_failures);
        self.publish_view_changes();
        for (scope, error) in failed {
            self.shared_views
                .seed_failures
                .entry(scope)
                .or_insert(error);
        }
    }

    fn seed_scope_name(&self, scope: &Scope) -> String {
        match scope {
            Scope::Channel(id) => self
                .durable
                .channels
                .iter()
                .find(|channel| channel.id == *id)
                .map(|channel| {
                    super::super::channel_ui::channel_display_title(
                        channel,
                        self.sidebar_model.project(channel.project_id),
                    )
                    .into_owned()
                })
                .unwrap_or_else(|| "this channel".into()),
            Scope::Worktree { repository, .. } => self
                .sidebar_model
                .projects
                .iter()
                .find(|project| project.repository == *repository)
                .map(|project| project.name.clone())
                .unwrap_or_else(|| "this project".into()),
        }
    }

    pub(in crate::app) fn refused_view_seeds_notice(&self) -> Option<Element<'_, Message>> {
        if !self.has_refused_view_seeds() {
            return None;
        }
        let severity = NoticeSeverity::Warning;
        let mut failures = self
            .shared_views
            .seed_failures
            .iter()
            .map(|(scope, error)| (self.seed_scope_name(scope), error))
            .collect::<Vec<_>>();
        failures.sort_by(|(name, error), (other_name, other_error)| {
            name.cmp(other_name).then(error.cmp(other_error))
        });
        let mut body = Vec::new();
        for (name, error) in failures {
            body.push(notice_body(format!(
                "Panes in {name} aren't shared with your other windows"
            )));
            body.push(notice_surface_detail(severity, error.as_str()));
        }
        if self.can_retry_view_seeds() {
            body.push(notice_surface_action(
                severity,
                "Retry",
                "Offer this channel's panes to the workspace again",
                widget::Id::new(RETRY_ID),
                Message::RetrySharedViewSeeds,
            ));
        } else {
            body.push(notice_surface_detail(
                severity,
                "Retry requested; waiting for the workspace.",
            ));
        }
        Some(notice_surface(
            severity,
            widget::column::with_children(body)
                .spacing(space::XS)
                .width(Length::Fill)
                .into(),
            None,
            false,
        ))
    }
}
