//! Shared, bounded presentation names across CLI, host, and UI.

pub const MAX_CARD_NAME_CHARS: usize = 256;

pub fn normalize_card_name(name: &str) -> Option<String> {
    let mut normalized = String::new();
    let mut kept = 0usize;
    let mut pending_space = false;
    for character in name.chars() {
        if kept == MAX_CARD_NAME_CHARS {
            break;
        }
        if character.is_whitespace() {
            // Tested before the control check, because a tab, newline or
            // carriage return is both: they separate words and so collapse to
            // one space rather than closing it up. The Unicode line and
            // paragraph separators are whitespace too, so they land here.
            // Interior runs collapse; leading whitespace is dropped.
            pending_space = kept > 0;
            continue;
        }
        if character.is_control() || is_name_formatting(character) {
            continue;
        }
        if pending_space {
            normalized.push(' ');
            kept += 1;
            pending_space = false;
            if kept == MAX_CARD_NAME_CHARS {
                break;
            }
        }
        normalized.push(character);
        kept += 1;
    }
    (!normalized.is_empty()).then_some(normalized)
}

/// Every Unicode bidi control: the three marks, the five embedding and
/// override codes, and the four isolates. They carry no glyph of their own but
/// reorder the text around them, and `char::is_control` covers C0 and C1 but
/// not these, which sit in ordinary printable categories. The line and
/// paragraph separators are not here because they are whitespace, and so are
/// already collapsed into a space. Other invisible format characters are kept
/// deliberately: a zero-width joiner is load-bearing inside an emoji sequence,
/// and unlike a bidi control none of them can reorder the text around them.
fn is_name_formatting(character: char) -> bool {
    matches!(
        character,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}
