use super::*;
use cagents_browser_import::{Profile, Site};
use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Debug)]
pub struct Found {
    profile: Profile,
    sites: Vec<Site>,
    selected: BTreeSet<String>,
    error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_cancels_the_import_surface_or_its_stop_confirmation() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        drop(app.update(Message::BrowserImport(Action::Open)));
        let focus = app.update(Message::BrowserImport(Action::Open));
        assert_eq!(
            super::super::tests::focused_element(app.browser_import_dialog().unwrap(), focus),
            Some(widget::Id::new("dialog.browser-import.cancel"))
        );
        let escape = || Message::KeyEvent {
            event: keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            },
            captured: false,
        };
        drop(app.update(escape()));
        assert!(!app.browser_import.open());
        drop(app.update(Message::BrowserImport(Action::Open)));
        app.browser_import.phase = Phase::Scanning;
        drop(app.update(escape()));
        assert!(app.browser_import.confirm_cancel);
        assert!(!app.browser_import.cancel.load(Ordering::Relaxed));
        drop(app.update(escape()));
        assert!(!app.browser_import.confirm_cancel);
        assert!(app.browser_import.busy());
        assert!(
            !app.browser_import.cancel.load(Ordering::Relaxed),
            "Escape from Stop import keeps the current scan"
        );
    }

    #[test]
    fn focused_import_filter_captured_escape_cancels_the_visible_import() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        drop(app.update(Message::BrowserImport(Action::Open)));
        app.browser_import.phase = Phase::Selection;
        let event = dialog_ui::tests::key_press(keyboard::key::Named::Escape);
        let (messages, captured) = dialog_ui::tests::key_control(
            app.dialog().unwrap(),
            widget::Id::new("dialog.browser-import.filter"),
            event.clone(),
        );
        assert!(captured, "the actual focused COSMIC input captures Escape");
        assert!(messages.is_empty());
        drop(app.update(Message::KeyEvent { event, captured }));
        assert!(!app.browser_import.open());

        drop(app.update(Message::BrowserImport(Action::Open)));
        app.browser_import.phase = Phase::Selection;
        app.transient_work_close = Some(window::Id::unique());
        drop(app.update(Message::KeyEvent {
            event: dialog_ui::tests::key_press(keyboard::key::Named::Escape),
            captured: true,
        }));
        assert!(
            app.browser_import.open(),
            "an obscured import must not consume the upper owner's Escape"
        );
        assert!(app.transient_work_close.is_some());
    }

    #[test]
    fn settings_import_creates_browser_before_its_modal_can_block_creation() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        assert!(app.durable.cards.is_empty());
        let _ = app.update(Message::BrowserImport(Action::Open));
        assert!(matches!(app.browser_import.phase, Phase::Intro));
        assert_eq!(app.durable.cards.len(), 1);
        assert!(matches!(app.durable.cards[0].kind, CardKind::Browser(_)));
        assert_eq!(app.workspace.focused(), Some(app.durable.cards[0].id));
    }

    #[test]
    fn dismissed_import_can_reopen_without_duplicating_a_loading_browser() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        drop(app.update(Message::BrowserImport(Action::Open)));
        let card = app.durable.cards[0].id;
        drop(app.update(Message::BrowserImport(Action::Cancel)));
        assert!(!app.browser_import.open());
        assert!(!app.modal_is_open());
        drop(app.update(Message::BrowserImport(Action::Open)));
        assert!(app.browser_import.open());
        assert!(app.modal_is_open());
        assert!(app.browser_import_dialog().is_some());
        assert_eq!(app.durable.cards.len(), 1);
        assert_eq!(app.durable.cards[0].id, card);
    }
}

pub struct Transfer {
    cookies: Mutex<Option<Vec<cagents_browser_import::Cookie>>>,
    profiles: usize,
    skipped: usize,
    errors: Vec<String>,
}

