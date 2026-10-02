//! Naming a command or a skill from the composer.
//!
//! Completion works on the word at the caret, wherever in the draft that is,
//! and choosing a row replaces that word and nothing else. A skill is part of
//! the sentence it is named in, so a reader can name several in one message
//! and go on writing around them; a command is the whole message, so it is
//! offered only where the provider will read it as one.

use crate::chat::{Skill, SkillReach, Snapshot};
use cosmic::widget::text_editor::{self, Cursor, Position};
use std::borrow::Cow;

use super::{COMPACT_COMMAND, PERMISSIONS_COMMAND, permissions_available};

/// The sigil every provider's commands are named with, and the one the
/// composer accepts for skills whatever the provider spells them with.
pub(super) const COMMAND_SIGIL: char = '/';

/// The word being completed: a sigil opening a word, up to the caret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Token {
    pub line: usize,
    /// The byte column of the sigil.
    pub start: usize,
    /// The byte column the word ends at, which may be past the caret when the
    /// reader has gone back into a word to change it.
    pub end: usize,
    pub sigil: char,
    /// What the reader has typed after the sigil, up to the caret.
    pub query: String,
    /// The rest of the name past the caret.
    pub tail: String,
    /// Whether the word is a slash with only whitespace before it in the
    /// whole draft, which is the one place a provider reads a command.
    pub leading: bool,
}

impl Token {
    /// The token the caret is in, or `None` when the reader is not naming
    /// something.
    ///
    /// `line` is the caret's line as the editor holds it, and the caret's
    /// column is a byte offset into it. `earlier_blank` answers whether every
    /// line above it is blank; it is only asked for a slash opening its line,
    /// so the common keystroke reads nothing but the line being typed on.
    pub fn at(
        line: &str,
        caret: Position,
        sigils: &[char],
        earlier_blank: impl FnOnce() -> bool,
    ) -> Option<Self> {
        // A column the editor reports is always on a character boundary; one
        // that is not means the draft changed under it, and there is nothing
        // sensible to complete.
        let before = line.get(..caret.column)?;
        let start = before
            .char_indices()
            .rev()
            .find(|(_, character)| character.is_whitespace())
            .map_or(0, |(index, character)| index + character.len_utf8());
        let word = &before[start..];
        let sigil = word.chars().next().filter(|sigil| sigils.contains(sigil))?;
        let query = &word[sigil.len_utf8()..];
        // Anything but a name inside the word makes it a path, an expression
        // or punctuation, `/usr/bin` or `$HOME/x`, rather than a name.
        let name_character =
            |character| is_name_character(character) || (sigil == '@' && character == '/');
        if !query.chars().all(name_character) {
            return None;
        }
        // The word runs on past the caret as far as the name does, so a comma
        // or a closing parenthesis after it stays where it is. A sigil typed
        // just ahead of a word names something of its own, and the word it
        // was typed against stays.
        let after = &line[caret.column..];
        let rest = if query.is_empty() {
            0
        } else {
            after
                .find(|character: char| !name_character(character))
                .unwrap_or(after.len())
        };
        if after[rest..].starts_with(sigils) {
            return None;
        }
        let leading = sigil == COMMAND_SIGIL && line[..start].trim().is_empty() && earlier_blank();
        Some(Self {
            line: caret.line,
            start,
            end: caret.column + rest,
            sigil,
            query: query.to_owned(),
            tail: after[..rest].to_owned(),
            leading,
        })
    }

    /// Keeps the part of the word past the caret only when the whole word is
    /// still part of a name among `rows`, as when the reader goes back into a
    /// name to change it. Otherwise it is a word of its own the sigil was
    /// typed against, as `/re|please` is, and choosing leaves it be.
    pub fn fit(&mut self, rows: &[Candidate]) {
        if self.tail.is_empty() {
            return;
        }
        let word = format!("{}{}", self.query, self.tail).to_lowercase();
        if !rows
            .iter()
            .any(|row| row.names().any(|name| name.to_lowercase().contains(&word)))
        {
            self.end -= self.tail.len();
            self.tail.clear();
        }
    }

    /// Whether the word, with the caret at its end, already sends what
    /// choosing `row` would put there, so there is nothing left for choosing
    /// it to do.
    ///
    /// The word is judged as it is sent: `/commits` goes to Codex as its
    /// `$commits`, and `/review` to OMP as its `/skill:review`. A command in
    /// another case is not the command, since it is sent as typed, nor is
    /// the bare name two skills share either of their links.
    pub fn spells(&self, row: &Candidate, skills: &[Skill]) -> bool {
        self.tail.is_empty()
            && invocations(&format!("{}{}", self.sigil, self.query), skills)
                == invocations(&row.insertion, skills)
    }

    /// The selection covering the word, from its sigil to its end, which is
    /// what a chosen row is pasted over.
    pub fn selection(&self) -> Cursor {
        Cursor {
            position: Position {
                line: self.line,
                column: self.end,
            },
            selection: Some(Position {
                line: self.line,
                column: self.start,
            }),
        }
    }
}

/// What a row names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// One of the provider's commands, which the provider reads only as the
    /// whole of a message.
    Command,
    /// A skill, which the provider reads anywhere in a message.
    Skill,
    File,
}

/// Fuzzy subsequence matching preserves path boundaries in the inserted name.
pub(super) fn files<'a>(paths: &'a [String], query: &str) -> Vec<Candidate<'a>> {
    let query = query.to_lowercase();
    let mut matches = paths
        .iter()
        .filter_map(|path| {
            let lower = path.to_lowercase();
            let mut remaining = query.chars();
            let mut next = remaining.next();
            let mut gaps = 0usize;
            for character in lower.chars() {
                if next == Some(character) {
                    next = remaining.next();
                } else if next.is_some() {
                    gaps += 1;
                }
            }
            next.is_none().then_some((gaps, path))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|(a, path_a), (b, path_b)| {
        a.cmp(b)
            .then_with(|| path_a.len().cmp(&path_b.len()))
            .then_with(|| path_a.cmp(path_b))
    });
    matches
        .into_iter()
        .take(100)
        .map(|(_, path)| Candidate {
            kind: Kind::File,
            name: path,
            insertion: Cow::Borrowed(path),
            description: "Worktree file",
        })
        .collect()
}

