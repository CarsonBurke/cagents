//! What a drag from another application carries, and the text a pane inserts
//! for it.
//!
//! A file manager offers dropped files as a `text/uri-list` of `file://` URIs;
//! a text source offers plain text. A terminal takes either the way Ghostty,
//! VTE and kitty do: each file's path, escaped so that a shell reads it back
//! as one word, followed by a space, sent as a paste so that a child which
//! asked for bracketed paste - Claude Code, for one, which is how it turns a
//! dropped image into an attachment - sees one. Nothing ends a file drop with
//! a newline, and the terminal types a drop into a child that did not ask for
//! bracketed paste with every line break and control key blanked out (see
//! [`crate::terminal::PasteOrigin::Drop`]), so a drop never runs anything by
//! itself. A chat composer is not a shell, and takes the same names as they
//! are.

use std::{
    borrow::Cow,
    hash::{DefaultHasher, Hash, Hasher},
    path::PathBuf,
};

use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget,
    clipboard::DndDestinationRectangles,
    layout, overlay, renderer,
    widget::{Operation, Tree},
};
use cosmic::iced::{Event, Length, Rectangle, Size, Vector, clipboard::dnd::DndAction, mouse};
use cosmic::{Element, Renderer, Theme, widget};

const URI_LIST: &str = "text/uri-list";

/// The drag formats a pane that inserts text accepts, most preferred first.
///
/// The compositor hands over the first of these the source offers, so a file
/// manager's list of URIs wins over the same files spelled as plain text,
/// whose paths could not be told apart from the spaces in them.
const MIME_TYPES: [&str; 4] = [
    URI_LIST,
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
];

/// Makes `content` the pane a drop lands in while the pointer is over it.
///
/// The wrapper is there whether or not the pane is `accepting`, because iced
/// matches widget state by position and a wrapper that came and went would
/// reset the pane's own state each time. A pane that is not accepting offers
/// no action, which the compositor shows the dragging reader as a refusal.
/// Only a copy is offered: a file manager told the drop was a move would be
/// entitled to delete the files it dragged.
///
/// `key` names the pane, and must be the same every time the pane is built.
/// The compositor keeps addressing a drag to the target it entered, by the id
/// the target registered then, until the pointer next moves; a pane whose id
/// changed with every rebuild - and a busy terminal rebuilds constantly -
/// would lose a drop released between two of them.
pub fn target<'a, Message: 'static>(
    content: impl Into<Element<'a, Message>>,
    key: impl Hash,
    accepting: bool,
    on_drop: impl Fn(Payload) -> Message + 'static,
) -> Element<'a, Message> {
    widget::dnd_destination(content, MIME_TYPES.into_iter().map(Cow::Borrowed).collect())
        .drag_id(drag_id(key))
        .action(if accepting {
            DndAction::Copy
        } else {
            DndAction::empty()
        })
        .preferred_action(DndAction::Copy)
        .on_finish(move |mime, data, _, _, _| on_drop(Payload::new(mime, data)))
        .into()
}

/// A drop target which reports entry and departure for attachment feedback.
pub fn target_with_hover<'a, Message: 'static>(
    content: impl Into<Element<'a, Message>>,
    key: impl Hash,
    accepting: bool,
    on_drop: impl Fn(Payload) -> Message + 'static,
    on_hover: impl Fn(bool) -> Message + 'static,
) -> Element<'a, Message> {
    let on_hover = std::sync::Arc::new(on_hover);
    let entering = on_hover.clone();
    widget::dnd_destination(content, MIME_TYPES.into_iter().map(Cow::Borrowed).collect())
        .drag_id(drag_id(key))
        .action(if accepting {
            DndAction::Copy
        } else {
            DndAction::empty()
        })
        .preferred_action(DndAction::Copy)
        .on_enter(move |_, _, _| entering(accepting))
        .on_leave(move || on_hover(false))
        .on_finish(move |mime, data, _, _, _| on_drop(Payload::new(mime, data)))
        .into()
}

