//! Compact, keyboard-accessible icon tabs and toolbar actions.

use std::borrow::Cow;

use cosmic::{
    Element,
    iced::{self, Length},
    widget,
};

use crate::interaction::tooltip;

/// Keep essential controls visible even when the desktop icon theme omits
/// these nonstandard names. Static handles retain renderer cache identity.
pub fn symbolic_icon(name: &str) -> widget::icon::Handle {
    use std::sync::LazyLock;
    static CHANGES: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round"><path d="M3 1.75h7l3 3v9.5H3zM10 2v3h3M5.5 8h5M8 5.5v5M5.5 12h5"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static TREE: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M2.5 3h4M4 5.5v7h3M4 8h3M9 8h4M9 12.5h4"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static PREVIEW: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.5"><path d="M1 8s2.5-4.5 7-4.5S15 8 15 8s-2.5 4.5-7 4.5S1 8 1 8Z"/><circle cx="8" cy="8" r="2"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static SPLIT_PANEL: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linejoin="round"><rect x="1.5" y="2.5" width="13" height="11" rx="1.5"/><path d="M10 3v10"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static PULL_REQUEST: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><circle cx="4" cy="3" r="1.5"/><circle cx="4" cy="13" r="1.5"/><circle cx="12" cy="13" r="1.5"/><path d="M4 4.5v7M12 11.5V6a3 3 0 0 0-3-3H7m2-2L7 3l2 2"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static MERGED_PULL_REQUEST: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><circle cx="4" cy="3" r="1.5"/><circle cx="4" cy="13" r="1.5"/><circle cx="12" cy="10" r="1.5"/><path d="M4 4.5v7M4 5a5 5 0 0 0 5 5h1.5"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static CLOSED_PULL_REQUEST: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><circle cx="4" cy="3" r="1.5"/><circle cx="4" cy="13" r="1.5"/><circle cx="12" cy="13" r="1.5"/><path d="M4 4.5v7M12 11.5V9M9 2l5 5M14 2l-5 5"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static CHEVRON_DOWN: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path d="m4 6 4 4 4-4" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/></svg>"#.as_slice()).symbolic(true)
    });
    static CHEVRON_UP: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path d="m4 10 4-4 4 4" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/></svg>"#.as_slice()).symbolic(true)
    });
    static CHEVRON_RIGHT: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path d="m6 4 4 4-4 4" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/></svg>"#.as_slice()).symbolic(true)
    });
    static PLUS: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path d="M4 8h8M8 4v8" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round"/></svg>"#.as_slice()).symbolic(true)
    });
    static CHANNEL: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path d="M2 2.5h12v9H7l-4 3v-3H2zM5 6h6M5 8.5h4" fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"/></svg>"#.as_slice()).symbolic(true)
    });
    static CHANNEL_ADD: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path d="M8 2.5H2v9h1v3l4-3h7V9M12 1v6M9 4h6" fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"/></svg>"#.as_slice()).symbolic(true)
    });
    static WORKTREE_ADD: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><circle cx="4" cy="3" r="1.5"/><circle cx="4" cy="13" r="1.5"/><path d="M4 4.5v7M4 9h5a3 3 0 0 0 3-3M12 1v5M9.5 3.5h5"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static ATTENTION: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><path d="M3 11.5h10l-1.5-2V6a3.5 3.5 0 0 0-7 0v3.5zM6.5 13.5a1.7 1.7 0 0 0 3 0M8 1v1"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static WORKING: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round"><circle cx="8" cy="8" r="4.5"/><circle cx="8" cy="8" r="1.5"/><path d="M8 1v2M8 13v2M1 8h2M13 8h2M3 3l1.5 1.5M11.5 11.5 13 13M3 13l1.5-1.5M11.5 4.5 13 3"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static WAITING: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><path d="M3 2.5h10a1 1 0 0 1 1 1v7a1 1 0 0 1-1 1H7l-4 3v-3H2V3.5a1 1 0 0 1 1-1ZM8 5v2.5"/><path d="M8 9.5h0"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static ADDED_FILE: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><path d="M3 1.5h6l4 4v9H3zM9 1.5v4h4M5.5 10h5M8 7.5v5"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static EDIT: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><path d="M9.5 3.5 12.5 6.5 6 13H3v-3zM8.5 4.5l3 3"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static RENAME: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><path d="M2.5 12.5h5M9.5 3.5 12.5 6.5 7 12H4v-3z"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static DUPLICATE: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><rect x="5.5" y="4.5" width="8" height="10" rx="1"/><path d="M10.5 4.5V3a1 1 0 0 0-1-1h-7a1 1 0 0 0-1 1v10a1 1 0 0 0 1 1H5"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static COPY_PATH: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><path d="M3 2.5h6l4 4v7H3zM9 2.5v4h4M5 9.5h6M5 12h4"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static COPY_RELATIVE: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><path d="M3 8.5h4.5M5.5 6.5 3 8.5l2.5 2M13 7.5h-4.5M10.5 5.5 13 7.5l-2.5 2"/></g></svg>"#.as_slice()).symbolic(true)
    });
    // A floppy disk, not the desktop theme's download-arrow tray: the save
    // action stands alone in toolbars with only a tooltip to name it.
    static SAVE: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><path d="M3 2.5h7l3 3v8H3zM6 2.5V6h5V2.5M6 13.5v-3h4v3"/></g></svg>"#.as_slice()).symbolic(true)
    });
    // A page plus an inward arrow makes the open action unambiguous at 16px.
    static OPEN_FILE: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><path d="M6 1.75h3.5l3.5 3.5v8.75H6zM9.5 1.75v3.5H13M2 8h5m-2-2 2 2-2 2"/></g></svg>"#.as_slice()).symbolic(true)
    });
    // The arrow enters the open folder rather than relying on a filled theme glyph.
    static OPEN_FOLDER: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><path d="M2 3.5h4.25l1.5 1.75H14L12.7 13.25H3.55zM2 6h12M1 8h5m-2-2 2 2-2 2"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static SIDEBAR: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linejoin="round"><rect x="1.5" y="2.5" width="13" height="11" rx="1.5"/><path d="M6 3v10"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static SETTINGS: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M12 8.25a3.75 3.75 0 1 0 0 7.5 3.75 3.75 0 0 0 0-7.5Zm9 3.75-2.25-.42a7.04 7.04 0 0 0-.72-1.74l1.29-1.88-1.94-1.94-1.88 1.29a7.04 7.04 0 0 0-1.74-.72L13.34 4h-2.68l-.42 2.25a7.04 7.04 0 0 0-1.74.72L6.62 6.02 4.68 7.96l1.29 1.88a7.04 7.04 0 0 0-.72 1.74L3 12v2.68l2.25.42c.18.61.42 1.19.72 1.74l-1.29 1.88 1.94 1.94 1.88-1.29c.55.3 1.13.54 1.74.72l.42 2.25h2.68l.42-2.25a7.04 7.04 0 0 0 1.74-.72l1.88 1.29 1.94-1.94-1.29-1.88c.3-.55.54-1.13.72-1.74l2.25-.42V12Z" fill="none" stroke="black" stroke-width="1.7" stroke-linejoin="round" stroke-linecap="round"/></svg>"#.as_slice()).symbolic(true)
    });
    static INSERT_TEXT: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path d="M3.5 3.25h9M8 3.25v9.5" fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round"/></svg>"#.as_slice()).symbolic(true)
    });
    static INFO: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round"><circle cx="8" cy="8" r="5.5"/><path d="M8 7.2v4.1M8 5.2h0"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static CLOSE: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path d="M4.75 4.75l6.5 6.5M11.25 4.75l-6.5 6.5" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round"/></svg>"#.as_slice()).symbolic(true)
    });
    static MINUS: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path d="M4 8h8" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round"/></svg>"#.as_slice()).symbolic(true)
    });
    static SEARCH: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><circle cx="7" cy="7" r="4.25" fill="none" stroke="black" stroke-width="1.4"/><path d="m10.25 10.25 3.25 3.25" fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round"/></svg>"#.as_slice()).symbolic(true)
    });
    match name {
        "edit-find-symbolic" | "system-search-symbolic" => SEARCH.clone(),
        "document-edit-symbolic" => CHANGES.clone(),
        "text-editor-symbolic" => EDIT.clone(),
        "edit-rename-symbolic" => RENAME.clone(),
        "edit-duplicate-symbolic" => DUPLICATE.clone(),
        "edit-copy-path-symbolic" => COPY_PATH.clone(),
        "edit-copy-relative-path-symbolic" => COPY_RELATIVE.clone(),
        "view-list-tree-symbolic" => TREE.clone(),
        "view-reveal-symbolic" => PREVIEW.clone(),
        "view-split-right-symbolic" => SPLIT_PANEL.clone(),
        "view-sidebar-start-symbolic" => SIDEBAR.clone(),
        "pull-request-symbolic" => PULL_REQUEST.clone(),
        "merged-pull-request-symbolic" => MERGED_PULL_REQUEST.clone(),
        "closed-pull-request-symbolic" => CLOSED_PULL_REQUEST.clone(),
        "go-down-symbolic" => CHEVRON_DOWN.clone(),
        "go-up-symbolic" => CHEVRON_UP.clone(),
        "go-next-symbolic" => CHEVRON_RIGHT.clone(),
        "list-add-symbolic" => PLUS.clone(),
        "list-remove-symbolic" => MINUS.clone(),
        "channel-symbolic" => CHANNEL.clone(),
        "channel-add-symbolic" => CHANNEL_ADD.clone(),
        "worktree-add-symbolic" => WORKTREE_ADD.clone(),
        "notifications-symbolic" => ATTENTION.clone(),
        "agent-working-symbolic" => WORKING.clone(),
        "agent-waiting-symbolic" => WAITING.clone(),
        "document-new-symbolic" => ADDED_FILE.clone(),
        "document-open-symbolic" => OPEN_FILE.clone(),
        "folder-open-symbolic" => OPEN_FOLDER.clone(),
        "document-save-symbolic" => SAVE.clone(),
        "document-save-as-symbolic" => SAVE.clone(),
        "emblem-system-symbolic" | "preferences-system-symbolic" => SETTINGS.clone(),
        "insert-text-symbolic" => INSERT_TEXT.clone(),
        "dialog-information-symbolic" => INFO.clone(),
        "window-close-symbolic" => CLOSE.clone(),
        _ => widget::icon::from_name(name).handle(),
    }
}

