//! Plain tables for interactive CLI output; JSON uses explicit command DTOs.
use std::path::Path;

use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::CommandOutput;

/// Render a successful listing in the requested format.
pub(super) fn listing_output<T>(
    command: &'static str,
    listing: T,
    human_output: bool,
    human: impl FnOnce(T) -> String,
    data: impl FnOnce(T) -> Value,
) -> CommandOutput {
    let (human, data) = if human_output {
        (human(listing), Value::Null)
    } else {
        (String::new(), data(listing))
    };
    CommandOutput {
        command,
        human,
        data,
    }
}

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
    render(&headers, rows.into_iter().map(Vec::from).collect(), empty)
}

/// A [`table`] without the columns no row has a value for, so a fact every
/// row lacks costs no space.
pub(super) fn sparse_table<const N: usize>(
    headers: [&str; N],
    rows: Vec<[String; N]>,
    empty: &str,
) -> String {
    let kept = columns_in_use(&rows);
    table_of(headers, rows, kept, empty)
}

/// Which of `rows`' columns any row has a value for. A value of `-` counts
/// as none. Tables of one listing that share these keep the same columns.
pub(super) fn columns_in_use<const N: usize>(rows: &[[String; N]]) -> [bool; N] {
    std::array::from_fn(|index| {
        rows.iter()
            .any(|row| !row[index].is_empty() && row[index] != "-")
    })
}

/// A [`table`] of only the columns `kept` marks.
pub(super) fn table_of<const N: usize>(
    headers: [&str; N],
    rows: Vec<[String; N]>,
    kept: [bool; N],
    empty: &str,
) -> String {
    let pick = |row: [String; N]| -> Vec<String> {
        row.into_iter()
            .zip(kept)
            .filter_map(|(value, kept)| kept.then_some(value))
            .collect()
    };
    render(
        &pick(headers.map(str::to_owned))
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        rows.into_iter().map(pick).collect(),
        empty,
    )
}

