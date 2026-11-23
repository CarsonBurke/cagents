//! The interface's symbolic icon set.
//!
//! Every glyph the shell draws is a variant of [`Icon`], so a misspelt or
//! retired name is a compile error rather than a blank square at runtime.
//! Every one is the application's own drawing under `src/icons/`, in one
//! style: a 16-unit square, round strokes of about 1.4 units, and ink kept
//! at least three quarters of a unit inside the edge. Icons from the desktop theme
//! varied in weight and shape, and some were wider than tall, so they did not
//! sit in the same box as the rest. `tests::every_glyph_stays_inside_its_box`
//! holds every drawing to that box.

use std::sync::LazyLock;

use cosmic::{
    Element,
    widget::{
        self,
        icon::{self, Handle},
    },
};

/// Declares [`Icon`] from its drawings, each named by its file under
/// `src/icons/`.
macro_rules! icons {
    ($($icon:ident => $file:literal,)*) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Icon {
            $($icon,)*
        }

        impl Icon {
            /// Every glyph, in declaration order: the order of [`HANDLES`].
            const ALL: &'static [Self] = &[$(Self::$icon,)*];

            const fn svg(self) -> &'static [u8] {
                match self {
                    $(Self::$icon => include_bytes!(concat!("../icons/", $file, ".svg")),)*
                }
            }

            #[cfg(test)]
            const fn file(self) -> &'static str {
                match self {
                    $(Self::$icon => $file,)*
                }
            }
        }
    };
}

icons! {
    AddedFile => "added-file",
    Agent => "agent",
    AgentBackground => "background-work",
    AgentWaiting => "waiting",
    AgentWorking => "working",
    Alarm => "alarm",
    Attachment => "attachment",
    Attention => "attention",
    Avatar => "avatar",
    Board => "board",
    Branch => "branch",
    Brightness => "brightness",
    Browser => "browser",
    Cancel => "cancel",
    Changes => "changes",
    Channel => "channel",
    ChannelAdd => "channel-add",
    Chat => "chat",
    Check => "check",
    CheckRunning => "check-running",
    Checkbox => "checkbox",
    CheckboxChecked => "checkbox-checked",
    CheckboxMixed => "checkbox-mixed",
    ChevronDown => "chevron-down",
    ChevronLeft => "chevron-left",
    ChevronRight => "chevron-right",
    ChevronUp => "chevron-up",
    Clear => "clear",
    Close => "close",
    ClosedPullRequest => "closed-pull-request",
    Comment => "comment",
    Commit => "commit",
    Compress => "compress",
    Copy => "copy",
    CopyPath => "copy-path",
    CopyRelativePath => "copy-relative",
    Delete => "delete",
    Diff => "diff",
    Display => "display",
    DraftPullRequest => "draft-pull-request",
    Edit => "edit",
    Ended => "ended",
    Error => "error",
    ExpandAll => "expand-all",
    ExpandDown => "expand-down",
    ExpandUp => "expand-up",
    Filter => "filter",
    First => "first",
    Folder => "folder",
    Grid => "grid",
    Hide => "hide",
    History => "history",
    Home => "home",
    Image => "image",
    Import => "import",
    Inbox => "inbox",
    Info => "info",
    InsertText => "insert-text",
    Jump => "jump",
    Keyboard => "keyboard",
    Last => "last",
    List => "list",
    Lock => "lock",
    MergedPullRequest => "merged-pull-request",
    Microphone => "microphone",
    Minus => "minus",
    More => "more",
    Night => "night",
    Open => "open",
    OpenFile => "open-file",
    OpenFolder => "open-folder",
    Permission => "permission",
    Plus => "plus",
    PopOut => "pop-out",
    Preview => "preview",
    PullRequest => "pull-request",
    Push => "push",
    Question => "question",
    Reasoning => "reasoning",
    Refresh => "refresh",
    Rename => "rename",
    RevertFile => "revert-file",
    ReviewApproved => "review-approved",
    ReviewChanges => "review-changes",
    ReviewRequired => "review-required",
    Save => "save",
    Search => "search",
    Send => "send",
    Settings => "settings",
    Sidebar => "sidebar",
    SplitPanel => "split-panel",
    Stop => "stop",
    Terminal => "terminal",
    TextFile => "text-file",
    Tree => "tree",
    Unavailable => "unavailable",
    Undo => "undo",
    Update => "update",
    UsageLimit => "usage-limit",
    Users => "users",
    Utilities => "utilities",
    Warning => "warning",
    Worktree => "worktree",
    WorktreeAdd => "worktree-add",
    ZoomFit => "zoom-fit",
    ZoomIn => "zoom-in",
    ZoomOut => "zoom-out",
}

