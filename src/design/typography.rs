//! The type scale and the text roles built on it.

use std::borrow::Cow;

use cosmic::{
    Renderer, Theme,
    iced::{
        font::{Font, Weight},
        widget::text::LineHeight,
    },
    widget::text::Text,
};

use super::{GRID_UNIT, RADIUS_XS, accent_color, muted_text, raised, space};

#[derive(Clone, Copy)]
struct Typography {
    interface_font: Font,
    interface_size: f32,
    chat_font: Font,
    chat_size: f32,
    revision: u64,
}

thread_local! {
    static REQUEST: std::cell::RefCell<Option<(String, f32, String, f32)>> = const { std::cell::RefCell::new(None) };
    static TYPOGRAPHY: std::cell::Cell<Typography> = std::cell::Cell::new(Typography {
        interface_font: Font::DEFAULT,
        interface_size: TEXT_BODY,
        chat_font: Font::DEFAULT,
        chat_size: TEXT_PROSE,
        revision: 0,
    });
}

/// Apply the active window's typography before building its widget tree. The
/// configuration lives on the UI thread, so font reads do not lock per label.
pub fn configure_typography(interface: &str, interface_size: f32, chat: &str, chat_size: f32) {
    let changed = REQUEST.with(|request| {
        let mut request = request.borrow_mut();
        if request.as_ref().is_some_and(|current| {
            current.0 == interface
                && current.1 == interface_size
                && current.2 == chat
                && current.3 == chat_size
        }) {
            return false;
        }
        *request = Some((
            interface.to_owned(),
            interface_size,
            chat.to_owned(),
            chat_size,
        ));
        true
    });
    if !changed {
        return;
    }
    let installed = |family: &str| {
        installed_font_families(false)
            .iter()
            .find(|name| name.eq_ignore_ascii_case(family.trim()))
    };
    let font_for = |family: &str, fallback: Font| {
        installed(family).map_or(fallback, |name| crate::terminal::monospace_font(name))
    };
    static DESKTOP: std::sync::LazyLock<cosmic::config::FontConfig> =
        std::sync::LazyLock::new(cosmic::config::interface_font);
    let interface_font = font_for(interface, Font::from(DESKTOP.clone()));
    let chat_font = font_for(chat, interface_font);
    cosmic::widget::text::set_default_size(interface_size);
    TYPOGRAPHY.with(|active| {
        let current = active.get();
        if (
            current.interface_font,
            current.interface_size,
            current.chat_font,
            current.chat_size,
        ) != (interface_font, interface_size, chat_font, chat_size)
        {
            // Update this process's toolkit theme without writing desktop preferences.
            cosmic::config::COSMIC_TK
                .write()
                .unwrap_or_else(|error| error.into_inner())
                .interface_font
                .family = installed(interface)
                .cloned()
                .unwrap_or_else(|| DESKTOP.family.clone());
            active.set(Typography {
                interface_font,
                interface_size,
                chat_font,
                chat_size,
                revision: current.revision.wrapping_add(1),
            });
        }
    });
}

/// Installed families are immutable for the running process, as is iced's font database.
pub fn installed_font_families(monospace: bool) -> &'static Vec<String> {
    fn discover(monospace: bool) -> Vec<String> {
        let fonts = cosmic::iced::advanced::graphics::text::font_system()
            .read()
            .unwrap_or_else(|error| error.into_inner());
        let mut names: Vec<_> = fonts
            .db()
            .faces()
            .filter(|face| !monospace || face.monospaced)
            .flat_map(|face| face.families.iter().map(|(name, _)| name.clone()))
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }
    static ALL: std::sync::LazyLock<Vec<String>> = std::sync::LazyLock::new(|| discover(false));
    static MONOSPACE: std::sync::LazyLock<Vec<String>> =
        std::sync::LazyLock::new(|| discover(true));
    if monospace { &MONOSPACE } else { &ALL }
}

