//! Complete-record prefixes for bounded Git listing captures.

use super::GitParseError;

#[derive(Clone, Copy, Debug)]
pub(super) enum Format {
    Status,
    Files,
    Numstat,
}

impl Format {
    fn name(self) -> &'static str {
        match self {
            Self::Status => "git status",
            Self::Files => "git ls-files",
            Self::Numstat => "git diff --numstat",
        }
    }
}

/// Retain complete logical entries, including both rename paths. An output
/// capture that ended mid-entry may omit that final entry; ordinary malformed
/// Git output remains an error rather than masquerading as truncation.
pub(super) fn prefix(
    output: &[u8],
    limit: usize,
    captured_truncated: bool,
    format: Format,
) -> Result<(&[u8], bool), GitParseError> {
    let mut position = 0;
    let mut complete = 0;
    let mut entries = 0;
    while position < output.len() {
        if entries >= limit
            && !(matches!(format, Format::Status) && output[position..].starts_with(b"# "))
        {
            return Ok((&output[..complete], true));
        }
        let Some(current) = record(output, &mut position) else {
            return if captured_truncated {
                Ok((&output[..complete], true))
            } else {
                Err(GitParseError::Truncated {
                    format: format.name(),
                })
            };
        };
        let header = matches!(format, Format::Status) && current.starts_with(b"# ");
        let continuation = match format {
            Format::Status if current.starts_with(b"2 ") => 1,
            Format::Numstat
                if current.splitn(3, |byte| *byte == b'\t').nth(2) == Some(b"".as_slice()) =>
            {
                2
            }
            _ => 0,
        };
        for _ in 0..continuation {
            if record(output, &mut position).is_none() {
                return if captured_truncated {
                    Ok((&output[..complete], true))
                } else {
                    Err(GitParseError::Truncated {
                        format: format.name(),
                    })
                };
            }
        }
        complete = position;
        if !header {
            entries += 1;
        }
    }
    Ok((&output[..complete], captured_truncated))
}

fn record<'a>(output: &'a [u8], position: &mut usize) -> Option<&'a [u8]> {
    let length = output
        .get(*position..)?
        .iter()
        .position(|byte| *byte == 0)?;
    let start = *position;
    *position += length + 1;
    Some(&output[start..start + length])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_limit_keeps_headers_and_complete_rename_pairs() {
        let output = b"# branch.head main\0# branch.oid (initial)\0? first\0? second\0";
        let (kept, limited) = prefix(output, 1, false, Format::Status).unwrap();
        assert_eq!(
            kept,
            b"# branch.head main\0# branch.oid (initial)\0? first\0"
        );
        assert!(limited);
        assert_eq!(
            prefix(output, 0, false, Format::Status).unwrap(),
            (
                b"# branch.head main\0# branch.oid (initial)\0".as_slice(),
                true
            )
        );
        let rename = b"2 RM rename metadata new name\0old name\0? next\0";
        assert_eq!(
            prefix(rename, 1, false, Format::Status).unwrap(),
            (
                b"2 RM rename metadata new name\0old name\0".as_slice(),
                true
            )
        );
        assert_eq!(
            prefix(
                b"? first\02 RM rename metadata new name\0",
                10,
                true,
                Format::Status
            )
            .unwrap(),
            (b"? first\0".as_slice(), true)
        );
        assert!(
            prefix(
                b"? first\02 RM rename metadata new name\0",
                10,
                false,
                Format::Status
            )
            .is_err()
        );
    }

    #[test]
    fn numstat_limit_keeps_both_rename_names_and_literal_trailing_tabs() {
        let output = b"1\t2\t\0old\0new\03\t4\tother\0";
        assert_eq!(
            prefix(output, 1, false, Format::Numstat).unwrap(),
            (b"1\t2\t\0old\0new\0".as_slice(), true)
        );
        let literal_tab = b"1\t2\tname\t\0";
        assert_eq!(
            prefix(literal_tab, 1, false, Format::Numstat).unwrap(),
            (literal_tab.as_slice(), false)
        );
        assert_eq!(
            prefix(b"1\t2\t\0old\0", 10, true, Format::Numstat).unwrap(),
            (b"".as_slice(), true)
        );
        assert!(prefix(b"1\t2\t\0old\0", 10, false, Format::Numstat).is_err());
    }

    #[test]
    fn file_list_limit_preserves_non_utf8_and_newline_names() {
        let output = b"H first\nname\0? invalid-\xff\0? third\0";
        assert_eq!(
            prefix(output, 2, false, Format::Files).unwrap(),
            (b"H first\nname\0? invalid-\xff\0".as_slice(), true)
        );
    }

    #[test]
    fn oversized_status_yields_a_bounded_parseable_snapshot() {
        let mut output = b"# branch.head main\0# branch.oid (initial)\0".to_vec();
        let path = "x".repeat(200);
        for index in 0..90_000 {
            output.extend_from_slice(format!("? {index}-{path}\0").as_bytes());
        }
        assert!(output.len() > 16 * 1024 * 1024);
        let (kept, truncated) = prefix(&output, 10_000, false, Format::Status).unwrap();
        assert!(truncated);
        let snapshot = crate::repository::parse_status(kept).unwrap();
        assert_eq!(snapshot.entries.len(), 10_000);
        assert_eq!(snapshot.dirty.untracked, 10_000);
    }
}