fn render(headers: &[&str], rows: Vec<Vec<String>>, empty: &str) -> String {
    if rows.is_empty() {
        return empty.into();
    }
    let columns = headers.len();
    let rows = rows
        .into_iter()
        .map(|row| row.iter().map(|value| plain(value)).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let widths: Vec<usize> = (0..columns)
        .map(|index| {
            rows.iter()
                .map(|row| row[index].as_str().width())
                .max()
                .unwrap_or(0)
                .max(headers[index].width())
        })
        .collect();
    let line = |row: &[String]| {
        row.iter()
            .enumerate()
            .map(|(index, value)| {
                if index + 1 == columns {
                    value.clone()
                } else {
                    format!(
                        "{value}{}",
                        " ".repeat(widths[index].saturating_sub(value.as_str().width()) + 2)
                    )
                }
            })
            .collect::<String>()
            .trim_end()
            .to_owned()
    };
    let titles: Vec<String> = headers.iter().map(|&header| header.to_owned()).collect();
    std::iter::once(line(&titles))
        .chain(rows.iter().map(|row| line(row)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The fewest characters a listed session or agent ID is shown by. Eight
/// hexadecimal digits keep a prefix unique among the few hundred sessions
/// and agents one daemon hosts, and the commands that name them, such as
/// `send`, `transcript` and `agent`, accept a unique prefix. `terminal`
/// commands take a full ID, so a plain shell's is listed whole.
pub(super) const MIN_ABBREVIATION: usize = 8;

/// The shortest prefix length, at least [`MIN_ABBREVIATION`], that tells
/// every one of `ids` apart, as Git abbreviates object names. One length
/// serves a whole listing, so its ID column stays aligned. A prefix never
/// ends at a UUID's hyphen.
pub(super) fn abbreviation<'a>(ids: impl IntoIterator<Item = &'a str>) -> usize {
    let mut ids: Vec<&str> = ids.into_iter().collect();
    ids.sort_unstable();
    ids.dedup();
    let longest = ids.iter().map(|id| id.len()).max().unwrap_or(0);
    let mut length = MIN_ABBREVIATION;
    while length < longest
        && (ids
            .windows(2)
            .any(|pair| prefix(pair[0], length) == prefix(pair[1], length))
            || ids.iter().any(|id| prefix(id, length).ends_with('-')))
    {
        length += 1;
    }
    length
}

/// `id` cut to `length` characters, whole when it is no longer.
pub(super) fn prefix(id: &str, length: usize) -> &str {
    id.char_indices()
        .nth(length)
        .map_or(id, |(end, _)| &id[..end])
}

/// `text` in at most `width` columns, ending in an ellipsis when cut.
pub(super) fn clip(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let mut clipped = String::new();
    let mut used = 0;
    for character in text.chars() {
        let size = character.width().unwrap_or(0);
        if used + size + 1 > width {
            break;
        }
        used += size;
        clipped.push(character);
    }
    clipped.truncate(clipped.trim_end().len());
    clipped.push('…');
    clipped
}

/// `path` as a listing scoped to the checkout at `root` shows it: relative
/// to that root, `.` for the root itself, and otherwise with the home
/// directory shortened to `~`.
pub(super) fn location(path: &Path, root: Option<&Path>) -> String {
    location_in(
        path,
        root,
        std::env::var_os("HOME").as_deref().map(Path::new),
    )
}

fn location_in(path: &Path, root: Option<&Path>, home: Option<&Path>) -> String {
    if let Some(relative) = root.and_then(|root| path.strip_prefix(root).ok()) {
        return if relative.as_os_str().is_empty() {
            ".".into()
        } else {
            relative.display().to_string()
        };
    }
    match home
        .filter(|home| home.is_absolute())
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(relative) if !relative.as_os_str().is_empty() => {
            format!("~/{}", relative.display())
        }
        _ => path.display().to_string(),
    }
}

/// A listing's next steps, each a runnable command and what it does, in the
/// one line every command ends its output with: "hint: `cagents …` does this;
/// `cagents …` that". Empty when there are none.
pub(super) fn hint<S: AsRef<str>>(steps: &[(S, &str)]) -> String {
    if steps.is_empty() {
        return String::new();
    }
    let steps = steps
        .iter()
        .map(|(command, does)| format!("`{}` {does}", command.as_ref()))
        .collect::<Vec<_>>();
    format!("hint: {}", steps.join("; "))
}

/// How a hint names this program: with the device this command ran
/// against, so a suggested command reaches the same daemon.
pub(super) fn program() -> String {
    match crate::device::selected() {
        Some(device) => format!("cagents --device {}", shell_word(&device.name)),
        None => "cagents".into(),
    }
}

/// `word` quoted for a POSIX shell when it needs to be.
pub(super) fn shell_word(word: &str) -> String {
    if !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./~:@%+=,".contains(c))
    {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// `text` with the home directory in any path it holds shortened to `~`,
/// as [`location`] shows a path.
pub(super) fn home_shortened(text: &str) -> String {
    home_shortened_in(text, std::env::var("HOME").ok().as_deref())
}

fn home_shortened_in(text: &str, home: Option<&str>) -> String {
    match home.map(|home| home.trim_end_matches('/')) {
        Some(home) if home.starts_with('/') && home.len() > 1 => {
            text.replace(&format!("{home}/"), "~/")
        }
        _ => text.to_owned(),
    }
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

#[cfg(all(test, not(any(feature = "host", feature = "ui"))))]
mod tests {
    use super::*;

    #[test]
    fn a_listing_runs_only_the_requested_renderer() {
        let human = listing_output("fixture.list", "item", true, String::from, |_| {
            panic!("rendered JSON for a human listing")
        });
        assert_eq!(human.human, "item");
        assert_eq!(human.data, Value::Null);

        let json = listing_output(
            "fixture.list",
            "item",
            false,
            |_| panic!("rendered a human listing for JSON"),
            Value::from,
        );
        assert!(json.human.is_empty());
        assert_eq!(json.data, "item");
    }

    #[test]
    fn notes_shorten_the_home_directory_in_the_paths_they_quote() {
        let error = "fatal: cannot open /home/me/repo/.git: denied";
        assert_eq!(
            home_shortened_in(error, Some("/home/me/")),
            "fatal: cannot open ~/repo/.git: denied"
        );
        assert_eq!(
            home_shortened_in("/home/meg/x", Some("/home/me")),
            "/home/meg/x"
        );
        assert_eq!(home_shortened_in("/x", Some("/")), "/x");
        assert_eq!(home_shortened_in("/x", None), "/x");
    }

    #[test]
    fn hinted_words_are_quoted_for_the_shell() {
        assert_eq!(shell_word("redact"), "redact");
        assert_eq!(shell_word("~/a/b-c"), "~/a/b-c");
        assert_eq!(shell_word("Just Make Money"), "'Just Make Money'");
        assert_eq!(shell_word("it's"), r"'it'\''s'");
        assert_eq!(shell_word(""), "''");
    }

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

    #[test]
    fn listed_ids_are_the_shortest_prefixes_that_tell_them_apart() {
        let a = "9f005968-f150-4c49-b1aa-c5431bab72f8";
        let b = "a669dbd8-ae3c-496e-96c8-0a9e2dca8d16";
        assert_eq!(abbreviation([a, b]), MIN_ABBREVIATION);
        assert_eq!(prefix(a, abbreviation([a, b])), "9f005968");
        // A repeated ID is one ID.
        assert_eq!(abbreviation([a, a]), MIN_ABBREVIATION);
        // Sharing the first eight digits, a prefix runs past the hyphen.
        let c = "9f005968-f1aa-4000-8000-000000000000";
        assert_eq!(prefix(a, abbreviation([a, c])), "9f005968-f15");
        let d = "9f005968-0000-4000-8000-000000000000";
        assert_eq!(prefix(a, abbreviation([a, d])), "9f005968-f");
        assert_eq!(abbreviation(std::iter::empty()), MIN_ABBREVIATION);
        assert_eq!(prefix("system", MIN_ABBREVIATION), "system");
        assert_eq!(prefix("界界界", 2), "界界");
    }

    #[test]
    fn clipped_text_fits_its_width_and_says_it_was_cut() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("exactly10!", 10), "exactly10!");
        assert_eq!(clip("one two three", 9), "one two…");
        assert_eq!(clip("界界界界", 5), "界界…");
    }

    #[test]
    fn locations_are_relative_to_the_listed_checkout_or_home() {
        let home = Some(Path::new("/home/me"));
        let root = Some(Path::new("/home/me/repo"));
        let at = |path: &str, root| location_in(Path::new(path), root, home);
        assert_eq!(at("/home/me/repo", root), ".");
        assert_eq!(at("/home/me/repo/.worktrees/fix", root), ".worktrees/fix");
        assert_eq!(at("/home/me/repository", root), "~/repository");
        assert_eq!(at("/home/me/other", None), "~/other");
        assert_eq!(at("/home/me", None), "/home/me");
        assert_eq!(at("/var/tmp/linked", root), "/var/tmp/linked");
        assert_eq!(
            location_in(Path::new("/srv/x"), None, Some(Path::new("relative"))),
            "/srv/x"
        );
    }

    #[test]
    fn hints_are_runnable_commands_in_one_line() {
        assert_eq!(hint::<&str>(&[]), "");
        assert_eq!(
            hint(&[
                ("cagents transcript SESSION", "reads one"),
                ("cagents ls --all", "lists every project"),
            ]),
            "hint: `cagents transcript SESSION` reads one; `cagents ls --all` lists every project"
        );
    }

    #[test]
    fn sparse_tables_drop_columns_no_row_has() {
        assert_eq!(
            sparse_table(
                ["A", "B", "C"],
                vec![
                    ["1".into(), "-".into(), "x".into()],
                    ["2".into(), String::new(), "y".into()],
                ],
                "None"
            ),
            "A  C\n1  x\n2  y"
        );
        assert_eq!(sparse_table(["A"], vec![], "None"), "None");
        assert_eq!(
            columns_in_use(&[["a".into(), "-".into()], [String::new(), "b".into()]]),
            [true, true]
        );
    }
}