pub fn interface_font() -> Font {
    TYPOGRAPHY.with(|active| active.get().interface_font)
}
pub fn interface_size() -> f32 {
    TYPOGRAPHY.with(|active| active.get().interface_size)
}
/// Retain a text role's proportion within the selected interface type scale.
pub fn scaled_interface_size(base_size: f32) -> f32 {
    interface_size() * base_size / TEXT_BODY
}
pub fn chat_font() -> Font {
    TYPOGRAPHY.with(|active| active.get().chat_font)
}
pub fn chat_size() -> f32 {
    TYPOGRAPHY.with(|active| active.get().chat_size)
}
pub fn typography_revision() -> u64 {
    TYPOGRAPHY.with(|active| active.get().revision)
}

/// The size of nearly everything in the shell: rows, controls, labels.
pub const TEXT_BODY: f32 = 13.0;
/// Prose that is read at length - an agent's answers and the draft being
/// written to it - one step above the shell's controls and captions.
pub const TEXT_PROSE: f32 = 14.0;
/// Secondary detail beside or beneath a body-sized label.
pub const TEXT_CAPTION: f32 = 11.0;
/// A heading is set at body size and told apart by weight, so a pane's
/// header does not outshout the rows under it.
pub const TEXT_HEADING: f32 = TEXT_BODY;
/// The name of a dialog, a form or a section of a page.
pub const TEXT_TITLE: f32 = 16.0;
/// The name of a whole page or an otherwise empty pane. Nothing in the shell
/// is set larger: a workbench's hierarchy comes from weight and position, and
/// a 24px or 29px headline beside 13px rows reads as a different product.
pub const TEXT_DISPLAY: f32 = 20.0;

/// The line box body-sized text sits in.
pub const LINE_BODY: f32 = 18.0;

/// The widest a column of prose is allowed to grow.
///
/// Reading a paragraph that spans a maximised window costs the eye its place
/// on every wrap. Typography practice puts the comfortable measure at roughly
/// 60 to 80 characters, which at our body size is this many logical pixels.
/// The transcript's own chrome still spans the full width; only the text is
/// held back.
pub const READABLE_MEASURE: f32 = 720.0;

type ShellText<'a> = Text<'a, Theme, Renderer>;

fn weighted(weight: Weight) -> Font {
    Font {
        weight,
        ..interface_font()
    }
}

pub fn body<'a>(text: impl Into<Cow<'a, str>> + 'a) -> ShellText<'a> {
    cosmic::widget::text::body(text)
        .font(interface_font())
        .size(interface_size())
        .line_height(LineHeight::Absolute((interface_size() + 5.0).into()))
}

pub fn caption<'a>(text: impl Into<Cow<'a, str>> + 'a) -> ShellText<'a> {
    cosmic::widget::text::caption(text)
        .font(interface_font())
        .size(scaled_interface_size(TEXT_CAPTION))
        .line_height(LineHeight::Absolute(scaled_interface_size(17.0).into()))
        .class(cosmic::theme::Text::Custom(muted_text))
}

/// Body text at medium weight: a pane's heading, a row's name, a button's label.
pub fn heading<'a>(text: impl Into<Cow<'a, str>> + 'a) -> ShellText<'a> {
    body(text).font(heading_font())
}

/// The face of [`heading`], for a widget that cannot be built from it.
pub fn heading_font() -> Font {
    weighted(Weight::Medium)
}

/// The name of a dialog, a form or a section of a page.
pub fn title<'a>(text: impl Into<Cow<'a, str>> + 'a) -> ShellText<'a> {
    cosmic::widget::text::body(text)
        .size(scaled_interface_size(TEXT_TITLE))
        .line_height(LineHeight::Absolute(
            scaled_interface_size(LINE_TITLE).into(),
        ))
        .font(title_font())
}

/// The line box title-sized text sits in.
pub const LINE_TITLE: f32 = 22.0;

/// The face of [`title`], for a widget that cannot be built from it.
pub fn title_font() -> Font {
    weighted(Weight::Bold)
}

