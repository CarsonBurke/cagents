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
    // A clock face with its hand swept back: the log walk, not a document.
    static HISTORY: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><path d="M2.5 8a5.5 5.5 0 1 0 1.6-3.9M2 2.5v3h3M8 5v3.2l2.2 1.3"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static INFO: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round"><circle cx="8" cy="8" r="5.5"/><path d="M8 7.2v4.1M8 5.2h0"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static POP_OUT: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><path d="M13 9v3.5a1 1 0 0 1-1 1H3.5a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1H7M10 2.5h3.5V6M13.5 2.5 8 8"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static CLOSE: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path d="M4.75 4.75l6.5 6.5M11.25 4.75l-6.5 6.5" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round"/></svg>"#.as_slice()).symbolic(true)
    });
    // Review marks: an empty box, a box with a check, and a box with a dash
    // for a folder whose files are only partly viewed. Drawn here so the
    // three read as one family whatever icon theme the desktop provides.
    static CHECKBOX: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><rect x="2.75" y="2.75" width="10.5" height="10.5" rx="2" fill="none" stroke="black" stroke-width="1.4"/></svg>"#.as_slice()).symbolic(true)
    });
    static CHECKBOX_CHECKED: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><rect x="2.75" y="2.75" width="10.5" height="10.5" rx="2" fill="none" stroke="black" stroke-width="1.4"/><path d="m5.25 8.25 1.9 1.9 3.6-4.1" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/></svg>"#.as_slice()).symbolic(true)
    });
    static CHECKBOX_MIXED: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><rect x="2.75" y="2.75" width="10.5" height="10.5" rx="2" fill="none" stroke="black" stroke-width="1.4"/><path d="M5.5 8h5" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round"/></svg>"#.as_slice()).symbolic(true)
    });
    static MINUS: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path d="M4 8h8" fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round"/></svg>"#.as_slice()).symbolic(true)
    });
    static SEARCH: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><circle cx="7" cy="7" r="4.25" fill="none" stroke="black" stroke-width="1.4"/><path d="m10.25 10.25 3.25 3.25" fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round"/></svg>"#.as_slice()).symbolic(true)
    });
    // Two edges with the content closing between them. The desktop themes
    // answer "compact" with a window-restore glyph, which is what made the
    // one control here that rewrites a conversation look like a control that
    // resizes a pane.
    static COMPRESS: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"><path d="M2.5 2.5h11M2.5 13.5h11M8 4.4V7.1M5.9 5.2 8 7.3l2.1-2.1M8 11.6V8.9M5.9 10.8 8 8.7l2.1 2.1"/></g></svg>"#.as_slice()).symbolic(true)
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
        "view-compress-symbolic" => COMPRESS.clone(),
        "pull-request-symbolic" => PULL_REQUEST.clone(),
        "merged-pull-request-symbolic" => MERGED_PULL_REQUEST.clone(),
        "closed-pull-request-symbolic" => CLOSED_PULL_REQUEST.clone(),
        "go-down-symbolic" => CHEVRON_DOWN.clone(),
        "go-up-symbolic" => CHEVRON_UP.clone(),
        "go-next-symbolic" => CHEVRON_RIGHT.clone(),
        "list-add-symbolic" => PLUS.clone(),
        "list-remove-symbolic" => MINUS.clone(),
        "checkbox-symbolic" => CHECKBOX.clone(),
        "checkbox-checked-symbolic" => CHECKBOX_CHECKED.clone(),
        "checkbox-mixed-symbolic" => CHECKBOX_MIXED.clone(),
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
        "document-open-recent-symbolic" => HISTORY.clone(),
        "dialog-information-symbolic" => INFO.clone(),
        "window-close-symbolic" => CLOSE.clone(),
        "view-pop-out-symbolic" => POP_OUT.clone(),
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
        // COSMIC's transparent container resets inherited icon color. Keep the
        // button's semantic status tone through this alignment-only wrapper.
        .class(cosmic::theme::Container::custom(|_| {
            iced::widget::container::Style::default()
        }))
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