/// One handle per glyph for the life of the process. A stable handle keeps
/// its rasterization in the renderer's cache instead of re-keying it on every
/// view.
static HANDLES: LazyLock<Vec<Handle>> = LazyLock::new(|| {
    Icon::ALL
        .iter()
        .map(|icon| icon::from_svg_bytes(icon.svg()).symbolic(true))
        .collect()
});

impl Icon {
    pub fn handle(self) -> Handle {
        HANDLES[self as usize].clone()
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
            include_bytes!("../../../../packaging/io.github.cagents.ceditor.svg").as_slice(),
        )
        .symbolic(false)
    });
    ICON.clone()
}

/// Native editor window artwork, independent of the installed icon theme.
pub fn ceditor_window_icon() -> cosmic::iced::window::Icon {
    let rgba = image::load_from_memory_with_format(
        include_bytes!("../../../../packaging/io.github.cagents.ceditor.png"),
        image::ImageFormat::Png,
    )
    .expect("embedded editor icon must be a valid PNG")
    .into_rgba8();
    let (width, height) = rgba.dimensions();
    cosmic::iced::window::icon::from_rgba(rgba.into_raw(), width, height)
        .expect("embedded editor icon must contain valid RGBA pixels")
}

const APPLICATION_ICON_PNG: &[u8] = include_bytes!("../../../../packaging/dev.cagents.Cagents.png");
/// The width and height of [`application_window_icon`], in pixels.
pub const APPLICATION_ICON_SIZE: u32 = 128;

/// Embedded artwork for initial window creation, independent of icon themes.
pub fn application_window_icon() -> cosmic::iced::window::Icon {
    let rgba = image::load_from_memory_with_format(APPLICATION_ICON_PNG, image::ImageFormat::Png)
        .expect("embedded application icon must be a valid PNG")
        .into_rgba8();
    cosmic::iced::window::icon::from_rgba(
        rgba.into_raw(),
        APPLICATION_ICON_SIZE,
        APPLICATION_ICON_SIZE,
    )
    .expect("embedded application icon dimensions must match its RGBA pixels")
}

/// A provider's mark as SVG source, and whether it is a symbolic drawing
/// that takes the surrounding icon colour rather than its own.
pub fn provider_artwork(provider: crate::agent::Provider) -> (&'static [u8], bool) {
    use crate::agent::Provider;

    match provider {
        Provider::Omp => (include_bytes!("../icons/omp.svg"), false),
        Provider::Claude => (include_bytes!("../icons/claude.svg"), false),
        Provider::Codex => (include_bytes!("../icons/codex.svg"), true),
        Provider::Antigravity => (include_bytes!("../icons/antigravity.svg"), false),
        Provider::Muse => (include_bytes!("../icons/muse.svg"), false),
    }
}

