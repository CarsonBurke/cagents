//! Images an answer's Markdown shows, such as `![Mockup](/var/tmp/mockup.png)`.
//!
//! A local image is drawn inline as a preview, the way an attached image is.
//! It is read and decoded on an image worker, within the same admission and
//! size limits, only once its place in the transcript comes near the
//! viewport. A decoded preview is kept per conversation and checked against
//! the file's modification time and length when it is seen again, so a
//! refresh or a streaming answer draws the pixels it already has, and a file
//! rewritten under the same name is decoded afresh.
//!
//! Web images are never fetched: that would reveal the conversation to
//! whichever host an answer names and spend the network on it. They, and
//! local images that cannot be shown, keep a compact link to their source.

use super::*;
use iced::advanced::image::Id;
use std::time::{Instant, SystemTime};

/// Decoded previews a conversation keeps. The least recently seen beyond
/// these give their pixels back and are decoded again if they are reached.
const READY: usize = 24;
/// How long a preview is trusted before seeing it again checks its file.
const REVALIDATE: Duration = Duration::from_secs(2);
/// How long an image that could not be shown waits before another look, for
/// a file an agent links before writing it.
const RETRY: Duration = Duration::from_secs(5);
/// How far outside the viewport an image starts loading.
const ANTICIPATE: f32 = 600.0;

/// An image a transcript names. A child conversation's relative paths have no
/// verified working directory, so the same text names a different image there.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    url: markdown::Uri,
    child: bool,
}

/// What identifies the contents of the file a preview was decoded from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

/// Why a local image shows as a link rather than a preview.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum Reason {
    Missing,
    Unreadable,
    NotImage,
    Unavailable,
}

impl Reason {
    fn label(self) -> &'static str {
        match self {
            Self::Missing => "File not found",
            Self::Unreadable => "Can't read the file",
            Self::NotImage => "Not a supported image",
            Self::Unavailable => "Can't preview",
        }
    }
}

/// What a worker found for one request.
#[derive(Clone)]
pub(in crate::app) enum Outcome {
    /// The file still has the stamp of the preview already shown.
    Unchanged,
    Ready {
        handle: iced::widget::image::Handle,
        path: PathBuf,
        stamp: Option<Stamp>,
    },
    Failed {
        reason: Reason,
        detail: String,
        stamp: Option<Stamp>,
    },
    Deferred(Pressure),
}

#[cfg(test)]
impl Outcome {
    pub(in crate::app) fn ready_for_test() -> Self {
        Self::Ready {
            handle: iced::widget::image::Handle::from_rgba(2, 1, vec![0; 8]),
            path: "/var/tmp/mockup.png".into(),
            stamp: None,
        }
    }
}

enum State {
    /// Not loaded yet, or released; `size` keeps a released preview's room.
    Unseen {
        size: Option<(u32, u32)>,
    },
    Loading {
        size: Option<(u32, u32)>,
    },
    Ready {
        handle: iced::widget::image::Handle,
        path: PathBuf,
        stamp: Option<Stamp>,
        checked: Instant,
    },
    Failed {
        reason: Reason,
        detail: String,
        /// The file a supported image could not be decoded from, so a look
        /// again reads it only once it changes.
        stamp: Option<Stamp>,
        at: Instant,
    },
    /// Waiting for image memory another preview holds.
    Waiting {
        size: Option<(u32, u32)>,
    },
}

struct Entry {
    key: Key,
    kind: Kind,
    state: State,
    /// The request whose answer is awaited; answers to any other are stale.
    attempt: u64,
    /// A check of a shown preview or failure is in flight.
    checking: bool,
    used: u64,
    /// Changes when the image must be asked for again wherever it is drawn,
    /// so a mark already near the viewport asks once more.
    sight: u64,
}

#[derive(Default)]
struct Store {
    entries: HashMap<u64, Entry>,
    ids: HashMap<Key, u64>,
    next: u64,
    /// The working directory and device the entries were read under.
    origin: Option<(PathBuf, Option<String>)>,
    /// Changes whenever what any image draws changes.
    version: u64,
    tick: u64,
    /// The conversation was hidden when last synced.
    hidden: bool,
}

