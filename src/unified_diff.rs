//! Two texts as the unified diff between them, as `diff -u` writes it.

use imara_diff::{Algorithm, Diff, InternedInput};

/// The unchanged lines shown either side of a change, as `diff -u` has it.
pub const CONTEXT: usize = 3;

/// `before` and `after` as a unified diff with [`CONTEXT`] lines of context,
/// each hunk's range followed by `section`.
///
/// Past `compared` bytes in all, the texts are listed as one hunk of removed
/// and added lines rather than compared.
pub fn unified(before: &str, after: &str, section: &str, compared: usize) -> String {
    // Text that stops short of a newline is still a last line, and comparing
    // it as written would report that line changed whenever one side went on.
    let ended = |text: &str| {
        if text.is_empty() || text.ends_with('\n') {
            text.to_owned()
        } else {
            format!("{text}\n")
        }
    };
    let (before, after) = (ended(before), ended(after));
    let mut diff = String::new();
    if before.len().saturating_add(after.len()) > compared {
        let lines =
            |text: &str| -> Vec<String> { text.split_inclusive('\n').map(str::to_owned).collect() };
        hunk(&mut diff, 0, &lines(&before), 0, &lines(&after), &[]);
    } else {
        let input = InternedInput::new(before.as_str(), after.as_str());
        let mut compared = Diff::compute(Algorithm::Histogram, &input);
        compared.postprocess_lines(&input);
        let lines = |tokens: &[imara_diff::Token]| -> Vec<String> {
            tokens
                .iter()
                .map(|token| input.interner[*token].to_owned())
                .collect()
        };
        let (before, after) = (lines(&input.before), lines(&input.after));
        // Changes whose context would touch or overlap share a hunk.
        let mut groups: Vec<Vec<(std::ops::Range<usize>, std::ops::Range<usize>)>> = Vec::new();
        for change in compared.hunks() {
            let change = (
                change.before.start as usize..change.before.end as usize,
                change.after.start as usize..change.after.end as usize,
            );
            match groups.last_mut() {
                Some(group)
                    if change.0.start - group.last().map_or(0, |last| last.0.end)
                        <= 2 * CONTEXT =>
                {
                    group.push(change)
                }
                _ => groups.push(vec![change]),
            }
        }
        for group in &groups {
            let (first, last) = (&group[0], &group[group.len() - 1]);
            let lead = first.0.start.min(CONTEXT);
            let trail = (before.len() - last.0.end).min(CONTEXT);
            hunk(
                &mut diff,
                first.0.start - lead,
                &before[first.0.start - lead..last.0.end + trail],
                first.1.start - lead,
                &after[first.1.start - lead..last.1.end + trail],
                group,
            );
        }
    }
    if !section.is_empty() {
        diff = diff
            .split_inclusive('\n')
            .map(|line| match line.strip_suffix('\n') {
                Some(range) if range.starts_with("@@ ") => format!("{range}{section}\n"),
                _ => line.to_owned(),
            })
            .collect();
    }
    diff
}

/// Writes one hunk: `before` and `after` are its lines on either side,
/// starting at the given line of each text, and `changes` the ranges of
/// those texts that differ. With no ranges given, every line does.
fn hunk(
    diff: &mut String,
    before_start: usize,
    before: &[String],
    after_start: usize,
    after: &[String],
    changes: &[(std::ops::Range<usize>, std::ops::Range<usize>)],
) {
    // A range of no lines is numbered by the line before it, as `diff` has it.
    let range = |start: usize, lines: usize| format!("{},{lines}", start + usize::from(lines > 0));
    diff.push_str(&format!(
        "@@ -{} +{} @@\n",
        range(before_start, before.len()),
        range(after_start, after.len())
    ));
    let mut write = |sign: char, lines: &[String]| {
        for line in lines {
            diff.push(sign);
            diff.push_str(line);
        }
    };
    let whole = [(
        before_start..before_start + before.len(),
        after_start..after_start + after.len(),
    )];
    let mut shown = before_start;
    for (removed, added) in if changes.is_empty() {
        &whole[..]
    } else {
        changes
    } {
        write(
            ' ',
            &before[shown - before_start..removed.start - before_start],
        );
        write(
            '-',
            &before[removed.start - before_start..removed.end - before_start],
        );
        write(
            '+',
            &after[added.start - after_start..added.end - after_start],
        );
        shown = removed.end;
    }
    write(' ', &before[shown - before_start..]);
}