/// Provider identity is shared by terminal and native-chat tab presentations.
pub fn provider_icon(provider: crate::agent::Provider) -> Handle {
    use crate::agent::Provider;

    static HANDLES: LazyLock<[Handle; Provider::ALL.len()]> = LazyLock::new(|| {
        Provider::ALL.map(|provider| {
            let (svg, symbolic) = provider_artwork(provider);
            icon::from_svg_bytes(svg).symbolic(symbolic)
        })
    });
    HANDLES[provider as usize].clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::Path;

    #[test]
    fn editor_window_icon_preserves_its_distinct_transparent_artwork() {
        let image = image::load_from_memory_with_format(
            include_bytes!("../../../../packaging/io.github.cagents.ceditor.png"),
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
        assert_ne!(pixels, application_window_icon().into_raw().0);
    }

    #[test]
    fn handles_follow_declaration_order() {
        for (index, icon) in Icon::ALL.iter().enumerate() {
            assert_eq!(*icon as usize, index, "{icon:?}");
        }
        assert_eq!(HANDLES.len(), Icon::ALL.len());
    }

    #[test]
    fn drawings_are_symbolic_svg() {
        for icon in Icon::ALL {
            let svg = std::str::from_utf8(icon.svg()).expect("an icon is UTF-8");
            assert!(svg.starts_with("<svg "), "{icon:?}");
            // Symbolic recoloring replaces black; any other ink would
            // survive it and ignore the control's tone.
            assert!(!svg.contains("stroke=\"#"), "{icon:?}");
            assert!(!svg.contains("fill=\"#"), "{icon:?}");
        }
    }

    /// Every drawing in `src/icons/` that is not provider artwork has a
    /// variant, so none is left behind to be drawn some other way.
    #[test]
    fn every_drawing_is_a_glyph() {
        let providers = ["antigravity", "claude", "codex", "muse", "omp"];
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/icons");
        let used: BTreeSet<&str> = Icon::ALL.iter().map(|icon| icon.file()).collect();
        let unused: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                let stem = path.file_stem()?.to_str()?.to_owned();
                (path.extension()? == "svg"
                    && !used.contains(stem.as_str())
                    && !providers.contains(&stem.as_str()))
                .then_some(stem)
            })
            .collect();
        assert!(
            unused.is_empty(),
            "drawings with no Icon variant: {unused:?}"
        );
    }

    /// How far ink must stay from each edge of the 16-unit box.
    const MARGIN: f32 = 0.75;

    /// Draws `tree` at `pixels` per unit, as the renderer fills a square
    /// raster, into a pixmap with `pad` whole pixels of empty border on each
    /// side, and returns the ink's bounds in units of the box: left, top,
    /// right, bottom.
    fn ink_bounds(tree: &resvg::usvg::Tree, pixels: f32, pad: u32) -> Option<[f32; 4]> {
        let side = (16.0 * pixels).ceil() as u32 + 2 * pad;
        let mut pixmap = resvg::tiny_skia::Pixmap::new(side, side).unwrap();
        resvg::render(
            tree,
            resvg::tiny_skia::Transform::from_row(pixels, 0.0, 0.0, pixels, pad as f32, pad as f32),
            &mut pixmap.as_mut(),
        );
        let mut bounds: Option<[u32; 4]> = None;
        for (index, pixel) in pixmap.pixels().iter().enumerate() {
            // Faint antialiasing below 1/32 coverage is not visible ink.
            if pixel.alpha() > 8 {
                let (x, y) = (index as u32 % side, index as u32 / side);
                let b = bounds.get_or_insert([x, y, x, y]);
                *b = [b[0].min(x), b[1].min(y), b[2].max(x + 1), b[3].max(y + 1)];
            }
        }
        bounds.map(|b| b.map(|edge| (edge as f32 - pad as f32) / pixels))
    }

    /// No glyph touches or crosses the edge of its box. The box is a
    /// 16-unit square, so the renderer scales it uniformly into a square
    /// icon of any size, and ink kept inside the margin stays inside the
    /// raster at every size and scale, clear of its edge.
    #[test]
    fn every_glyph_stays_inside_its_box() {
        let options = resvg::usvg::Options::default();
        for icon in Icon::ALL {
            let tree = resvg::usvg::Tree::from_data(icon.svg(), &options).expect("an icon parses");
            assert_eq!(
                (tree.size().width(), tree.size().height()),
                (16.0, 16.0),
                "{icon:?} must draw in a 16-unit square"
            );

            // Measured finely, the ink keeps its margin on every side.
            let [left, top, right, bottom] = ink_bounds(&tree, 32.0, 64).expect("an icon has ink");
            let inset = [left, top, 16.0 - right, 16.0 - bottom];
            // One measuring pixel of tolerance.
            assert!(
                inset.iter().all(|edge| *edge >= MARGIN - 1.0 / 32.0),
                "{icon:?} ({}.svg) comes within {inset:?} units of its edge; keep {MARGIN}",
                icon.file()
            );
        }
    }
}
