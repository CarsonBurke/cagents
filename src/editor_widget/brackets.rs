//! Lightweight lexical filtering for the bracket matcher. Only bracket
//! tokens are retained; source text is never copied.
fn lifetime(text: &str, index: usize) -> bool {
    let previous = text[..index].trim_end().as_bytes().last();
    if !previous.is_some_and(|byte| matches!(byte, b'<' | b'&' | b':' | b',' | b'+')) { return false; }
    let tail = &text.as_bytes()[index+1..];
    let length = tail.iter().take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_').count();
    length > 0 && tail.get(length) != Some(&b'\'')
}

pub(super) fn on_line(text: &str, mut block: usize) -> (Vec<(usize, char)>, usize) {
    let bytes = text.as_bytes(); let mut tokens = Vec::new(); let mut index = 0;
    while index < bytes.len() {
        if block > 0 {
            if bytes.get(index..index+2) == Some(b"/*") { block += 1; index += 2; }
            else if bytes.get(index..index+2) == Some(b"*/") { block -= 1; index += 2; }
            else { index += 1; }
            continue;
        }
        match bytes[index] {
            b'/' if bytes.get(index+1) == Some(&b'/') => break,
            b'#' if text[..index].trim().is_empty() && !bytes.get(index+1).is_some_and(|next| matches!(next, b'[' | b'!')) => break,
            b'/' if bytes.get(index+1) == Some(&b'*') => { block = 1; index += 2; },
            quote @ (b'"' | b'\'') if quote == b'"' || (!lifetime(text, index) && bytes[index+1..].contains(&quote)) => {
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == b'\\' { index = (index+2).min(bytes.len()); }
                    else if bytes[index] == quote { index += 1; break; }
                    else { index += 1; }
                }
            }
            bracket @ (b'(' | b')' | b'[' | b']' | b'{' | b'}') => { tokens.push((index, bracket as char)); index += 1; }
            _ => index += 1,
        }
    }
    (tokens, block)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn comments_quotes_and_rust_lifetimes_do_not_hide_real_code_brackets() {
        assert_eq!(on_line("fn a<'a>() { let x = \"}\"; /* { */", 0).0.iter().map(|(_, bracket)| *bracket).collect::<String>(), "(){");
        assert_eq!(on_line("// {", 0).0, vec![]);
        assert_eq!(on_line("fn a<'a>(x: &'a str) {", 0).0.iter().map(|(_, bracket)| *bracket).collect::<String>(), "(){");
        assert_eq!(on_line("/* {", 0).1, 1);
        let (tokens, block) = on_line("} */ real()", 1);
        assert_eq!(tokens.iter().map(|(_, bracket)| *bracket).collect::<String>(), "()"); assert_eq!(block, 0);
        let (_, block) = on_line("/* outer /* inner */", 0);
        assert_eq!(block, 1);
        assert!(on_line("} inside outer", block).0.is_empty());
    }
}