impl Store {
    fn id(&mut self, key: Key) -> u64 {
        if let Some(id) = self.ids.get(&key) {
            return *id;
        }
        self.next += 1;
        let id = self.next;
        self.ids.insert(key.clone(), id);
        self.entries.insert(
            id,
            Entry {
                kind: kind(&key.url),
                key,
                state: State::Unseen { size: None },
                attempt: 0,
                checking: false,
                used: 0,
                sight: 0,
            },
        );
        id
    }

    fn changed(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    /// Gives back the pixels of the least recently seen previews beyond
    /// [`READY`]. A released preview keeps its room and is decoded again the
    /// next time it comes near the viewport, rather than at once: were more
    /// than [`READY`] previews in view, each would release the next.
    fn trim(&mut self) {
        let mut ready: Vec<(u64, u64)> = self
            .entries
            .iter()
            .filter(|(_, entry)| matches!(entry.state, State::Ready { .. }))
            .map(|(id, entry)| (entry.used, *id))
            .collect();
        if ready.len() <= READY {
            return;
        }
        ready.sort_unstable();
        for (_, id) in &ready[..ready.len() - READY] {
            release(self.entries.get_mut(id).expect("listed entry"));
        }
        self.changed();
    }
}

fn release(entry: &mut Entry) {
    if let State::Ready { handle, .. } = &entry.state {
        let size = match handle {
            iced::widget::image::Handle::Rgba { width, height, .. } => Some((*width, *height)),
            _ => None,
        };
        entry.state = State::Unseen { size };
        entry.checking = false;
        entry.attempt = entry.attempt.wrapping_add(1);
    }
}

/// Makes any answer in flight for `entry` stale, and asks for it again
/// wherever it is drawn.
fn unanswered(entry: &mut Entry) {
    entry.checking = false;
    entry.attempt = entry.attempt.wrapping_add(1);
    entry.sight = entry.sight.wrapping_add(1);
}

/// What an image needs once a worker's answer is recorded.
enum Next {
    Nothing,
    /// To load again once memory another preview holds is given back.
    Wait(Pressure),
    /// To be looked at again after [`RETRY`], should it still be in view.
    Recheck,
}

/// A conversation's Markdown images.
#[derive(Clone, Default)]
pub(in crate::app) struct Figures(Arc<std::sync::Mutex<Store>>);

/// What one request asks a worker to do.
struct Request {
    url: markdown::Uri,
    attempt: u64,
    known: Option<Stamp>,
}

impl Figures {
    fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        self.0.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// The handle a transcript's prepared items draw images through.
    pub(in crate::app) fn shared(&self, child: bool) -> Shared {
        Shared {
            figures: self.clone(),
            child,
        }
    }

    /// Changes whenever any image draws differently.
    pub(in crate::app) fn version(&self) -> u64 {
        self.store().version
    }

    /// Forgets images read under another working directory or device, and
    /// gives back every preview's pixels while the conversation is hidden.
    fn sync(&self, root: &Path, device: Option<&str>, visible: bool) {
        let mut store = self.store();
        let origin = (root.to_owned(), device.map(str::to_owned));
        if store.origin.as_ref() != Some(&origin) {
            let forget = store.origin.is_some();
            store.origin = Some(origin);
            if forget {
                for entry in store.entries.values_mut() {
                    entry.state = State::Unseen { size: None };
                    unanswered(entry);
                }
                store.changed();
            }
        }
        let hidden = std::mem::replace(&mut store.hidden, !visible);
        if !visible {
            // Pixels are given back, and a load in flight or waiting for
            // memory is dropped: each is asked for again once it is seen,
            // as is an image that could not be shown, whose look again
            // waits for the conversation to be shown.
            let mut changed = false;
            for entry in store.entries.values_mut() {
                match entry.state {
                    State::Ready { .. } => release(entry),
                    State::Loading { size } | State::Waiting { size } => {
                        entry.state = State::Unseen { size };
                        entry.checking = false;
                        entry.attempt = entry.attempt.wrapping_add(1);
                    }
                    State::Failed { .. } if !hidden => {}
                    State::Unseen { .. } | State::Failed { .. } => continue,
                }
                entry.sight = entry.sight.wrapping_add(1);
                changed = true;
            }
            if changed {
                store.changed();
            }
        }
    }