impl std::fmt::Debug for Transfer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserImportTransfer")
            .field("profiles", &self.profiles)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub enum Action {
    Open,
    Consent,
    Scanned(u64, Result<Vec<Found>, String>),
    Site(usize, String, bool),
    Profile(usize, bool),
    Filter(String),
    Page(usize),
    Import,
    Extracted(u64, Arc<Transfer>),
    Finished(u64, Result<crate::browser::ImportReport, String>),
    Cancel,
    KeepWorking,
    ConfirmCancel,
}

#[derive(Default)]
pub(super) struct State {
    pub(super) offered: bool,
    phase: Phase,
    generation: u64,
    cancel: Arc<AtomicBool>,
    confirm_cancel: bool,
    found: Vec<Found>,
    profiles: usize,
    skipped: usize,
    errors: Vec<String>,
    query: String,
    page: usize,
    rows: Vec<SelectionRow>,
}

const PAGE_ROWS: usize = 60;

#[derive(Clone, Copy)]
enum SelectionRow {
    Profile(usize),
    Site(usize, usize),
    Error(usize),
}

fn selection_rows(found: &[Found], query: &str) -> Vec<SelectionRow> {
    let query = query.to_lowercase();
    let mut rows = Vec::new();
    for (index, profile) in found.iter().enumerate() {
        let matches_profile = profile.profile.name.to_lowercase().contains(&query)
            || profile
                .profile
                .browser
                .label()
                .to_lowercase()
                .contains(&query);
        let start = rows.len();
        rows.push(SelectionRow::Profile(index));
        if profile.error.is_some() && (query.is_empty() || matches_profile) {
            rows.push(SelectionRow::Error(index));
        }
        for (site, entry) in profile.sites.iter().enumerate() {
            if matches_profile || entry.host.contains(&query) {
                rows.push(SelectionRow::Site(index, site));
            }
        }
        if rows.len() == start + 1 && !matches_profile {
            rows.pop();
        }
    }
    rows
}

#[derive(Default)]
enum Phase {
    #[default]
    Closed,
    Intro,
    Scanning,
    Selection,
    Extracting,
    Importing,
    Finished(String),
}

