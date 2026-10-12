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
//! transcript never saw.

/// One command of a pipeline or list, with its words unquoted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Simple {
    /// The command and its arguments, after any `NAME=value` prefixes.
    pub(crate) argv: Vec<String>,
    /// What the command reads on standard input, where the line says.
    pub(crate) stdin: Stdin,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum Stdin {
    /// Inherited, or from somewhere the line does not show.
    #[default]
    Unknown,
    /// A heredoc's or a herestring's text.
    Text(String),
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
/// lex: an unterminated quote is a command no shell ran as written.
pub(crate) fn commands(line: &str) -> Option<Vec<Simple>> {
    commands_at(line, 0)
}

/// How deep `sh -c` wrapping is followed. Real wrappers nest once or twice.
const MAX_NESTING: usize = 4;

fn commands_at(line: &str, depth: usize) -> Option<Vec<Simple>> {
    let (tokens, mut heredocs) = lex(line)?;
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
                    "<" => current.stdin = Stdin::Unknown,
                    _ => {}
                }
            }
            Token::Separator(separator) => {
                finish(&mut current, &mut simples);
                assignments = true;
                if separator == "|" {
                    current.stdin = Stdin::Pipe;
                }
            }
        }
    }
    finish(&mut current, &mut simples);
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
fn lex(line: &str) -> Option<(Vec<Token>, Vec<String>)> {
    let mut lexer = Lexer {
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
        // `2>&1`, `>&-`: a descriptor, not a word to read.
        if operator.ends_with('&') {
            while self
                .peek(0)
                .is_some_and(|character| character.is_ascii_digit() || character == '-')
            {
                self.at += 1;
            }
            if self.peek(0).is_none_or(|character| " \t\n;|&)".contains(character)) {
                return Some(());
            }
        }
        while matches!(self.peek(0), Some(' ' | '\t')) {
            self.at += 1;
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
        for (delimiter, strip_tabs) in std::mem::take(&mut self.pending) {
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
            self.heredocs.push(body);
        }
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

    /// `$(…)`, kept as written: its parentheses and quotes are its own.
    fn substitution(&mut self, word: &mut String) -> Option<()> {
        let start = self.at;
        self.at += 2;
        let mut depth = 1;
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
                _ => {}
            }
            self.at += 1;
        }
        word.extend(&self.chars[start..self.at]);
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
    fn an_unterminated_quote_does_not_lex() {
        assert_eq!(commands("cagents send x \"oops"), None);
        assert_eq!(commands("echo $(unclosed"), None);
    }
}