    /// Asks again for an image that could not be shown, wherever it is drawn
    /// near the viewport, once [`RETRY`] has passed.
    fn recheck(&self, id: u64) {
        let mut store = self.store();
        let Some(entry) = store.entries.get_mut(&id) else {
            return;
        };
        if matches!(entry.state, State::Failed { .. }) {
            entry.sight = entry.sight.wrapping_add(1);
            store.changed();
        }
    }

    /// What loading image `id` needs now, if anything, for a transcript
    /// showing a child conversation or not.
    fn begin(&self, id: u64, child: bool, remote: bool) -> Option<Request> {
        let mut store = self.store();
        store.tick += 1;
        let tick = store.tick;
        let entry = store.entries.get_mut(&id)?;
        if entry.key.child != child {
            return None;
        }
        entry.used = tick;
        if entry.kind != Kind::Local {
            return None;
        }
        let known = match &entry.state {
            State::Unseen { size } | State::Waiting { size } => {
                entry.state = State::Loading { size: *size };
                None
            }
            State::Loading { .. } => return None,
            _ if entry.checking => return None,
            State::Ready { checked, stamp, .. }
                if !remote && stamp.is_some() && checked.elapsed() >= REVALIDATE =>
            {
                entry.checking = true;
                *stamp
            }
            State::Failed { at, stamp, .. } if at.elapsed() >= RETRY => {
                entry.checking = true;
                *stamp
            }
            State::Ready { .. } | State::Failed { .. } => return None,
        };
        entry.attempt = entry.attempt.wrapping_add(1);
        Some(Request {
            url: entry.key.url.clone(),
            attempt: entry.attempt,
            known,
        })
    }

    /// Records a worker's answer, and says what the image needs next.
    fn complete(&self, id: u64, attempt: u64, outcome: Outcome) -> Next {
        let mut store = self.store();
        let Some(entry) = store.entries.get_mut(&id) else {
            return Next::Nothing;
        };
        if entry.attempt != attempt {
            return Next::Nothing;
        }
        let checking = std::mem::take(&mut entry.checking);
        let changed = match outcome {
            Outcome::Unchanged => {
                match &mut entry.state {
                    State::Ready { checked, .. } => *checked = Instant::now(),
                    State::Failed { at, .. } => *at = Instant::now(),
                    _ => {}
                }
                false
            }
            Outcome::Ready {
                handle,
                path,
                stamp,
            } => {
                entry.state = State::Ready {
                    handle,
                    path,
                    stamp,
                    checked: Instant::now(),
                };
                true
            }
            Outcome::Failed {
                reason,
                detail,
                stamp,
            } => {
                let same = matches!(
                    &entry.state,
                    State::Failed { reason: was, detail: had, .. } if *was == reason && *had == detail
                );
                entry.state = State::Failed {
                    reason,
                    detail,
                    stamp,
                    at: Instant::now(),
                };
                !same
            }
            // A preview already shown stays while the newer file waits for
            // memory; it is checked again the next time it is seen.
            Outcome::Deferred(_) if checking => false,
            Outcome::Deferred(pressure) => {
                let size = match entry.state {
                    State::Loading { size } | State::Waiting { size } => size,
                    _ => None,
                };
                entry.state = State::Waiting { size };
                store.changed();
                return Next::Wait(pressure);
            }
        };
        let failed = matches!(entry.state, State::Failed { .. });
        if changed {
            store.changed();
            store.trim();
        }
        if failed { Next::Recheck } else { Next::Nothing }
    }

    /// Starts loading image `id` of a parent conversation, as a worker would.
    #[cfg(test)]
    pub(in crate::app) fn begin_for_test(&self, id: u64) -> u64 {
        self.begin(id, false, false)
            .expect("an image to load")
            .attempt
    }

    /// The preview drawn with `image`, for the image context menu's Copy.
    pub(in crate::app) fn resolve(&self, image: Id) -> Option<clipboard::Resolved> {
        let store = self.store();
        store.entries.values().find_map(|entry| match &entry.state {
            State::Ready { handle, path, .. } if handle.id() == image => {
                Some(clipboard::Resolved {
                    item: None,
                    handle: handle.clone(),
                    path: Some(path.to_string_lossy().into_owned()),
                    url: None,
                })
            }
            _ => None,
        })
    }

    fn url(&self, id: u64, child: bool) -> Option<markdown::Uri> {
        let store = self.store();
        let entry = store.entries.get(&id)?;
        (entry.key.child == child).then(|| entry.key.url.clone())
    }