/// A drag id for the target named `key`.
///
/// Ids are shared by every drop target in the window. Iced numbers the ones
/// it allocates itself up from zero, so a key's id is kept to the upper half
/// of the range, clear of them.
fn drag_id(key: impl Hash) -> u64 {
    // `DefaultHasher::new` is unkeyed, so a key hashes the same way on every
    // rebuild.
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish() | 1 << 63
}

/// Places the drop targets inside `content` where the compositor looks for
/// them, for a window whose interface is drawn at `scale`.
///
/// Iced lays the interface out in units already divided by the interface
/// scale, while the compositor hit-tests a drag in the surface's own units, so
/// at any interface scale but one each target would otherwise sit, shrunk,
/// toward the window's top-left corner and a drop would land in whichever pane
/// happened to be under that misplaced rectangle.
pub fn surface<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    scale: f32,
) -> Element<'a, Message> {
    Element::new(Surface {
        content: content.into(),
        scale: f64::from(scale),
    })
}

struct Surface<'a, Message> {
    content: Element<'a, Message>,
    scale: f64,
}

impl<Message> Widget<Message, Theme, Renderer> for Surface<'_, Message> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }

    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        self.content
            .as_widget()
            .a11y_nodes(layout, &tree.children[0], cursor)
    }

    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        destinations: &mut DndDestinationRectangles,
    ) {
        let mut inner = DndDestinationRectangles::new();
        self.content
            .as_widget()
            .drag_destinations(&tree.children[0], layout, renderer, &mut inner);
        let mut rectangles = inner.into_rectangles();
        for destination in &mut rectangles {
            let rectangle = &mut destination.rectangle;
            rectangle.x *= self.scale;
            rectangle.y *= self.scale;
            rectangle.width *= self.scale;
            rectangle.height *= self.scale;
        }
        destinations.append(&mut rectangles);
    }
}

/// Dropped data as it arrived, in the format the drag delivered it in.
#[derive(Clone)]
pub struct Payload {
    mime: String,
    data: Vec<u8>,
}

impl std::fmt::Debug for Payload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // App-message diagnostics must never expose the dropped paths or text.
        formatter
            .debug_struct("Payload")
            .field("mime", &self.mime)
            .field("bytes", &self.data.len())
            .finish()
    }
}

impl Payload {
    pub fn new(mime: String, data: Vec<u8>) -> Self {
        Self { mime, data }
    }

    pub fn decode(&self) -> Dropped {
        if self.mime == URI_LIST {
            Dropped::Items(uri_list(
                &self.data,
                crate::file_url::host_name().as_deref(),
            ))
        } else {
            // A source that NUL-terminates its string, as some X11 clients
            // still do, means nothing by the terminator.
            let text = self.data.strip_suffix(b"\0").unwrap_or(&self.data);
            Dropped::Text(String::from_utf8_lossy(text).into_owned())
        }
    }
}

/// What a drop means once decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dropped {
    /// The entries of a URI list, in the order the source listed them.
    Items(Vec<Item>),
    /// Text, inserted as it is, the way a paste of it would be.
    Text(String),
}

/// One entry of a dropped URI list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    /// A file on this machine, its path decoded from the URI.
    File(PathBuf),
    /// Any other URI - a web address dragged out of a browser, or a file on
    /// another host - which is inserted as the URI itself.
    Uri(String),
    /// An entry that is not a URI at all.
    Unreadable,
}

/// The text a drop inserts, and how many of its entries it had to leave out.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Insertion {
    pub text: String,
    pub refused: usize,
}