/// One row of the completion list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Candidate<'a> {
    pub kind: Kind,
    /// The name the row is matched by.
    pub name: &'a str,
    /// What the row puts in the draft. A path-qualified Codex skill is shown
    /// by name in the picker while retaining its exact insertion text.
    pub insertion: Cow<'a, str>,
    pub description: &'a str,
}

impl<'a> Candidate<'a> {
    pub fn label(&self) -> Cow<'a, str> {
        if self.kind == Kind::Skill && self.insertion.starts_with("[$") {
            Cow::Owned(format!("${}", self.name))
        } else {
            self.insertion.clone()
        }
    }

    /// The invocation without its sigil, as `skill:review` is OMP's
    /// `/skill:review`; `None` for a link.
    fn bare(&self) -> Option<&str> {
        bare(&self.insertion)
    }

    /// Every name the row is matched by: its own, and the provider's
    /// spelling of it when that differs.
    fn names(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.name).chain(self.bare().filter(|bare| *bare != self.name))
    }
}

/// `invocation` without the sigil it opens with, or `None` for one that is
/// not a sigil and a name, such as Codex's `[$name](path)` link.
fn bare(invocation: &str) -> Option<&str> {
    let sigil = invocation
        .chars()
        .next()
        .filter(|sigil| !sigil.is_alphanumeric() && *sigil != '[')?;
    Some(&invocation[sigil.len_utf8()..])
}

/// The sigil a skill's invocation opens with: `$` for Codex's `$name`, and
/// for its `[$name](path)`, which names one of two skills sharing a name.
fn sigil_of(invocation: &str) -> Option<char> {
    invocation
        .strip_prefix('[')
        .unwrap_or(invocation)
        .chars()
        .next()
        .filter(|sigil| !sigil.is_alphanumeric())
}

/// The sigils that open a completion for this provider: the command slash,
/// and whatever the provider spells its skills with.
pub(super) fn sigils(snapshot: &Snapshot) -> Vec<char> {
    let mut sigils = vec![COMMAND_SIGIL, '@'];
    for skill in &snapshot.skills {
        if let Some(sigil) = sigil_of(&skill.invocation)
            && !sigils.contains(&sigil)
        {
            sigils.push(sigil);
        }
    }
    sigils
}

/// The rows offered for `token`, best first.
///
/// A name the query spells out is ranked first, then one the query opens, then
/// one that merely contains it, since that is the order a reader typing from
/// memory means them in; within a rank the provider's own order is kept.
/// Matching ignores case, because a reader completing is not thinking about
/// how the author capitalized a name.
pub(super) fn candidates<'a>(snapshot: &'a Snapshot, token: &Token) -> Vec<Candidate<'a>> {
    let query = token.query.to_lowercase();
    let rank = |name: &str| {
        let name = name.to_lowercase();
        if name == query {
            Some(0)
        } else if name.starts_with(&query) {
            Some(1)
        } else if name.contains(&query) {
            Some(2)
        } else {
            None
        }
    };
    let mut ranked: Vec<(u8, Candidate<'a>)> = Vec::new();
    // A command is only a command at the head of a message; offered anywhere
    // else it would be sent as words the provider does not act on.
    if token.sigil == COMMAND_SIGIL && token.leading {
        let commands = snapshot
            .commands
            .iter()
            .filter(|command| {
                !permissions_available(snapshot)
                    || !command
                        .name
                        .trim_start_matches(COMMAND_SIGIL)
                        .eq_ignore_ascii_case(PERMISSIONS_COMMAND)
            })
            .map(|command| {
                (
                    command.name.trim_start_matches(COMMAND_SIGIL),
                    command.description.as_str(),
                )
            });
        // Compaction is a capability rather than an advertised command for
        // some providers; the composer answers to it either way, so the list
        // says so too.
        let compact = (snapshot.capabilities.compact
            && !snapshot
                .commands
                .iter()
                .any(|command| command.name.trim_start_matches(COMMAND_SIGIL) == COMPACT_COMMAND))
        .then_some((COMPACT_COMMAND, "Summarize earlier turns to free context"));
        let permissions = permissions_available(snapshot).then_some((
            PERMISSIONS_COMMAND,
            "Resolve blocked tools and manage permissions",
        ));
        for (name, description) in commands.chain(compact).chain(permissions) {
            if let Some(rank) = rank(name) {
                ranked.push((
                    rank,
                    Candidate {
                        kind: Kind::Command,
                        name,
                        insertion: Cow::Owned(format!("{COMMAND_SIGIL}{name}")),
                        description,
                    },
                ));
            }
        }
    }
    for skill in &snapshot.skills {
        // The slash reaches every skill, because it is how a reader arriving
        // from any agent asks for one; the provider's own sigil reaches only
        // its own skills.
        if token.sigil != COMMAND_SIGIL && sigil_of(&skill.invocation) != Some(token.sigil) {
            continue;
        }
        // A skill is also found by the provider's own spelling of it, so a
        // reader typing OMP's `/skill:re` still gets `/skill:review`.
        let spelled = bare(&skill.invocation)
            .filter(|bare| *bare != skill.name)
            .and_then(|bare| rank(bare));
        if let Some(rank) = rank(&skill.name).into_iter().chain(spelled).min() {
            let linked = skill.invocation.starts_with("[$");
            let unique = snapshot
                .skills
                .iter()
                .filter(|other| other.name == skill.name)
                .nth(1)
                .is_none();
            ranked.push((
                rank,
                Candidate {
                    kind: Kind::Skill,
                    name: &skill.name,
                    // Keep the exact linked path in the catalog for sending,
                    // while a unique skill can be selected by its short name.
                    insertion: if linked && unique {
                        Cow::Owned(format!("${}", skill.name))
                    } else {
                        Cow::Borrowed(&skill.invocation)
                    },
                    description: &skill.description,
                },
            ));
        }
    }
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().map(|(_, candidate)| candidate).collect()
}

/// Puts `insertion` over `token` as one edit and leaves the caret after it,
/// ready for the rest of the sentence.
///
/// A space follows the name, so the reader can go on writing and a word the
/// sigil was typed against is not run into it. Mid-sentence the caret steps
/// over the space already there rather than doubling it, and punctuation
/// right after the word stays against it.
pub(super) fn apply(
    composer: &mut text_editor::Content<cosmic::Renderer>,
    token: &Token,
    insertion: &str,
) {
    let next = composer
        .line(token.line)
        .and_then(|line| line.text.get(token.end..)?.chars().next());
    let text = match next {
        Some(next) if !is_name_character(next) => insertion.to_owned(),
        _ => format!("{insertion} "),
    };
    composer.move_to(token.selection());
    composer.perform(text_editor::Action::Edit(text_editor::Edit::Paste(
        std::sync::Arc::new(text),
    )));
    if next.is_some_and(char::is_whitespace) {
        composer.perform(text_editor::Action::Move(text_editor::Motion::Right));
    }
}