/// The same target at caption scale, for controls that belong to a message
/// rather than to a toolbar.
pub fn compact_icon_control<'a, Message: Clone + 'a>(
    icon: widget::icon::Handle,
    label: impl Into<Cow<'a, str>>,
    on_press: Option<Message>,
) -> IconControl<'a, Message> {
    let label = label.into();
    let content = widget::container(widget::icon(icon).size(crate::design::COMPACT_ICON_SIZE))
        .class(cosmic::theme::Container::custom(|_| {
            iced::widget::container::Style::default()
        }))
        .center(Length::Fill);
    IconControl {
        button: widget::button::custom(content)
            .name(label.clone())
            .class(control_style(false, None))
            .width(crate::design::COMPACT_CONTROL_SIZE)
            .height(crate::design::COMPACT_CONTROL_SIZE)
            .padding(0)
            .on_press_maybe(on_press),
        label,
        selected: false,
        enabled: true,
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

/// Independent toolbar commands, grouped by spacing without a selector surface.
pub fn toolbar_actions<'a, Message: 'a>(
    controls: Vec<Element<'a, Message>>,
) -> Element<'a, Message> {
    widget::container(
        widget::row::with_children(controls)
            .spacing(0)
            .align_y(iced::Alignment::Center),
    )
    .padding(2)
    .into()
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
                radius: crate::design::RADIUS_S.into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }))
    .into()
}

/// One entry of a [`view_tabs`] bar.
pub struct ViewTab<'a, Message> {
    /// The short visible name.
    pub label: Cow<'a, str>,
    /// The accessible name and tooltip, which say what the label cannot fit.
    pub description: Cow<'a, str>,
    /// A quiet trailing figure, such as how many rows the view holds, and the
    /// text style it draws in, e.g. [`crate::design::muted_text`].
    pub count: Option<(
        Cow<'a, str>,
        fn(&cosmic::Theme) -> iced::widget::text::Style,
    )>,
    pub selected: bool,
    pub id: Option<widget::Id>,
    /// What choosing the tab raises. The selected tab publishes nothing:
    /// choosing the view already on screen is not an intent.
    pub on_press: Option<Message>,
}

/// A pane's single row of text tabs.
///
/// Views of one pane are peers, so they are words on one baseline rather than
/// unlabeled glyphs in a boxed selector: the selected view reads from its
/// filled surface and full-strength label, the others stay quiet until
/// hovered. Nothing else belongs in the row; a view's own options live in the
/// pane's header, beside the identity they act on.
///
/// `compact` tightens each tab's inset for a narrow pane. The row clips
/// rather than spilling past the pane if its words still do not fit.
pub fn view_tabs<'a, Message: Clone + 'a>(
    tabs: Vec<ViewTab<'a, Message>>,
    compact: bool,
) -> Element<'a, Message> {
    let spacing = cosmic::theme::spacing();
    let inset = if compact {
        spacing.space_xxs
    } else {
        spacing.space_xs
    };
    let row = widget::row::with_children(
        tabs.into_iter()
            .map(|tab| {
                let ViewTab {
                    label,
                    description,
                    count,
                    selected,
                    id,
                    on_press,
                } = tab;
                let mut content = widget::row::with_capacity(2)
                    .spacing(spacing.space_xxs)
                    .align_y(iced::Alignment::Center)
                    .push(crate::design::body(label).wrapping(iced::widget::text::Wrapping::None));
                // The count is drawn, so it is spoken too: a name that left it
                // out would describe a different tab than the one on screen.
                let name: Cow<'a, str> = match &count {
                    Some((count, _)) => format!("{description}: {count}").into(),
                    None => description,
                };
                if let Some((count, style)) = count {
                    content = content.push(
                        crate::design::caption(count)
                            .wrapping(iced::widget::text::Wrapping::None)
                            .class(cosmic::theme::Text::Custom(style)),
                    );
                }
                let mut button =
                    widget::button::custom(widget::container(content).center_y(Length::Fill))
                        .class(view_tab_style(selected))
                        .name(name.clone())
                        .selected(selected)
                        .padding([0, inset])
                        .height(crate::design::DENSE_ROW_HEIGHT)
                        .on_press_maybe(if selected { None } else { on_press });
                if let Some(id) = id {
                    button = button.id(id);
                }
                tooltip(crate::interaction::hover(button), name)
            })
            .collect::<Vec<_>>(),
    )
    .spacing(spacing.space_xxxs)
    .align_y(iced::Alignment::Center);
    widget::container(row).width(Length::Fill).clip(true).into()
}