impl Insertion {
    /// Why entries were left out, for the reader who dropped them.
    pub fn refusal(&self) -> Option<String> {
        match self.refused {
            0 => None,
            1 => Some(
                "Skipped a dropped item: it is not a readable address, or its name has control \
                 characters or is not valid UTF-8, so it cannot be inserted as typed text."
                    .into(),
            ),
            count => Some(format!(
                "Skipped {count} dropped items: they are not readable addresses, or their names \
                 have control characters or are not valid UTF-8, so they cannot be inserted as \
                 typed text."
            )),
        }
    }
}

/// How the entries of a dropped list are written into the text they join.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spelling {
    /// Escaped so that a shell - or an agent CLI that reads its input the way
    /// a shell would - reads each entry back as one word.
    Shell,
    /// As they are, for prose such as a chat message.
    Plain,
}

impl Dropped {
    /// The text to insert for this drop.
    ///
    /// Each entry becomes one word, spelled as `spelling` says, followed by a
    /// space so that the next drop or the next argument does not run into it.
    /// A name that could only be inserted as control characters is left out
    /// rather than altered: a newline in it would be an Enter to any child not
    /// in bracketed paste mode, and inserting some other name than the one
    /// dropped would be worse than inserting none.
    pub fn insertion(&self, spelling: Spelling) -> Insertion {
        match self {
            Self::Text(text) => Insertion {
                text: text.clone(),
                refused: 0,
            },
            Self::Items(items) => {
                let mut insertion = Insertion::default();
                for item in items {
                    let word = match item {
                        Item::File(path) => path.to_str(),
                        Item::Uri(uri) => Some(uri.as_str()),
                        Item::Unreadable => None,
                    };
                    match word.filter(|word| !word.chars().any(char::is_control)) {
                        Some(word) => {
                            match spelling {
                                Spelling::Shell => insertion.text.push_str(&shell_escape(word)),
                                Spelling::Plain => insertion.text.push_str(word),
                            }
                            insertion.text.push(' ');
                        }
                        None => insertion.refused += 1,
                    }
                }
                insertion
            }
        }
    }
}

/// Parses a `text/uri-list` (RFC 2483): one URI per line, `#` lines are
/// comments. Lines end in CRLF by the letter of the format and in a bare LF
/// from more than one real source, so both are accepted.
///
/// A file URI names this machine by an empty host or `localhost`, and some
/// sources - KDE's among them - by `local_host`, the machine's own name.
fn uri_list(data: &[u8], local_host: Option<&str>) -> Vec<Item> {
    data.split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .filter(|line| !line.is_empty() && !line.starts_with(b"#"))
        .map(|line| {
            let Some(mut uri) = std::str::from_utf8(line)
                .ok()
                .and_then(|line| url::Url::parse(line).ok())
            else {
                return Item::Unreadable;
            };
            if uri.scheme() == "file" {
                // The path is percent-decoded to the file name's own bytes,
                // which need not be UTF-8. A host other than this one names
                // a file this machine cannot open, so it stays a URI.
                crate::file_url::localize(&mut uri, || local_host);
                if let Ok(path) = uri.to_file_path() {
                    return Item::File(path);
                }
            }
            Item::Uri(uri.into())
        })
        .collect()
}