/// Embedded application artwork does not depend on the installed icon theme.
pub fn ceditor_icon() -> widget::icon::Handle {
    static ICON: std::sync::LazyLock<widget::icon::Handle> = std::sync::LazyLock::new(|| {
        widget::icon::from_svg_bytes(
            include_bytes!("../packaging/io.github.cagents.ceditor.svg").as_slice(),
        )
        .symbolic(false)
    });
    ICON.clone()
}

/// Provider identity is shared by terminal and native-chat tab presentations.
pub fn provider_icon(provider: crate::agent::Provider) -> widget::icon::Handle {
    use std::sync::LazyLock;
    static OMP: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(include_bytes!("icons/omp.svg").as_slice()).symbolic(false)
    });
    static CLAUDE: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(include_bytes!("icons/claude.svg").as_slice()).symbolic(false)
    });
    static CODEX: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(include_bytes!("icons/codex.svg").as_slice()).symbolic(true)
    });
    match provider {
        crate::agent::Provider::Omp => OMP.clone(),
        crate::agent::Provider::Claude => CLAUDE.clone(),
        crate::agent::Provider::Codex => CODEX.clone(),
    }
}

pub struct IconControl<'a, Message> {
    button: widget::button::Button<'a, Message>,
    label: Cow<'a, str>,
    selected: bool,
    enabled: bool,
}