    fn ready(&self, id: u64, child: bool) -> Option<(iced::widget::image::Handle, PathBuf)> {
        let store = self.store();
        match store.entries.get(&id)? {
            Entry {
                key,
                state: State::Ready { handle, path, .. },
                ..
            } if key.child == child => Some((handle.clone(), path.clone())),
            _ => None,
        }
    }
}

/// What a prepared item draws its images through: its conversation's
/// [`Figures`], and whether it belongs to a child conversation.
#[derive(Clone, Default)]
pub(in crate::app) struct Shared {
    figures: Figures,
    child: bool,
}

/// How an image's destination is reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// A path, absolute or relative to the conversation's working directory,
    /// or a `file:` URL.
    Local,
    Web,
    /// Inline data or another scheme, which is not previewed.
    Other,
}

fn kind(url: &str) -> Kind {
    if Path::new(url).is_absolute() {
        return Kind::Local;
    }
    match Url::parse(url) {
        Ok(parsed) => match parsed.scheme() {
            "http" | "https" => Kind::Web,
            "file" if parsed.to_file_path().is_ok() => Kind::Local,
            _ => Kind::Other,
        },
        Err(_) if url.is_empty() => Kind::Other,
        Err(_) => Kind::Local,
    }
}

/// Whether image `url` names a file, which is drawn as a preview where it can
/// be read, rather than a one-line link.
pub(in crate::app::chat_ui) fn previewed(url: &str) -> bool {
    kind(url) == Kind::Local
}

/// The file image `url` names, found as a link in the same answer is, so the
/// preview, Copy image path and Open image source agree: relative to the
/// working directory, which a child conversation does not have, and
/// percent-decoded, without a query or fragment.
fn located(url: &str, root: &Path, child: bool) -> Option<PathBuf> {
    let base = if Path::new(url).is_absolute() || Url::parse(url).is_ok() {
        PathBuf::from("/.chat")
    } else if !child && root.is_absolute() {
        root.join(".chat")
    } else {
        return None;
    };
    match crate::document_link::resolve(&base, url) {
        Ok(crate::document_link::DocumentLink::Local { path, .. }) => Some(path),
        _ => None,
    }
}

/// Reads and decodes one image on an image worker.
fn read(url: &str, origin: Origin, known: Option<Stamp>) -> Outcome {
    let Some(shown) = located(url, &origin.root, origin.child) else {
        return Outcome::Failed {
            reason: Reason::Unavailable,
            detail: format!(
                "Cannot preview {}: this conversation has no verified working directory. Ask for an absolute image path.",
                clipped(url, TIP)
            ),
            stamp: None,
        };
    };
    // Another device's files are read by its daemon, which reports no
    // modification time; a local file is identified before it is read.
    let stamp = if origin.device.is_none() {
        let failed = |reason, detail| Outcome::Failed {
            reason,
            detail,
            stamp: None,
        };
        match std::fs::metadata(&shown) {
            Ok(metadata) if metadata.is_file() => Some(Stamp {
                modified: metadata.modified().ok(),
                len: metadata.len(),
            }),
            Ok(_) => {
                return failed(
                    Reason::NotImage,
                    format!("{} is not a regular file.", shown.display()),
                );
            }
            // The reason already says what went wrong; the detail names the
            // file, and why it can't be read without the OS error number.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return failed(Reason::Missing, shown.display().to_string());
            }
            Err(error) => {
                return failed(
                    Reason::Unreadable,
                    format!("{}: {}", shown.display(), error.kind()),
                );
            }
        }
    } else {
        None
    };
    // A file already shown, or already found not to be an image, is read
    // again only once it changes.
    if stamp.is_some() && stamp == known {
        return Outcome::Unchanged;
    }
    let bytes = match file_bytes_with_request(&shown, &origin, crate::device_service::request) {
        Ok(bytes) => bytes,
        Err(detail) => {
            return Outcome::Failed {
                reason: Reason::Unavailable,
                detail,
                stamp,
            };
        }
    };
    match bounded_handle(bytes, budget()) {
        Loaded::Ready(handle) => Outcome::Ready {
            handle,
            path: shown,
            stamp,
        },
        Loaded::Deferred(pressure) => Outcome::Deferred(pressure),
        Loaded::Unavailable => Outcome::Failed {
            reason: Reason::NotImage,
            detail: format!(
                "Cannot preview {}: unsupported, damaged or oversized image.",
                shown.display()
            ),
            stamp,
        },
        Loaded::Failed(detail) => Outcome::Failed {
            reason: Reason::Unavailable,
            detail,
            stamp,
        },
    }
}