/// Escapes `word` so that a POSIX shell, zsh or fish reads it back as one
/// literal word.
///
/// Every ASCII character outside a conservative set that no shell treats
/// specially gets a backslash, the form Ghostty and macOS Terminal insert and
/// the one agent CLIs undo when they recognise a dropped path. Characters
/// beyond ASCII are left alone: no shell splits or expands on them, and a
/// reader should see their file's name, not an encoding of it. Control
/// characters have no backslash form every shell agrees on; see
/// [`Dropped::insertion`].
fn shell_escape(word: &str) -> Cow<'_, str> {
    let plain = |character: char| {
        !character.is_ascii()
            || character.is_ascii_alphanumeric()
            || matches!(
                character,
                '/' | '.' | '_' | '-' | '+' | ',' | ':' | '@' | '%' | '='
            )
    };
    if word.chars().all(plain) {
        return Cow::Borrowed(word);
    }
    let mut escaped = String::with_capacity(word.len() + 8);
    for character in word.chars() {
        if !plain(character) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    Cow::Owned(escaped)
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    use super::*;

    fn dropped(mime: &str, data: &[u8]) -> Dropped {
        Payload::new(mime.into(), data.to_vec()).decode()
    }

    fn inserted(mime: &str, data: &[u8]) -> Insertion {
        dropped(mime, data).insertion(Spelling::Shell)
    }

    #[test]
    fn a_dropped_file_becomes_its_path_and_a_space() {
        assert_eq!(
            inserted(URI_LIST, b"file:///home/reader/notes.txt\r\n"),
            Insertion {
                text: "/home/reader/notes.txt ".into(),
                refused: 0,
            }
        );
    }

    #[test]
    fn several_files_are_separate_words_in_the_order_dropped() {
        assert_eq!(
            inserted(
                URI_LIST,
                b"file:///tmp/b.png\r\nfile:///tmp/a%20b.png\r\nfile:///tmp/c\r\n"
            )
            .text,
            "/tmp/b.png /tmp/a\\ b.png /tmp/c "
        );
    }

    #[test]
    fn a_uri_list_takes_bare_line_feeds_comments_and_no_final_newline() {
        assert_eq!(
            dropped(URI_LIST, b"# from a file manager\nfile:///a\n\nfile:///b"),
            Dropped::Items(vec![Item::File("/a".into()), Item::File("/b".into())])
        );
    }

    #[test]
    fn percent_encoding_is_decoded_to_the_names_own_bytes() {
        assert_eq!(
            dropped(
                URI_LIST,
                b"file:///tmp/%C3%A9t%C3%A9%20%E6%97%A5%E6%9C%AC.txt"
            ),
            Dropped::Items(vec![Item::File("/tmp/été 日本.txt".into())])
        );
        // Unicode is inserted as itself; only the space is escaped.
        assert_eq!(
            inserted(
                URI_LIST,
                b"file:///tmp/%C3%A9t%C3%A9%20%E6%97%A5%E6%9C%AC.txt"
            )
            .text,
            "/tmp/été\\ 日本.txt "
        );
        // A name that is not UTF-8 decodes exactly, though no terminal can be
        // sent it as text.
        assert_eq!(
            dropped(URI_LIST, b"file:///tmp/caf%E9"),
            Dropped::Items(vec![Item::File(
                OsString::from_vec(b"/tmp/caf\xe9".to_vec()).into()
            )])
        );
    }

    #[test]
    fn shell_metacharacters_cannot_split_or_expand_a_name() {
        let name = "/tmp/it's $(rm -rf ~) `id` a;b|c&d>e<f *?[x]{y} !#~\"\\ ^";
        let escaped = shell_escape(name);
        assert_eq!(
            escaped,
            "/tmp/it\\'s\\ \\$\\(rm\\ -rf\\ \\~\\)\\ \\`id\\`\\ a\\;b\\|c\\&d\\>e\\<f\\ \
             \\*\\?\\[x\\]\\{y\\}\\ \\!\\#\\~\\\"\\\\\\ \\^"
        );
        // Undoing the escapes gives the name back, so a shell reads one word.
        let mut unescaped = String::new();
        let mut characters = escaped.chars();
        while let Some(character) = characters.next() {
            unescaped.push(if character == '\\' {
                characters.next().unwrap()
            } else {
                character
            });
        }
        assert_eq!(unescaped, name);
        assert!(matches!(
            shell_escape("/home/a-b_c/x.y+z,1:2@h%3=4"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn a_name_with_control_characters_is_refused_not_rewritten() {
        let insertion = inserted(
            URI_LIST,
            b"file:///tmp/evil%0Arm%20-rf%20~\r\n\
              file:///tmp/ok\r\n\
              file:///tmp/esc%1B%5B201~\r\n\
              file:///tmp/tab%09\r\n\
              file:///tmp/c1%C2%9B\r\n\
              file:///tmp/cr%0D\r\n",
        );
        assert_eq!(insertion.text, "/tmp/ok ");
        assert_eq!(insertion.refused, 5);
        assert!(
            insertion
                .refusal()
                .unwrap()
                .starts_with("Skipped 5 dropped items")
        );
    }

    #[test]
    fn a_name_that_is_not_utf8_is_refused() {
        let insertion = inserted(URI_LIST, b"file:///tmp/caf%E9\r\nfile:///tmp/ok\r\n");
        assert_eq!(insertion.text, "/tmp/ok ");
        assert_eq!(insertion.refused, 1);
        assert!(
            insertion
                .refusal()
                .unwrap()
                .starts_with("Skipped a dropped item")
        );
    }

    #[test]
    fn other_uris_are_inserted_as_themselves() {
        assert_eq!(
            dropped(
                URI_LIST,
                b"https://example.com/a?b=c&d#e\r\nfile://elsewhere.example/share/x\r\n"
            ),
            Dropped::Items(vec![
                Item::Uri("https://example.com/a?b=c&d#e".into()),
                Item::Uri("file://elsewhere.example/share/x".into()),
            ])
        );
        assert_eq!(
            inserted(URI_LIST, b"https://example.com/a?b=c&d#e").text,
            "https://example.com/a\\?b=c\\&d\\#e "
        );
        // `localhost` is this machine.
        assert_eq!(
            dropped(URI_LIST, b"file://localhost/tmp/x"),
            Dropped::Items(vec![Item::File("/tmp/x".into())])
        );
    }

    #[test]
    fn a_line_that_is_not_a_uri_is_counted_not_inserted() {
        let insertion = inserted(URI_LIST, b"not a uri\r\n\xff\xfe\r\nfile:///ok\r\n");
        assert_eq!(insertion.text, "/ok ");
        assert_eq!(insertion.refused, 2);
    }

    #[test]
    fn a_file_uri_naming_this_machine_is_a_local_file() {
        assert_eq!(
            uri_list(
                b"file://Workstation/tmp/a%20b\r\nfile://elsewhere/tmp/c\r\n",
                Some("workstation")
            ),
            vec![
                Item::File("/tmp/a b".into()),
                Item::Uri("file://elsewhere/tmp/c".into()),
            ]
        );
        // Without a name for this machine only the spellings every source
        // agrees on are local.
        assert_eq!(
            uri_list(b"file://workstation/tmp/a", None),
            vec![Item::Uri("file://workstation/tmp/a".into())]
        );
    }

    #[test]
    fn prose_takes_the_names_as_they_are() {
        let insertion = dropped(
            URI_LIST,
            b"file:///tmp/a%20b.png\r\nhttps://example.com/a?b=c&d#e\r\nfile:///tmp/x%0Ay\r\n",
        )
        .insertion(Spelling::Plain);
        assert_eq!(
            insertion.text,
            "/tmp/a b.png https://example.com/a?b=c&d#e "
        );
        // A control character is no more insertable into a draft.
        assert_eq!(insertion.refused, 1);
    }

    #[test]
    fn dropped_text_is_inserted_as_a_paste_of_it_would_be() {
        assert_eq!(
            inserted("text/plain;charset=utf-8", b"echo 'hi'\nthere\0"),
            Insertion {
                text: "echo 'hi'\nthere".into(),
                refused: 0,
            }
        );
        assert_eq!(inserted("UTF8_STRING", b"").text, "");
    }

    #[test]
    fn diagnostics_never_show_what_was_dropped() {
        let payload = Payload::new(URI_LIST.into(), b"file:///home/reader/secret".to_vec());
        let debug = format!("{payload:?}");
        assert!(!debug.contains("secret"), "{debug}");
        assert!(debug.contains("text/uri-list"), "{debug}");
    }
}
