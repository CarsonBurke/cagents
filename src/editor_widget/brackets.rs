//! Lightweight lexical filtering for the bracket matcher. Only bracket
//! tokens are visited as they are read; source text is never copied.
use std::ops::ControlFlow;

fn charge(budget: &mut usize, work: usize) -> Result<(), ()> {
    *budget = budget.checked_sub(work).ok_or(())?;
    Ok(())
}

fn lifetime(text: &str, index: usize, budget: &mut usize) -> Result<bool, ()> {
    charge(budget, index)?;
    let previous = text[..index].trim_end().as_bytes().last();
    if !previous.is_some_and(|byte| matches!(byte, b'<' | b'&' | b':' | b',' | b'+')) {
        return Ok(false);
    }
    let tail = &text.as_bytes()[index + 1..];
    let mut length = 0;
    for byte in tail {
        charge(budget, 1)?;
        if !byte.is_ascii_alphanumeric() && *byte != b'_' {
            break;
        }
        length += 1;
    }
    Ok(length > 0 && tail.get(length) != Some(&b'\''))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct State {
    comment_depth: usize,
    quote: Option<u8>,
    raw_hashes: Option<usize>,
}

/// The visitor may stop before the rest of a long line is scanned. Every scan
/// and lookahead spends the same caller-owned work budget.
pub(super) fn scan_line<B>(
    text: &str,
    mut state: State,
    budget: &mut usize,
    mut visit: impl FnMut(usize, char) -> ControlFlow<B>,
) -> Result<ControlFlow<B, State>, ()> {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        charge(budget, 1)?;
        if state.comment_depth > 0 {
            if bytes.get(index..index + 2) == Some(b"/*") {
                state.comment_depth += 1;
                index += 2;
            } else if bytes.get(index..index + 2) == Some(b"*/") {
                state.comment_depth -= 1;
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if let Some(hashes) = state.raw_hashes {
            charge(budget, hashes)?;
            if bytes[index] == b'"'
                && bytes
                    .get(index + 1..index + 1 + hashes)
                    .is_some_and(|suffix| suffix.iter().all(|byte| *byte == b'#'))
            {
                state.raw_hashes = None;
                index += 1 + hashes;
            } else {
                index += 1;
            }
            continue;
        }
        if let Some(quote) = state.quote {
            if bytes[index] == b'\\' {
                index = (index + 2).min(bytes.len());
            } else if bytes[index] == quote {
                state.quote = None;
                index += 1;
            } else {
                index += 1;
            }
            continue;
        }
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => break,
            b'#' if {
                charge(budget, index)?;
                text[..index].trim().is_empty()
            } && !bytes
                .get(index + 1)
                .is_some_and(|next| matches!(next, b'[' | b'!')) =>
            {
                break;
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                state.comment_depth = 1;
                index += 2;
            }
            b'r' => {
                let mut hashes = 0;
                while bytes.get(index + 1 + hashes) == Some(&b'#') {
                    charge(budget, 1)?;
                    hashes += 1;
                }
                if bytes.get(index + 1 + hashes) == Some(&b'"') {
                    state.raw_hashes = Some(hashes);
                    index += hashes + 2;
                } else {
                    index += 1;
                }
            }
            quote @ (b'"' | b'\'')
                if quote == b'"'
                    || (!lifetime(text, index, budget)? && {
                        charge(budget, bytes.len() - index - 1)?;
                        bytes[index + 1..].contains(&quote)
                    }) =>
            {
                state.quote = Some(quote);
                index += 1;
            }
            bracket @ (b'(' | b')' | b'[' | b']' | b'{' | b'}') => {
                if let ControlFlow::Break(result) = visit(index, bracket as char) {
                    return Ok(ControlFlow::Break(result));
                }
                index += 1;
            }
            _ => index += 1,
        }
    }
    Ok(ControlFlow::Continue(state))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn on_line(text: &str, state: State) -> (Vec<(usize, char)>, State) {
        let mut tokens = Vec::new();
        let result = scan_line(text, state, &mut usize::MAX, |at, bracket| {
            tokens.push((at, bracket));
            ControlFlow::<()>::Continue(())
        })
        .unwrap();
        let ControlFlow::Continue(state) = result else {
            unreachable!()
        };
        (tokens, state)
    }

    #[test]
    fn streaming_stops_at_the_first_pair_and_bounds_prefix_and_lookahead_work() {
        let text = "()".repeat(4 * 1024 * 1024);
        let mut budget = 8;
        let result = scan_line(&text, State::default(), &mut budget, |at, _| {
            if at == 1 {
                ControlFlow::Break(at)
            } else {
                ControlFlow::Continue(())
            }
        });
        assert_eq!(result, Ok(ControlFlow::Break(1)));
        assert_eq!(budget, 6, "the rest of the 8MiB line must not be scanned");
        for text in [
            "x".repeat(1024),
            format!("r{}\"()\"", "#".repeat(1024)),
            format!("'{}", "x".repeat(1024)),
        ] {
            assert!(
                scan_line(&text, State::default(), &mut 16, |_, _| {
                    ControlFlow::<()>::Continue(())
                })
                .is_err()
            );
        }
    }
    #[test]
    fn comments_quotes_and_rust_lifetimes_do_not_hide_real_code_brackets() {
        assert_eq!(
            on_line("fn a<'a>() { let x = \"}\"; /* { */", State::default())
                .0
                .iter()
                .map(|(_, bracket)| *bracket)
                .collect::<String>(),
            "(){"
        );
        assert_eq!(on_line("// {", State::default()).0, vec![]);
        assert_eq!(
            on_line("fn a<'a>(x: &'a str) {", State::default())
                .0
                .iter()
                .map(|(_, bracket)| *bracket)
                .collect::<String>(),
            "(){"
        );
        assert_eq!(on_line("/* {", State::default()).1.comment_depth, 1);
        let (tokens, block) = on_line(
            "} */ real()",
            State {
                comment_depth: 1,
                ..State::default()
            },
        );
        assert_eq!(
            tokens
                .iter()
                .map(|(_, bracket)| *bracket)
                .collect::<String>(),
            "()"
        );
        assert_eq!(block, State::default());
        let (_, block) = on_line("/* outer /* inner */", State::default());
        assert_eq!(block.comment_depth, 1);
        assert!(on_line("} inside outer", block).0.is_empty());
    }
}