/// The name of a whole page or an otherwise empty pane.
pub fn display<'a>(text: impl Into<Cow<'a, str>> + 'a) -> ShellText<'a> {
    cosmic::widget::text::body(text)
        .size(scaled_interface_size(TEXT_DISPLAY))
        .line_height(LineHeight::Absolute(
            scaled_interface_size(26.0).into(),
        ))
        .font(weighted(Weight::Bold))
}

/// Markdown shares the shell's semantic palette and a compact heading scale.
pub fn markdown_settings(theme: &Theme, size: f32) -> cosmic::widget::markdown::Settings {
    use cosmic::{iced, widget::markdown};
    let size = size * chat_size() / TEXT_PROSE;
    let mut style = markdown::Style::from(if theme.theme_type.is_dark() {
        iced::Theme::Dark
    } else {
        iced::Theme::Light
    });
    style.font = chat_font();
    style.link_color = accent_color(theme);
    style.inline_code_color = theme.cosmic().background(false).on.into();
    style.inline_code_highlight.background = raised(theme).into();
    style.inline_code_highlight.border.radius = RADIUS_XS.into();
    markdown::Settings {
        h1_size: (size + GRID_UNIT).into(),
        h2_size: (size + GRID_UNIT / 2.0).into(),
        h3_size: (size + 1.0).into(),
        h4_size: size.into(),
        h5_size: size.into(),
        h6_size: size.into(),
        code_size: size.into(),
        spacing: space::S.into(),
        ..markdown::Settings::with_text_size(size, style)
    }
}

#[cfg(test)]
mod settings_typography_tests {
    use super::*;