/// A centered, fixed-size icon target with a quiet surface selected state.
/// Native buttons retain keyboard focus, accessibility, and enabled cursors.
pub fn icon_control<'a, Message: Clone + 'a>(
    icon: widget::icon::Handle,
    label: impl Into<Cow<'a, str>>,
    selected: bool,
    enabled: bool,
    on_press: Option<Message>,
) -> IconControl<'a, Message> {
    let label = label.into();
    let content = widget::container(widget::icon(icon).size(crate::design::ROW_ICON_SIZE))
        .center(Length::Fill);
    IconControl {
        button: widget::button::custom(content)
            .name(label.clone())
            .selected(selected && enabled)
            .class(control_style(selected && enabled, None))
            .width(crate::design::PRIMARY_CONTROL_SIZE)
            .height(crate::design::PRIMARY_CONTROL_SIZE)
            .padding(0)
            .on_press_maybe(if enabled { on_press } else { None }),
        label,
        selected: selected && enabled,
        enabled,
    }
}

impl<'a, Message: Clone + 'a> IconControl<'a, Message> {
    pub fn id(mut self, id: widget::Id) -> Self {
        self.button = self.button.id(id);
        self
    }

    pub fn icon_color(self, color: iced::Color) -> Self {
        self.icon_tone(crate::design::Tone::Fixed(color))
    }

    /// The semantic tone variant: resolved from the drawing theme, so a row
    /// builder never locks the process-global theme to name a color.
    pub fn icon_tone(mut self, tone: crate::design::Tone) -> Self {
        self.button = self.button.class(control_style(self.selected, Some(tone)));
        self
    }
}

