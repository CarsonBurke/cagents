//! Where each scope's canonical view is in the model this window holds.
//!
//! A complete publication looks up the view of every scope it publishes and
//! of every transfer's source. Hashing every listed view's scope to find
//! those cost a pass over thousands of historical worktree views on every
//! save. The index is kept instead, with the scopes it was built from, and is
//! built again only when those differ from the model's. Comparing them by
//! value needs no hashing and no writer's cooperation, so a model replaced
//! or edited anywhere is never read through a stale index.

use std::collections::HashMap;

use crate::workspace_model::views::{Scope, View};

#[derive(Default)]
pub(super) struct ViewIndex {
    /// The model's view scopes, in order, as the index was built from them.
    scopes: Vec<Scope>,
    /// The first view listing each scope.
    first: HashMap<Scope, usize>,
    /// Times the index was built or extended, so tests can tell it was kept.
    #[cfg(test)]
    pub(super) builds: usize,
}

impl ViewIndex {
    /// The first of `views` listing each scope, built again only if `views`
    /// lists other scopes than the index was built from.
    pub(super) fn refresh(&mut self, views: &[View]) -> &HashMap<Scope, usize> {
        let common = self
            .scopes
            .iter()
            .zip(views)
            .take_while(|(kept, view)| **kept == view.scope)
            .count();
        if common == self.scopes.len() && common == views.len() {
            return &self.first;
        }
        tracing::debug!(target: "cagents::workspace_pipeline", stage = "view_index",
            kept = common, views = views.len(), "Indexed the model's views");
        #[cfg(test)]
        {
            self.builds += 1;
        }
        if common < self.scopes.len() {
            // A view left or changed scope; every later index may move.
            self.scopes.clear();
            self.first.clear();
        }
        // Views appended after the kept ones extend the index in place.
        for (index, view) in views.iter().enumerate().skip(self.scopes.len()) {
            self.scopes.push(view.scope.clone());
            self.first.entry(view.scope.clone()).or_insert(index);
        }
        &self.first
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ChannelId;

    fn view(scope: &Scope) -> View {
        let mut view = View::empty(ChannelId::new());
        view.scope = scope.clone();
        view
    }

    /// What a fresh scan of `views` finds first for each scope.
    fn scanned(views: &[View]) -> HashMap<Scope, usize> {
        let mut first = HashMap::new();
        for (index, view) in views.iter().enumerate() {
            first.entry(view.scope.clone()).or_insert(index);
        }
        first
    }

    #[test]
    fn the_index_follows_every_change_to_the_listed_scopes() {
        let scopes = (0..6)
            .map(|index| {
                if index % 2 == 0 {
                    Scope::Channel(ChannelId::new())
                } else {
                    Scope::Worktree {
                        repository: "/repo/.git".into(),
                        path: format!("/repo/wt-{index}").into(),
                    }
                }
            })
            .collect::<Vec<_>>();
        let mut views = scopes.iter().take(3).map(view).collect::<Vec<_>>();
        let mut index = ViewIndex::default();
        let check = |index: &mut ViewIndex, views: &[View], builds: usize| {
            assert_eq!(*index.refresh(views), scanned(views));
            assert_eq!(index.builds, builds);
        };
        check(&mut index, &views, 1);
        check(&mut index, &views, 1);
        // Panes and revisions are not indexed.
        views[1].revision += 1;
        check(&mut index, &views, 1);
        // Appended, and a duplicate keeps the first view.
        views.push(view(&scopes[3]));
        views.push(view(&scopes[0]));
        check(&mut index, &views, 2);
        assert_eq!(index.refresh(&views)[&scopes[0]], 0);
        // Removed from the front, the middle and the end.
        views.remove(0);
        check(&mut index, &views, 3);
        assert_eq!(index.refresh(&views)[&scopes[0]], 3);
        views.remove(1);
        check(&mut index, &views, 4);
        views.pop();
        check(&mut index, &views, 5);
        // A view's scope changed in place, and the model replaced.
        views[0].scope = scopes[5].clone();
        check(&mut index, &views, 6);
        let replaced = scopes.iter().rev().map(view).collect::<Vec<_>>();
        check(&mut index, &replaced, 7);
        check(&mut index, &[], 8);
        check(&mut index, &[], 8);
    }
}
