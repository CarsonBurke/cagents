//! The simple commands in a line of shell, as far as reading one needs.
//!
//! An agent rarely runs `cagents send ID MESSAGE` bare. It wraps it: `2>&1 |
//! tail -1` after it, `cd repo &&` before it, the message in a heredoc, the
//! whole line inside `bash -lc '…'` when Codex runs it. Recognizing the
//! command means taking those apart the way a shell would, without running
//! anything: quoting is undone, redirections are set aside, and a heredoc's
//! body is attached to the command that reads it.
//!
//! Expansions are not performed. `$NAME`, `$(…)` and backticks stay in a word
//! as written, which is the honest rendering of an argument whose value the
//! transcript never saw. The one exception is `"$(cat <<'EOF' … EOF)"`, the
//! way agents pass a long message as one argument: its value is the heredoc,
//! which the line does show.

/// One command of a pipeline or list, with its words unquoted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Simple {
    /// The command and its arguments, after any `NAME=value` prefixes.
    pub(crate) argv: Vec<String>,
    /// What the command reads on standard input, where the line says.
    pub(crate) stdin: Stdin,
    /// Whether it runs only if the command before it failed, as after `||`,
    /// so the line does not say that it ran.
    pub(crate) guarded: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum Stdin {
    /// Inherited, or from somewhere the line does not show.
    #[default]
    Unknown,
    /// A heredoc's or a herestring's text.
    Text(String),
    /// The file a `<` redirection names.
    File(String),
    /// The command before it in a pipeline.
    Pipe,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    /// `|`, `||`, `&&`, `;`, `&`, a newline, or a parenthesis: anything that
    /// ends a simple command.
    Separator(&'static str),
    /// A redirection operator; the word after it is its target.
    Redirect(&'static str),
}

/// The simple commands in `line`, in order, unwrapping `sh -c SCRIPT` and its
/// relatives into the commands of their script. `None` when the line does not
/// lex, as an unterminated quote is a command no shell ran as written, or
/// when it holds a compound command: a loop's or a conditional's body runs
/// any number of times, which its words alone do not tell.
pub(crate) fn commands(line: &str) -> Option<Vec<Simple>> {
    commands_at(line, 0)
}

/// The words that open or continue a compound command where a command name
/// would be.
const RESERVED: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "select", "function", "{", "}",
];

/// How deep `sh -c` wrapping and `$(cat <<EOF …)` are followed. Real lines
/// nest once or twice.
const MAX_NESTING: usize = 4;

fn commands_at(line: &str, depth: usize) -> Option<Vec<Simple>> {
    let (tokens, mut heredocs) = lex(line, depth)?;
    let mut simples = Vec::new();
    let mut current = Simple::default();
    let mut assignments = true;
    let mut tokens = tokens.into_iter();
    let finish = |current: &mut Simple, simples: &mut Vec<Simple>| {
        let simple = std::mem::take(current);
        if !simple.argv.is_empty() {
            simples.push(simple);
        }
    };
    while let Some(token) = tokens.next() {
        match token {
            Token::Word(word) => {
                if assignments && is_assignment(&word) {
                    continue;
                }
                assignments = false;
                current.argv.push(word);
            }
            Token::Redirect(operator) => {
                let target = match tokens.next() {
                    Some(Token::Word(word)) => word,
                    _ => return None,
                };
                match operator {
                    "<<" | "<<-" => {
                        // `lex` gives every heredoc operator a body.
                        let body = if heredocs.is_empty() {
                            String::new()
                        } else {
                            heredocs.remove(0)
                        };
                        current.stdin = Stdin::Text(body);
                    }
                    "<<<" => current.stdin = Stdin::Text(format!("{target}\n")),
                    "<" => current.stdin = Stdin::File(target),
                    _ => {}
                }
            }
            // A pipeline may go on past the end of a line.
            Token::Separator("\n") if current.argv.is_empty() && current.stdin == Stdin::Pipe => {}
            Token::Separator(separator) => {
                finish(&mut current, &mut simples);
                assignments = true;
                match separator {
                    "|" => current.stdin = Stdin::Pipe,
                    "||" => current.guarded = true,
                    _ => {}
                }
            }
        }
    }
    finish(&mut current, &mut simples);
    if simples
        .iter()
        .any(|simple| RESERVED.contains(&simple.argv[0].as_str()))
    {
        return None;
    }
    if depth >= MAX_NESTING {
        return Some(simples);
    }
    let mut unwrapped = Vec::with_capacity(simples.len());
    for simple in simples {
        match script(&simple.argv) {
            Some(script) => unwrapped.extend(commands_at(script, depth + 1)?),
            None => unwrapped.push(simple),
        }
    }
    Some(unwrapped)
}

/// The script of `sh -c SCRIPT`, `bash -lc SCRIPT` and the like.
fn script(argv: &[String]) -> Option<&str> {
    let (program, arguments) = argv.split_first()?;
    if !matches!(
        program.rsplit('/').next(),
        Some("sh" | "bash" | "zsh" | "dash" | "ksh")
    ) {
        return None;
    }
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        let flags = argument.strip_prefix('-')?;
        if flags.starts_with('-') || flags.is_empty() {
            return None;
        }
        if flags.contains('c') {
            return arguments.next().map(String::as_str);
        }
        // `-o option` takes a value.
        if flags.ends_with('o') {
            arguments.next();
        }
    }
    None
}