impl<'a, Message: Clone + 'a> From<IconControl<'a, Message>> for Element<'a, Message> {
    fn from(control: IconControl<'a, Message>) -> Self {
        let IconControl {
            button,
            label,
            enabled,
            ..
        } = control;
        let button: Element<'a, Message> = if enabled {
            crate::interaction::hover(button).into()
        } else {
            button.into()
        };
        tooltip(button, label)
    }
}

/// Shared anchored popup surface. COSMIC owns placement, outside-click dismissal
/// and overlay hit testing; all application popovers share the same frame.
pub fn popover<'a, Message: Clone + 'static>(
    anchor: impl Into<Element<'a, Message>>,
    content: Option<Element<'a, Message>>,
    position: widget::popover::Position,
    on_close: Message,
) -> Element<'a, Message> {
    let mut popup = widget::popover(anchor)
        .position(position)
        .on_close(on_close);
    if let Some(content) = content {
        popup = popup.popup(widget::container(content).padding(6).class(
            cosmic::theme::Container::custom(|theme| iced::widget::container::Style {
                background: Some(iced::Background::Color(
                    theme.current_container().component.base.into(),
                )),
                border: iced::Border {
                    color: theme.current_container().component.divider.into(),
                    width: crate::design::BORDER_WIDTH,
                    radius: theme.cosmic().corner_radii.radius_s.into(),
                },
                shadow: iced::Shadow {
                    color: iced::Color::from_rgba(0.0, 0.0, 0.0, 0.24),
                    offset: iced::Vector::new(0.0, 4.0),
                    blur_radius: 16.0,
                },
                ..Default::default()
            }),
        ));
    }
    popup.into()
}

/// A full-width menu action with a stable icon column and a secondary description.
pub fn popover_item<'a, Message: Clone + 'a>(
    icon: widget::icon::Handle,
    title: &'a str,
    description: &'a str,
    on_press: Message,
) -> widget::button::Button<'a, Message> {
    widget::button::custom(
        widget::row::with_children(vec![
            widget::container(widget::icon(icon).size(20))
                .center_x(28)
                .center_y(36)
                .into(),
            widget::column::with_children(vec![
                crate::design::body(title).into(),
                crate::design::caption(description).into(),
            ])
            .spacing(2)
            .width(Length::Fill)
            .into(),
        ])
        .spacing(8)
        .align_y(iced::Alignment::Center),
    )
    .name(title)
    .class(control_style(false, None))
    .padding([6, 8])
    .width(Length::Fill)
    .on_press(on_press)
}