/// Starts loading image `id` for the conversation `runtime` shows, if it
/// needs it.
pub(in crate::app::chat_ui) fn load(runtime: &mut Runtime, id: u64) -> Task<Message> {
    if !runtime.visible.load(Ordering::Relaxed) {
        return Task::none();
    }
    let origin = Origin::of(runtime);
    let Some(request) = runtime
        .figures
        .begin(id, origin.child, origin.device.is_some())
    else {
        return Task::none();
    };
    let target = runtime.target;
    let Request {
        url,
        attempt,
        known,
    } = request;
    cosmic::task::future(async move {
        let outcome = run_worker(move || read(&url, origin, known))
            .await
            .unwrap_or_else(|detail| Outcome::Failed {
                reason: Reason::Unavailable,
                detail,
                stamp: None,
            });
        action(Action::FigureLoaded(target, id, attempt, outcome))
    })
}

/// Records a worker's answer, and schedules what the image needs next.
pub(in crate::app::chat_ui) fn complete(
    runtime: &mut Runtime,
    id: u64,
    attempt: u64,
    outcome: Outcome,
) -> Task<Message> {
    let next = runtime.figures.complete(id, attempt, outcome);
    // A conversation hidden while its image was read keeps no pixels and
    // waits for nothing; it asks again once it is shown.
    sync(runtime);
    if !runtime.visible.load(Ordering::Relaxed) {
        return Task::none();
    }
    let target = runtime.target;
    match next {
        Next::Nothing => Task::none(),
        Next::Wait(pressure) => cosmic::task::future(async move {
            pressure.available().await;
            action(Action::LoadFigure(target, id))
        }),
        Next::Recheck => cosmic::task::future(async move {
            tokio::time::sleep(RETRY).await;
            action(Action::RecheckFigure(target, id))
        }),
    }
}

/// Looks again at an image that could not be shown, if it is still in view.
pub(in crate::app::chat_ui) fn recheck(runtime: &mut Runtime, id: u64) {
    if runtime.visible.load(Ordering::Relaxed) {
        runtime.figures.recheck(id);
    }
}

/// Keeps a conversation's images to its current working directory and
/// device, and releases their pixels while it is hidden.
pub(super) fn sync(runtime: &Runtime) {
    runtime.figures.sync(
        &runtime.session.launch.recipe.working_directory,
        crate::device::selected().map(|device| device.name.as_str()),
        runtime.visible.load(Ordering::Relaxed),
    );
}

