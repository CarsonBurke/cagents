//! One plain table style for interactive CLI output.

pub(super) fn table(headers: &[&str], rows: Vec<Vec<String>>, empty: &str) -> String {
    if rows.is_empty() {
        return empty.into();
    }
    let clean = |text: &str| {
        text.chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect::<String>()
    };
    let rows = rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| clean(&value))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let widths = headers
        .iter()
        .enumerate()
        .map(|(index, title)| {
            rows.iter()
                .filter_map(|row| row.get(index))
                .map(|value| value.chars().count())
                .max()
                .unwrap_or_default()
                .max(title.len())
        })
        .collect::<Vec<_>>();
    let line = |row: &[String]| {
        row.iter()
            .enumerate()
            .map(|(index, value)| {
                if index + 1 == headers.len() {
                    value.clone()
                } else {
                    format!(
                        "{value}{}",
                        " ".repeat(
                            widths
                                .get(index)
                                .copied()
                                .unwrap_or_default()
                                .saturating_sub(value.chars().count())
                                + 2
                        )
                    )
                }
            })
            .collect::<String>()
    };
    std::iter::once(line(
        &headers.iter().map(|v| (*v).into()).collect::<Vec<_>>(),
    ))
    .chain(rows.iter().map(|row| line(row)))
    .collect::<Vec<_>>()
    .join("\n")
}