/// A connected mode selector using the same compact targets as toolbar actions.
/// Pass `icon_control(...).into()` children; each retains its own focus and tooltip.
pub fn segmented_controls<'a, Message: 'a>(
    controls: Vec<Element<'a, Message>>,
) -> Element<'a, Message> {
    widget::container(
        widget::row::with_children(controls)
            .spacing(0)
            .align_y(iced::Alignment::Center),
    )
    .padding(2)
    .class(cosmic::theme::Container::custom(|theme| {
        iced::widget::container::Style {
            background: Some(iced::Background::Color(
                theme.current_container().component.base.into(),
            )),
            border: iced::Border {
                color: theme.current_container().component.divider.into(),
                width: crate::design::BORDER_WIDTH,
                radius: theme.cosmic().corner_radii.radius_m.into(),
            },
            ..Default::default()
        }
    }))
    .into()
}

/// The shell's flat control surface: no border, no fill until hovered.
///
/// Exposed so a surface that composes its own row - a transcript step, a
/// navigation entry - presses the same treatment as the icon controls beside
/// it instead of reaching for a stock button class.
pub fn control_style(selected: bool, tone: Option<crate::design::Tone>) -> cosmic::theme::Button {
    let appearance = move |focused: bool, theme: &cosmic::Theme| widget::button::Style {
        background: (selected || focused).then(|| {
            iced::Background::Color(
                theme
                    .current_container()
                    .component
                    .selected_state_color()
                    .into(),
            )
        }),
        border_radius: theme.cosmic().corner_radii.radius_s.into(),
        border_width: 0.0,
        border_color: theme.current_container().component.on_disabled.into(),
        icon_color: Some(tone.map_or_else(
            || theme.current_container().component.on.into(),
            |tone| tone.resolve(theme),
        )),
        ..widget::button::Style::new()
    };
    cosmic::theme::Button::Custom {
        active: Box::new(appearance),
        disabled: Box::new(move |theme| widget::button::Style {
            // Symbolic SVG recoloring ignores alpha, so mix against the
            // surface instead of relying on `on_disabled` transparency.
            icon_color: Some(muted_icon_color(theme)),
            background: None,
            ..appearance(false, theme)
        }),
        hovered: Box::new(move |focused, theme| widget::button::Style {
            background: Some(iced::Background::Color(
                theme.current_container().component.hover.into(),
            )),
            ..appearance(focused, theme)
        }),
        pressed: Box::new(move |focused, theme| widget::button::Style {
            background: Some(iced::Background::Color(
                theme.current_container().component.pressed.into(),
            )),
            ..appearance(focused, theme)
        }),
    }
}

fn muted_icon_color(theme: &cosmic::Theme) -> iced::Color {
    let on: iced::Color = theme.current_container().component.on.into();
    let base: iced::Color = theme.current_container().component.base.into();
    iced::Color {
        r: on.r * 0.38 + base.r * 0.62,
        g: on.g * 0.38 + base.g * 0.62,
        b: on.b * 0.38 + base.b * 0.62,
        a: 1.0,
    }
}

/// A small activity glyph that redraws only while mounted, visible and focused.
/// The widget owns its clock: there is no application subscription or view rebuild.
///
/// `color` is the tone the indicator draws in. The pinned COSMIC spinner takes
/// its bar color from the active accent and has no style of its own, so the
/// tone is applied by substituting the accent for this subtree rather than
/// dropped on the floor.
pub fn activity_spinner<'a, Message: Clone + 'a>(
    size: f32,
    color: iced::Color,
) -> Element<'a, Message> {
    spinner(size, crate::design::Tone::Fixed(color))
}

/// [`activity_spinner`] in a semantic tone, resolved from the drawing theme.
///
/// Per-row indicators must use this: naming the color eagerly meant
/// `cosmic::theme::active()` - a process-global theme lock plus a clone of the
/// active `Theme` - once per visible row per frame.
pub fn activity_spinner_tone<'a, Message: Clone + 'a>(
    size: f32,
    tone: fn(&cosmic::Theme) -> iced::Color,
) -> Element<'a, Message> {
    spinner(size, crate::design::Tone::Themed(tone))
}

fn spinner<'a, Message: Clone + 'a>(size: f32, tone: crate::design::Tone) -> Element<'a, Message> {
    crate::interaction::accent_tint(
        cosmic::widget::progress_bar::indeterminate_circular().size(size),
        tone,
    )
}