/// What `script` prints when it is only `cat` reading a heredoc, less the
/// trailing newlines a command substitution drops.
fn heredoc_output(script: &str, depth: usize) -> Option<String> {
    match commands_at(script, depth)?.as_slice() {
        [Simple {
            argv,
            stdin: Stdin::Text(body),
            ..
        }] if argv == &["cat"] => Some(body.trim_end_matches('\n').to_owned()),
        _ => None,
    }
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

/// The tokens of `line`, and the bodies of its heredocs in the order their
/// operators appear.
fn lex(line: &str, depth: usize) -> Option<(Vec<Token>, Vec<String>)> {
    let mut lexer = Lexer {
        depth,
        chars: line.chars().collect(),
        at: 0,
        tokens: Vec::new(),
        heredocs: Vec::new(),
        pending: Vec::new(),
    };
    lexer.run()?;
    // A heredoc whose body the line never reached still has an operator.
    let missing = lexer.pending.len();
    lexer
        .heredocs
        .extend(std::iter::repeat_n(String::new(), missing));
    Some((lexer.tokens, lexer.heredocs))
}

struct Lexer {
    /// How deeply the line being lexed is nested in others.
    depth: usize,
    chars: Vec<char>,
    at: usize,
    tokens: Vec<Token>,
    heredocs: Vec<String>,
    /// Heredocs whose operator has been read and whose body starts on the
    /// next line: their delimiter, and whether leading tabs are stripped.
    pending: Vec<(String, bool)>,
}

impl Lexer {
    fn peek(&self, offset: usize) -> Option<char> {
        self.chars.get(self.at + offset).copied()
    }

    fn run(&mut self) -> Option<()> {
        while let Some(character) = self.peek(0) {
            match character {
                ' ' | '\t' => self.at += 1,
                '\\' if self.peek(1) == Some('\n') => self.at += 2,
                '\n' => {
                    self.at += 1;
                    self.tokens.push(Token::Separator("\n"));
                    self.bodies();
                }
                '#' => {
                    while self.peek(0).is_some_and(|character| character != '\n') {
                        self.at += 1;
                    }
                }
                '|' | '&' | ';' | '(' | ')' => self.separator(),
                // `<(…)` and `>(…)` are process substitutions: words.
                '<' | '>' if self.peek(1) == Some('(') => self.word()?,
                '<' | '>' => self.redirect()?,
                _ => self.word()?,
            }
        }
        Some(())
    }

    fn separator(&mut self) {
        let pair = (self.peek(0), self.peek(1));
        let (token, width) = match pair {
            (Some('|'), Some('|')) => ("||", 2),
            (Some('&'), Some('&')) => ("&&", 2),
            (Some(';'), Some(';')) => (";;", 2),
            // `|&` pipes standard error too, and `&>` redirects both.
            (Some('|'), Some('&')) => ("|", 2),
            (Some('&'), Some('>')) => {
                self.at += 1;
                let _ = self.redirect();
                return;
            }
            (Some('|'), _) => ("|", 1),
            (Some('&'), _) => ("&", 1),
            (Some(';'), _) => (";", 1),
            (Some('('), _) => ("(", 1),
            _ => (")", 1),
        };
        self.at += width;
        self.tokens.push(Token::Separator(token));
    }

    /// A redirection starting at `<` or `>`, past any descriptor before it.
    fn redirect(&mut self) -> Option<()> {
        let rest: String = self.chars[self.at..].iter().take(3).collect();
        let operator = ["<<<", "<<-", "<<", "<&", "<>", "<", ">>", ">&", ">|", ">"]
            .into_iter()
            .find(|operator| rest.starts_with(operator))?;
        self.at += operator.chars().count();
        while matches!(self.peek(0), Some(' ' | '\t')) {
            self.at += 1;
        }
        // `2>&1`, `>&-`: a descriptor, not a word to read. `>& file`
        // redirects both streams to a file, which is.
        if operator.ends_with('&') {
            let ends = |character: Option<char>| {
                character.is_none_or(|character| " \t\n;|&)".contains(character))
            };
            let start = self.at;
            while self
                .peek(0)
                .is_some_and(|character| character.is_ascii_digit() || character == '-')
            {
                self.at += 1;
            }
            if self.at > start && ends(self.peek(0)) {
                return Some(());
            }
            self.at = start;
            if ends(self.peek(0)) {
                return Some(());
            }
        }
        let target = self.read_word()?;
        // Whether the delimiter was quoted only decides whether the body is
        // expanded, and nothing here is expanded either way.
        if matches!(operator, "<<" | "<<-") {
            self.pending.push((target.clone(), operator == "<<-"));
        }
        self.tokens.push(Token::Redirect(operator));
        self.tokens.push(Token::Word(target));
        Some(())
    }

    /// Reads the bodies of the heredocs opened on the line just ended.
    fn bodies(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        let bodies = self.read_bodies(pending);
        self.heredocs.extend(bodies);
    }

    /// Reads the bodies of `pending` heredocs, in order, from the start of a
    /// line.
    fn read_bodies(&mut self, pending: Vec<(String, bool)>) -> Vec<String> {
        let mut bodies = Vec::with_capacity(pending.len());
        for (delimiter, strip_tabs) in pending {
            let mut body = String::new();
            loop {
                if self.at >= self.chars.len() {
                    break;
                }
                let end = self.chars[self.at..]
                    .iter()
                    .position(|character| *character == '\n')
                    .map_or(self.chars.len(), |offset| self.at + offset);
                let line: String = self.chars[self.at..end].iter().collect();
                self.at = (end + 1).min(self.chars.len());
                let line = if strip_tabs {
                    line.trim_start_matches('\t').to_owned()
                } else {
                    line
                };
                if line == delimiter {
                    break;
                }
                body.push_str(&line);
                body.push('\n');
            }
            bodies.push(body);
        }
        bodies
    }

    fn word(&mut self) -> Option<()> {
        // A descriptor written before a redirection, as in `2>&1`.
        let digits = self.chars[self.at..]
            .iter()
            .take_while(|character| character.is_ascii_digit())
            .count();
        if digits > 0 && matches!(self.peek(digits), Some('<' | '>')) {
            self.at += digits;
            return self.redirect();
        }
        let word = self.read_word()?;
        self.tokens.push(Token::Word(word));
        Some(())
    }

    /// One word, its quoting removed. Stops at whitespace or an operator.
    fn read_word(&mut self) -> Option<String> {
        let mut word = String::new();
        while let Some(character) = self.peek(0) {
            match character {
                '<' | '>' if self.peek(1) == Some('(') => self.substitution(&mut word)?,
                ' ' | '\t' | '\n' | '|' | '&' | ';' | '<' | '>' | '(' | ')' => break,
                '\\' => {
                    match self.peek(1) {
                        Some('\n') => {}
                        Some(escaped) => word.push(escaped),
                        None => word.push('\\'),
                    }
                    self.at += 2;
                }
                '\'' => {
                    self.at += 1;
                    loop {
                        match self.peek(0)? {
                            '\'' => break,
                            character => word.push(character),
                        }
                        self.at += 1;
                    }
                    self.at += 1;
                }
                '"' => {
                    self.at += 1;
                    self.double_quoted(&mut word)?;
                }
                '$' if self.peek(1) == Some('\'') => {
                    self.at += 2;
                    self.ansi_quoted(&mut word)?;
                }
                '$' if self.peek(1) == Some('(') => self.substitution(&mut word)?,
                '`' => self.backticks(&mut word)?,
                character => {
                    word.push(character);
                    self.at += 1;
                }
            }
        }
        Some(word)
    }

    /// The inside of `"…"`, from after its opening quote to after its close.
    fn double_quoted(&mut self, word: &mut String) -> Option<()> {
        loop {
            match self.peek(0)? {
                '"' => {
                    self.at += 1;
                    return Some(());
                }
                '\\' => {
                    match self.peek(1)? {
                        '\n' => {}
                        escaped @ ('$' | '`' | '"' | '\\') => word.push(escaped),
                        other => {
                            word.push('\\');
                            word.push(other);
                        }
                    }
                    self.at += 2;
                }
                '$' if self.peek(1) == Some('(') => self.substitution(word)?,
                '`' => self.backticks(word)?,
                character => {
                    word.push(character);
                    self.at += 1;
                }
            }
        }
    }

    /// The inside of `$'…'`, with its C escapes resolved.
    fn ansi_quoted(&mut self, word: &mut String) -> Option<()> {
        loop {
            match self.peek(0)? {
                '\'' => {
                    self.at += 1;
                    return Some(());
                }
                '\\' => {
                    let escaped = self.peek(1)?;
                    word.push(match escaped {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        'e' | 'E' => '\u{1b}',
                        '0' => '\0',
                        other => other,
                    });
                    self.at += 2;
                }
                character => {
                    word.push(character);
                    self.at += 1;
                }
            }
        }
    }

    /// `$(…)`, or `<(…)` and `>(…)`, kept as written: its parentheses,
    /// quotes and heredocs are its own. Only a `$(…)` that is nothing but
    /// `cat` reading a heredoc is replaced, by what it would print.
    fn substitution(&mut self, word: &mut String) -> Option<()> {
        let start = self.at;
        let arithmetic = self.peek(2) == Some('(');
        self.at += 2;
        let mut depth = 1;
        let mut pending = Vec::new();
        while depth > 0 {
            match self.peek(0)? {
                '(' => depth += 1,
                ')' => depth -= 1,
                '\\' => self.at += 1,
                '\'' => {
                    self.at += 1;
                    while self.peek(0)? != '\'' {
                        self.at += 1;
                    }
                }
                '"' => {
                    self.at += 1;
                    let mut ignored = String::new();
                    self.double_quoted(&mut ignored)?;
                    continue;
                }
                // A heredoc's body is not shell: its quotes and parentheses
                // are text, and it starts on the next line.
                '<' if !arithmetic && self.peek(1) == Some('<') && self.peek(2) != Some('<') => {
                    self.at += 2;
                    let strip_tabs = self.peek(0) == Some('-');
                    if strip_tabs {
                        self.at += 1;
                    }
                    while matches!(self.peek(0), Some(' ' | '\t')) {
                        self.at += 1;
                    }
                    pending.push((self.read_word()?, strip_tabs));
                    continue;
                }
                '\n' if !pending.is_empty() => {
                    self.at += 1;
                    let pending = std::mem::take(&mut pending);
                    self.read_bodies(pending);
                    continue;
                }
                _ => {}
            }
            self.at += 1;
        }
        let written: String = self.chars[start..self.at].iter().collect();
        let output = written
            .strip_prefix("$(")
            .filter(|_| !arithmetic && self.depth < MAX_NESTING)
            .and_then(|script| heredoc_output(&script[..script.len() - 1], self.depth + 1));
        word.push_str(output.as_deref().unwrap_or(&written));
        Some(())
    }

    /// `` `…` ``, kept as written.
    fn backticks(&mut self, word: &mut String) -> Option<()> {
        let start = self.at;
        self.at += 1;
        loop {
            match self.peek(0)? {
                '`' => break,
                '\\' => self.at += 2,
                _ => self.at += 1,
            }
        }
        self.at += 1;
        word.extend(&self.chars[start..self.at]);
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(line: &str) -> Vec<Vec<String>> {
        commands(line)
            .expect("lexes")
            .into_iter()
            .map(|simple| simple.argv)
            .collect()
    }

    #[test]
    fn quoting_is_undone_and_redirections_and_pipes_are_set_aside() {
        assert_eq!(
            argv(r#"cagents send 95fcd1b9 "From the user: \"stop\" — it's done" 2>&1 | tail -1"#),
            vec![
                vec![
                    "cagents".to_owned(),
                    "send".into(),
                    "95fcd1b9".into(),
                    r#"From the user: "stop" — it's done"#.into()
                ],
                vec!["tail".into(), "-1".into()],
            ]
        );
        assert_eq!(
            argv("cd /repo && FOO=1 cagents send x 'a b' >/dev/null; echo $'one\\ntwo'"),
            vec![
                vec!["cd".to_owned(), "/repo".into()],
                vec!["cagents".into(), "send".into(), "x".into(), "a b".into()],
                vec!["echo".into(), "one\ntwo".into()],
            ]
        );
    }

    #[test]
    fn a_heredoc_is_the_stdin_of_the_command_that_opened_it() {
        let commands = commands(
            "cagents send abcd - <<'EOF' && echo done\nLine with $HOME\n  indented\nEOF\n",
        )
        .unwrap();
        assert_eq!(commands[0].argv, ["cagents", "send", "abcd", "-"]);
        assert_eq!(
            commands[0].stdin,
            Stdin::Text("Line with $HOME\n  indented\n".into())
        );
        assert_eq!(commands[1].argv, ["echo", "done"]);
        let piped = super::commands("cat <<EOF | cagents ask parent -\nhi\nEOF").unwrap();
        assert_eq!(piped[0].stdin, Stdin::Text("hi\n".into()));
        assert_eq!(piped[1].stdin, Stdin::Pipe);
        let herestring = super::commands("cagents send a - <<< \"hello there\"").unwrap();
        assert_eq!(herestring[0].stdin, Stdin::Text("hello there\n".into()));
    }

    #[test]
    fn shell_wrappers_are_unwrapped_and_expansions_are_kept_as_written() {
        assert_eq!(
            argv(r#"/bin/bash -lc "cagents send \"$PEER\" \"see $(git rev-parse HEAD)\"""#),
            vec![vec![
                "cagents".to_owned(),
                "send".into(),
                "$PEER".into(),
                "see $(git rev-parse HEAD)".into()
            ]]
        );
        assert_eq!(
            argv("sh -c 'cagents whoami' # comment"),
            vec![vec!["cagents".to_owned(), "whoami".into()]]
        );
    }

    #[test]
    fn a_heredoc_substituted_into_an_argument_is_its_value() {
        let line = "cagents send peer \"$(cat <<'EOF'\nIt's done (mostly).\nSee \"notes\".\nEOF\n)\" 2>&1";
        assert_eq!(
            argv(line),
            vec![vec![
                "cagents".to_owned(),
                "send".into(),
                "peer".into(),
                "It's done (mostly).\nSee \"notes\".".into()
            ]]
        );
        // Anything else substituted stays as written.
        assert_eq!(
            argv("cagents send peer \"$(cat notes.md)\" $((1 << 2))"),
            vec![vec![
                "cagents".to_owned(),
                "send".into(),
                "peer".into(),
                "$(cat notes.md)".into(),
                "$((1 << 2))".into()
            ]]
        );
        assert_eq!(
            argv("diff <(cagents inbox) expected"),
            vec![vec![
                "diff".to_owned(),
                "<(cagents inbox)".into(),
                "expected".into()
            ]]
        );
    }

    #[test]
    fn a_line_says_which_commands_it_runs_and_how_often() {
        // A loop's or a conditional's body runs any number of times.
        for line in [
            "for s in a b; do\n  cagents send \"$s\" hi\ndone",
            "if ready; then\n  cagents send x hi\nfi",
            "{ cagents send x hi; }",
        ] {
            assert_eq!(commands(line), None, "{line:?}");
        }
        let guarded = commands("true || cagents send x hi && echo sent").unwrap();
        assert_eq!(
            guarded.iter().map(|simple| simple.guarded).collect::<Vec<_>>(),
            [false, true, false]
        );
        let continued = commands("echo hi |\ncagents send x -").unwrap();
        assert_eq!(continued[1].stdin, Stdin::Pipe);
        assert_eq!(
            argv("cagents send x hi >& /dev/null 2>&1 <& 0"),
            vec![vec![
                "cagents".to_owned(),
                "send".into(),
                "x".into(),
                "hi".into()
            ]]
        );
    }

    #[test]
    fn an_unterminated_quote_does_not_lex() {
        assert_eq!(commands("cagents send x \"oops"), None);
        assert_eq!(commands("echo $(unclosed"), None);
    }
}
