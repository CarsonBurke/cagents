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

/// The tallest a preview is drawn; its width keeps to the reading measure.
const MAX_HEIGHT: f32 = 360.0;
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
    },
    Deferred(Pressure),
}

enum State {
    /// Not loaded yet, or released; `size` keeps a released preview's room.
    Unseen {
        size: Option<(u32, u32)>,
    },
    Loading,
    Ready {
        handle: iced::widget::image::Handle,
        path: PathBuf,
        stamp: Option<Stamp>,
        checked: Instant,
    },
    Failed {
        reason: Reason,
        detail: String,
        at: Instant,
    },
    /// Waiting for image memory another preview holds.
    Waiting,
}

struct Entry {
    key: Key,
    state: State,
    /// The request whose answer is awaited; answers to any other are stale.
    attempt: u64,
    /// A check of a shown preview or failure is in flight.
    checking: bool,
    used: u64,
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
                key,
                state: State::Unseen { size: None },
                attempt: 0,
                checking: false,
                used: 0,
            },
        );
        id
    }

    fn changed(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    /// Gives back the pixels of the least recently seen previews beyond
    /// [`READY`].
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

/// A conversation's Markdown images.
#[derive(Clone, Default)]
pub(in crate::app) struct Figures(Arc<std::sync::Mutex<Store>>);

/// What one request asks a worker to do.
struct Request {
    path: PathBuf,
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
                    entry.checking = false;
                    entry.attempt = entry.attempt.wrapping_add(1);
                }
                store.changed();
            }
        }
        if !visible {
            let mut released = false;
            for entry in store.entries.values_mut() {
                released |= matches!(entry.state, State::Ready { .. });
                release(entry);
            }
            if released {
                store.changed();
            }
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
        let Kind::Local(path) = kind(&entry.key.url) else {
            return None;
        };
        let known = match &entry.state {
            State::Unseen { .. } | State::Waiting => {
                entry.state = State::Loading;
                None
            }
            State::Loading => return None,
            _ if entry.checking => return None,
            State::Ready { checked, stamp, .. }
                if !remote && stamp.is_some() && checked.elapsed() >= REVALIDATE =>
            {
                entry.checking = true;
                *stamp
            }
            State::Failed { at, .. } if at.elapsed() >= RETRY => {
                entry.checking = true;
                None
            }
            State::Ready { .. } | State::Failed { .. } => return None,
        };
        entry.attempt = entry.attempt.wrapping_add(1);
        Some(Request {
            path,
            attempt: entry.attempt,
            known,
        })
    }

    /// Records a worker's answer. Returns the memory a deferred load waits on.
    fn complete(&self, id: u64, attempt: u64, outcome: Outcome) -> Option<Pressure> {
        let mut store = self.store();
        let entry = store.entries.get_mut(&id)?;
        if entry.attempt != attempt {
            return None;
        }
        let checking = std::mem::take(&mut entry.checking);
        let changed = match outcome {
            Outcome::Unchanged => {
                if let State::Ready { checked, .. } = &mut entry.state {
                    *checked = Instant::now();
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
            Outcome::Failed { reason, detail } => {
                let same = matches!(
                    &entry.state,
                    State::Failed { reason: was, detail: had, .. } if *was == reason && *had == detail
                );
                entry.state = State::Failed {
                    reason,
                    detail,
                    at: Instant::now(),
                };
                !same
            }
            // A preview already shown stays while the newer file waits for
            // memory; it is checked again the next time it is seen.
            Outcome::Deferred(_) if checking => false,
            Outcome::Deferred(pressure) => {
                entry.state = State::Waiting;
                store.changed();
                return Some(pressure);
            }
        };
        if changed {
            store.changed();
            store.trim();
        }
        None
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
#[derive(Debug, PartialEq)]
enum Kind {
    /// A path, absolute or relative to the conversation's working directory,
    /// or a `file:` URL.
    Local(PathBuf),
    Web,
    /// Inline data or another scheme, which is not previewed.
    Other,
}

fn kind(url: &str) -> Kind {
    if Path::new(url).is_absolute() {
        return Kind::Local(PathBuf::from(url));
    }
    match Url::parse(url) {
        Ok(parsed) => match parsed.scheme() {
            "http" | "https" => Kind::Web,
            "file" => parsed
                .to_file_path()
                .map_or(Kind::Other, |path| Kind::Local(path)),
            _ => Kind::Other,
        },
        Err(_) if url.is_empty() => Kind::Other,
        Err(_) => Kind::Local(PathBuf::from(url)),
    }
}

/// The file a request names, as the reader would copy it: a relative path
/// joined to the working directory, which a child conversation does not have.
fn absolute(path: &Path, root: &Path, child: bool) -> Option<PathBuf> {
    if path.is_absolute() {
        Some(path.to_owned())
    } else {
        (!child && root.is_absolute()).then(|| root.join(path))
    }
}

/// Reads and decodes one image on an image worker.
fn read(path: PathBuf, origin: Origin, known: Option<Stamp>) -> Outcome {
    let Some(shown) = absolute(&path, &origin.root, origin.child) else {
        return Outcome::Failed {
            reason: Reason::Unavailable,
            detail: format!(
                "Cannot preview {path:?}: this conversation has no verified working directory. Ask for an absolute image path."
            ),
        };
    };
    // Another device's files are read by its daemon, which reports no
    // modification time; a local file is identified before it is read.
    let stamp = if origin.device.is_none() {
        match std::fs::metadata(&shown) {
            Ok(metadata) if metadata.is_file() => Some(Stamp {
                modified: metadata.modified().ok(),
                len: metadata.len(),
            }),
            Ok(_) => {
                return Outcome::Failed {
                    reason: Reason::NotImage,
                    detail: format!("{} is not a regular file.", shown.display()),
                };
            }
            Err(error) => {
                return Outcome::Failed {
                    reason: match error.kind() {
                        std::io::ErrorKind::NotFound => Reason::Missing,
                        _ => Reason::Unreadable,
                    },
                    detail: format!("{}: {error}", shown.display()),
                };
            }
        }
    } else {
        None
    };
    if stamp.is_some() && stamp == known {
        return Outcome::Unchanged;
    }
    let bytes = match file_bytes_with_request(&path, &origin, crate::device_service::request) {
        Ok(bytes) => bytes,
        Err(detail) => {
            return Outcome::Failed {
                reason: Reason::Unavailable,
                detail,
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
        },
        Loaded::Failed(detail) => Outcome::Failed {
            reason: Reason::Unavailable,
            detail,
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
        path,
        attempt,
        known,
    } = request;
    cosmic::task::future(async move {
        let outcome = run_worker(move || read(path, origin, known))
            .await
            .unwrap_or_else(|detail| Outcome::Failed {
                reason: Reason::Unavailable,
                detail,
            });
        action(Action::FigureLoaded(target, id, attempt, outcome))
    })
}

/// Records a worker's answer, and waits for memory for one deferred.
pub(in crate::app::chat_ui) fn complete(
    runtime: &mut Runtime,
    id: u64,
    attempt: u64,
    outcome: Outcome,
) -> Task<Message> {
    let Some(pressure) = runtime.figures.complete(id, attempt, outcome) else {
        return Task::none();
    };
    let target = runtime.target;
    cosmic::task::future(async move {
        pressure.available().await;
        action(Action::LoadFigure(target, id))
    })
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
    /// Image `url` as an answer shows it: a preview, or a compact link to its
    /// source. `alt` names it for the tooltip and assistive technology;
    /// `title` is shown beneath a preview only when the answer gives one.
    pub(in crate::app::chat_ui) fn view(
        &self,
        target: Target,
        url: &markdown::Uri,
        alt: &str,
        title: &str,
    ) -> Element<'static, Message> {
        let kind = kind(url);
        let (id, shown) = {
            let mut store = self.figures.store();
            let id = store.id(Key {
                url: url.clone(),
                child: self.child,
            });
            let shown = match &store.entries[&id].state {
                State::Ready { handle, .. } => Shown::Ready(handle.clone()),
                State::Failed { reason, detail, .. } => Shown::Failed(*reason, detail.clone()),
                State::Waiting => Shown::Waiting,
                State::Unseen { size } => Shown::Pending(*size),
                State::Loading => Shown::Pending(None),
            };
            (id, shown)
        };
        let local = matches!(kind, Kind::Local(_));
        let name: String = if alt.trim().is_empty() {
            match &kind {
                Kind::Local(path) => path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| url.to_string()),
                _ => url.to_string(),
            }
        } else {
            alt.to_owned()
        };
        let open = action(Action::Figure(target, id, Command::Open));
        let element = match (local, shown) {
            (true, Shown::Ready(handle)) => {
                let image = handle.id();
                let preview = iced::widget::image(handle.clone())
                    .width(Length::Shrink)
                    .height(Length::Shrink)
                    .content_fit(iced::ContentFit::Contain);
                let preview =
                    widget::button::custom(widget::container(preview).max_height(MAX_HEIGHT))
                        .class(crate::ui_controls::control_style(false, None))
                        .padding(0)
                        .name(name.clone())
                        .on_press(open);
                let preview = crate::interaction::context_menu_lazy(
                    crate::interaction::tooltip(preview, name),
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
                let mut rows = vec![preview];
                if !title.trim().is_empty() {
                    rows.push(design::caption(title.to_owned()).into());
                }
                // Seeing a preview again checks that its file is unchanged.
                rows.push(sighted(target, id, url));
                widget::column::with_children(rows)
                    .spacing(step::GAP_XS)
                    .into()
            }
            (true, Shown::Pending(Some((width, height)))) => widget::column::with_children(vec![
                widget::container(iced::widget::Space::new())
                    .width(Length::Fixed(width as f32))
                    .height(Length::Fixed((height as f32).min(MAX_HEIGHT)))
                    .max_width(width as f32)
                    .class(design::surface(
                        Some(design::sunken),
                        [design::RADIUS_S; 4],
                        false,
                    ))
                    .into(),
                sighted(target, id, url),
            ])
            .into(),
            (true, Shown::Pending(None)) => widget::column::with_children(vec![
                line(
                    url,
                    Icon::Image,
                    name,
                    None,
                    Some("Loading image…"),
                    target,
                    id,
                    true,
                ),
                sighted(target, id, url),
            ])
            .into(),
            (true, Shown::Waiting) => line(
                url,
                Icon::Image,
                name,
                None,
                Some("Waiting for image memory…"),
                target,
                id,
                true,
            ),
            (true, Shown::Failed(reason, detail)) => widget::column::with_children(vec![
                line(
                    url,
                    Icon::Warning,
                    name,
                    Some(detail),
                    Some(reason.label()),
                    target,
                    id,
                    true,
                ),
                // A file linked before it is written shows once it exists.
                sighted(target, id, url),
            ])
            .into(),
            (false, _) => line(
                url,
                Icon::Image,
                name,
                Some(url.to_string()),
                Some(match kind {
                    Kind::Web => "Web image, not downloaded",
                    _ => "Not previewed",
                }),
                target,
                id,
                false,
            ),
        };
        element
    }
}

enum Shown {
    Ready(iced::widget::image::Handle),
    Failed(Reason, String),
    Waiting,
    Pending(Option<(u32, u32)>),
}

/// Asks for image `id` once its place comes near the viewport. A row is
/// told of a redraw only where it meets the viewport, so the mark is a pixel
/// square rather than empty.
fn sighted(target: Target, id: u64, url: &markdown::Uri) -> Element<'static, Message> {
    iced::widget::sensor(iced::widget::Space::new().width(1).height(1))
        .key(url.to_string())
        .anticipate(ANTICIPATE)
        .on_show(move |_| action(Action::LoadFigure(target, id)))
        .into()
}

/// One compact line for an image without a preview: a glyph, a link to its
/// source named for the image, and why it shows no preview.
fn line(
    url: &markdown::Uri,
    glyph: Icon,
    name: String,
    tip: Option<String>,
    note: Option<&'static str>,
    target: Target,
    id: u64,
    local: bool,
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
        let copy = if local {
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
                let path = ready.map(|(_, path)| path).or_else(|| match kind(&url) {
                    Kind::Local(path) => absolute(&path, &root, child),
                    _ => None,
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
