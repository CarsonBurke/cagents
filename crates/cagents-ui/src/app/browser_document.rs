//! What a browser pane shows for a page Servo cannot render.
//!
//! A PDF is fetched again as a file and read in the pane through
//! [`PdfDocument`]; any other such file is offered as a download or to the
//! application the desktop opens it with. The browser's page stays loaded
//! underneath, hidden, so its history, address and reload work as before,
//! and navigating anywhere else puts it back on screen.

use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::browser::{FetchProgress, Fetched, UnrenderableDocument, fetch_document, save_into};
use crate::pdf_viewer::{PdfAction, PdfDocument};

/// Tells one document's results from those of the document it replaced.
static GENERATIONS: AtomicU64 = AtomicU64::new(0);

pub(super) struct BrowserDocument {
    document: UnrenderableDocument,
    generation: u64,
    fetch: Fetch,
    /// What the fetch running or finished was started for.
    intent: Intent,
    viewer: Option<PdfDocument>,
    /// Where the reader saved the file, once they have.
    saved: Option<PathBuf>,
    saving: bool,
    /// Whether another application was given the cached copy, which then
    /// stays until the cache's own expiry rather than vanishing under it.
    handed_out: bool,
    /// Dropping the document stops its fetch.
    _fetching: Option<iced::task::Handle>,
}

enum Fetch {
    Idle,
    Running {
        received: u64,
        total: Option<u64>,
    },
    Done(Fetched),
    Failed(String),
    /// The reader stopped loading a PDF.
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Intent {
    View,
    Save,
    Open,
}

#[derive(Debug, Clone)]
pub enum Action {
    Progress(u64, FetchProgress),
    Viewer(u64, PdfAction),
    Saved(u64, Result<PathBuf, String>),
    Download,
    OpenExternally,
    Retry,
    Cancel,
    ShowSaved,
}

impl BrowserDocument {
    pub(super) fn url(&self) -> &Url {
        &self.document.url
    }

    /// Whether this still answers for a page the browser reported again, as
    /// after a reload: a document that failed is worth another try.
    pub(super) fn holds(&self, document: &UnrenderableDocument) -> bool {
        self.document.url == document.url
            && !matches!(self.fetch, Fetch::Failed(_) | Fetch::Stopped)
    }

    /// Renders the open PDF's pages again for a display of another density.
    pub(super) fn rescale(&mut self, card: CardId, scale_factor: f32) -> Task<Message> {
        let generation = self.generation;
        self.viewer.as_mut().map_or_else(Task::none, |viewer| {
            viewer.set_scale_factor(scale_factor).map(move |action| {
                cosmic::Action::App(Message::BrowserDocument(
                    card,
                    Action::Viewer(generation, action),
                ))
            })
        })
    }

    /// Renders the open PDF's pages again for `theme`, unless the reader
    /// chose a tone themselves.
    pub(super) fn retone(&mut self, card: CardId, theme: &cosmic::Theme) -> Task<Message> {
        let generation = self.generation;
        self.viewer.as_mut().map_or_else(Task::none, |viewer| {
            viewer
                .set_theme_tone(crate::pdf_viewer::theme_tone(theme))
                .map(move |action| {
                    cosmic::Action::App(Message::BrowserDocument(
                        card,
                        Action::Viewer(generation, action),
                    ))
                })
        })
    }
}

/// The cached copy is the application's, and nothing reads it once the
/// reader has left the page; a later fetch's pruning is only the backstop.
impl Drop for BrowserDocument {
    fn drop(&mut self) {
        let Fetch::Done(fetched) = &self.fetch else {
            return;
        };
        if fetched.original || self.handed_out {
            return;
        }
        if let Some(directory) = fetched.path.parent().map(Path::to_path_buf) {
            // Off the UI thread: a large file can take a moment to unlink.
            let _ = std::thread::Builder::new()
                .name("cagents-document-cleanup".into())
                .spawn(move || std::fs::remove_dir_all(directory));
        }
    }
}

fn cache_directory() -> Option<PathBuf> {
    crate::instance::project_dirs().map(|dirs| dirs.cache_dir().join("browser-documents"))
}

fn downloads_directory() -> Option<PathBuf> {
    let dirs = directories::UserDirs::new()?;
    Some(
        dirs.download_dir()
            .map_or_else(|| dirs.home_dir().join("Downloads"), Path::to_path_buf),
    )
}

/// A file's name for the reader, from the URL when nothing has been fetched.
fn display_name(document: &UnrenderableDocument, fetch: &Fetch) -> String {
    if let Fetch::Done(fetched) = fetch {
        return fetched.file_name();
    }
    document
        .url
        .path_segments()
        .and_then(|mut segments| segments.rfind(|segment| !segment.is_empty()))
        .map(str::to_owned)
        .or_else(|| document.url.host_str().map(str::to_owned))
        .unwrap_or_else(|| "this file".to_owned())
}

impl Cagents {
    /// The browser loaded a page it could not show.
    pub(super) fn browser_document_found(
        &mut self,
        card: CardId,
        document: UnrenderableDocument,
    ) -> Task<Message> {
        let Some(CardRuntime::Browser(browser)) = self.cards.get_mut(&card) else {
            return Task::none();
        };
        if browser
            .document
            .as_ref()
            .is_some_and(|current| current.holds(&document))
        {
            return Task::none();
        }
        let pdf = document.is_pdf();
        browser.document = Some(Box::new(BrowserDocument {
            document,
            generation: GENERATIONS.fetch_add(1, Ordering::Relaxed),
            fetch: Fetch::Idle,
            intent: Intent::View,
            viewer: None,
            saved: None,
            saving: false,
            handed_out: false,
            _fetching: None,
        }));
        if pdf {
            self.start_browser_document_fetch(card, Intent::View)
        } else {
            Task::none()
        }
    }

