//! Immutable native-line snapshots. Editing replaces only affected lines and
//! logarithmically many AVL ancestors; cloning a snapshot never copies text.
use super::{Line, LineEnding, Position};
use std::{
    borrow::Cow,
    fmt,
    sync::{Arc, OnceLock},
};

#[derive(Clone)]
struct SavedLine {
    text: Arc<str>,
    ending: LineEnding,
    terminator: &'static str,
    hash: u64,
    power: u64,
}

enum Node {
    Line(SavedLine),
    Branch {
        left: Arc<Node>,
        right: Arc<Node>,
        height: u8,
        lines: usize,
        bytes: usize,
        compatible: bool,
        hash: u64,
        power: u64,
    },
}
impl Node {
    fn height(&self) -> u8 {
        match self {
            Self::Line(_) => 1,
            Self::Branch { height, .. } => *height,
        }
    }
    fn lines(&self) -> usize {
        match self {
            Self::Line(_) => 1,
            Self::Branch { lines, .. } => *lines,
        }
    }
    fn bytes(&self) -> usize {
        match self {
            Self::Line(line) => line.text.len() + line.terminator.len(),
            Self::Branch { bytes, .. } => *bytes,
        }
    }
    fn hash(&self) -> u64 {
        match self {
            Self::Line(line) => line.hash,
            Self::Branch { hash, .. } => *hash,
        }
    }
    fn power(&self) -> u64 {
        match self {
            Self::Line(line) => line.power,
            Self::Branch { power, .. } => *power,
        }
    }
    fn compatible(&self) -> bool {
        match self {
            Self::Line(line) => matches!(
                line.ending,
                LineEnding::Lf | LineEnding::CrLf | LineEnding::None
            ),
            Self::Branch { compatible, .. } => *compatible,
        }
    }
    fn byte_offset(&self, position: Position) -> Option<usize> {
        match self {
            Self::Line(line) => (position.line == 0
                && position.column <= line.text.len()
                && line.text.is_char_boundary(position.column))
            .then_some(position.column),
            Self::Branch { left, right, .. } => {
                if position.line < left.lines() {
                    left.byte_offset(position)
                } else {
                    right
                        .byte_offset(Position {
                            line: position.line - left.lines(),
                            column: position.column,
                        })
                        .map(|offset| left.bytes() + offset)
                }
            }
        }
    }
    fn children(&self) -> (&Arc<Node>, &Arc<Node>) {
        match self {
            Self::Branch { left, right, .. } => (left, right),
            Self::Line(_) => unreachable!("only branches need balancing"),
        }
    }
    fn line(&self, index: usize) -> Option<&SavedLine> {
        match self {
            Self::Line(line) => (index == 0).then_some(line),
            Self::Branch { left, right, .. } => {
                if index < left.lines() {
                    left.line(index)
                } else {
                    right.line(index - left.lines())
                }
            }
        }
    }
    fn write(&self, text: &mut String) {
        match self {
            Self::Line(line) => {
                text.push_str(&line.text);
                text.push_str(line.terminator);
            }
            Self::Branch { left, right, .. } => {
                left.write(text);
                right.write(text);
            }
        }
    }
}
fn branch(left: Arc<Node>, right: Arc<Node>) -> Arc<Node> {
    Arc::new(Node::Branch {
        height: left.height().max(right.height()) + 1,
        lines: left.lines() + right.lines(),
        bytes: left.bytes() + right.bytes(),
        compatible: left.compatible() && right.compatible(),
        hash: left
            .hash()
            .wrapping_mul(right.power())
            .wrapping_add(right.hash()),
        power: left.power().wrapping_mul(right.power()),
        left,
        right,
    })
}
fn balance(left: Arc<Node>, right: Arc<Node>) -> Arc<Node> {
    if left.height() > right.height() + 1 {
        let (a, b) = left.children();
        if a.height() >= b.height() {
            branch(Arc::clone(a), branch(Arc::clone(b), right))
        } else {
            let (b, c) = b.children();
            branch(
                branch(Arc::clone(a), Arc::clone(b)),
                branch(Arc::clone(c), right),
            )
        }
    } else if right.height() > left.height() + 1 {
        let (b, c) = right.children();
        if c.height() >= b.height() {
            branch(branch(left, Arc::clone(b)), Arc::clone(c))
        } else {
            let (a, b) = b.children();
            branch(
                branch(left, Arc::clone(a)),
                branch(Arc::clone(b), Arc::clone(c)),
            )
        }
    } else {
        branch(left, right)
    }
}
fn join(left: Option<Arc<Node>>, right: Option<Arc<Node>>) -> Option<Arc<Node>> {
    let (left, right) = match (left, right) {
        (None, right) => return right,
        (left, None) => return left,
        (Some(left), Some(right)) => (left, right),
    };
    Some(if left.height() > right.height() + 1 {
        let (a, b) = left.children();
        balance(
            Arc::clone(a),
            join(Some(Arc::clone(b)), Some(right)).expect("nonempty join"),
        )
    } else if right.height() > left.height() + 1 {
        let (b, c) = right.children();
        balance(
            join(Some(left), Some(Arc::clone(b))).expect("nonempty join"),
            Arc::clone(c),
        )
    } else {
        branch(left, right)
    })
}
fn split(root: Option<Arc<Node>>, at: usize) -> (Option<Arc<Node>>, Option<Arc<Node>>) {
    let Some(root) = root else {
        return (None, None);
    };
    if at == 0 {
        return (None, Some(root));
    }
    if at >= root.lines() {
        return (Some(root), None);
    }
    let (left, right) = root.children();
    if at < left.lines() {
        let (a, b) = split(Some(Arc::clone(left)), at);
        (a, join(b, Some(Arc::clone(right))))
    } else {
        let (a, b) = split(Some(Arc::clone(right)), at - left.lines());
        (join(Some(Arc::clone(left)), a), b)
    }
}
fn build(lines: &[SavedLine]) -> Option<Arc<Node>> {
    match lines.len() {
        0 => None,
        1 => Some(Arc::new(Node::Line(lines[0].clone()))),
        _ => {
            let middle = lines.len() / 2;
            Some(branch(
                build(&lines[..middle]).unwrap(),
                build(&lines[middle..]).unwrap(),
            ))
        }
    }
}