/// What a reader can do with an image from its context menu or by clicking.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(in crate::app) enum Command {
    /// Opens the preview in the image viewer, or the source without one.
    Open,
    CopyPath,
    OpenSource,
    CopyLink,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MenuEntry {
    Figure(Command),
    CopyImage(Id),
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Menu {
    target: Target,
    id: u64,
    entry: MenuEntry,
}

impl widget::menu::Action for Menu {
    type Message = Message;

    fn message(&self) -> Message {
        match self.entry {
            MenuEntry::Figure(command) => action(Action::Figure(self.target, self.id, command)),
            MenuEntry::CopyImage(image) => action(Action::Image(
                ImageRef::Transcript(self.target, image),
                ImageCommand::Copy,
            )),
        }
    }
}

fn menu(
    target: Target,
    id: u64,
    entries: &[(&'static str, MenuEntry)],
) -> Vec<widget::menu::Tree<Message>> {
    let items = entries
        .iter()
        .map(|(label, entry)| {
            widget::menu::Item::Button(
                *label,
                None,
                Menu {
                    target,
                    id,
                    entry: *entry,
                },
            )
        })
        .collect();
    widget::menu::items(&HashMap::new(), items)
}

impl Shared {
    /// Changes whenever any image this item could show draws differently.
    pub(in crate::app::chat_ui) fn version(&self) -> u64 {
        self.figures.version()
    }

    /// Image `url` as an answer shows it: a preview, or a compact link to its
    /// source. `alt` names it for the tooltip and assistive technology; the
    /// answer draws a title beneath it.
    pub(in crate::app::chat_ui) fn view(
        &self,
        target: Target,
        url: &markdown::Uri,
        alt: &str,
    ) -> Element<'static, Message> {
        let (id, kind, sight, shown) = {
            let mut store = self.figures.store();
            let id = store.id(Key {
                url: url.clone(),
                child: self.child,
            });
            let entry = &store.entries[&id];
            let shown = match &entry.state {
                State::Ready { handle, .. } => Shown::Ready(handle.clone()),
                State::Failed { reason, detail, .. } => Shown::Failed(*reason, detail.clone()),
                State::Waiting { size } => Shown::Waiting(*size),
                State::Unseen { size } => Shown::Pending(*size),
                State::Loading { size } => Shown::Pending(*size),
            };
            (id, entry.kind, entry.sight, shown)
        };
        let name = if alt.trim().is_empty() {
            match kind {
                Kind::Local => url.rsplit('/').find(|part| !part.is_empty()).unwrap_or(url),
                _ => url,
            }
        } else {
            alt
        };
        let name = clipped(name, NAME).into_owned();
        let mark = || sighted(target, id, sight);
        let line = |glyph, tip: Option<String>, note| {
            line(url, glyph, name.clone(), tip, note, target, id, kind)
        };
        match (kind, shown) {
            (Kind::Local, Shown::Ready(handle)) => {
                let image = handle.id();
                let preview = widget::button::custom(super::preview::image(handle))
                    .class(crate::ui_controls::control_style(false, None))
                    .padding(0)
                    .name(name.clone())
                    .on_press(action(Action::Figure(target, id, Command::Open)));
                let preview = crate::interaction::context_menu_lazy(
                    crate::interaction::tooltip(preview, name.clone()),
                    move || {
                        menu(
                            target,
                            id,
                            &[
                                ("Open image", MenuEntry::Figure(Command::Open)),
                                ("Copy image", MenuEntry::CopyImage(image)),
                                ("Copy image path", MenuEntry::Figure(Command::CopyPath)),
                                ("Open image source", MenuEntry::Figure(Command::OpenSource)),
                            ],
                        )
                    },
                );
                // Seeing a preview again checks that its file is unchanged.
                widget::column::with_children(vec![preview, mark()]).into()
            }
            (Kind::Local, Shown::Pending(Some((width, height)))) => {
                widget::column::with_children(vec![super::preview::room(width, height), mark()])
                    .into()
            }
            (Kind::Local, Shown::Pending(None)) => widget::column::with_children(vec![
                line(Icon::Image, None, Some("Loading image…")),
                mark(),
            ])
            .into(),
            (Kind::Local, Shown::Waiting(Some((width, height)))) => {
                // The memory waiter starts the next read. Keep the preview's
                // room, including its sensor's pixel, without asking early.
                widget::column::with_children(vec![
                    super::preview::room(width, height),
                    iced::widget::Space::new().width(1).height(1).into(),
                ])
                .into()
            }
            (Kind::Local, Shown::Waiting(None)) => {
                line(Icon::Image, None, Some("Waiting for image memory…"))
            }
            (Kind::Local, Shown::Failed(reason, detail)) => widget::column::with_children(vec![
                line(Icon::Warning, Some(detail), Some(reason.label())),
                // A file linked before it is written shows once it exists.
                mark(),
            ])
            .into(),
            (Kind::Web | Kind::Other, _) => line(
                Icon::Image,
                Some(clipped(url, TIP).into_owned()),
                Some(if kind == Kind::Web {
                    "Web image, not downloaded"
                } else {
                    "Not previewed"
                }),
            ),
        }
    }
}

/// The most of an image's name a link shows, and of its address a tooltip.
const NAME: usize = 80;
const TIP: usize = 400;

/// `text`, cut to fewer than `limit` characters and an ellipsis if longer.
fn clipped(text: &str, limit: usize) -> std::borrow::Cow<'_, str> {
    use unicode_segmentation::UnicodeSegmentation;
    let mut starts = text.grapheme_indices(true).map(|(at, _)| at);
    match (starts.nth(limit - 1), starts.next()) {
        (Some(cut), Some(_)) => format!("{}…", &text[..cut]).into(),
        _ => text.into(),
    }
}

enum Shown {
    Ready(iced::widget::image::Handle),
    Failed(Reason, String),
    Waiting(Option<(u32, u32)>),
    Pending(Option<(u32, u32)>),
}

/// Asks for image `id` whenever its place comes near the viewport, and again
/// wherever it is drawn when `sight` changes. A row is told of a redraw only
/// where it meets the viewport, so the mark is a pixel square rather than
/// empty.
fn sighted(target: Target, id: u64, sight: u64) -> Element<'static, Message> {
    iced::widget::sensor(iced::widget::Space::new().width(1).height(1))
        .key((id, sight))
        .anticipate(ANTICIPATE)
        .on_show(move |_| action(Action::LoadFigure(target, id)))
        // Leaving the viewport is what lets coming back ask again.
        .on_hide(action(Action::FigureOutOfSight))
        .into()
}

