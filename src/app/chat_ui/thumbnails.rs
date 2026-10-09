//! Small immutable previews for the current composer's image attachments.
use super::*;

const RECENT: usize = 8;

struct Entry {
    source: Arc<PathBuf>,
    loaded: Option<images::Loaded>,
}
#[derive(Default)]
pub(super) struct Cache(HashMap<PathBuf, Entry>);
impl Cache {
    pub(super) fn sync(&mut self, attachments: &[Attachment]) {
        let paths: Vec<_> = attachments
            .iter()
            .rev()
            .filter(|attachment| {
                attachment
                    .path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| {
                        chat::IMAGE_EXTENSIONS
                            .iter()
                            .any(|known| extension.eq_ignore_ascii_case(known))
                    })
            })
            .take(RECENT)
            .map(|attachment| &attachment.path)
            .collect();
        self.0.retain(|path, _| paths.contains(&path));
        for path in paths {
            self.0.entry(path.clone()).or_insert_with(|| Entry {
                source: Arc::new(path.clone()),
                loaded: None,
            });
        }
    }
    pub(super) fn clear(&mut self) {
        self.0.clear();
    }
    pub(super) fn handle(&self, path: &Path) -> Option<&iced::widget::image::Handle> {
        match self.0.get(path)?.loaded.as_ref()? {
            images::Loaded::Ready(handle) => Some(handle),
            _ => None,
        }
    }
    pub(super) fn complete(&mut self, path: &Arc<PathBuf>, loaded: images::Loaded) {
        if let Some(entry) = self.0.get_mut(path.as_path())
            && Arc::ptr_eq(&entry.source, path)
        {
            entry.loaded = Some(loaded);
        }
    }
    pub(super) fn retry_deferred(&mut self) {
        for (path, entry) in &mut self.0 {
            if matches!(entry.loaded, Some(images::Loaded::Deferred(_))) {
                entry.source = Arc::new(path.clone());
                entry.loaded = None;
            }
        }
    }
    pub(super) fn subscriptions(&self, target: Target) -> Vec<Subscription<Message>> {
        let mut subscriptions: Vec<_> = self
            .0
            .values()
            .filter(|entry| entry.loaded.is_none())
            .map(|entry| {
                Subscription::run_with(
                    Source {
                        target,
                        path: entry.source.clone(),
                    },
                    stream,
                )
            })
            .collect();
        // Temporary shared-budget pressure should not suppress a thumbnail for
        // its entire attachment lifetime. Visible composers retry slowly;
        // invalid/special files remain failed and never enter this timer.
        if self
            .0
            .values()
            .any(|entry| matches!(entry.loaded, Some(images::Loaded::Deferred(_))))
        {
            subscriptions.push(
                iced::time::every(Duration::from_secs(2))
                    .with(target)
                    .map(|(target, _)| action(Action::RetryThumbnails(target))),
            );
        }
        subscriptions
    }
}
#[derive(Clone)]
struct Source {
    target: Target,
    path: Arc<PathBuf>,
}
impl PartialEq for Source {
    fn eq(&self, other: &Self) -> bool {
        self.target == other.target && Arc::ptr_eq(&self.path, &other.path)
    }
}
impl Eq for Source {}
impl std::hash::Hash for Source {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::any::TypeId::of::<Self>().hash(state);
        self.target.hash(state);
        Arc::as_ptr(&self.path).hash(state);
    }
}
fn stream(source: &Source) -> Pin<Box<dyn iced::futures::Stream<Item = Message> + Send>> {
    let source = source.clone();
    Box::pin(iced::futures::stream::once(async move {
        let path = source.path.clone();
        let loaded = images::worker(move || images::thumbnail(&path)).await;
        action(Action::ThumbnailLoaded(source.target, source.path, loaded))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn attachment(path: &str) -> Attachment {
        Attachment { path: path.into() }
    }
    #[test]
    fn removed_and_readded_paths_reject_the_previous_worker() {
        let mut cache = Cache::default();
        cache.sync(&[attachment("/fixture/image.png")]);
        let old = cache
            .0
            .get(Path::new("/fixture/image.png"))
            .unwrap()
            .source
            .clone();
        cache.sync(&[]);
        cache.sync(&[attachment("/fixture/image.png")]);
        let current = cache
            .0
            .get(Path::new("/fixture/image.png"))
            .unwrap()
            .source
            .clone();
        assert!(!Arc::ptr_eq(&old, &current));
        cache.complete(
            &old,
            images::Loaded::Ready(iced::widget::image::Handle::from_rgba(
                1,
                1,
                vec![1, 2, 3, 255],
            )),
        );
        assert!(cache.handle(&current).is_none());
        assert!(cache.0.get(current.as_path()).unwrap().loaded.is_none());
        cache.complete(
            &current,
            images::Loaded::Ready(iced::widget::image::Handle::from_rgba(
                1,
                1,
                vec![4, 5, 6, 255],
            )),
        );
        assert!(cache.handle(&current).is_some());
    }
    #[test]
    fn budget_pressure_retries_with_new_identity_but_invalid_files_do_not() {
        let mut cache = Cache::default();
        cache.sync(&[
            attachment("/fixture/deferred.png"),
            attachment("/fixture/invalid.png"),
        ]);
        let deferred = cache
            .0
            .get(Path::new("/fixture/deferred.png"))
            .unwrap()
            .source
            .clone();
        let invalid = cache
            .0
            .get(Path::new("/fixture/invalid.png"))
            .unwrap()
            .source
            .clone();
        cache.complete(&deferred, images::deferred_for_test());
        cache.complete(&invalid, images::Loaded::Unavailable);
        cache.retry_deferred();
        let retry = &cache.0.get(deferred.as_path()).unwrap().source;
        assert!(!Arc::ptr_eq(&deferred, retry));
        assert!(cache.0.get(deferred.as_path()).unwrap().loaded.is_none());
        assert!(Arc::ptr_eq(
            &invalid,
            &cache.0.get(invalid.as_path()).unwrap().source
        ));
        assert!(matches!(
            cache.0.get(invalid.as_path()).unwrap().loaded,
            Some(images::Loaded::Unavailable)
        ));
        cache.complete(&deferred, images::Loaded::Unavailable);
        assert!(cache.0.get(deferred.as_path()).unwrap().loaded.is_none());
    }
    #[test]
    fn cache_keeps_only_recent_images_and_does_not_restart_failed_loads() {
        let mut cache = Cache::default();
        let mut attachments: Vec<_> = (0..20)
            .map(|index| attachment(&format!("/fixture/{index}.PNG")))
            .collect();
        attachments.push(attachment("/fixture/text.txt"));
        cache.sync(&attachments);
        assert_eq!(cache.0.len(), RECENT);
        assert!(!cache.0.contains_key(Path::new("/fixture/11.PNG")));
        assert!(cache.0.contains_key(Path::new("/fixture/12.PNG")));
        let source = cache
            .0
            .get(Path::new("/fixture/19.PNG"))
            .unwrap()
            .source
            .clone();
        cache.complete(&source, images::Loaded::Unavailable);
        cache.sync(&attachments);
        assert!(Arc::ptr_eq(
            &source,
            &cache.0.get(source.as_path()).unwrap().source
        ));
        assert!(matches!(
            cache.0.get(source.as_path()).unwrap().loaded,
            Some(images::Loaded::Unavailable)
        ));
        cache.clear();
        assert!(cache.0.is_empty());
    }
}
