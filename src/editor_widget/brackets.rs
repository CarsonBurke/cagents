//! Lightweight lexical filtering for the bracket matcher. Only bracket
//! tokens are retained; source text is never copied.
fn lifetime(text: &str, index: usize) -> bool {
    let previous = text[..index].trim_end().as_bytes().last();
    if !previous.is_some_and(|byte| matches!(byte, b'<' | b'&' | b':' | b',' | b'+')) { return false; }
    let tail = &text.as_bytes()[index+1..];
    let length = tail.iter().take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_').count();
    length > 0 && tail.get(length) != Some(&b'\'')
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct State {
    comment_depth: usize,
    quote: Option<u8>,
    raw_hashes: Option<usize>,
}

pub(super) fn on_line(text: &str, mut state: State) -> (Vec<(usize, char)>, State) {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if state.comment_depth > 0 {
            if bytes.get(index..index+2) == Some(b"/*") { state.comment_depth += 1; index += 2; }
            else if bytes.get(index..index+2) == Some(b"*/") { state.comment_depth -= 1; index += 2; }
            else { index += 1; }
            continue;
        }
        if let Some(hashes) = state.raw_hashes {
            if bytes[index] == b'"' && bytes.get(index+1..index+1+hashes).is_some_and(|suffix| suffix.iter().all(|byte| *byte == b'#')) {
                state.raw_hashes = None; index += 1 + hashes;
            } else { index += 1; }
            continue;
        }
        if let Some(quote) = state.quote {
            if bytes[index] == b'\\' { index = (index+2).min(bytes.len()); }
            else if bytes[index] == quote { state.quote = None; index += 1; }
            else { index += 1; }
            continue;
        }
        match bytes[index] {
            b'/' if bytes.get(index+1) == Some(&b'/') => break,
            b'#' if text[..index].trim().is_empty() && !bytes.get(index+1).is_some_and(|next| matches!(next, b'[' | b'!')) => break,
            b'/' if bytes.get(index+1) == Some(&b'*') => { state.comment_depth = 1; index += 2; },
            b'r' if {
                let hashes = bytes[index+1..].iter().take_while(|byte| **byte == b'#').count();
                bytes.get(index+1+hashes) == Some(&b'"')
            } => {
                let hashes = bytes[index+1..].iter().take_while(|byte| **byte == b'#').count();
                state.raw_hashes = Some(hashes); index += hashes + 2;
            },
            quote @ (b'"' | b'\'') if quote == b'"' || (!lifetime(text, index) && bytes[index+1..].contains(&quote)) => {
                state.quote = Some(quote); index += 1;
            },
            bracket @ (b'(' | b')' | b'[' | b']' | b'{' | b'}') => { tokens.push((index, bracket as char)); index += 1; },
            _ => index += 1,
        }
    }
    (tokens, state)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn comments_quotes_and_rust_lifetimes_do_not_hide_real_code_brackets() {
        assert_eq!(on_line("fn a<'a>() { let x = \"}\"; /* { */", State::default()).0.iter().map(|(_, bracket)| *bracket).collect::<String>(), "(){");
        assert_eq!(on_line("// {", State::default()).0, vec![]);
        assert_eq!(on_line("fn a<'a>(x: &'a str) {", State::default()).0.iter().map(|(_, bracket)| *bracket).collect::<String>(), "(){");
        assert_eq!(on_line("/* {", State::default()).1.comment_depth, 1);
        let (tokens, block) = on_line("} */ real()", State { comment_depth: 1, ..State::default() });
        assert_eq!(tokens.iter().map(|(_, bracket)| *bracket).collect::<String>(), "()"); assert_eq!(block, State::default());
        let (_, block) = on_line("/* outer /* inner */", State::default());
        assert_eq!(block.comment_depth, 1);
        assert!(on_line("} inside outer", block).0.is_empty());
    }
}