/// One compact line for an image without a preview: a glyph, a link to its
/// source named for the image, and why it shows no preview.
#[allow(clippy::too_many_arguments)]
fn line(
    url: &markdown::Uri,
    glyph: Icon,
    name: String,
    tip: Option<String>,
    note: Option<&'static str>,
    target: Target,
    id: u64,
    kind: Kind,
) -> Element<'static, Message> {
    // The surface around the transcript binds this to the working directory
    // the image was shown from, as it does every link in an answer.
    let link: Element<'static, Message> = widget::button::link(name)
        .on_press(action(Action::OpenSharedDocument(target, url.clone())))
        .into();
    let link = match tip {
        Some(tip) => crate::interaction::tooltip(link, tip),
        None => link,
    };
    let mut row = vec![
        crate::ui_controls::passive_icon(glyph, design::COMPACT_ICON_SIZE),
        link,
    ];
    if let Some(note) = note {
        row.push(design::caption(note).into());
    }
    let row = widget::row::with_children(row)
        .spacing(step::GAP_S)
        .align_y(iced::Alignment::Center);
    crate::interaction::context_menu_lazy(row, move || {
        let copy = if kind == Kind::Local {
            ("Copy image path", MenuEntry::Figure(Command::CopyPath))
        } else {
            ("Copy image link", MenuEntry::Figure(Command::CopyLink))
        };
        menu(
            target,
            id,
            &[
                ("Open image source", MenuEntry::Figure(Command::OpenSource)),
                copy,
            ],
        )
    })
}

impl Cagents {
    /// Acts on Markdown image `id` that the conversation `target` shows.
    pub(in crate::app) fn figure_command(
        &mut self,
        target: Target,
        id: u64,
        command: Command,
    ) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        let child = runtime.selected_subagent.is_some();
        let Some(url) = runtime.figures.url(id, child) else {
            return Task::none();
        };
        let ready = runtime.figures.ready(id, child);
        let root = runtime.session.launch.recipe.working_directory.clone();
        let document = runtime.document_link_origin();
        match command {
            Command::Open => match ready {
                Some((handle, path)) => {
                    let device = crate::device::selected().map(|device| device.name.clone());
                    self.chat_ui.image_viewer =
                        Some(Gallery::file(path, device.as_deref(), handle));
                    Task::none()
                }
                None => self.figure_command(target, id, Command::OpenSource),
            },
            Command::OpenSource => {
                self.open_document_link_from(Some(target.card), document.as_deref(), &url)
            }
            Command::CopyPath => {
                let path = ready.map(|(_, path)| path).or_else(|| {
                    (kind(&url) == Kind::Local)
                        .then(|| located(&url, &root, child))
                        .flatten()
                });
                match path {
                    Some(path) => {
                        text_copy::invalidate();
                        self.status = Some(TransientNotice::success("Copied image path"));
                        iced::clipboard::write(path.to_string_lossy().into_owned())
                            .map(cosmic::Action::App)
                    }
                    None => {
                        self.status = Some(TransientNotice::warning(
                            "This image's path is relative to a working directory this conversation doesn't have.",
                        ));
                        Task::none()
                    }
                }
            }
            Command::CopyLink => {
                text_copy::invalidate();
                self.status = Some(TransientNotice::success("Copied image link"));
                iced::clipboard::write(url.to_string()).map(cosmic::Action::App)
            }
        }
    }
}

#[cfg(test)]
mod tests;