/// Whether `character` can be part of a skill's name where it is named in a
/// message: the punctuation Codex reads a `$name` mention with, which also
/// covers Claude's `plugin:skill` names, around letters and digits.
fn is_name_character(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '-' | ':')
}

/// A skill a message names.
#[derive(Debug)]
struct Named<'a> {
    /// The byte range of the word naming it.
    start: usize,
    end: usize,
    skill: &'a Skill,
    /// Whether the word spells the skill otherwise than the provider invokes
    /// it: the composer's `/name`, or the invocation in another case, which
    /// the provider would not find. It is sent in the provider's spelling.
    respelled: bool,
}

/// Whether a name ending where `rest` begins ends a word: a name running on
/// into a slash is a path segment, and one running into `.md` a file name.
fn ends_word(rest: &str) -> bool {
    let mut after = rest.chars();
    match after.next() {
        None => true,
        Some('.') => !after.next().is_some_and(char::is_alphanumeric),
        Some(next) => {
            next.is_whitespace() || matches!(next, ',' | ';' | '!' | '?' | ')' | ']' | '}' | '"')
        }
    }
}

/// Skills by the words that name them, each a sigil and a name.
///
/// A draft names few skills, so each word is looked up by walking the
/// catalog rather than by maps rebuilt on every frame the draft is read.
struct Names<'a> {
    skills: &'a [Skill],
}

impl<'a> Names<'a> {
    fn of(skills: &'a [Skill]) -> Self {
        Self { skills }
    }

    /// The one skill `name` after `sigil` names: the one spelled so exactly,
    /// or else the one spelled so in another case. A word two skills answer
    /// to names neither.
    ///
    /// A word without a lowercase letter is not taken for a name in another
    /// case: `$DEBUG` is a variable, not the `debug` skill shouted.
    fn get(&self, sigil: char, name: &str) -> Option<&'a Skill> {
        let exact = self.only(sigil, |spelling| spelling == name);
        if exact.is_some() || !name.chars().any(char::is_lowercase) {
            return exact.flatten();
        }
        fn folded(text: &str) -> impl Iterator<Item = char> + '_ {
            text.chars().flat_map(char::to_lowercase)
        }
        self.only(sigil, |spelling| folded(spelling).eq(folded(name)))
            .flatten()
    }

    /// Whether any skill answers to a word `matches`, and if so the one
    /// that alone does.
    fn only(&self, sigil: char, matches: impl Fn(&str) -> bool) -> Option<Option<&'a Skill>> {
        let mut answering = self.skills.iter().filter(|skill| {
            Self::spellings(skill).any(|(known, spelling)| known == sigil && matches(spelling))
        });
        let first = answering.next()?;
        Some(answering.next().is_none().then_some(first))
    }

    /// Every word naming `skill`: its invocation, and the composer's slash
    /// before its name or before its invocation's.
    fn spellings(skill: &Skill) -> impl Iterator<Item = (char, &str)> {
        let invoked = skill.invocation.chars().next().zip(bare(&skill.invocation));
        std::iter::once((COMMAND_SIGIL, skill.name.as_str()))
            .chain(
                skill
                    .invocation
                    .starts_with("[$")
                    .then_some(('$', skill.name.as_str())),
            )
            .chain(
                invoked
                    .into_iter()
                    .flat_map(|(sigil, bare)| [(sigil, bare), (COMMAND_SIGIL, bare)]),
            )
    }
}

/// Every skill `message` names, in order: a word that opens with a skill's
/// invocation, or with the composer's slash and a name only one skill has.
/// A word opens after whitespace, or an opening parenthesis or quote.
/// Nothing quoted as code names a skill.
fn named<'a>(message: &str, skills: &'a [Skill]) -> Vec<Named<'a>> {
    // A draft naming nothing costs no more than a look at its characters,
    // which it is on every frame.
    let mut openers = vec![COMMAND_SIGIL];
    for skill in skills {
        for opener in [skill.invocation.chars().next(), sigil_of(&skill.invocation)]
            .into_iter()
            .flatten()
        {
            if !openers.contains(&opener) {
                openers.push(opener);
            }
        }
    }
    if skills.is_empty() || !message.contains(openers.as_slice()) {
        return Vec::new();
    }
    let names = Names::of(skills);
    let links: Vec<&Skill> = skills
        .iter()
        .filter(|skill| bare(&skill.invocation).is_none())
        .collect();
    let mut named = Vec::new();
    let mut previous = None;
    // Every backtick opens or closes code, which a fence's three do once.
    let mut code = false;
    let mut resume = 0;
    for (index, character) in message.char_indices() {
        let opens_word = previous
            .is_none_or(|previous: char| previous.is_whitespace() || matches!(previous, '(' | '"'));
        previous = Some(character);
        if character == '`' {
            code = !code;
        }
        if index < resume || !opens_word || code || !openers.contains(&character) {
            continue;
        }
        let rest = &message[index..];
        // A link names its skill only as it is written, path and all.
        let linked = links.iter().find(|skill| {
            rest.starts_with(skill.invocation.as_str())
                && ends_word(&rest[skill.invocation.len()..])
        });
        let found = linked
            .map(|skill| (*skill, skill.invocation.len()))
            .or_else(|| {
                let after = &rest[character.len_utf8()..];
                let name = &after[..after
                    .find(|character: char| !is_name_character(character))
                    .unwrap_or(after.len())];
                let length = character.len_utf8() + name.len();
                (!name.is_empty() && ends_word(&rest[length..]))
                    .then(|| names.get(character, name))
                    .flatten()
                    .map(|skill| (skill, length))
            });
        if let Some((skill, length)) = found {
            resume = index + length;
            named.push(Named {
                start: index,
                end: resume,
                skill,
                respelled: rest[..length] != skill.invocation,
            });
        }
    }
    named
}