/// [`control_style`]'s surface with a label tone: the selected tab keeps its
/// fill and full-strength label even though it publishes nothing, and the
/// rest read as quieter peers rather than as disabled controls.
fn view_tab_style(selected: bool) -> cosmic::theme::Button {
    let appearance = move |focused: bool, theme: &cosmic::Theme| {
        let component = &theme.current_container().component;
        widget::button::Style {
            background: (selected || focused)
                .then(|| iced::Background::Color(component.selected_state_color().into())),
            border_radius: theme.cosmic().corner_radii.radius_s.into(),
            border_width: 0.0,
            text_color: Some(if selected {
                component.on.into()
            } else {
                crate::design::muted_color(theme)
            }),
            ..widget::button::Style::new()
        }
    };
    cosmic::theme::Button::Custom {
        active: Box::new(appearance),
        disabled: Box::new(move |theme| appearance(false, theme)),
        hovered: Box::new(move |focused, theme| widget::button::Style {
            background: Some(iced::Background::Color(
                theme.current_container().component.hover.into(),
            )),
            text_color: Some(theme.current_container().component.on.into()),
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

/// The instant every activity spinner measures its rotation from.
///
/// Phase has to come from one value shared by the whole process rather than
/// from each widget's own mount time, so two indicators that appear a second
/// apart still draw the same arc instead of chasing each other.
static SPINNER_EPOCH: std::sync::LazyLock<cosmic::iced::time::Instant> =
    std::sync::LazyLock::new(cosmic::iced::time::Instant::now);

/// The arc the spinner draws `elapsed` after [`SPINNER_EPOCH`], as start and
/// end angles in radians measured from twelve o'clock.
///
/// Deriving the angle from elapsed time rather than accumulating a per-frame
/// delta means a dropped frame skips ahead instead of rewinding, and two
/// spinners mounted at different moments stay in phase. The reduction to a
/// single turn happens in `f64` because a shell left running overnight
/// accumulates enough seconds that `f32` can no longer resolve a frame.
fn spinner_arc(elapsed: std::time::Duration) -> (f32, f32) {
    use std::f32::consts::TAU;

    let period = crate::design::SPINNER_PERIOD.as_secs_f64();
    let turns = (elapsed.as_secs_f64() % period) / period;
    let start = turns as f32 * TAU - std::f32::consts::FRAC_PI_2;
    (start, start + crate::design::SPINNER_ARC_SPAN * TAU)
}

/// A small activity glyph that owns its clock: there is no application
/// subscription and no view rebuild behind its motion.
///
/// `color` is the tone the arc draws in.
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
    Element::new(ActivitySpinner { size, tone })
}

/// An arc of fixed span turning at a fixed rate.
///
/// COSMIC's own indeterminate indicator grows its arc to nearly a full circle
/// and shrinks it back twice a second on top of the rotation. That pulse reads
/// as a progress report the shell has no way to make, so the shell draws its
/// own indicator instead of restyling the pinned one.
struct ActivitySpinner {
    size: f32,
    tone: crate::design::Tone,
}

#[derive(Default)]
struct SpinnerState {
    /// Time since [`SPINNER_EPOCH`] as of the last frame, kept here because
    /// `draw` is handed no clock of its own.
    elapsed: std::time::Duration,
}

impl<Message> cosmic::iced::advanced::Widget<Message, cosmic::Theme, cosmic::Renderer>
    for ActivitySpinner
{
    fn tag(&self) -> cosmic::iced::advanced::widget::tree::Tag {
        cosmic::iced::advanced::widget::tree::Tag::of::<SpinnerState>()
    }

    fn state(&self) -> cosmic::iced::advanced::widget::tree::State {
        cosmic::iced::advanced::widget::tree::State::new(SpinnerState::default())
    }

    fn size(&self) -> cosmic::iced::Size<Length> {
        cosmic::iced::Size {
            width: Length::Fixed(self.size),
            height: Length::Fixed(self.size),
        }
    }

    fn layout(
        &mut self,
        _tree: &mut cosmic::iced::advanced::widget::Tree,
        _renderer: &cosmic::Renderer,
        limits: &cosmic::iced::advanced::layout::Limits,
    ) -> cosmic::iced::advanced::layout::Node {
        cosmic::iced::advanced::layout::atomic(limits, self.size, self.size)
    }

    fn update(
        &mut self,
        tree: &mut cosmic::iced::advanced::widget::Tree,
        event: &cosmic::iced::Event,
        layout: cosmic::iced::advanced::Layout<'_>,
        _cursor: cosmic::iced::mouse::Cursor,
        _renderer: &cosmic::Renderer,
        _clipboard: &mut dyn cosmic::iced::advanced::Clipboard,
        shell: &mut cosmic::iced::advanced::Shell<'_, Message>,
        viewport: &cosmic::iced::Rectangle,
    ) {
        // A spinner nobody can see must not hold the frame clock. Indicators
        // are mounted in cards sitting in an off-screen workspace column and
        // in strips scrolled past their own viewport, and asking for the next
        // frame from each of them pins the whole window to the compositor's
        // cadence on behalf of nothing - worst exactly when many agents are
        // working at once, which is when the shell can least afford it.
        if !layout.bounds().intersects(viewport) {
            return;
        }
        if let cosmic::iced::Event::Window(cosmic::iced::window::Event::RedrawRequested(now)) =
            event
        {
            let state = tree.state.downcast_mut::<SpinnerState>();
            state.elapsed = now.saturating_duration_since(*SPINNER_EPOCH);
            shell.request_redraw();
        }
    }

    fn draw(
        &self,
        tree: &cosmic::iced::advanced::widget::Tree,
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
        _style: &cosmic::iced::advanced::renderer::Style,
        layout: cosmic::iced::advanced::Layout<'_>,
        _cursor: cosmic::iced::mouse::Cursor,
        _viewport: &cosmic::iced::Rectangle,
    ) {
        use cosmic::iced::advanced::Renderer as _;
        use cosmic::iced::advanced::graphics::geometry::Renderer as _;
        use cosmic::widget::canvas;

        let bounds = layout.bounds();
        if bounds.width < 1.0 || bounds.height < 1.0 {
            return;
        }
        // The stroke and radius are the pinned indicator's, so replacing the
        // animation does not resize any indicator already on screen.
        let bar_height = (bounds.width / 12.0).max(2.0);
        let radius = (bounds.width - bar_height) / 2.0;
        if radius <= 0.0 {
            return;
        }

        let state = tree.state.downcast_ref::<SpinnerState>();
        let (start, end) = spinner_arc(state.elapsed);
        let mut frame = canvas::Frame::new(&*renderer, bounds.size());
        let mut builder = canvas::path::Builder::new();
        builder.arc(canvas::path::Arc {
            center: frame.center(),
            radius,
            start_angle: cosmic::iced::Radians(start),
            end_angle: cosmic::iced::Radians(end),
        });
        frame.stroke(
            &builder.build(),
            canvas::Stroke::default()
                .with_color(self.tone.resolve(theme))
                .with_width(bar_height)
                .with_line_cap(canvas::LineCap::Round),
        );
        let geometry = frame.into_geometry();

        renderer.with_translation(cosmic::iced::Vector::new(bounds.x, bounds.y), |renderer| {
            renderer.draw_geometry(geometry);
        });
    }
}

/// The sweep a radial gauge fills for `fraction`, as start and end angles in
/// radians measured from twelve o'clock, or `None` when nothing is used.
///
/// A fraction outside the unit range is clamped rather than wrapped: a
/// provider reporting more context than its own window says the reader is
/// full, not that the ring should start a second lap. A non-finite figure
/// fills nothing, because a ring is a claim about a proportion and there is
/// none to make.
fn radial_sweep(fraction: f32) -> Option<(f32, f32)> {
    use std::f32::consts::{FRAC_PI_2, TAU};

    let filled = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    (filled > 0.0).then(|| (-FRAC_PI_2, -FRAC_PI_2 + filled * TAU))
}

/// A ring whose filled sweep is the fraction of a budget in use.
///
/// `tone` is read from the drawing theme, so a gauge whose urgency changes
/// does not have to be rebuilt to change colour.
///
/// The linear capsule this replaces cost a status line the width of a whole
/// caption and, at the height a caption row allows, read as a crack in the
/// layout. A ring carries the same proportion in the space of a glyph, which
/// is what lets the budget stay pinned to the pane's right edge however
/// narrow the pane gets.
pub fn budget_radial<'a, Message: 'a>(
    fraction: f32,
    tone: fn(&cosmic::Theme) -> iced::Color,
) -> Element<'a, Message> {
    Element::new(BudgetRadial {
        fraction,
        tone: crate::design::Tone::Themed(tone),
    })
}

struct BudgetRadial {
    fraction: f32,
    tone: crate::design::Tone,
}

impl<Message> cosmic::iced::advanced::Widget<Message, cosmic::Theme, cosmic::Renderer>
    for BudgetRadial
{
    fn size(&self) -> cosmic::iced::Size<Length> {
        cosmic::iced::Size {
            width: Length::Fixed(crate::design::RADIAL_GAUGE_SIZE),
            height: Length::Fixed(crate::design::RADIAL_GAUGE_SIZE),
        }
    }

    fn layout(
        &mut self,
        _tree: &mut cosmic::iced::advanced::widget::Tree,
        _renderer: &cosmic::Renderer,
        limits: &cosmic::iced::advanced::layout::Limits,
    ) -> cosmic::iced::advanced::layout::Node {
        cosmic::iced::advanced::layout::atomic(
            limits,
            crate::design::RADIAL_GAUGE_SIZE,
            crate::design::RADIAL_GAUGE_SIZE,
        )
    }

    fn draw(
        &self,
        _tree: &cosmic::iced::advanced::widget::Tree,
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
        _style: &cosmic::iced::advanced::renderer::Style,
        layout: cosmic::iced::advanced::Layout<'_>,
        _cursor: cosmic::iced::mouse::Cursor,
        _viewport: &cosmic::iced::Rectangle,
    ) {
        use cosmic::iced::advanced::Renderer as _;
        use cosmic::iced::advanced::graphics::geometry::Renderer as _;
        use cosmic::widget::canvas;

        let bounds = layout.bounds();
        let radius = (bounds.width.min(bounds.height) - crate::design::RADIAL_GAUGE_STROKE) / 2.0;
        if radius <= 0.0 {
            return;
        }
        let stroke = canvas::Stroke::default().with_width(crate::design::RADIAL_GAUGE_STROKE);
        let mut frame = canvas::Frame::new(&*renderer, bounds.size());
        // The unfilled ring is drawn whatever the occupancy, because a gauge
        // has to read as a whole for its filled part to mean anything.
        let mut track = canvas::path::Builder::new();
        track.circle(frame.center(), radius);
        frame.stroke(
            &track.build(),
            stroke.with_color(crate::design::track(theme)),
        );
        if let Some((start, end)) = radial_sweep(self.fraction) {
            let mut used = canvas::path::Builder::new();
            used.arc(canvas::path::Arc {
                center: frame.center(),
                radius,
                start_angle: cosmic::iced::Radians(start),
                end_angle: cosmic::iced::Radians(end),
            });
            frame.stroke(&used.build(), stroke.with_color(self.tone.resolve(theme)));
        }
        let geometry = frame.into_geometry();

        renderer.with_translation(cosmic::iced::Vector::new(bounds.x, bounds.y), |renderer| {
            renderer.draw_geometry(geometry);
        });
    }
}

/// The filled width of a linear gauge `width` wide at `fraction`, clamped the
/// same way as [`radial_sweep`]: past full is full, and a non-finite figure
/// fills nothing.
fn bar_fill(width: f32, fraction: f32) -> f32 {
    let filled = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    width * filled
}

/// A thin horizontal gauge the width of its row, for a list of budgets read
/// against each other.
///
/// The ring is right where a budget sits beside a caption; where several
/// windows are listed one under another, bars of one length let the eye
/// compare them down the column, which rings of one size cannot.
pub fn budget_bar<'a, Message: 'a>(
    fraction: f32,
    tone: crate::design::Tone,
) -> Element<'a, Message> {
    Element::new(BudgetBar { fraction, tone })
}

struct BudgetBar {
    fraction: f32,
    tone: crate::design::Tone,
}

impl<Message> cosmic::iced::advanced::Widget<Message, cosmic::Theme, cosmic::Renderer>
    for BudgetBar
{
    fn size(&self) -> cosmic::iced::Size<Length> {
        cosmic::iced::Size {
            width: Length::Fill,
            height: Length::Fixed(crate::design::BUDGET_BAR_HEIGHT),
        }
    }

    fn layout(
        &mut self,
        _tree: &mut cosmic::iced::advanced::widget::Tree,
        _renderer: &cosmic::Renderer,
        limits: &cosmic::iced::advanced::layout::Limits,
    ) -> cosmic::iced::advanced::layout::Node {
        cosmic::iced::advanced::layout::atomic(
            limits,
            Length::Fill,
            crate::design::BUDGET_BAR_HEIGHT,
        )
    }

    fn draw(
        &self,
        _tree: &cosmic::iced::advanced::widget::Tree,
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
        _style: &cosmic::iced::advanced::renderer::Style,
        layout: cosmic::iced::advanced::Layout<'_>,
        _cursor: cosmic::iced::mouse::Cursor,
        _viewport: &cosmic::iced::Rectangle,
    ) {
        use cosmic::iced::advanced::Renderer as _;

        let bounds = layout.bounds();
        let radius = bounds.height / 2.0;
        renderer.fill_quad(
            cosmic::iced::advanced::renderer::Quad {
                bounds,
                border: iced::Border {
                    radius: radius.into(),
                    ..Default::default()
                },
                ..Default::default()
            },
            crate::design::track(theme),
        );
        let filled = bar_fill(bounds.width, self.fraction);
        if filled > 0.0 {
            renderer.fill_quad(
                cosmic::iced::advanced::renderer::Quad {
                    bounds: cosmic::iced::Rectangle {
                        // Never narrower than the bar is tall, so a sliver of
                        // use still reads as a rounded mark rather than a seam.
                        width: filled.max(bounds.height).min(bounds.width),
                        ..bounds
                    },
                    border: iced::Border {
                        radius: radius.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                self.tone.resolve(theme),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;
    use std::time::Duration;

    /// The defect the spinner replaced was an arc that grew and shrank while
    /// it turned, so both halves of "constant span, constant rate" are worth
    /// holding onto.
    #[test]
    fn the_arc_keeps_its_span_and_turns_at_a_constant_rate() {
        let period = crate::design::SPINNER_PERIOD;
        let span = crate::design::SPINNER_ARC_SPAN * TAU;
        let step = period / 8;

        let mut previous: Option<f32> = None;
        for tick in 0..8 {
            let (start, end) = spinner_arc(step * tick);
            assert!(
                (end - start - span).abs() < 1e-4,
                "the arc spans {} rad at tick {tick}, not {span}",
                end - start
            );
            if let Some(previous) = previous {
                assert!(
                    (start - previous - span_per(step, period)).abs() < 1e-4,
                    "the arc advanced {} rad over one step at tick {tick}",
                    start - previous
                );
            }
            previous = Some(start);
        }
    }

    fn span_per(step: Duration, period: Duration) -> f32 {
        step.as_secs_f32() / period.as_secs_f32() * TAU
    }

    /// A spinner that has been turning all day has to draw the same arc as
    /// one mounted a moment ago, which is what keeps several of them in phase
    /// and what stops the angle losing resolution as the shell stays open.
    #[test]
    fn the_angle_depends_only_on_time_within_the_period() {
        let period = crate::design::SPINNER_PERIOD;
        for offset in [Duration::ZERO, period / 3, period / 2] {
            let (fresh, _) = spinner_arc(offset);
            let (aged, _) = spinner_arc(offset + period * 72_000);
            assert!(
                (fresh - aged).abs() < 1e-4,
                "a whole number of turns later the arc starts at {aged} rather than {fresh}"
            );
        }
    }

    /// A spinner mounted in an off-screen card or scrolled past the end of a
    /// strip must not keep asking for the next frame: several of those at
    /// once pin the whole window to the compositor's cadence for motion
    /// nobody can see.
    #[test]
    fn an_offscreen_spinner_stops_asking_for_frames() {
        use cosmic::iced::advanced::{Layout, Shell, clipboard, layout, mouse, widget::Tree};
        use cosmic::iced::{Event, Point, Rectangle, Size, window};

        let renderer = headless_renderer();
        let viewport = Rectangle::new(Point::ORIGIN, Size::new(400.0, 300.0));
        // Half a period after the shared epoch, so a spinner that does read
        // the clock lands on a distinctly non-zero angle.
        let redraw = Event::Window(window::Event::RedrawRequested(
            *SPINNER_EPOCH + crate::design::SPINNER_PERIOD / 2,
        ));

        let requested = |origin: Point| {
            let mut element: Element<'_, ()> = spinner(
                24.0,
                crate::design::Tone::Themed(crate::design::accent_color),
            );
            let mut tree = Tree::new(element.as_widget());
            let node = layout::Node::new(Size::new(24.0, 24.0)).move_to(origin);
            let mut messages = Vec::new();
            let mut shell = Shell::new(&mut messages);
            element.as_widget_mut().update(
                &mut tree,
                &redraw,
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut clipboard::Null,
                &mut shell,
                &viewport,
            );
            (
                shell.redraw_request(),
                tree.state.downcast_ref::<SpinnerState>().elapsed,
            )
        };

        let (visible, advanced) = requested(Point::new(8.0, 8.0));
        assert_eq!(visible, window::RedrawRequest::NextFrame);
        assert_ne!(advanced, Duration::ZERO, "the visible arc did not advance");

        let (hidden, frozen) = requested(Point::new(8.0, 900.0));
        assert_eq!(hidden, window::RedrawRequest::Wait);
        assert_eq!(frozen, Duration::ZERO, "the hidden arc advanced anyway");
    }

    /// The ring is the only thing a reader looks at before deciding to
    /// compact, so a full budget has to look full and an empty one has to
    /// draw no sweep at all rather than a stray dot at twelve o'clock.
    #[test]
    fn the_bar_fill_is_the_clamped_fraction_of_its_width() {
        assert_eq!(bar_fill(200.0, 0.25), 50.0);
        assert_eq!(bar_fill(200.0, 1.7), 200.0);
        for nothing in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert_eq!(bar_fill(200.0, nothing), 0.0, "{nothing} filled the bar");
        }
    }

    #[test]
    fn the_radial_sweep_is_the_fraction_of_the_budget_in_use() {
        let quarter = radial_sweep(0.25).expect("a quarter of a budget is a sweep");
        assert!((quarter.1 - quarter.0 - TAU / 4.0).abs() < 1e-4);
        let full = radial_sweep(1.0).expect("a full budget is a whole ring");
        assert!((full.1 - full.0 - TAU).abs() < 1e-4);
        // A provider reporting past its own window still means "full".
        let over = radial_sweep(2.5).expect("an overrun budget is a whole ring");
        assert_eq!(over, full);
        for nothing in [0.0, -1.0, f32::NAN] {
            assert_eq!(
                radial_sweep(nothing),
                None,
                "{nothing} drew a sweep of its own"
            );
        }
    }

    fn headless_renderer() -> cosmic::Renderer {
        use cosmic::iced::advanced::renderer::Headless;

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(<cosmic::Renderer as Headless>::new(
                cosmic::iced::Font::DEFAULT,
                cosmic::iced::Pixels::from(16),
                Some("tiny-skia"),
            ))
            .expect("create a headless renderer")
    }
}
