//! Plain tables for interactive CLI output; JSON uses explicit command DTOs.
use unicode_width::UnicodeWidthStr;

pub(super) fn plain(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

pub(super) fn table<const N: usize>(
    headers: [&str; N],
    rows: Vec<[String; N]>,
    empty: &str,
) -> String {
    if rows.is_empty() {
        return empty.into();
    }
    let rows = rows
        .into_iter()
        .map(|row| row.map(|value| plain(&value)))
        .collect::<Vec<_>>();
    let widths: [usize; N] = std::array::from_fn(|index| {
        rows.iter()
            .map(|row| row[index].as_str().width())
            .max()
            .unwrap_or(0)
            .max(headers[index].width())
    });
    let line = |row: &[String; N]| {
        row.iter()
            .enumerate()
            .map(|(index, value)| {
                if index + 1 == N {
                    value.clone()
                } else {
                    format!(
                        "{value}{}",
                        " ".repeat(widths[index].saturating_sub(value.as_str().width()) + 2)
                    )
                }
            })
            .collect::<String>()
    };
    let titles = std::array::from_fn(|index| headers[index].to_owned());
    std::iter::once(line(&titles))
        .chain(rows.iter().map(line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A byte count in the largest binary unit that keeps it at least one.
pub(super) fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub(super) fn relative_time(at_ms: u64, now_ms: u64) -> String {
    let future = at_ms > now_ms;
    let seconds = at_ms.abs_diff(now_ms) / 1000;
    let (amount, unit) = if seconds < 60 {
        (seconds, "sec")
    } else if seconds < 3600 {
        (seconds / 60, "min")
    } else if seconds < 86400 {
        (seconds / 3600, "hr")
    } else if seconds < 172800 {
        (1, "day")
    } else {
        (seconds / 86400, "days")
    };
    if seconds == 0 {
        "just now".into()
    } else if future {
        format!("in {amount} {unit}")
    } else {
        format!("{amount} {unit} ago")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_relative_times_handle_clock_skew_without_raw_epoch_values() {
        assert_eq!(relative_time(120_000, 120_000), "just now");
        assert_eq!(relative_time(60_000, 120_000), "1 min ago");
        assert_eq!(relative_time(180_000, 120_000), "in 1 min");
        assert_eq!(relative_time(0, 7_200_000), "2 hr ago");
        assert_eq!(relative_time(0, 172_800_000), "2 days ago");
    }

    #[test]
    fn cli_sizes_use_binary_units() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(1023), "1023 B");
        assert_eq!(size(1536), "1.5 KiB");
        assert_eq!(size(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }

    #[test]
    fn cli_table_aligns_wide_and_combining_names_and_has_an_empty_state() {
        assert_eq!(
            table(["NAME", "DEFAULT"], vec![], "No accounts"),
            "No accounts"
        );
        assert_eq!(
            table(
                ["NAME", "DEFAULT"],
                vec![
                    ["界".into(), "yes".into()],
                    ["e\u{301}".into(), "no".into()],
                ],
                "No accounts"
            ),
            "NAME  DEFAULT\n界    yes\ne\u{301}     no"
        );
    }

    #[test]
    fn cli_table_keeps_control_characters_inside_one_plain_row() {
        let output = table(["NAME"], vec![["name\n\t\u{1b}end".into()]], "No accounts");
        assert_eq!(output, "NAME\nname   end");
        assert!(!output.contains('\u{1b}'));
    }
}