/// The message with each skill it names spelled the way the provider
/// invokes it, wherever in the message it is named.
///
/// The slash is the composer's way to name a skill whatever the provider, so
/// a reader who types `/commits` to Codex, which acts on `$commits`, still
/// gets the skill, and one who types `$Commits` gets it too, where Codex
/// would look the name up exactly. A word is only rewritten when it names
/// one skill outright;
/// a path, a file name, a URL, a name the provider does not have or two
/// skills share, or anything quoted as code is left as written.
pub(super) fn invocations<'a>(message: &'a str, skills: &[Skill]) -> Cow<'a, str> {
    let mut rewritten = String::new();
    let mut copied = 0;
    for named in named(message, skills) {
        if !named.respelled {
            continue;
        }
        rewritten.push_str(&message[copied..named.start]);
        rewritten.push_str(&named.skill.invocation);
        copied = named.end;
    }
    if copied == 0 {
        return Cow::Borrowed(message);
    }
    rewritten.push_str(&message[copied..]);
    Cow::Owned(rewritten)
}

/// `message` from its first word when that word names a skill, since a
/// provider loads a leading skill only from the message's first character,
/// and otherwise exactly as written, indentation and all.
pub(super) fn from_leading_skill<'a>(message: &'a str, skills: &[Skill]) -> &'a str {
    let trimmed = message.trim_start();
    if trimmed.len() != message.len()
        && named(trimmed, skills)
            .first()
            .is_some_and(|named| named.start == 0)
    {
        trimmed
    } else {
        message
    }
}

/// The skills `message` names that the provider will not load with it, each
/// once, spelled as it will be sent: a provider that loads only a leading
/// skill or only the first leaves every other one to reach the agent as
/// words.
pub(super) fn unloaded<'a>(message: &str, skills: &'a [Skill], reach: SkillReach) -> Vec<&'a str> {
    if reach == SkillReach::Anywhere || skills.is_empty() {
        return Vec::new();
    }
    // What loads is judged on the message as it is sent, which starts at its
    // first word.
    let message = message.trim_start();
    let named = named(message, skills);
    // At most one skill loads, and naming it again later does not make it
    // any less loaded.
    let loaded = match reach {
        SkillReach::Anywhere => named.first(),
        SkillReach::Leading => named.first().filter(|first| first.start == 0),
        SkillReach::First => first_loaded(message, &named),
    };
    let mut unloaded: Vec<&'a str> = Vec::new();
    for named in &named {
        let invocation = named.skill.invocation.as_str();
        if loaded.is_none_or(|loaded| !std::ptr::eq(loaded.skill, named.skill))
            && !unloaded.contains(&invocation)
        {
            unloaded.push(invocation);
        }
    }
    unloaded
}

/// The skill a provider that loads only the first one loads with `message`,
/// as oh-my-pi reads it: the first skill standing apart as a word between
/// whitespace, unless the message opens with a command that is not a skill,
/// or with a shell escape, which it runs instead of reading.
fn first_loaded<'m, 'a>(message: &str, named: &'m [Named<'a>]) -> Option<&'m Named<'a>> {
    let apart = |named: &&Named| {
        message[..named.start]
            .chars()
            .next_back()
            .is_none_or(char::is_whitespace)
            && message[named.end..]
                .chars()
                .next()
                .is_none_or(char::is_whitespace)
    };
    if message.starts_with(COMMAND_SIGIL) {
        return named.first().filter(|first| first.start == 0).filter(apart);
    }
    // `!`, and a `$` or `$$` before a space, run a shell command.
    let escaped = message.starts_with('!')
        || message
            .strip_prefix("$$")
            .or_else(|| message.strip_prefix('$'))
            .is_some_and(|rest| {
                rest.chars()
                    .next()
                    .is_none_or(|next| matches!(next, ' ' | '\t' | '\n' | '\r'))
            });
    if escaped {
        return None;
    }
    named.iter().find(apart)
}

/// Where the keyboard is in the completion list, and whether the reader sent
/// it away.
#[derive(Debug, Default)]
pub(super) struct State {
    /// The highlighted row, which Enter and Tab choose.
    pub highlighted: usize,
    /// Where the word Escape dismissed the list for starts, as its line and
    /// byte column. The list stays away while the reader moves about that
    /// word, and returns once they type.
    pub dismissed: Option<(usize, usize)>,
    /// Whether the reader has moved the highlight since the list opened,
    /// which makes Enter a choice even before anything follows the sigil.
    pub browsed: bool,
    /// How far the list is scrolled, so moving the highlight can keep its row
    /// in view.
    pub offset: f32,
}

impl State {
    /// Starts the list again from its first row, after a keystroke that may
    /// have changed the word at the caret. An edit also brings back a list
    /// Escape sent away. Returns whether the list has to scroll back up.
    pub fn restart(&mut self, edited: bool) -> bool {
        self.highlighted = 0;
        self.browsed = false;
        if edited {
            self.dismissed = None;
        }
        std::mem::take(&mut self.offset) > 0.0
    }

    /// Whether Enter chooses `highlighted` for `token`, rather than doing
    /// what it does in the editor: sending, or starting a new line.
    ///
    /// Enter must never change words the reader meant to send. A name
    /// already spelled out is sent, so a reader typing `/clear` from memory
    /// needs one Enter, not two. Until the reader looks into the list, Enter
    /// chooses only a name the word opens: a message ending in a bare "/" or
    /// "$", or in "see /docs", is sent as written rather than completed to
    /// the first name or to one that merely contains the word.
    pub fn enter_chooses(&self, token: &Token, highlighted: &Candidate, skills: &[Skill]) -> bool {
        let query = token.query.to_lowercase();
        !token.spells(highlighted, skills)
            && (self.browsed
                || (!query.is_empty()
                    && highlighted
                        .names()
                        .any(|name| name.to_lowercase().starts_with(&query))))
    }

