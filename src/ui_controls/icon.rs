//! The interface's symbolic icon set.
//!
//! Every glyph the shell draws is a variant of [`Icon`], so a misspelt or
//! retired name is a compile error rather than a blank square at runtime.
//! Whether a glyph is the application's own drawing or is left to the desktop
//! icon theme is decided once, in the table below, and is invisible to callers.

use std::sync::LazyLock;

use cosmic::{
    Element,
    widget::{
        self,
        icon::{self, Handle},
    },
};

/// Declares [`Icon`] from two tables: the application's own drawings, named by
/// their file under `src/icons/`, and the names left to the desktop theme.
macro_rules! icons {
    (
        bundled { $($bundled:ident => $file:literal,)* }
        themed { $($themed:ident => $name:literal,)* }
    ) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Icon {
            $($bundled,)*
            $($themed,)*
        }

        impl Icon {
            /// Every glyph, in declaration order: the order of [`BUNDLED`].
            const ALL: &'static [Self] = &[$(Self::$bundled,)* $(Self::$themed,)*];

            const fn source(self) -> Source {
                match self {
                    $(Self::$bundled => Source::Bundled(
                        include_bytes!(concat!("../icons/", $file, ".svg")),
                    ),)*
                    $(Self::$themed => Source::Theme($name),)*
                }
            }
        }
    };
}

