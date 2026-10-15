//! Plain tables for interactive CLI output; JSON uses explicit command DTOs.
use unicode_width::UnicodeWidthStr;

pub(super) fn plain(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

pub(super) fn table<const N: usize>(headers: [&str; N], rows: Vec<[String; N]>, empty: &str) -> String {
    if rows.is_empty() {
        return empty.into();
    }
    let rows = rows.into_iter().map(|row| row.map(|value| plain(&value))).collect::<Vec<_>>();
    let widths: [usize; N] = std::array::from_fn(|index| {
        rows.iter().map(|row| row[index].as_str().width())
            .max().unwrap_or(0).max(headers[index].width())
    });
    let line = |row: &[String; N]| row.iter().enumerate().map(|(index, value)| {
        if index + 1 == N { value.clone() } else {
            format!("{value}{}", " ".repeat(widths[index].saturating_sub(value.as_str().width()) + 2))
        }
    }).collect::<String>();
    let titles = std::array::from_fn(|index| headers[index].to_owned());
    std::iter::once(line(&titles)).chain(rows.iter().map(line)).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_table_aligns_wide_and_combining_names_and_has_an_empty_state() {
        assert_eq!(table(["NAME", "DEFAULT"], vec![], "No accounts"), "No accounts");
        assert_eq!(table(["NAME", "DEFAULT"], vec![
            ["界".into(), "yes".into()],
            ["e\u{301}".into(), "no".into()],
        ], "No accounts"), "NAME  DEFAULT\n界    yes\ne\u{301}     no");
    }

    #[test]
    fn cli_table_keeps_control_characters_inside_one_plain_row() {
        let output = table(["NAME"], vec![["name\n\t\u{1b}end".into()]], "No accounts");
        assert_eq!(output, "NAME\nname   end");
        assert!(!output.contains('\u{1b}'));
    }
}