    /// Moves the highlight one row among `count`, wrapping at either end, and
    /// returns the offset that brings its row into a `viewport` of rows
    /// `row` tall when the list has to scroll for it.
    pub fn step(&mut self, down: bool, count: usize, row: f32, viewport: f32) -> Option<f32> {
        if count == 0 {
            return None;
        }
        self.browsed = true;
        let current = self.highlighted.min(count - 1);
        self.highlighted = if down {
            (current + 1) % count
        } else {
            (current + count - 1) % count
        };
        let top = self.highlighted as f32 * row;
        let bottom = top + row;
        let offset = if top < self.offset {
            top
        } else if bottom > self.offset + viewport {
            bottom - viewport
        } else {
            return None;
        };
        self.offset = offset;
        Some(offset)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn native_permissions_completion_overrides_provider_dialog_and_is_leading_only() {
        let mut snapshot = snapshot();
        snapshot.capabilities.permission_modes = vec!["auto".into()];
        snapshot.commands.push(SlashCommand {
            name: "/PERMISSIONS".into(),
            description: "Terminal permissions dialog".into(),
        });
        assert_eq!(insertions(&snapshot, "/per|"), vec!["/permissions"]);
        assert!(insertions(&snapshot, "check /per|").is_empty());
    }

    #[test]
    fn file_mentions_fuzzy_match_paths_and_replace_only_the_token() {
        use super::*;
        let paths = vec![
            "src/app/chat_ui.rs".to_owned(),
            "src/chat.rs".to_owned(),
            "docs/guide.md".to_owned(),
        ];
        let candidates = files(&paths, "s/ch");
        assert_eq!(candidates.first().unwrap().name, "src/chat.rs");
        assert!(candidates.iter().all(|row| row.kind == Kind::File));
        assert!(files(&paths, "missing").is_empty());
        let token = Token::at(
            "read @src/ch carefully",
            Position {
                line: 0,
                column: 12,
            },
            &['@'],
            || false,
        )
        .unwrap();
        assert_eq!(
            (token.start, token.end, token.query.as_str()),
            (5, 12, "src/ch")
        );
    }
    use super::*;
    use crate::chat::{Capabilities, Skill, SlashCommand};

    fn caret(line: usize, column: usize) -> Position {
        Position { line, column }
    }

    /// The token at `caret` in `draft`, found as the composer finds it.
    fn token_in(draft: &str, caret: Position, sigils: &[char]) -> Option<Token> {
        let lines: Vec<&str> = draft.split('\n').collect();
        Token::at(lines.get(caret.line)?, caret, sigils, || {
            lines[..caret.line]
                .iter()
                .all(|line| line.trim().is_empty())
        })
    }

    fn token(draft: &str, caret: Position) -> Option<Token> {
        token_in(draft, caret, &['/', '$'])
    }

    /// The caret placed where `|` is in `draft`, and the draft without it.
    fn marked(draft: &str) -> (String, Position) {
        let (before, after) = draft.split_once('|').expect("a caret mark");
        let line = before.matches('\n').count();
        let column = before.len() - before.rfind('\n').map_or(0, |index| index + 1);
        (format!("{before}{after}"), caret(line, column))
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            commands: vec![
                SlashCommand {
                    name: "clear".into(),
                    description: "Start over".into(),
                },
                SlashCommand {
                    name: "review".into(),
                    description: "Review the diff".into(),
                },
            ],
            skills: vec![
                Skill {
                    name: "commits".into(),
                    description: "Write commits".into(),
                    invocation: "/commits".into(),
                },
                Skill {
                    name: "pr-review".into(),
                    description: "Review a pull request".into(),
                    invocation: "/pr-review".into(),
                },
            ],
            ..Snapshot::default()
        }
    }

    fn insertions(snapshot: &Snapshot, draft: &str) -> Vec<String> {
        let (draft, caret) = marked(draft);
        let token = token_in(&draft, caret, &sigils(snapshot)).expect("a token");
        candidates(snapshot, &token)
            .into_iter()
            .map(|row| row.insertion.into_owned())
            .collect()
    }

    #[test]
    fn the_word_at_the_caret_is_completed_wherever_it_is() {
        let (draft, at) = marked("please /rev| the diff");
        let found = token(&draft, at).expect("a token mid-sentence");
        assert_eq!((found.start, found.end), (7, 11));
        assert_eq!(found.query, "rev");
        assert!(!found.leading);

        // Back inside a word, the whole word is replaced, not just its head.
        let (draft, at) = marked("/re|view later");
        let found = token(&draft, at).expect("a token under the caret");
        assert_eq!((found.start, found.end, found.query.as_str()), (0, 7, "re"));
        assert!(found.leading);

        // Blank lines ahead of a word still leave it at the head of the message.
        let (draft, at) = marked("\n  \n  /cl|");
        assert!(token(&draft, at).expect("a token").leading);
        let (draft, at) = marked("context\n/cl|");
        assert!(!token(&draft, at).expect("a token").leading);
        // Only a slash opens a command, wherever it is.
        let (draft, at) = marked("$cl|");
        assert!(!token(&draft, at).expect("a token").leading);

        // The word ends where the name does, so punctuation after it stays.
        let (draft, at) = marked("try /com|mits, then");
        let found = token(&draft, at).expect("a token before a comma");
        assert_eq!(&draft[found.start..found.end], "/commits");
    }

    #[test]
    fn words_that_are_not_names_open_nothing() {
        for draft in [
            "plain|",
            "src/li|b.rs",
            "/usr/bi|n",
            "echo $HOME/x|",
            "a /skill |",
            "/co|mmits/src",
            "(/co|)",
            "",
        ] {
            let (draft, at) = if draft.contains('|') {
                marked(draft)
            } else {
                (draft.to_owned(), caret(0, 0))
            };
            assert_eq!(token(&draft, at), None, "{draft:?}");
        }
        // A column the editor could not have reported is not trusted.
        assert_eq!(token("é/x", caret(0, 1)), None);
        assert_eq!(token("/x", caret(1, 0)), None);
    }

    #[test]
    fn multibyte_text_around_the_word_keeps_byte_columns_right() {
        let (draft, at) = marked("naïve\u{a0}/cö|mm\u{3000}rest");
        let found = token(&draft, at).expect("a token after a no-break space");
        assert_eq!(&draft[found.start..found.end], "/cömm");
        assert_eq!(found.query, "cö");
    }