#[derive(Clone, Copy)]
enum Source {
    /// The application's own drawing. It keeps controls legible where the
    /// desktop theme omits a name or answers it through libcosmic's prefix
    /// fallback, which retries `internet-web-browser-symbolic` as the
    /// full-color application icon that a symbolic render flattens into a
    /// solid disc.
    Bundled(&'static [u8]),
    /// A freedesktop name that ships under exactly that name in the stock
    /// COSMIC theme chain (Cosmic, then Pop), which
    /// `tests::themed_icons_ship_in_the_installed_cosmic_theme` checks.
    Theme(&'static str),
}

icons! {
    bundled {
        AddedFile => "added-file",
        Agent => "agent",
        AgentWaiting => "waiting",
        AgentWorking => "working",
        Attention => "attention",
        Board => "board",
        Branch => "branch",
        Browser => "browser",
        Changes => "changes",
        Channel => "channel",
        ChannelAdd => "channel-add",
        Chat => "chat",
        Checkbox => "checkbox",
        CheckboxChecked => "checkbox-checked",
        CheckboxMixed => "checkbox-mixed",
        ChevronDown => "chevron-down",
        ChevronLeft => "chevron-left",
        ChevronRight => "chevron-right",
        ChevronUp => "chevron-up",
        Close => "close",
        ClosedPullRequest => "closed-pull-request",
        Comment => "comment",
        Commit => "commit",
        Compress => "compress",
        CopyPath => "copy-path",
        CopyRelativePath => "copy-relative",
        Diff => "diff",
        Duplicate => "duplicate",
        Edit => "edit",
        ExpandAll => "expand-all",
        ExpandDown => "expand-down",
        ExpandUp => "expand-up",
        Hide => "hide",
        History => "history",
        Info => "info",
        InsertText => "insert-text",
        Keyboard => "keyboard",
        MergedPullRequest => "merged-pull-request",
        Minus => "minus",
        OpenFile => "open-file",
        OpenFolder => "open-folder",
        Plus => "plus",
        PopOut => "pop-out",
        Preview => "preview",
        PullRequest => "pull-request",
        Push => "push",
        Rename => "rename",
        RevertFile => "revert-file",
        Save => "save",
        Search => "search",
        Settings => "settings",
        Sidebar => "sidebar",
        SplitPanel => "split-panel",
        Tree => "tree",
        WorktreeAdd => "worktree-add",
    }
    themed {
        Alarm => "alarm-symbolic",
        Attachment => "mail-attachment-symbolic",
        Avatar => "avatar-default-symbolic",
        Brightness => "display-brightness-symbolic",
        Cancel => "process-stop-symbolic",
        Check => "emblem-ok-symbolic",
        Clear => "edit-clear-symbolic",
        Copy => "edit-copy-symbolic",
        Delete => "edit-delete-symbolic",
        Display => "preferences-desktop-display-symbolic",
        Error => "dialog-error-symbolic",
        Filter => "view-filter-symbolic",
        First => "go-first-symbolic",
        Folder => "folder-symbolic",
        Grid => "view-grid-symbolic",
        Home => "go-home-symbolic",
        Image => "image-x-generic-symbolic",
        Import => "document-import-symbolic",
        Jump => "go-jump-symbolic",
        Last => "go-last-symbolic",
        List => "view-list-symbolic",
        Loading => "content-loading-symbolic",
        Lock => "changes-prevent-symbolic",
        More => "view-more-symbolic",
        Night => "weather-clear-night-symbolic",
        NotificationSettings => "preferences-system-notifications-symbolic",
        PanDown => "pan-down-symbolic",
        PanEnd => "pan-end-symbolic",
        Private => "view-private-symbolic",
        Refresh => "view-refresh-symbolic",
        Select => "object-select-symbolic",
        Send => "mail-send-symbolic",
        Stop => "media-playback-stop-symbolic",
        Terminal => "utilities-terminal-symbolic",
        TextEditor => "accessories-text-editor-symbolic",
        TextFile => "text-x-generic-symbolic",
        Unavailable => "action-unavailable-symbolic",
        Undo => "edit-undo-symbolic",
        Update => "software-update-available-symbolic",
        Users => "system-users-symbolic",
        Utilities => "applications-utilities-symbolic",
        Warning => "dialog-warning-symbolic",
        WebBrowser => "web-browser-symbolic",
        ZoomFit => "zoom-fit-best-symbolic",
        ZoomIn => "zoom-in-symbolic",
        ZoomOut => "zoom-out-symbolic",
    }
}

/// One handle per bundled glyph for the life of the process. A stable handle
/// keeps its rasterization in the renderer's cache instead of re-keying it on
/// every view. Themed glyphs have no entry: their name is resolved against
/// the icon theme each time, so a theme changed at runtime takes effect.
static BUNDLED: LazyLock<Vec<Option<Handle>>> = LazyLock::new(|| {
    Icon::ALL
        .iter()
        .map(|icon| match icon.source() {
            Source::Bundled(svg) => Some(icon::from_svg_bytes(svg).symbolic(true)),
            Source::Theme(_) => None,
        })
        .collect()
});

impl Icon {
    pub fn handle(self) -> Handle {
        match self.source() {
            Source::Bundled(_) => BUNDLED[self as usize]
                .clone()
                .expect("a bundled glyph has a cached handle"),
            Source::Theme(name) => icon::from_name(name).handle(),
        }
    }
}

impl From<Icon> for Handle {
    fn from(icon: Icon) -> Self {
        icon.handle()
    }
}

/// An informative glyph with the same resting ink as an inactive navigation
/// row. Use this outside buttons, where no control style supplies icon color.
pub fn passive_icon<'a, Message: 'a>(icon: impl Into<Handle>, size: u16) -> Element<'a, Message> {
    widget::container(widget::icon(icon.into()).size(size))
        .class(cosmic::theme::Container::custom(|theme| {
            widget::container::Style {
                icon_color: Some(crate::design::muted_icon_color(theme)),
                ..Default::default()
            }
        }))
        .into()
}

/// Embedded application artwork does not depend on the installed icon theme.
pub fn ceditor_icon() -> Handle {
    static ICON: LazyLock<Handle> = LazyLock::new(|| {
        icon::from_svg_bytes(
            include_bytes!("../../packaging/io.github.cagents.ceditor.svg").as_slice(),
        )
        .symbolic(false)
    });
    ICON.clone()
}

/// Native editor window artwork, independent of the installed icon theme.
pub fn ceditor_window_icon() -> cosmic::iced::window::Icon {
    let rgba = image::load_from_memory_with_format(
        include_bytes!("../../packaging/io.github.cagents.ceditor.png"),
        image::ImageFormat::Png,
    )
    .expect("embedded editor icon must be a valid PNG")
    .into_rgba8();
    let (width, height) = rgba.dimensions();
    cosmic::iced::window::icon::from_rgba(rgba.into_raw(), width, height)
        .expect("embedded editor icon must contain valid RGBA pixels")
}

/// Provider identity is shared by terminal and native-chat tab presentations.
pub fn provider_icon(provider: crate::agent::Provider) -> Handle {
    use crate::agent::Provider;

    static OMP: LazyLock<Handle> = LazyLock::new(|| {
        icon::from_svg_bytes(include_bytes!("../icons/omp.svg").as_slice()).symbolic(false)
    });
    static CLAUDE: LazyLock<Handle> = LazyLock::new(|| {
        icon::from_svg_bytes(include_bytes!("../icons/claude.svg").as_slice()).symbolic(false)
    });
    static CODEX: LazyLock<Handle> = LazyLock::new(|| {
        icon::from_svg_bytes(include_bytes!("../icons/codex.svg").as_slice()).symbolic(true)
    });
    match provider {
        Provider::Omp => OMP.clone(),
        Provider::Claude => CLAUDE.clone(),
        Provider::Codex => CODEX.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    #[test]
    fn editor_window_icon_preserves_its_distinct_transparent_artwork() {
        let image = image::load_from_memory_with_format(
            include_bytes!("../../packaging/io.github.cagents.ceditor.png"),
            image::ImageFormat::Png,
        )
        .unwrap()
        .into_rgba8();
        let (pixels, size) = ceditor_window_icon().into_raw();
        assert_eq!((size.width, size.height), (128, 128));
        assert_eq!(pixels, image.as_raw().as_slice());
        for (x, y) in [(0, 0), (127, 0), (0, 127), (127, 127)] {
            assert_eq!(image.get_pixel(x, y)[3], 0);
        }
        assert!(image.pixels().any(|pixel| pixel[3] == 255));
        assert_ne!(pixels, crate::app::application_window_icon().into_raw().0);
    }

    #[test]
    fn handles_follow_declaration_order() {
        for (index, icon) in Icon::ALL.iter().enumerate() {
            assert_eq!(*icon as usize, index, "{icon:?}");
        }
        assert_eq!(BUNDLED.len(), Icon::ALL.len());
    }

    #[test]
    fn bundled_drawings_are_symbolic_svg() {
        for icon in Icon::ALL {
            if let Source::Bundled(svg) = icon.source() {
                let svg = std::str::from_utf8(svg).expect("an icon is UTF-8");
                assert!(svg.starts_with("<svg "), "{icon:?}");
                // Symbolic recoloring replaces black; any other ink would
                // survive it and ignore the control's tone.
                assert!(!svg.contains("stroke=\"#"), "{icon:?}");
                assert!(!svg.contains("fill=\"#"), "{icon:?}");
            }
        }
    }

    /// Where the COSMIC theme chain is installed, confirm it still carries
    /// every name the application leaves to it.
    #[test]
    fn themed_icons_ship_in_the_installed_cosmic_theme() {
        fn visit(dir: &Path, stems: &mut BTreeSet<String>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for path in entries.flatten().map(|entry| entry.path()) {
                if path.is_dir() {
                    visit(&path, stems);
                } else if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
                    stems.insert(stem.to_owned());
                }
            }
        }
        let data_dirs = std::env::var_os("XDG_DATA_DIRS")
            .filter(|dirs| !dirs.is_empty())
            .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
        let theme_roots: Vec<PathBuf> = std::env::split_paths(&data_dirs)
            .flat_map(|dir| ["Cosmic", "Pop"].map(|theme| dir.join("icons").join(theme)))
            .filter(|root| root.is_dir())
            .collect();
        if !theme_roots.iter().any(|root| root.ends_with("Cosmic")) {
            eprintln!("the Cosmic icon theme is not installed; skipping");
            return;
        }
        let mut stems = BTreeSet::new();
        for root in &theme_roots {
            visit(root, &mut stems);
        }
        let missing: Vec<_> = Icon::ALL
            .iter()
            .filter_map(|icon| match icon.source() {
                Source::Theme(name) if !stems.contains(name) => Some((icon, name)),
                _ => None,
            })
            .collect();
        assert!(
            missing.is_empty(),
            "missing from {theme_roots:?}; draw them under src/icons instead: {missing:?}"
        );
    }
}