struct Chunks<'a> {
    stack: Vec<&'a Node>,
    terminator: Option<&'a str>,
}
impl<'a> Chunks<'a> {
    fn new(root: Option<&'a Node>) -> Self {
        Self {
            stack: root.into_iter().collect(),
            terminator: None,
        }
    }
}
impl<'a> Iterator for Chunks<'a> {
    type Item = &'a str;
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(terminator) = self.terminator.take() {
            return Some(terminator);
        }
        while let Some(node) = self.stack.pop() {
            match node {
                Node::Line(line) => {
                    self.terminator = Some(line.terminator);
                    return Some(&line.text);
                }
                Node::Branch { left, right, .. } => {
                    self.stack.push(right);
                    self.stack.push(left);
                }
            }
        }
        None
    }
}

/// A Send, renderer-independent view of exact native lines. Serialization is
/// cached once per revision and may run on a worker while the UI keeps editing.
#[derive(Clone)]
pub struct TextSnapshot {
    root: Option<Arc<Node>>,
    serialized: Arc<OnceLock<Arc<str>>>,
    #[cfg(test)]
    serializations: Arc<std::sync::atomic::AtomicUsize>,
}
impl PartialEq for TextSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.same_text(other)
    }
}
impl Eq for TextSnapshot {}
impl fmt::Debug for TextSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TextSnapshot")
            .field("lines", &self.line_count())
            .field("bytes", &self.len())
            .finish()
    }
}
impl TextSnapshot {
    pub(super) fn from_lines<'a>(
        lines: impl IntoIterator<Item = (&'a str, LineEnding, &'static str)>,
    ) -> Self {
        let lines: Vec<_> = lines
            .into_iter()
            .map(|(text, ending, terminator)| {
                let (hash, power) = text.bytes().chain(terminator.bytes()).fold(
                    (0_u64, 1_u64),
                    |(hash, power), byte| {
                        (
                            hash.wrapping_mul(257).wrapping_add(u64::from(byte) + 1),
                            power.wrapping_mul(257),
                        )
                    },
                );
                SavedLine {
                    text: Arc::from(text),
                    ending,
                    terminator,
                    hash,
                    power,
                }
            })
            .collect();
        Self {
            root: build(&lines),
            serialized: Arc::new(OnceLock::new()),
            #[cfg(test)] serializations: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
    pub fn len(&self) -> usize {
        self.root.as_ref().map_or(0, |root| root.bytes())
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn line_count(&self) -> usize {
        self.root.as_ref().map_or(0, |root| root.lines())
    }
    /// A functional replacement for protocol-compatible native lines. Only
    /// the boundary lines, inserted text and AVL ancestors are copied.
    pub fn replaced(&self, start: Position, end: Position, inserted: &str) -> Option<Self> {
        if !self.protocol_compatible_lines()
            || (start.line, start.column) > (end.line, end.column)
            || inserted.as_bytes().iter().enumerate().any(|(index, byte)| {
                *byte == b'\r' && inserted.as_bytes().get(index + 1) != Some(&b'\n')
            })
        {
            return None;
        }
        let first = self.root.as_ref()?.line(start.line)?;
        let last = self.root.as_ref()?.line(end.line)?;
        let prefix = first.text.get(..start.column)?;
        let suffix = last.text.get(end.column..)?;
        let mut boundary = String::with_capacity(prefix.len() + inserted.len() + suffix.len());
        boundary.push_str(prefix);
        boundary.push_str(inserted);
        boundary.push_str(suffix);
        let mut lines = Vec::new();
        let mut parts = boundary.split('\n').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                lines.push((part, last.ending, last.terminator));
            } else if let Some(text) = part.strip_suffix('\r') {
                lines.push((text, LineEnding::CrLf, "\r\n"));
            } else {
                lines.push((part, LineEnding::Lf, "\n"));
            }
        }
        let mut result = self.clone();
        result.replace_lines(start.line, end.line + 1, lines);
        Some(result)
    }
    /// A shape-independent, constant-time inequality hint. A matching hint
    /// must be confirmed by same_text; it is deliberately not an identity.
    pub fn content_signature(&self) -> (usize, u64) {
        (self.len(), self.root.as_ref().map_or(0, |root| root.hash()))
    }
    pub fn same_text(&self, other: &Self) -> bool {
        if self.content_signature() != other.content_signature() {
            return false;
        }
        match (&self.root, &other.root) {
            (Some(left), Some(right)) if Arc::ptr_eq(left, right) => return true,
            _ => {}
        }
        let mut left: Vec<_> = self.root.as_deref().into_iter().collect();
        let mut right: Vec<_> = other.root.as_deref().into_iter().collect();
        fn expand<'a>(stack:&mut Vec<&'a Node>,node:&'a Node)->bool {
            if let Node::Branch {left,right,..} = node {
                stack.pop();stack.push(right);stack.push(left);true
            } else {false}
        }
        while let (Some(&a),Some(&b)) = (left.last(),right.last()) {
            if std::ptr::eq(a,b) {left.pop();right.pop();continue;}
            if a.bytes() == b.bytes() {
                if let (Node::Line(a),Node::Line(b)) = (a,b) {
                    if !a.text.bytes().chain(a.terminator.bytes()).eq(b.text.bytes().chain(b.terminator.bytes())) {return false;}
                    left.pop();right.pop();continue;
                }
                let expanded_left = expand(&mut left,a);
                let expanded_right = expand(&mut right,b);
                debug_assert!(expanded_left || expanded_right);
            } else if a.bytes() > b.bytes() && expand(&mut left,a) {
                continue;
            } else if b.bytes() > a.bytes() && expand(&mut right,b) {
                continue;
            } else {
                // Equal bytes can have different native line topology. Keep
                // exact semantics in that rare case without materialization.
                return Chunks::new(self.root.as_deref()).flat_map(str::bytes)
                    .eq(Chunks::new(other.root.as_deref()).flat_map(str::bytes));
            }
        }
        left.iter().all(|node| node.bytes()==0) && right.iter().all(|node| node.bytes()==0)
    }
    pub fn protocol_compatible_lines(&self) -> bool {
        self.root.as_ref().is_none_or(|root| root.compatible())
    }
    pub fn byte_offset(&self, position: Position) -> Option<usize> {
        self.root.as_ref()?.byte_offset(position)
    }
    pub fn line(&self, index: usize) -> Option<Line<'_>> {
        let line = self.root.as_ref()?.line(index)?;
        Some(Line {
            text: Cow::Borrowed(&line.text),
            ending: line.ending,
        })
    }
    #[cfg(test)]
    pub fn is_materialized(&self) -> bool { self.serialized.get().is_some() }
    /// Serialize a transient payload without retaining a second full copy
    /// in every historical native snapshot (for example LSP wire messages).
    pub fn to_string_uncached(&self) -> String {
        #[cfg(test)]
        self.serializations.fetch_add(1,std::sync::atomic::Ordering::Relaxed);
        let mut text = String::with_capacity(self.len());
        if let Some(root) = &self.root {root.write(&mut text);}
        text
    }
    #[cfg(test)]
    pub fn serialization_count(&self)->usize {self.serializations.load(std::sync::atomic::Ordering::Relaxed)}
    pub fn text(&self) -> Arc<str> {
        Arc::clone(self.serialized.get_or_init(|| Arc::from(self.to_string_uncached())))
    }
    pub(super) fn replace_lines<'a>(
        &mut self,
        first: usize,
        old_end: usize,
        lines: impl IntoIterator<Item = (&'a str, LineEnding, &'static str)>,
    ) {
        assert!(
            first <= old_end && old_end <= self.line_count(),
            "native snapshot range remains valid"
        );
        let replacement = Self::from_lines(lines);
        let (left, rest) = split(self.root.clone(), first);
        let (_, right) = split(rest, old_end - first);
        self.root = join(join(left, replacement.root), right);
        self.serialized = Arc::new(OnceLock::new());
        #[cfg(test)]
        { self.serializations = Arc::new(std::sync::atomic::AtomicUsize::new(0)); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn assert_balanced(node: &Node) {
        if let Node::Branch {
            left,
            right,
            height,
            lines,
            bytes,
            ..
        } = node
        {
            assert!(left.height().abs_diff(right.height()) <= 1);
            assert_eq!(*height, left.height().max(right.height()) + 1);
            assert_eq!(*lines, left.lines() + right.lines());
            assert_eq!(*bytes, left.bytes() + right.bytes());
            assert_balanced(left);
            assert_balanced(right);
        }
    }
    #[test]
    fn functional_replacements_preserve_native_lines_without_serializing() {
        let original = TextSnapshot::from_lines([
            ("α", LineEnding::CrLf, "\r\n"),
            ("middle", LineEnding::Lf, "\n"),
            ("last", LineEnding::None, ""),
        ]);
        let changed = original
            .replaced(
                Position { line: 0, column: 2 },
                Position { line: 1, column: 3 },
                "insert\nnew",
            )
            .unwrap();
        assert!(!original.is_materialized());
        assert!(!changed.is_materialized());
        assert_eq!(changed.text().as_ref(), "αinsert\nnewdle\nlast");
        assert_eq!(original.text().as_ref(), "α\r\nmiddle\nlast");
        assert!(
            original
                .replaced(
                    Position { line: 0, column: 1 },
                    Position { line: 0, column: 2 },
                    "x"
                )
                .is_none()
        );
        assert!(
            original
                .replaced(
                    Position { line: 1, column: 0 },
                    Position { line: 0, column: 0 },
                    "x"
                )
                .is_none()
        );
        assert!(
            original
                .replaced(
                    Position { line: 0, column: 0 },
                    Position { line: 0, column: 0 },
                    "\r"
                )
                .is_none()
        );
        let at_end = original
            .replaced(
                Position { line: 2, column: 4 },
                Position { line: 2, column: 4 },
                "\r\n",
            )
            .unwrap();
        assert_eq!(at_end.text().as_ref(), "α\r\nmiddle\nlast\r\n");
        assert_eq!(at_end.line_count(), 4);
        if let Some(root) = &at_end.root {
            assert_balanced(root);
        }
    }

    #[test]
    fn persistent_line_edits_keep_old_snapshots_and_balanced_aggregates() {
        let mut values: Vec<_> = (0..1024).map(|n| format!("line{n}\n")).collect();
        let mut snapshot = TextSnapshot::from_lines(
            values
                .iter()
                .map(|s| (s.trim_end_matches('\n'), LineEnding::Lf, "\n")),
        );
        let old = snapshot.clone();
        let old_text = old.text();
        let mut random = 0x12345678_u64;
        for step in 0..300 {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            let first = random as usize % (values.len() + 1);
            let removed = (random as usize / 71 % 5).min(values.len() - first);
            let inserted: Vec<_> = (0..random as usize / 23 % 5)
                .map(|n| format!("edit{step}-{n}\n"))
                .collect();
            snapshot.replace_lines(
                first,
                first + removed,
                inserted
                    .iter()
                    .map(|s| (s.trim_end_matches('\n'), LineEnding::Lf, "\n")),
            );
            values.splice(first..first + removed, inserted);
            assert_eq!(snapshot.text().as_ref(), values.concat());
            assert_eq!(
                snapshot.len(),
                values.iter().map(String::len).sum::<usize>()
            );
            assert_eq!(snapshot.line_count(), values.len());
            let rebuilt = TextSnapshot::from_lines(
                values
                    .iter()
                    .map(|s| (s.trim_end_matches('\n'), LineEnding::Lf, "\n")),
            );
            assert_eq!(snapshot.content_signature(), rebuilt.content_signature());
            assert!(snapshot.same_text(&rebuilt));
            if let Some(root) = &snapshot.root {
                assert_balanced(root);
            }
        }
        assert!(Arc::ptr_eq(&old.text(), &old_text));
        assert_eq!(old.line_count(), 1024);
        assert_eq!(old.line(1000).unwrap().text, "line1000");
        fn send_sync<T: Send + Sync>() {}
        send_sync::<TextSnapshot>();
    }
}

#[cfg(test)]
mod native_contracts {
    use super::super::{Action, Content, Cursor, Edit};
    use super::*;

    #[test]
    fn immutable_native_snapshot_survives_background_serialization_and_replay() {
        for source in ["café\r\n雪\nlast", "a\rb\n\rc\r\n", "", "one\n"] {
            let mut content: Content = Content::with_text(source);
            let snapshot = content.text_snapshot();
            let expected = content.text();
            let worker = std::thread::spawn(move || (snapshot.text(), snapshot));
            content.set_cursor(Cursor {
                position: Position { line: 0, column: 0 },
                selection: None,
            });
            content.perform(Action::Edit(Edit::Paste(Arc::new("inserted\n".to_owned()))));
            let (old, snapshot) = worker.join().unwrap();
            assert_eq!(old.as_ref(), expected);
            assert!(Arc::ptr_eq(&old, &snapshot.text()));
            let current = content.text_snapshot();
            assert_eq!(current.text().as_ref(), content.text());
            assert_eq!(current.line_count(), content.line_count());
            assert_eq!(current.len(), content.text().len());
            let mut bytes = 0;
            for index in 0..current.line_count() {
                let line = current.line(index).unwrap();
                assert_eq!(
                    current.byte_offset(Position {
                        line: index,
                        column: 0
                    }),
                    Some(bytes)
                );
                assert_eq!(line.text, content.line(index).unwrap().text);
                bytes += line.text.len()
                    + super::super::terminator(line.ending, index + 1 == current.line_count())
                        .len();
            }
            assert_eq!(
                current.protocol_compatible_lines(),
                !(source.contains('\r') && !source.contains("café\r\n"))
            );
        }
    }
}