    #[test]
    fn commands_are_offered_only_at_the_head_of_a_message() {
        let snapshot = snapshot();
        assert_eq!(
            insertions(&snapshot, "/|"),
            ["/clear", "/review", "/commits", "/pr-review"]
        );
        // Mid-message a command would be sent as words; a skill still works.
        assert_eq!(insertions(&snapshot, "then /|"), ["/commits", "/pr-review"]);
        // A name the query opens ranks above one that merely contains it.
        assert_eq!(insertions(&snapshot, "/REV|"), ["/review", "/pr-review"]);
        // A name spelled out ranks above longer ones it opens, so Enter on it
        // sends rather than completing to one of them.
        let mut snapshot = snapshot;
        snapshot.skills[0].name = "review-fast".into();
        snapshot.skills[0].invocation = "/review-fast".into();
        snapshot.commands[1].name = "review-all".into();
        snapshot.skills[1].name = "review".into();
        snapshot.skills[1].invocation = "/review".into();
        assert_eq!(
            insertions(&snapshot, "/review|"),
            ["/review", "/review-all", "/review-fast"]
        );
    }

    /// OMP invokes a skill as `/skill:name`, so a reader typing that finds it
    /// as well as one typing its bare name.
    #[test]
    fn a_skill_is_found_by_the_provider_spelling_of_it() {
        let snapshot = Snapshot {
            skills: vec![Skill {
                name: "review".into(),
                description: String::new(),
                invocation: "/skill:review".into(),
            }],
            ..Snapshot::default()
        };
        assert_eq!(insertions(&snapshot, "/skill:re|"), ["/skill:review"]);
        assert_eq!(insertions(&snapshot, "then /rev|"), ["/skill:review"]);
        assert_eq!(
            invocations("/review then /skill:review", &snapshot.skills),
            "/skill:review then /skill:review"
        );
        // Back inside the provider's spelling, the whole word is replaced.
        let (draft, at) = marked("/skill:re|view");
        let mut found = token(&draft, at).expect("a token");
        found.fit(&candidates(&snapshot, &found));
        assert_eq!(&draft[found.start..found.end], "/skill:review");
    }

    #[test]
    fn the_provider_sigil_reaches_only_its_own_skills() {
        let snapshot = Snapshot {
            skills: vec![Skill {
                name: "commits".into(),
                description: String::new(),
                invocation: "$commits".into(),
            }],
            capabilities: Capabilities {
                compact: true,
                ..Capabilities::default()
            },
            ..Snapshot::default()
        };
        assert_eq!(sigils(&snapshot), ['/', '@', '$']);
        assert_eq!(insertions(&snapshot, "use $co|"), ["$commits"]);
        // The slash names a skill in the provider's own spelling, and offers
        // compaction where it is a capability rather than a listed command.
        assert_eq!(insertions(&snapshot, "/c|"), ["/compact", "$commits"]);
    }

