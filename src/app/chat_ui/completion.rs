//! Naming a command or a skill from the composer.
//!
//! Completion works on the word at the caret, wherever in the draft that is,
//! and choosing a row replaces that word and nothing else. A skill is part of
//! the sentence it is named in, so a reader can name several in one message
//! and go on writing around them; a command is the whole message, so it is
//! offered only where the provider will read it as one.

use crate::chat::{Skill, Snapshot};
use cosmic::widget::text_editor::{self, Cursor, Position};
use std::borrow::Cow;

use super::COMPACT_COMMAND;

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
        if !query.chars().all(is_name_character) {
            return None;
        }
        // The word runs on past the caret as far as the name does, so a comma
        // or a closing parenthesis after it stays where it is.
        let after = &line[caret.column..];
        let rest = after
            .find(|character: char| !is_name_character(character))
            .unwrap_or(after.len());
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
            leading,
        })
    }

    /// Whether the word already names what `insertion` does, with the caret
    /// at its end, so there is nothing left for choosing it to do. The sigil
    /// is not compared: `/commits` is sent to Codex as its `$commits`.
    pub fn spells(&self, insertion: &str) -> bool {
        let mut name = insertion.chars();
        name.next();
        self.end == self.start + self.sigil.len_utf8() + self.query.len()
            && name.as_str() == self.query
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
}

/// One row of the completion list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Candidate<'a> {
    pub kind: Kind,
    /// What the row puts in the draft, which is also what it shows: the
    /// reader sees exactly the text that will be sent.
    pub insertion: String,
    pub description: &'a str,
}