    #[test]
    fn interface_size_reaches_toolkit_text_and_actual_context_menu_layout() {
        use cosmic::iced::{
            Event, Point, Rectangle, Size, Vector,
            advanced::{Layout, Shell, clipboard, layout, widget::Tree},
            mouse,
        };
        use cosmic::widget;

        #[derive(Clone, Copy, PartialEq, Eq)]
        struct Action;
        impl widget::menu::Action for Action {
            type Message = ();
            fn message(&self) {}
        }

        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let viewport = Rectangle::with_size(Size::new(900.0, 700.0));
        let text_height = |mut element: cosmic::Element<'_, ()>| {
            let mut tree = Tree::new(element.as_widget());
            element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, viewport.size()),
            ).size().height
        };
        let menu_height = || {
            let items = widget::menu::items(
                &std::collections::HashMap::new(),
                vec![widget::menu::Item::Button("Rename pane", None, Action)],
            );
            let mut element: cosmic::Element<'_, ()> = widget::context_menu(
                widget::Space::new().width(300.0).height(100.0),
                Some(items),
            ).into();
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, viewport.size()),
            );
            let pointer = Point::new(10.0, 10.0);
            let cursor = mouse::Cursor::Available(pointer);
            let mut messages = Vec::new();
            let mut shell = Shell::new(&mut messages);
            element.as_widget_mut().update(
                &mut tree,
                &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right)),
                Layout::new(&node),
                cursor,
                &renderer,
                &mut clipboard::Null,
                &mut shell,
                &viewport,
            );
            let mut overlay = element.as_widget_mut().overlay(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &viewport,
                Vector::ZERO,
            ).expect("right click mounts the actual toolkit context menu");
            let surface = layout::Node::new(viewport.size());
            overlay.as_overlay_mut().update(
                &Event::Mouse(mouse::Event::CursorMoved { position: pointer }),
                Layout::new(&surface),
                cursor,
                &renderer,
                &mut clipboard::Null,
                &mut shell,
            );
            let menu = overlay.as_overlay_mut().layout(&renderer, viewport.size());
            menu.children().first().expect("context menu row laid out").size().height
        };

        configure_typography("", 13.0, "", 14.0);
        let small_label = text_height(widget::text("Rename pane").into());
        let small_menu = menu_height();
        let small_root = text_height(widget::menu::root::<(), Renderer>("Pane").into());
        let small_checkbox = text_height(crate::ui_controls::field::checkbox(false).label("Choice").into());
        let small_toggle = text_height(crate::ui_controls::field::toggle(false).label("Choice").into());
        let small_button = text_height(crate::ui_controls::button::secondary("Action").into());
        let small_caption = text_height(caption("Caption").into());
        let toolkit_label_height = || {
            use cosmic::iced::advanced::widget::Operation;
            struct Label(Option<f32>);
            impl Operation for Label {
                fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) { visit(self); }
                fn text(&mut self, _: Option<&widget::Id>, bounds: Rectangle, text: &str) {
                    if text == "Toolkit action" { self.0 = Some(bounds.height); }
                }
            }
            let mut element: cosmic::Element<'_, ()> = widget::button::text("Toolkit action").into();
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(&mut tree, &renderer, &layout::Limits::new(Size::ZERO, viewport.size()));
            let mut label = Label(None);
            element.as_widget_mut().operate(&mut tree, Layout::new(&node), &renderer, &mut label);
            label.0.expect("actual toolkit button label mounted")
        };
        let small_toolkit_label = toolkit_label_height();
        let explicit = text_height(widget::text("Fixed caption").size(12.0).into());
        configure_typography("", 24.0, "", 14.0);
        let large_label = text_height(widget::text("Rename pane").into());
        let large_menu = menu_height();
        let large_root = text_height(widget::menu::root::<(), Renderer>("Pane").into());
        assert!(large_label > small_label * 1.5, "unsized toolkit labels inherit interface size");
        assert!(large_root > small_root, "actual menu roots inherit interface size");
        assert!(large_menu > small_menu, "context-menu rows grow with their labels");
        assert!(large_menu >= large_label + 8.0, "menu padding must not clip the line box");
        assert!(text_height(crate::ui_controls::field::checkbox(false).label("Choice").into()) > small_checkbox);
        assert!(text_height(crate::ui_controls::field::toggle(false).label("Choice").into()) > small_toggle);
        let large_button = text_height(crate::ui_controls::button::secondary("Action").into());
        assert!(large_button > small_button && large_button >= interface_size() + 5.0);
        assert!(toolkit_label_height() > small_toolkit_label * 1.5, "toolkit buttons inherit interface size");
        let large_caption = text_height(caption("Caption").into());
        assert!(large_caption > small_caption);
        assert!((large_caption - scaled_interface_size(17.0)).abs() < 0.01, "caption line box follows its type size");
        assert_eq!(text_height(widget::text("Fixed caption").size(12.0).into()), explicit);
        configure_typography("", 13.0, "", 14.0);
    }

    #[test]
    fn interface_and_chat_preferences_reach_rendered_text_and_markdown() {
        use cosmic::iced::{
            Size,
            advanced::{layout, widget::Tree},
        };
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        configure_typography("", 13.0, "", 14.0);
        let height = || {
            let mut label: cosmic::Element<'_, ()> = body("Workbench label").into();
            let mut tree = Tree::new(label.as_widget());
            label
                .as_widget_mut()
                .layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, Size::new(600.0, 100.0)),
                )
                .size()
                .height
        };
        let before = height();
        let revision = typography_revision();
        configure_typography("DejaVu Sans Mono", 18.0, "DejaVu Serif", 21.0);
        assert!(height() > before);
        assert_eq!(
            interface_font().family,
            cosmic::iced::font::Family::Name("DejaVu Sans Mono")
        );
        let markdown = markdown_settings(&Theme::dark(), TEXT_PROSE);
        assert_eq!(markdown.text_size.0, 21.0);
        assert_eq!(
            markdown.style.font.family,
            cosmic::iced::font::Family::Name("DejaVu Serif")
        );
        assert!(
            typography_revision() > revision,
            "retained transcripts must refresh"
        );
        configure_typography("", 13.0, "", 14.0);
        let desktop = interface_font();
        configure_typography(
            "Cagents Missing Font Fixture",
            13.0,
            "Cagents Missing Font Fixture",
            14.0,
        );
        assert_eq!(interface_font(), desktop);
        assert_eq!(chat_font(), desktop);
        configure_typography("", 13.0, "", 14.0);
    }
}