impl State {
    pub(super) fn open(&self) -> bool {
        !matches!(self.phase, Phase::Closed)
    }
    pub(super) fn escape_action(&self) -> Action {
        if self.confirm_cancel {
            Action::KeepWorking
        } else {
            Action::Cancel
        }
    }
    fn busy(&self) -> bool {
        matches!(
            self.phase,
            Phase::Scanning | Phase::Extracting | Phase::Importing
        )
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

fn message(action: Action) -> Message {
    Message::BrowserImport(action)
}

impl Cagents {
    pub(super) fn update_browser_import(&mut self, action: Action) -> Task<Message> {
        let focus_cancel = matches!(&action, Action::Open | Action::Cancel | Action::KeepWorking);
        match action {
            Action::Open => {
                if self.browser_import.busy() {
                    return Task::none();
                }
                self.browser_import.offered = true;
                self.browser_import.phase = Phase::Intro;
                self.browser_import.confirm_cancel = false;
                self.browser_import.errors.clear();
                if !self.durable.cards.iter().any(|card| {
                    card.existence.is_open() && matches!(card.kind, CardKind::Browser(_))
                }) {
                    // Create the card in this authorized update. Queuing a
                    // public OpenBrowser message after mounting the intro
                    // would be rejected by the modal input fence.
                    return self.add_browser(Url::parse("about:blank").unwrap()).chain(
                        iced::widget::operation::focus(widget::Id::new(
                            "dialog.browser-import.cancel",
                        ))
                        .map(cosmic::Action::App),
                    );
                }
            }
            Action::Consent => {
                if !matches!(
                    self.browser_import.phase,
                    Phase::Intro | Phase::Selection | Phase::Finished(_)
                ) {
                    return Task::none();
                }
                self.browser_import.generation += 1;
                self.browser_import.cancel = Arc::new(AtomicBool::new(false));
                self.browser_import.phase = Phase::Scanning;
                self.browser_import.errors.clear();
                let generation = self.browser_import.generation;
                let cancel = self.browser_import.cancel.clone();
                return cosmic::task::future(async move {
                    let found = tokio::task::spawn_blocking(move || {
                        let Some(paths) = directories::BaseDirs::new() else {
                            return Err(
                                "The local browser profile directories are unavailable.".into()
                            );
                        };
                        Ok(
                            cagents_browser_import::discover(paths.home_dir(), paths.config_dir())
                                .into_iter()
                                .map(|profile| {
                                    match cagents_browser_import::scan(&profile, &cancel) {
                                        Ok(sites) => Found {
                                            selected: sites
                                                .iter()
                                                .map(|site| site.host.clone())
                                                .collect(),
                                            profile,
                                            sites,
                                            error: None,
                                        },
                                        Err(error) => Found {
                                            profile,
                                            sites: Vec::new(),
                                            selected: BTreeSet::new(),
                                            error: Some(error.to_string()),
                                        },
                                    }
                                })
                                .collect(),
                        )
                    })
                    .await
                    .unwrap_or_else(|_| {
                        Err("The local profile scan stopped. Retry the scan.".into())
                    });
                    message(Action::Scanned(generation, found))
                });
            }
            Action::Scanned(generation, found) if generation == self.browser_import.generation => {
                self.browser_import.confirm_cancel = false;
                if self.browser_import.cancel.load(Ordering::Relaxed) {
                    self.browser_import.phase =
                        Phase::Finished("Scan cancelled. No cookies were imported.".into());
                } else {
                    match found {
                        Ok(found) => {
                            self.browser_import.rows = selection_rows(&found, "");
                            self.browser_import.query.clear();
                            self.browser_import.page = 0;
                            self.browser_import.found = found;
                            self.browser_import.phase = Phase::Selection;
                        }
                        Err(error) => self.browser_import.phase = Phase::Finished(error),
                    }
                }
            }
            Action::Site(index, host, selected)
                if matches!(self.browser_import.phase, Phase::Selection) =>
            {
                if let Some(found) = self.browser_import.found.get_mut(index)
                    && found.sites.iter().any(|site| site.host == host)
                {
                    if selected {
                        found.selected.insert(host);
                    } else {
                        found.selected.remove(&host);
                    }
                }
            }
            Action::Profile(index, selected)
                if matches!(self.browser_import.phase, Phase::Selection) =>
            {
                if let Some(found) = self.browser_import.found.get_mut(index) {
                    found.selected = if selected {
                        found.sites.iter().map(|site| site.host.clone()).collect()
                    } else {
                        BTreeSet::new()
                    };
                }
            }
            Action::Filter(query) if matches!(self.browser_import.phase, Phase::Selection) => {
                self.browser_import.rows = selection_rows(&self.browser_import.found, &query);
                self.browser_import.query = query;
                self.browser_import.page = 0;
            }
            Action::Page(page) if matches!(self.browser_import.phase, Phase::Selection) => {
                self.browser_import.page =
                    page.min(self.browser_import.rows.len().saturating_sub(1) / PAGE_ROWS);
            }
            Action::Import if matches!(self.browser_import.phase, Phase::Selection) => {
                let selected = self
                    .browser_import
                    .found
                    .iter()
                    .filter(|found| !found.selected.is_empty())
                    .cloned()
                    .collect::<Vec<_>>();
                if selected.is_empty() {
                    self.browser_import.phase =
                        Phase::Finished("No sites selected. No cookies were imported.".into());
                    return Task::none();
                }
                self.browser_import.phase = Phase::Extracting;
                let generation = self.browser_import.generation;
                let cancel = self.browser_import.cancel.clone();
                return cosmic::task::future(async move {
                    let extracted = tokio::task::spawn_blocking(move || {
                        let mut keys = cagents_browser_import::DesktopKeys::default();
                        let mut cookies = Vec::new();
                        let mut seen = BTreeSet::new();
                        let mut bytes = 0usize;
                        let mut profiles = 0;
                        let mut skipped = 0;
                        let mut errors = Vec::new();
                        let now = (unix_millis() / 1000) as i64;
                        for found in selected {
                            if cancel.load(Ordering::Relaxed) {
                                break;
                            }
                            match cagents_browser_import::extract(
                                &found.profile,
                                &found.selected,
                                &mut keys,
                                &cancel,
                                now,
                            ) {
                                Ok(batch) => {
                                    profiles += 1;
                                    skipped += batch.skipped;
                                    for cookie in batch.cookies {
                                        // The jar replaces by name/domain/path,
                                        // including when host-only differs.
                                        // Keep the first profile's scope too.
                                        if seen.insert((
                                            cookie.host.clone(),
                                            cookie.path.clone(),
                                            cookie.name.clone(),
                                        )) {
                                            bytes = bytes.saturating_add(cookie.value.len() + cookie.name.len() + cookie.path.len());
                                            if bytes > 64 * 1024 * 1024 || cookies.len() >= 80_000 {
                                                errors.push("The selected profiles exceed the import size limit. Import fewer profiles at a time.".into());
                                                return Transfer {cookies: Mutex::new(None), profiles: 0, skipped:0, errors};
                                            }
                                            cookies.push(cookie);
                                        }
                                    }
                                }
                                Err(error) => errors.push(format!(
                                    "{} / {}: {error}",
                                    found.profile.browser.label(),
                                    found.profile.name
                                )),
                            }
                        }
                        Transfer {
                            cookies: Mutex::new(Some(cookies)),
                            profiles,
                            skipped,
                            errors,
                        }
                    })
                    .await
                    .unwrap_or_else(|_| Transfer {
                        cookies: Mutex::new(None),
                        profiles: 0,
                        skipped: 0,
                        errors: vec![
                            "The local extraction worker stopped. Retry the import.".into(),
                        ],
                    });
                    message(Action::Extracted(generation, Arc::new(extracted)))
                });
            }
            Action::Extracted(generation, transfer)
                if generation == self.browser_import.generation =>
            {
                self.browser_import.profiles = transfer.profiles;
                self.browser_import.skipped = transfer.skipped;
                self.browser_import.errors = transfer.errors.clone();
                let cookies = transfer
                    .cookies
                    .lock()
                    .ok()
                    .and_then(|mut cookies| cookies.take())
                    .unwrap_or_default();
                if self.browser_import.cancel.load(Ordering::Relaxed) {
                    self.browser_import.confirm_cancel = false;
                    self.browser_import.phase =
                        Phase::Finished("Import cancelled. No cookies were written.".into());
                    return Task::none();
                }
                let (sender, result) = crossbeam_channel::bounded(1);
                let request = crate::browser::ImportRequest {
                    cookies,
                    cancel: self.browser_import.cancel.clone(),
                    result: sender,
                };
                let Some(CardRuntime::Browser(browser)) = self
                    .cards
                    .values()
                    .find(|card| matches!(card, CardRuntime::Browser(_)))
                else {
                    self.browser_import.phase =
                        Phase::Finished("Open an in-app browser tab and retry.".into());
                    return Task::none();
                };
                if browser.handle.import_cookies(request).is_err() {
                    self.browser_import.phase = Phase::Finished(
                        "The browser is unavailable. Reopen its tab and retry.".into(),
                    );
                    return Task::none();
                }
                self.browser_import.phase = Phase::Importing;
                return cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        result.recv().unwrap_or_else(|_| {
                            Err("The browser stopped before import completed.".into())
                        })
                    })
                    .await
                    .unwrap_or_else(|_| Err("The browser import worker stopped.".into()));
                    message(Action::Finished(generation, result))
                });
            }
            Action::Finished(generation, result)
                if generation == self.browser_import.generation =>
            {
                self.browser_import.confirm_cancel = false;
                self.browser_import.phase = Phase::Finished(match result {
                    Ok(report) => format!(
                        "{}Imported {} cookies for {} sites from {} profiles. {} unsupported, partitioned, or expired cookies were skipped. Reload a tab to use its imported login.",
                        if report.cancelled {
                            "Stopped early. "
                        } else {
                            ""
                        },
                        report.cookies,
                        report.sites,
                        self.browser_import.profiles,
                        report.skipped + self.browser_import.skipped
                    ),
                    Err(error) => error,
                });
            }
            Action::Cancel => {
                if self.browser_import.busy() {
                    self.browser_import.confirm_cancel = true;
                } else {
                    self.browser_import.phase = Phase::Closed;
                    self.browser_import.found.clear();
                }
            }
            Action::KeepWorking => self.browser_import.confirm_cancel = false,
            Action::ConfirmCancel => {
                self.browser_import.cancel.store(true, Ordering::Relaxed);
                self.browser_import.confirm_cancel = false;
            }
            _ => {}
        }
        if focus_cancel && self.browser_import.open() {
            iced::widget::operation::focus(widget::Id::new("dialog.browser-import.cancel"))
                .map(cosmic::Action::App)
        } else {
            Task::none()
        }
    }

    // Match the owners drawn above this surface in App::dialog. Captured
    // Escape from its focused filter still belongs to this visible dialog.
    pub(super) fn browser_import_on_top(&self) -> bool {
        self.browser_import.open()
            && !self.automation_approval_open()
            && self.transient_work_close.is_none()
            && !self.automation_ui.draft_dialog_open()
            && !self.device_ui.open
            && !self
                .chat_ui
                .image_viewer
                .as_ref()
                .is_some_and(chat_ui::images::Gallery::is_modal)
            && self.terminal_paste.is_none()
    }

    pub(super) fn browser_import_dialog(&self) -> Option<Element<'_, Message>> {
        let state = &self.browser_import;
        if !state.open() {
            return None;
        }
        let mut content: Vec<Element<'_, Message>> =
            vec![crate::design::title("Import browser logins").into()];
        let button = |label: &'static str, action| {
            let cancel = matches!(action, Action::Cancel | Action::KeepWorking);
            let button = if matches!(action, Action::ConfirmCancel) {
                crate::ui_controls::button::destructive(label)
            } else {
                crate::ui_controls::button::secondary(label)
            };
            let button = button.on_press(message(action));
            if cancel {
                button
                    .id(widget::Id::new("dialog.browser-import.cancel"))
                    .into()
            } else {
                button.into()
            }
        };
        if state.confirm_cancel {
            content.push(notice_body("Stop this import? A scan may be waiting for the desktop keyring. Cookies already written into the in-app browser remain there."));
            content.push(super::dialog_ui::confirmation_actions(
                button("Keep going", Action::KeepWorking),
                button("Stop import", Action::ConfirmCancel),
            ));
        } else {
            match &state.phase {
                Phase::Intro => {
                    content.push(notice_body("Read cookie databases from Chrome, Chromium, Brave, Firefox, and Zen profiles on this machine? You choose the sites before importing. Encrypted Chrome-family cookies may ask to unlock your desktop keyring."));
                    content.push(notice_body("Cookie values stay in this app's browser and are never sent to its terminal service. Source profiles are read-only. Imported logins apply to all in-app tabs for this app session; some sites may still require signing in."));
                    content.push(
                        widget::row::with_children(vec![
                            button("Skip", Action::Cancel),
                            button("Scan local profiles", Action::Consent),
                        ])
                        .spacing(space::S)
                        .into(),
                    );
                }
                Phase::Scanning | Phase::Extracting | Phase::Importing => {
                    content.push(notice_body(if state.cancel.load(Ordering::Relaxed) {
                        "Stopping… Close a pending keyring prompt to finish cancelling."
                    } else {
                        match state.phase {
                            Phase::Scanning => "Reading local profile names and sites…",
                            Phase::Extracting => {
                                "Reading selected cookies. Your desktop keyring may ask for access…"
                            }
                            _ => "Importing selected sites into the in-app browser…",
                        }
                    }));
                    content.push(button("Cancel", Action::Cancel));
                }
                Phase::Selection => {
                    if state.found.is_empty() {
                        content.push(notice_body(
                            "No supported profiles with cookie databases were found.",
                        ));
                    }
                    content.push(notice_body("Choose sites to import. If profiles overlap, the first selected profile wins. Only SameSite=None cookies are supported; restricted, container, and partitioned cookies are skipped. Some logins may still need signing in."));
                    content.push(
                        crate::ui_controls::field::search("Filter sites or profiles", &state.query)
                            .id(widget::Id::new("dialog.browser-import.filter"))
                            .on_input(|query| message(Action::Filter(query)))
                            .into(),
                    );
                    let mut rows: Vec<Element<'_, Message>> = Vec::new();
                    for row in state
                        .rows
                        .iter()
                        .skip(state.page * PAGE_ROWS)
                        .take(PAGE_ROWS)
                    {
                        match *row {
                            SelectionRow::Profile(index) => {
                                let found = &state.found[index];
                                rows.push(
                                    crate::ui_controls::field::checkbox(
                                        !found.sites.is_empty()
                                            && found.selected.len() == found.sites.len(),
                                    )
                                    .label(format!(
                                        "{} / {} · {} selected",
                                        found.profile.browser.label(),
                                        found.profile.name,
                                        found.selected.len()
                                    ))
                                    .on_toggle(move |selected| {
                                        message(Action::Profile(index, selected))
                                    })
                                    .into(),
                                );
                            }
                            SelectionRow::Error(index) => {
                                if let Some(error) = &state.found[index].error {
                                    rows.push(crate::design::body(error.clone()).into());
                                }
                            }
                            SelectionRow::Site(index, site) => {
                                let found = &state.found[index];
                                let site = &found.sites[site];
                                let host = site.host.clone();
                                rows.push(
                                    widget::container(
                                        crate::ui_controls::field::checkbox(
                                            found.selected.contains(&site.host),
                                        )
                                        .label(format!("{} · {} cookies", site.host, site.cookies))
                                        .on_toggle(
                                            move |selected| {
                                                message(Action::Site(index, host.clone(), selected))
                                            },
                                        ),
                                    )
                                    .padding([0, 20])
                                    .into(),
                                );
                            }
                        }
                    }
                    let pages = state.rows.len().div_ceil(PAGE_ROWS).max(1);
                    content.push(
                        widget::row::with_children(vec![
                            crate::ui_controls::button::secondary("Previous")
                                .on_press_maybe(
                                    (state.page > 0).then(|| message(Action::Page(state.page - 1))),
                                )
                                .into(),
                            crate::design::body(format!("Page {} of {}", state.page + 1, pages))
                                .into(),
                            crate::ui_controls::button::secondary("Next")
                                .on_press_maybe(
                                    (state.page + 1 < pages)
                                        .then(|| message(Action::Page(state.page + 1))),
                                )
                                .into(),
                        ])
                        .spacing(space::S)
                        .into(),
                    );
                    content.push(
                        widget::scrollable(widget::column::with_children(rows).spacing(space::S))
                            .height(Length::Fixed(360.0))
                            .into(),
                    );
                    content.push(
                        widget::row::with_children(vec![
                            button("Cancel", Action::Cancel),
                            button("Scan again", Action::Consent),
                            button("Import selected", Action::Import),
                        ])
                        .spacing(space::S)
                        .into(),
                    );
                }
                Phase::Finished(detail) => {
                    content.push(notice_body(detail.clone()));
                    for error in &state.errors {
                        content.push(crate::design::body(error.clone()).into());
                    }
                    content.push(
                        widget::row::with_children(vec![
                            button("Done", Action::Cancel),
                            button("Retry", Action::Consent),
                        ])
                        .spacing(space::S)
                        .into(),
                    );
                }
                Phase::Closed => {}
            }
        }
        Some(dialog_ui::overlay(
            dialog_ui::surface(
                widget::column::with_children(content).spacing(space::L),
                620.0,
            ),
            Some(message(Action::Cancel)),
            1.0,
        ))
    }
}