/// The sigils that open a completion for this provider: the command slash,
/// and whatever the provider spells its skills with.
pub(super) fn sigils(snapshot: &Snapshot) -> Vec<char> {
    let mut sigils = vec![COMMAND_SIGIL];
    for skill in &snapshot.skills {
        if let Some(sigil) = skill.invocation.chars().next()
            && !sigil.is_alphanumeric()
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
/// memory means them in; within a rank the provider's own order is kept. Matching ignores case, because a reader
/// completing is not thinking about how the author capitalized a name.
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
        let commands = snapshot.commands.iter().map(|command| {
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
        for (name, description) in commands.chain(compact) {
            if let Some(rank) = rank(name) {
                ranked.push((
                    rank,
                    Candidate {
                        kind: Kind::Command,
                        insertion: format!("{COMMAND_SIGIL}{name}"),
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
        if token.sigil != COMMAND_SIGIL && !skill.invocation.starts_with(token.sigil) {
            continue;
        }
        if let Some(rank) = rank(&skill.name) {
            ranked.push((
                rank,
                Candidate {
                    kind: Kind::Skill,
                    insertion: skill.invocation.clone(),
                    description: skill.description.as_str(),
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
/// At the end of a line a space follows the name, so the reader can go on
/// writing. Mid-sentence the caret steps over the space already there rather
/// than doubling it, and punctuation right after the word stays against it.
pub(super) fn apply(
    composer: &mut text_editor::Content<cosmic::Renderer>,
    token: &Token,
    insertion: &str,
) {
    let next = composer
        .line(token.line)
        .and_then(|line| line.text.get(token.end..)?.chars().next());
    let text = match next {
        None => format!("{insertion} "),
        Some(_) => insertion.to_owned(),
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

/// The message with each skill the reader named with a slash spelled the way
/// the provider invokes it, wherever in the message it is named.
///
/// The slash is the composer's way to name a skill whatever the provider, so
/// a reader who types `/commits` to Codex, which acts on `$commits`, still
/// gets the skill. A word is only rewritten when it names a skill outright;
/// a path, a URL, a name the provider does not have, or anything quoted as
/// code is left as written.
pub(super) fn invocations<'a>(message: &'a str, skills: &[Skill]) -> Cow<'a, str> {
    let mut rewritten = String::new();
    let mut copied = 0;
    let mut previous = None;
    // Every backtick opens or closes code, which a fence's three do once.
    let mut code = false;
    for (index, character) in message.char_indices() {
        let opens_word = previous.is_none_or(char::is_whitespace);
        previous = Some(character);
        if character == '`' {
            code = !code;
        }
        if character != COMMAND_SIGIL || !opens_word || code {
            continue;
        }
        let rest = &message[index + 1..];
        let name = &rest[..rest
            .find(|character: char| !is_name_character(character))
            .unwrap_or(rest.len())];
        // A name that runs into another slash is a path segment.
        if name.is_empty() || rest[name.len()..].starts_with(COMMAND_SIGIL) {
            continue;
        }
        let Some(skill) = skills.iter().find(|skill| {
            skill.name.eq_ignore_ascii_case(name)
                || skill
                    .invocation
                    .get(1..)
                    .is_some_and(|bare| bare.eq_ignore_ascii_case(name))
        }) else {
            continue;
        };
        rewritten.push_str(&message[copied..index]);
        rewritten.push_str(&skill.invocation);
        copied = index + 1 + name.len();
    }
    if copied == 0 {
        return Cow::Borrowed(message);
    }
    rewritten.push_str(&message[copied..]);
    Cow::Owned(rewritten)
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
    /// A name already spelled out is left to send, so a reader typing
    /// `/clear` from memory needs one Enter, not two. Mid-sentence, until the
    /// reader looks into the list, Enter chooses only a name the word opens:
    /// a sentence ending "cd /" or "see /docs" is sent as written rather than
    /// completed to some name that merely contains it.
    pub fn enter_chooses(&self, token: &Token, highlighted: &str) -> bool {
        let mut name = highlighted.chars();
        name.next();
        !token.spells(highlighted)
            && (self.browsed
                || token.leading
                || (!token.query.is_empty()
                    && name
                        .as_str()
                        .to_lowercase()
                        .starts_with(&token.query.to_lowercase())))
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
            .map(|row| row.insertion)
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
        assert_eq!(sigils(&snapshot), ['/', '$']);
        assert_eq!(insertions(&snapshot, "use $co|"), ["$commits"]);
        // The slash names a skill in the provider's own spelling, and offers
        // compaction where it is a capability rather than a listed command.
        assert_eq!(insertions(&snapshot, "/c|"), ["/compact", "$commits"]);
    }

    fn apply_at(draft: &str, insertion: &str) -> (String, Position) {
        let (draft, at) = marked(draft);
        let mut composer = text_editor::Content::with_text(&draft);
        composer.move_to(Cursor {
            position: at,
            selection: None,
        });
        let found = token(&composer.text(), at).expect("a token");
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

    /// Enter goes to the list only while choosing still does something, so
    /// a name typed out in full sends in one keystroke, and a sentence ending
    /// in a bare slash is not completed behind the reader's back.
    #[test]
    fn enter_chooses_only_while_there_is_something_to_choose() {
        let mut state = State::default();
        let at = |draft: &str| {
            let (draft, at) = marked(draft);
            token(&draft, at).expect("a token")
        };
        assert!(state.enter_chooses(&at("/cle|"), "/clear"));
        assert!(!state.enter_chooses(&at("/clear|"), "/clear"));
        // Codex is sent `$commits` for a slash, so the slash spelling is whole.
        assert!(!state.enter_chooses(&at("use /commits|"), "$commits"));
        // Back inside a whole word, choosing still replaces the rest of it.
        assert!(state.enter_chooses(&at("/cl|ear"), "/clear"));
        // A bare slash chooses at the head of a message, where a lone slash
        // would never be sent, but not mid-sentence until the reader browses.
        assert!(state.enter_chooses(&at("/|"), "/clear"));
        assert!(!state.enter_chooses(&at("cd /|"), "/commits"));
        // Nor does a word only contained in a name, until the reader browses.
        assert!(state.enter_chooses(&at("see /Com|"), "/commits"));
        assert!(!state.enter_chooses(&at("see /its|"), "/commits"));
        state.step(true, 2, 28.0, 56.0);
        assert!(state.enter_chooses(&at("cd /|"), "/commits"));
        assert!(state.enter_chooses(&at("see /its|"), "/commits"));
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
        assert_eq!(
            invocations("naïve /PLUGIN:alpha", &claude),
            "naïve /plugin:alpha"
        );
    }
}