    /// Two Codex skills sharing a name are each invoked by their path, and
    /// the provider's sigil still reaches both.
    #[test]
    fn skills_sharing_a_name_are_offered_by_their_links() {
        let snapshot = Snapshot {
            skills: vec![
                Skill {
                    name: "lint".into(),
                    description: "user · Lint".into(),
                    invocation: "[$lint](/home/u/.codex/skills/lint/SKILL.md)".into(),
                },
                Skill {
                    name: "lint".into(),
                    description: "repo · Lint".into(),
                    invocation: "[$lint](/repo/.codex/skills/lint/SKILL.md)".into(),
                },
            ],
            ..Snapshot::default()
        };
        assert_eq!(sigils(&snapshot), ['/', '@', '$']);
        assert_eq!(
            insertions(&snapshot, "then $li|"),
            [
                "[$lint](/home/u/.codex/skills/lint/SKILL.md)",
                "[$lint](/repo/.codex/skills/lint/SKILL.md)"
            ]
        );
        // A slash names neither, since it could mean either.
        assert!(matches!(
            invocations("run /lint", &snapshot.skills),
            Cow::Borrowed(_)
        ));
        // A link spelled out in the draft is left exactly as it is.
        let linked = "run [$lint](/repo/.codex/skills/lint/SKILL.md) now";
        assert!(matches!(
            invocations(linked, &snapshot.skills),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn a_unique_linked_skill_uses_a_short_draft_and_exact_sent_path() {
        let path = "[$steward-pr](/home/u/skills/steward-pr/SKILL.md)";
        let snapshot = Snapshot {
            skills: vec![Skill {
                name: "steward-pr".into(),
                description: "Own a pull request".into(),
                invocation: path.into(),
            }],
            ..Snapshot::default()
        };
        assert_eq!(insertions(&snapshot, "$ste|"), ["$steward-pr"]);
        assert_eq!(
            invocations("Use $steward-pr now", &snapshot.skills),
            format!("Use {path} now")
        );
        let (text, at) = marked("$steward-pr|");
        let row = candidates(&snapshot, &token(&text, at).unwrap()).remove(0);
        assert!(token(&text, at).unwrap().spells(&row, &snapshot.skills));
    }

    fn apply_at(draft: &str, insertion: &str) -> (String, Position) {
        let (draft, at) = marked(draft);
        let mut composer = text_editor::Content::with_text(&draft);
        composer.move_to(Cursor {
            position: at,
            selection: None,
        });
        let mut found = token(&composer.text(), at).expect("a token");
        let snapshot = snapshot();
        found.fit(&candidates(&snapshot, &found));
        apply(&mut composer, &found, insertion);
        (composer.text(), composer.cursor().position)
    }

    /// Choosing a row changes the word being typed and nothing else, so
    /// several skills can be named in one message around its prose.
    #[test]
    fn choosing_replaces_only_the_word_and_keeps_the_draft() {
        assert_eq!(
            apply_at("/comm|", "/commits"),
            ("/commits ".into(), caret(0, 9))
        );
        assert_eq!(
            apply_at("please /comm| and push", "/commits"),
            ("please /commits and push".into(), caret(0, 16))
        );
        assert_eq!(
            apply_at("first line\nthen /re|view it", "/pr-review"),
            ("first line\nthen /pr-review it".into(), caret(1, 16))
        );
        assert_eq!(
            apply_at("quoted\u{a0}$co|", "$commits"),
            ("quoted\u{a0}$commits ".into(), caret(0, 17))
        );
        assert_eq!(
            apply_at("try /comm|its, then", "/commits"),
            ("try /commits, then".into(), caret(0, 12))
        );
        // A slash typed against a word leaves the word to follow the name.
        assert_eq!(
            apply_at("/|please look", "/review"),
            ("/review please look".into(), caret(0, 8))
        );
        assert_eq!(
            apply_at("/re|please look", "/review"),
            ("/review please look".into(), caret(0, 8))
        );

        // A second skill after the first leaves the first where it was.
        let (draft, _) = apply_at("run /comm|", "/commits");
        let (draft, _) = apply_at(&format!("{draft}then /pr|"), "/pr-review");
        assert_eq!(draft, "run /commits then /pr-review ");
    }

    #[test]
    fn the_highlight_wraps_and_keeps_its_row_in_view() {
        let mut state = State::default();
        // Up from the first row wraps to the last, scrolling it into view.
        assert_eq!(state.step(false, 10, 28.0, 84.0), Some(196.0));
        assert_eq!(state.highlighted, 9);
        // Down from the last wraps to the first, back at the top.
        assert_eq!(state.step(true, 10, 28.0, 84.0), Some(0.0));
        assert_eq!(state.highlighted, 0);
        // Moving within the visible rows does not scroll.
        assert_eq!(state.step(true, 10, 28.0, 84.0), None);
        assert_eq!(state.step(true, 10, 28.0, 84.0), None);
        assert_eq!(state.step(true, 10, 28.0, 84.0), Some(28.0));
        assert_eq!(state.highlighted, 3);

        state.dismissed = Some((0, 0));
        assert!(state.restart(false));
        assert_eq!(state.highlighted, 0);
        assert!(!state.browsed);
        assert!(
            state.dismissed.is_some(),
            "moving the caret keeps Escape's word"
        );
        assert!(!state.restart(true));
        assert!(state.dismissed.is_none(), "typing brings the list back");
    }

    /// Enter goes to the list only while choosing still does something, and
    /// never rewrites words the reader meant to send: a name typed out in
    /// full sends in one keystroke, and a sentence ending in a bare sigil or
    /// in a word only contained in a name is sent as written.
    #[test]
    fn enter_chooses_only_a_name_the_word_opens() {
        let mut state = State::default();
        let at = |draft: &str| {
            let (draft, at) = marked(draft);
            token(&draft, at).expect("a token")
        };
        let row = |kind, name: &'static str, insertion: &'static str| Candidate {
            kind,
            name,
            insertion: Cow::Borrowed(insertion),
            description: "",
        };
        let skill = |name: &str, invocation: &str| Skill {
            name: name.into(),
            description: String::new(),
            invocation: invocation.into(),
        };
        let clear = row(Kind::Command, "clear", "/clear");
        let codex = [
            skill("commits", "$commits"),
            skill("Commits", "$Commits"),
            skill("lint", "[$lint](/home/lint/SKILL.md)"),
            skill("lint", "[$lint](/repo/lint/SKILL.md)"),
        ];
        let commits = row(Kind::Skill, "commits", "$commits");
        assert!(state.enter_chooses(&at("/cle|"), &clear, &codex));
        assert!(!state.enter_chooses(&at("/clear|"), &clear, &codex));
        // A command is sent as typed, so one in another case is chosen, which
        // spells it as the provider does.
        assert!(state.enter_chooses(&at("/CLEAR|"), &clear, &codex));
        // Codex is sent `$commits` for a slash, so the slash spelling is whole.
        assert!(!state.enter_chooses(&at("use /commits|"), &commits, &codex));
        // Of two skills whose names differ only in case, each spelling names
        // its own, and another names neither.
        assert!(state.enter_chooses(&at("$Commits|"), &commits, &codex));
        assert!(state.enter_chooses(&at("$COMMITS|"), &commits, &codex));
        // Back inside a whole word, choosing still replaces the rest of it.
        assert!(state.enter_chooses(&at("/cl|ear"), &clear, &codex));
        // A bare sigil is sent as it is, at the head of a message or not.
        assert!(!state.enter_chooses(&at("/|"), &clear, &codex));
        assert!(!state.enter_chooses(&at("cd /|"), &commits, &codex));
        assert!(!state.enter_chooses(&at("cost $|"), &commits, &codex));
        // A word a name opens chooses it; one the name merely contains does
        // not.
        assert!(state.enter_chooses(&at("see /Com|"), &commits, &codex));
        assert!(!state.enter_chooses(&at("see /its|"), &commits, &codex));
        // A skill sharing its name with another is named by its link, which
        // the bare name does not spell.
        let linked = row(Kind::Skill, "lint", "[$lint](/repo/lint/SKILL.md)");
        assert!(state.enter_chooses(&at("$lint|"), &linked, &codex));
        assert!(state.enter_chooses(&at("/lint|"), &linked, &codex));

        // OMP's own spelling of a skill is whole, as is the slash before its
        // name, which is sent as OMP spells it; a word either opens chooses.
        let omp = [skill("review", "/skill:review")];
        let review = row(Kind::Skill, "review", "/skill:review");
        assert!(!state.enter_chooses(&at("/skill:review|"), &review, &omp));
        assert!(!state.enter_chooses(&at("/review|"), &review, &omp));
        assert!(state.enter_chooses(&at("/skill:re|"), &review, &omp));
        assert!(state.enter_chooses(&at("/rev|"), &review, &omp));
        // A name in another case is found, and sent as OMP spells it.
        assert!(!state.enter_chooses(&at("/Review|"), &review, &omp));

        // Once the reader looks into the list, Enter takes what they chose.
        state.step(true, 2, 28.0, 56.0);
        assert!(state.enter_chooses(&at("cd /|"), &commits, &codex));
        assert!(state.enter_chooses(&at("see /its|"), &commits, &codex));
        assert!(!state.enter_chooses(&at("/clear|"), &clear, &codex));
    }

    #[test]
    fn a_path_qualified_skill_has_a_short_picker_label() {
        let candidate = Candidate {
            kind: Kind::Skill,
            name: "review",
            insertion: Cow::Borrowed("[$review](/workspace/skills/review/SKILL.md)"),
            description: "Review changes",
        };
        assert_eq!(candidate.label(), "$review");
        assert_eq!(
            candidate.insertion,
            "[$review](/workspace/skills/review/SKILL.md)"
        );
    }

    /// What is sent names every skill the way the provider invokes it, at
    /// the head of the message or anywhere after, and leaves everything else
    /// exactly as written.
    #[test]
    fn every_skill_named_in_a_message_is_sent_as_the_provider_invokes_it() {
        let codex = [
            Skill {
                name: "commits".into(),
                description: String::new(),
                invocation: "$commits".into(),
            },
            Skill {
                name: "pr-review".into(),
                description: String::new(),
                invocation: "$pr-review".into(),
            },
        ];
        assert_eq!(
            invocations("/commits then /PR-review, and push", &codex),
            "$commits then $pr-review, and push"
        );
        assert_eq!(
            invocations("line one\n/commits.", &codex),
            "line one\n$commits."
        );
        // Code closes again, and a skill named after it is still found.
        assert_eq!(
            invocations("`/commits` then /commits", &codex),
            "`/commits` then $commits"
        );
        for untouched in [
            "see /usr/commits and https://x/commits",
            "/commits/x a/commits /unknown",
            "$commits already spelled",
            "run `/commits` or\n```\n/commits\n```",
            "",
        ] {
            assert!(
                matches!(invocations(untouched, &codex), Cow::Borrowed(_)),
                "{untouched:?}"
            );
        }
        // A provider whose skills are slash commands is sent the draft as it
        // was typed, several skills and all.
        let claude = [Skill {
            name: "plugin:alpha".into(),
            description: String::new(),
            invocation: "/plugin:alpha".into(),
        }];
        assert_eq!(
            invocations("/plugin:alpha x /plugin:alpha", &claude),
            "/plugin:alpha x /plugin:alpha"
        );
        // A name in another case is sent as the provider spells it, since
        // the provider looks it up exactly.
        assert_eq!(
            invocations("naïve /PLUGIN:alpha", &claude),
            "naïve /plugin:alpha"
        );
        // A shouted word is a variable, not a name in another case.
        assert_eq!(
            invocations("$COMMITS and $Pr-Review", &codex),
            "$COMMITS and $pr-review"
        );
    }

    /// A name only ends where a word does, so a file, a path or a sentence
    /// running on from a name is not taken for the skill.
    #[test]
    fn a_skill_is_named_only_by_a_whole_word() {
        let codex = [Skill {
            name: "commits".into(),
            description: String::new(),
            invocation: "$commits".into(),
        }];
        for (message, sent) in [
            ("/commits.", "$commits."),
            ("/commits. Then", "$commits. Then"),
            (
                "try (/commits) or \"/commits\"",
                "try ($commits) or \"$commits\"",
            ),
            ("/commits!", "$commits!"),
        ] {
            assert_eq!(invocations(message, &codex), sent, "{message:?}");
        }
        // A name ends a word, so a file name, a path or a longer name
        // running on from it is not the skill, and one only opens a word.
        for untouched in [
            "/commits.md",
            "/commits.rs:12",
            "/commits/x",
            "/commits'",
            "/commitsx",
            "x/commits",
            "'/commits",
        ] {
            assert!(
                matches!(invocations(untouched, &codex), Cow::Borrowed(_)),
                "{untouched:?}"
            );
        }
    }

    /// Two skills whose names differ only in case are each named by their own
    /// spelling, and any other spelling names neither.
    #[test]
    fn a_name_in_another_case_names_only_a_skill_it_cannot_be_mistaken_for() {
        let skill = |name: &str| Skill {
            name: name.into(),
            description: String::new(),
            invocation: format!("${name}"),
        };
        let codex = [skill("Foo"), skill("foo"), skill("bar")];
        // A shouted word is a variable, not a name in another case.
        for untouched in ["$Foo and $foo", "$FOO", "/FOO", "/fOo", "$BAR", "/BAR"] {
            assert!(
                matches!(invocations(untouched, &codex), Cow::Borrowed(_)),
                "{untouched:?}"
            );
        }
        assert_eq!(
            invocations("/foo /Foo $Bar /bAR", &codex),
            "$foo $Foo $bar $bar"
        );
    }

    /// The hint names exactly the skills the provider will send as words.
    #[test]
    fn unloaded_skills_are_those_past_the_provider_reach() {
        let claude = [
            Skill {
                name: "alpha".into(),
                description: String::new(),
                invocation: "/alpha".into(),
            },
            Skill {
                name: "beta".into(),
                description: String::new(),
                invocation: "/beta".into(),
            },
        ];
        // Claude loads a skill only as the message's first word.
        assert!(unloaded("  /alpha then /alpha", &claude, SkillReach::Leading).is_empty());
        assert_eq!(
            unloaded("/alpha then /beta and /beta", &claude, SkillReach::Leading),
            ["/beta"]
        );
        assert_eq!(
            unloaded("please /alpha", &claude, SkillReach::Leading),
            ["/alpha"]
        );
        // Omp loads the first skill wherever it is, and no other.
        let omp = [
            Skill {
                name: "alpha".into(),
                description: String::new(),
                invocation: "/skill:alpha".into(),
            },
            Skill {
                name: "beta".into(),
                description: String::new(),
                invocation: "/skill:beta".into(),
            },
        ];
        assert!(unloaded("please /skill:alpha", &omp, SkillReach::First).is_empty());
        // The composer's slash names an OMP skill by its own name as well.
        assert_eq!(
            unloaded("/alpha and /beta", &omp, SkillReach::First),
            ["/skill:beta"]
        );
        // Only a skill set apart by whitespace is read, and one opening the
        // message is read to the next space.
        assert!(unloaded("/skill:alpha again /alpha", &omp, SkillReach::First).is_empty());
        assert_eq!(
            unloaded("(/skill:alpha) and /skill:beta", &omp, SkillReach::First),
            ["/skill:alpha"]
        );
        assert_eq!(
            unloaded("/skill:alpha, /skill:beta", &omp, SkillReach::First),
            ["/skill:alpha", "/skill:beta"]
        );
        // A message opening with another command, or run as a shell command,
        // loads no skill at all.
        for opening in ["/compact", "!ls", "$ ls", "$$ ls"] {
            assert_eq!(
                unloaded(&format!("{opening} /skill:alpha"), &omp, SkillReach::First),
                ["/skill:alpha"],
                "{opening:?}"
            );
        }
        assert!(unloaded("$HOME /skill:alpha", &omp, SkillReach::First).is_empty());
        assert_eq!(
            unloaded("/skill:alpha and /skill:beta", &omp, SkillReach::First),
            ["/skill:beta"]
        );
        // Code names nothing, so a message opening with it loads no skill.
        assert_eq!(
            unloaded("`/beta` /alpha", &claude, SkillReach::Leading),
            ["/alpha"]
        );
        // Codex loads every skill it is sent.
        assert!(unloaded("/alpha /beta", &claude, SkillReach::Anywhere).is_empty());
    }
}