    /// The browser moved to `url`; a document for another page is put away.
    /// A fragment, such as a PDF link's `#page=2`, is the same document.
    pub(super) fn browser_document_left(&mut self, card: CardId, url: &Url) {
        let without_fragment = |url: &Url| {
            let mut url = url.clone();
            url.set_fragment(None);
            url
        };
        if let Some(CardRuntime::Browser(browser)) = self.cards.get_mut(&card)
            && browser
                .document
                .as_ref()
                .is_some_and(|document| without_fragment(document.url()) != without_fragment(url))
        {
            browser.document = None;
        }
    }

    fn browser_document_mut(
        &mut self,
        card: CardId,
        generation: Option<u64>,
    ) -> Option<&mut BrowserDocument> {
        match self.cards.get_mut(&card) {
            Some(CardRuntime::Browser(browser)) => {
                browser.document.as_deref_mut().filter(|document| {
                    generation.is_none_or(|generation| document.generation == generation)
                })
            }
            _ => None,
        }
    }

    fn start_browser_document_fetch(&mut self, card: CardId, intent: Intent) -> Task<Message> {
        let Some(cache) = cache_directory() else {
            if let Some(document) = self.browser_document_mut(card, None) {
                document.fetch = Fetch::Failed("There's no cache folder to download into.".into());
            }
            return Task::none();
        };
        let Some(document) = self.browser_document_mut(card, None) else {
            return Task::none();
        };
        let generation = document.generation;
        document.intent = intent;
        document.fetch = Fetch::Running {
            received: 0,
            total: None,
        };
        let (task, handle) = Task::run(
            fetch_document(document.document.clone(), cache),
            move |progress| {
                cosmic::Action::App(Message::BrowserDocument(
                    card,
                    Action::Progress(generation, progress),
                ))
            },
        )
        .abortable();
        document._fetching = Some(handle.abort_on_drop());
        task
    }

    pub(super) fn update_browser_document(
        &mut self,
        card: CardId,
        action: Action,
    ) -> Task<Message> {
        match action {
            Action::Progress(generation, progress) => {
                let tone = crate::pdf_viewer::theme_tone(&cosmic::theme::active());
                let scale =
                    effective_surface_scale(self.window_scale_factor, self.settings.ui_scale);
                let Some(document) = self.browser_document_mut(card, Some(generation)) else {
                    return Task::none();
                };
                match progress {
                    FetchProgress::Receiving { received, total } => {
                        document.fetch = Fetch::Running { received, total };
                        Task::none()
                    }
                    FetchProgress::Finished(Err(message)) => {
                        document._fetching = None;
                        document.fetch = Fetch::Failed(message);
                        Task::none()
                    }
                    FetchProgress::Finished(Ok(fetched)) => {
                        document._fetching = None;
                        let intent = document.intent;
                        let path = fetched.path.clone();
                        let pdf = fetched.is_pdf;
                        document.fetch = Fetch::Done(fetched);
                        match intent {
                            // A server that called something a PDF is not
                            // believed until the bytes agree; anything else
                            // is still offered as a file.
                            Intent::View if pdf => {
                                let (viewer, task) = PdfDocument::open(path, tone, scale);
                                document.viewer = Some(viewer);
                                task.map(move |action| {
                                    cosmic::Action::App(Message::BrowserDocument(
                                        card,
                                        Action::Viewer(generation, action),
                                    ))
                                })
                            }
                            Intent::View => Task::none(),
                            Intent::Save => self.save_browser_document(card),
                            Intent::Open => self.open_browser_document(card),
                        }
                    }
                }
            }
            Action::Viewer(generation, action) => {
                let Some(viewer) = self
                    .browser_document_mut(card, Some(generation))
                    .and_then(|document| document.viewer.as_mut())
                else {
                    return Task::none();
                };
                viewer.update(action).map(move |action| {
                    cosmic::Action::App(Message::BrowserDocument(
                        card,
                        Action::Viewer(generation, action),
                    ))
                })
            }
            Action::Download => self.save_browser_document(card),
            Action::OpenExternally => self.open_browser_document(card),
            Action::Retry => {
                let tone = crate::pdf_viewer::theme_tone(&cosmic::theme::active());
                let scale =
                    effective_surface_scale(self.window_scale_factor, self.settings.ui_scale);
                let Some(document) = self.browser_document_mut(card, None) else {
                    return Task::none();
                };
                // A PDF that was fetched but failed to open is opened again;
                // anything else is fetched again.
                if let (Some(viewer), Fetch::Done(fetched)) = (&document.viewer, &document.fetch)
                    && viewer.failure().is_some()
                {
                    let generation = document.generation;
                    let (viewer, task) = PdfDocument::open(fetched.path.clone(), tone, scale);
                    document.viewer = Some(viewer);
                    return task.map(move |action| {
                        cosmic::Action::App(Message::BrowserDocument(
                            card,
                            Action::Viewer(generation, action),
                        ))
                    });
                }
                let intent = document.intent;
                self.start_browser_document_fetch(card, intent)
            }
            Action::Cancel => {
                if let Some(document) = self.browser_document_mut(card, None)
                    && matches!(document.fetch, Fetch::Running { .. })
                {
                    document._fetching = None;
                    document.fetch = match document.intent {
                        Intent::View => Fetch::Stopped,
                        Intent::Save | Intent::Open => Fetch::Idle,
                    };
                }
                Task::none()
            }
            Action::Saved(generation, result) => {
                let Some(document) = self.browser_document_mut(card, Some(generation)) else {
                    return Task::none();
                };
                document.saving = false;
                match result {
                    Ok(path) => {
                        // A copy moved out of the cache is now the saved one,
                        // so opening it later opens what the reader kept.
                        if let Fetch::Done(fetched) = &mut document.fetch
                            && document.viewer.is_none()
                            && !fetched.original
                        {
                            fetched.path = path.clone();
                            fetched.original = true;
                        }
                        let name = path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let folder = folder_name(&path);
                        document.saved = Some(path);
                        self.status = Some(TransientNotice::success(format!(
                            "Saved {name} to {folder}"
                        )));
                    }
                    Err(message) => {
                        self.status = Some(TransientNotice::error(message));
                    }
                }
                Task::none()
            }
            Action::ShowSaved => {
                let Some(path) = self
                    .browser_document_mut(card, None)
                    .and_then(|document| document.saved.clone())
                else {
                    return Task::none();
                };
                cosmic::task::future(async move {
                    Message::FileRevealed(file_manager::reveal(&path).await)
                })
            }
        }
    }

    /// Saves the file into the reader's downloads folder, fetching it first
    /// when it has not been.
    fn save_browser_document(&mut self, card: CardId) -> Task<Message> {
        let Some(document) = self.browser_document_mut(card, None) else {
            return Task::none();
        };
        let Fetch::Done(fetched) = &document.fetch else {
            if matches!(document.fetch, Fetch::Running { .. }) {
                document.intent = Intent::Save;
                return Task::none();
            }
            return self.start_browser_document_fetch(card, Intent::Save);
        };
        if document.saving {
            return Task::none();
        }
        let Some(downloads) = downloads_directory() else {
            self.status = Some(TransientNotice::error(
                "There's no Downloads folder to save into.".to_owned(),
            ));
            return Task::none();
        };
        document.saving = true;
        let fetched = fetched.clone();
        // The viewer still reads its pages from the cached copy, and an
        // application it was handed to may still have it open.
        let keep = document.viewer.is_some() || document.handed_out;
        let generation = document.generation;
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || save_into(&fetched, &downloads, keep))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()))
                .map_err(|error| format!("The file couldn't be saved: {error}"));
            Message::BrowserDocument(card, Action::Saved(generation, result))
        })
    }

    /// Opens the file with the desktop's application for it, fetching it
    /// first when it has not been.
    fn open_browser_document(&mut self, card: CardId) -> Task<Message> {
        let Some(document) = self.browser_document_mut(card, None) else {
            return Task::none();
        };
        let path = match (&document.saved, &document.fetch) {
            (Some(saved), _) => saved.clone(),
            (None, Fetch::Done(fetched)) => fetched.path.clone(),
            (None, Fetch::Running { .. }) => {
                document.intent = Intent::Open;
                return Task::none();
            }
            (None, Fetch::Idle | Fetch::Failed(_) | Fetch::Stopped) => {
                return self.start_browser_document_fetch(card, Intent::Open);
            }
        };
        let Ok(url) = Url::from_file_path(&path) else {
            return Task::none();
        };
        document.handed_out = true;
        let opened = self.system_url_opener.open(url);
        cosmic::task::future(async move { Message::BrowserDocumentOpened(opened.await) })
    }

    /// The pane's content while it holds a document, drawn over the hidden
    /// page.
    pub(super) fn browser_document_view<'a>(
        &'a self,
        card: CardId,
        document: &'a BrowserDocument,
    ) -> Element<'a, Message> {
        let generation = document.generation;
        let content: Element<'a, Message> = match (&document.viewer, &document.fetch) {
            // The first page brings every page's size, so until it lands
            // there is no column to lay out.
            (Some(viewer), _) if viewer.page_count() == 0 && viewer.failure().is_none() => {
                crate::ui_controls::placeholder("Loading PDF")
                    .icon(Icon::TextFile)
                    .into()
            }
            (Some(viewer), _) if viewer.failure().is_none() => {
                let mut controls: Vec<Element<'a, Message>> = viewer
                    .controls()
                    .into_iter()
                    .map(|control| {
                        control.map(move |action| {
                            Message::BrowserDocument(card, Action::Viewer(generation, action))
                        })
                    })
                    .collect();
                controls.push(
                    widget::row::with_children(self.browser_document_file_actions(card, document))
                        .spacing(space::XS)
                        .align_y(Alignment::Center)
                        .into(),
                );
                // Groups wrap onto another line in a narrow pane rather than
                // pushing the file actions out of reach.
                widget::column::with_children(vec![
                    widget::container(
                        widget::row::with_children(controls)
                            .align_y(Alignment::Center)
                            .spacing(space::L)
                            .wrap()
                            .vertical_spacing(space::XXS),
                    )
                    .width(Length::Fill)
                    // One line is as tall as the browser's own toolbar.
                    .padding([
                        (BROWSER_TOOLBAR_HEIGHT - design::PRIMARY_CONTROL_SIZE) / 2.0,
                        space::S,
                    ])
                    .class(cosmic::theme::Container::Background)
                    .into(),
                    crate::design::horizontal_rule(),
                    viewer.view().map(move |action| {
                        Message::BrowserDocument(card, Action::Viewer(generation, action))
                    }),
                ])
                .height(Length::Fill)
                .into()
            }
            (Some(viewer), _) => crate::ui_controls::placeholder("This PDF can't be shown")
                .icon(Icon::Warning)
                .detail(viewer.failure().unwrap_or_default())
                .action(
                    widget::column::with_children(vec![
                        self.browser_document_fallbacks(card, document),
                        crate::ui_controls::button::secondary("Try again")
                            .on_press(Message::BrowserDocument(card, Action::Retry))
                            .into(),
                    ])
                    .spacing(space::S)
                    .align_x(Alignment::Center),
                )
                .into(),
            (None, Fetch::Running { received, total }) => {
                let progress = match total {
                    Some(total) => format!(
                        "{} of {}",
                        crate::chat::approximate_size(*received),
                        crate::chat::approximate_size(*total)
                    ),
                    None => crate::chat::approximate_size(*received),
                };
                let (title, icon) = match document.intent {
                    Intent::View => ("Loading PDF", Icon::TextFile),
                    Intent::Save | Intent::Open => ("Downloading", Icon::Download),
                };
                crate::ui_controls::placeholder(title)
                    .icon(icon)
                    .detail(progress)
                    .action(
                        crate::ui_controls::button::secondary("Cancel")
                            .on_press(Message::BrowserDocument(card, Action::Cancel)),
                    )
                    .into()
            }
            (None, Fetch::Failed(message)) => {
                let title = match document.intent {
                    Intent::View => "This PDF couldn't be loaded",
                    Intent::Save | Intent::Open => "This file couldn't be downloaded",
                };
                crate::ui_controls::placeholder(title)
                    .icon(Icon::Warning)
                    .detail(message.as_str())
                    .action(retry_actions(card))
                    .into()
            }
            (None, Fetch::Stopped) => crate::ui_controls::placeholder("Stopped loading")
                .icon(Icon::Stop)
                .detail(display_name(&document.document, &document.fetch))
                .action(retry_actions(card))
                .into(),
            (None, fetch) => {
                let name = display_name(&document.document, fetch);
                let (title, detail) = match (fetch, &document.saved) {
                    // Once saved, the panel says where the file went, so the
                    // reader knows what Open and Show in folder act on.
                    (_, Some(saved)) => (
                        format!("Saved to {}", folder_name(saved)),
                        saved
                            .file_name()
                            .map_or(name, |name| name.to_string_lossy().into_owned()),
                    ),
                    _ => (
                        "The browser can't show this file".to_owned(),
                        format!("{name} · {}", document.document.content_type),
                    ),
                };
                crate::ui_controls::placeholder(title)
                    .icon(Icon::TextFile)
                    .detail(detail)
                    .action(self.browser_document_fallbacks(card, document))
                    .into()
            }
        };
        widget::container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .class(cosmic::theme::Container::Background)
            .into()
    }

    /// Download and Open, as compact toolbar controls beside a PDF.
    fn browser_document_file_actions<'a>(
        &self,
        card: CardId,
        document: &BrowserDocument,
    ) -> Vec<Element<'a, Message>> {
        let mut actions = Vec::with_capacity(3);
        if document.saved.is_some() {
            actions.push(
                crate::ui_controls::icon_button(Icon::OpenFolder, "Show in folder")
                    .on_press(Message::BrowserDocument(card, Action::ShowSaved))
                    .into(),
            );
        } else {
            actions.push(
                crate::ui_controls::icon_button(Icon::Download, "Download")
                    .on_press_maybe(
                        (!document.saving)
                            .then_some(Message::BrowserDocument(card, Action::Download)),
                    )
                    .into(),
            );
        }
        actions.push(
            crate::ui_controls::icon_button(Icon::PopOut, "Open in default app")
                .on_press(Message::BrowserDocument(card, Action::OpenExternally))
                .into(),
        );
        actions
    }

    /// Download and Open as the way out of a panel; once the file is saved,
    /// Open and Show in folder.
    fn browser_document_fallbacks<'a>(
        &self,
        card: CardId,
        document: &BrowserDocument,
    ) -> Element<'a, Message> {
        let buttons: Vec<Element<'a, Message>> = if document.saved.is_some() {
            vec![
                crate::ui_controls::button::primary("Open")
                    .on_press(Message::BrowserDocument(card, Action::OpenExternally))
                    .into(),
                crate::ui_controls::button::secondary("Show in folder")
                    .on_press(Message::BrowserDocument(card, Action::ShowSaved))
                    .into(),
            ]
        } else {
            vec![
                crate::ui_controls::button::primary("Download")
                    .on_press_maybe(
                        (!document.saving)
                            .then_some(Message::BrowserDocument(card, Action::Download)),
                    )
                    .into(),
                crate::ui_controls::button::secondary("Open in default app")
                    .on_press(Message::BrowserDocument(card, Action::OpenExternally))
                    .into(),
            ]
        };
        widget::row::with_children(buttons).spacing(space::S).into()
    }
}

/// The name of the folder a saved file went into, such as Downloads.
fn folder_name(path: &Path) -> String {
    path.parent()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Downloads".to_owned())
}

/// The way back from a fetch that failed or was stopped: another try, or the
/// system browser, which may manage what this one couldn't.
fn retry_actions<'a>(card: CardId) -> Element<'a, Message> {
    widget::row::with_children(vec![
        crate::ui_controls::button::primary("Try again")
            .on_press(Message::BrowserDocument(card, Action::Retry))
            .into(),
        crate::ui_controls::button::secondary("Open in system browser")
            .on_press(Message::OpenBrowserInSystem(card))
            .into(),
    ])
    .spacing(space::S)
    .into()
}
