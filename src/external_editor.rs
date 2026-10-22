//! Installed editor choices for native context menus. Discovery never runs in view.
use crate::sidebar::SidebarEditor;
use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Debug, Clone)]
pub struct Editor {
    pub choice: SidebarEditor,
    pub executable: PathBuf,
}

pub fn installed() -> Vec<Editor> {
    use std::os::unix::fs::PermissionsExt;
    let paths: Vec<_> = env::var_os("PATH")
        .map(|value| {
            env::split_paths(&value)
                .filter(|path| path.is_absolute())
                .collect()
        })
        .unwrap_or_default();
    let data = desktop_directories();
    let mut editors: Vec<Editor> = Vec::new();
    for (id, name, binaries, desktops, fallback_icon) in [
        (
            "ceditor",
            "ceditor",
            &["ceditor"][..],
            &["io.github.cagents.ceditor.desktop"][..],
            "io.github.cagents.ceditor",
        ),
        (
            "code",
            "VS Code",
            &["code"][..],
            &["code.desktop", "visual-studio-code.desktop"][..],
            "visual-studio-code",
        ),
        (
            "codium",
            "VSCodium",
            &["codium"][..],
            &["codium.desktop", "vscodium.desktop"][..],
            "vscodium",
        ),
        (
            "cursor",
            "Cursor",
            &["cursor"][..],
            &["cursor.desktop"][..],
            "cursor",
        ),
        (
            "zed",
            "Zed",
            &["zed", "zeditor"][..],
            &["dev.zed.Zed.desktop", "zed.desktop"][..],
            "dev.zed.Zed",
        ),
        (
            "sublime",
            "Sublime Text",
            &["subl"][..],
            &["sublime_text.desktop", "sublime-text.desktop"][..],
            "sublime-text",
        ),
    ] {
        let executable = binaries
            .iter()
            .flat_map(|binary| paths.iter().map(move |path| path.join(binary)))
            .find(|path| {
                path.metadata().is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
            });
        let executable = executable.or_else(|| {
            if id != "ceditor" {
                return None;
            }
            let sibling = env::current_exe().ok()?.parent()?.join("ceditor");
            sibling
                .metadata()
                .is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
                .then_some(sibling)
        });
        let Some(executable) = executable else {
            continue;
        };
        let icon = data
            .iter()
            .flat_map(|directory| desktops.iter().map(move |name| directory.join(name)))
            .find_map(|path| desktop_icon(&path))
            .unwrap_or_else(|| fallback_icon.into());
        editors.push(Editor {
            choice: SidebarEditor {
                id: id.into(),
                name: name.into(),
                icon,
            },
            executable,
        });
    }
    editors
}

fn desktop_directories() -> Vec<PathBuf> {
    let mut directories = Vec::new();
    if let Some(home) = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
    {
        directories.push(home.join("applications"));
    }
    let data = env::var_os("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    directories.extend(
        env::split_paths(&data)
            .filter(|path| path.is_absolute())
            .map(|path| path.join("applications")),
    );
    directories
}

fn desktop_icon(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut text = String::new();
    fs::File::open(path)
        .ok()?
        .take(64 * 1024)
        .read_to_string(&mut text)
        .ok()?;
    let mut entry = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            entry = line == "[Desktop Entry]";
        }
        if entry && let Some(icon) = line.strip_prefix("Icon=") {
            let icon = icon.trim();
            if !icon.is_empty() && !icon.contains('\0') {
                return Some(icon.into());
            }
        }
    }
    None
}

/// Where the caret sits in the file being opened, measured in every unit an
/// editor's command line counts columns in.
///
/// Lines are counted as VS Code and Zed count them in the saved text: `\n`,
/// `\r\n` and a lone `\r` each end one. Neither the built-in editor's lines
/// nor [`crate::lsp`]'s are quite that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caret {
    /// Zero-based.
    line: u32,
    /// What precedes the caret on its line, in Unicode scalars.
    scalars: u32,
    /// The same, in UTF-16 code units.
    utf16: u32,
}

impl Caret {
    /// The caret on zero-based `line`, after `before`: the line's text up to
    /// the caret.
    #[must_use]
    pub fn new(line: u32, before: &str) -> Self {
        let count = |units: usize| u32::try_from(units).unwrap_or(u32::MAX);
        Self {
            line,
            scalars: count(before.chars().count()),
            utf16: count(before.encode_utf16().count()),
        }
    }
}

/// How an editor's command line is told where to put the caret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Syntax {
    /// `-g path:line:column`. `-g` is a boolean flag that makes every path
    /// argument, including those after `--`, carry a position.
    Goto,
    /// `path:line:column`.
    Suffix,
}

/// What a column counts.
#[derive(Debug, Clone, Copy)]
enum Unit {
    Utf16,
    Scalars,
}

/// The position syntax of the editor `id` names, if its command line has one.
///
/// Both numbers are 1-based in every syntax here.
fn position_syntax(id: &str) -> Option<(Syntax, Unit)> {
    match id {
        // `code --help`: `-g --goto <file:line[:character]>`. The column
        // becomes a Monaco `startColumn`, which indexes the line's UTF-16
        // string. Cursor and VSCodium inherit VS Code's command line.
        "code" | "codium" | "cursor" => Some((Syntax::Goto, Unit::Utf16)),
        // Zed's CLI parses `path:row:column` and, since zed-industries/zed
        // #51238 (March 2026), walks `column - 1` characters along the row
        // through `point_from_external_input`. Releases before it read the
        // column as a byte offset, which agrees only on ASCII lines.
        "zed" => Some((Syntax::Suffix, Unit::Scalars)),
        // `subl --help`: "Filenames may be given a :line or :line:column
        // suffix". Sublime Text counts columns in Unicode characters.
        "sublime" => Some((Syntax::Suffix, Unit::Scalars)),
        // ceditor has no position argument; anything else is unknown.
        _ => None,
    }
}

/// The arguments that open `path` in the editor `id` names, at `caret` when
/// that editor's command line can take a position.
///
/// The path is always one argv value after `--`, so neither a shell nor the
/// editor's option parser ever reads it.
fn arguments(id: &str, path: &Path, caret: Option<Caret>) -> Vec<OsString> {
    let positioned = caret
        .zip(position_syntax(id))
        .and_then(|(caret, (syntax, unit))| {
            let text = path.to_str()?;
            // The editor reads the position back out of the argument: VS Code
            // takes every `:`-separated number in the whole path, and Zed
            // reads both `name:row:column` and `name(row,column)` from the
            // file name. A path that already has those characters would be
            // cut in the wrong place, and opening at the top is better than
            // opening the wrong file.
            if text.contains(':') || (syntax == Syntax::Suffix && text.contains('(')) {
                return None;
            }
            let column = match unit {
                Unit::Utf16 => caret.utf16,
                Unit::Scalars => caret.scalars,
            };
            let target = format!(
                "{text}:{}:{}",
                caret.line.saturating_add(1),
                column.saturating_add(1)
            );
            Some((syntax, target))
        });
    match positioned {
        Some((Syntax::Goto, target)) => vec!["-g".into(), "--".into(), target.into()],
        Some((Syntax::Suffix, target)) => vec!["--".into(), target.into()],
        None => vec!["--".into(), path.into()],
    }
}

/// Launch a previously discovered editor, passing paths as a single argv value.
pub fn launch(editor: &Editor, path: &Path, caret: Option<Caret>) -> Result<(), String> {
    if !path.is_absolute() || !path.exists() {
        return Err("The selected file or directory is unavailable.".into());
    }
    let mut child = Command::new(&editor.executable)
        .args(arguments(&editor.choice.id, path, caret))
        .current_dir(if path.is_file() {
            path.parent().unwrap_or(path)
        } else {
            path
        })
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            format!(
                "Could not open {} in {}: {error}",
                path.display(),
                editor.choice.name
            )
        })?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATH: &str = "/work/src/main.rs";

    fn argv(id: &str, path: &str, caret: Option<Caret>) -> Vec<String> {
        arguments(id, Path::new(path), caret)
            .into_iter()
            .map(|argument| argument.into_string().unwrap())
            .collect()
    }

    #[test]
    fn a_caret_is_measured_in_every_unit_an_editor_counts() {
        // One scalar of each UTF-8 length, the last outside the Basic
        // Multilingual Plane and so two UTF-16 units.
        let caret = Caret::new(119, "aé→😀");
        assert_eq!(
            caret,
            Caret {
                line: 119,
                scalars: 4,
                utf16: 5,
            }
        );
        assert_eq!(
            Caret::new(0, ""),
            Caret {
                line: 0,
                scalars: 0,
                utf16: 0,
            }
        );
    }

    #[test]
    fn known_editors_open_at_the_caret_in_their_own_syntax() {
        // Line 120, column 8 as a reader counts them.
        let caret = Some(Caret::new(119, "let x ="));
        for id in ["code", "codium", "cursor"] {
            assert_eq!(
                argv(id, PATH, caret),
                ["-g", "--", "/work/src/main.rs:120:8"],
                "{id}"
            );
        }
        for id in ["zed", "sublime"] {
            assert_eq!(
                argv(id, PATH, caret),
                ["--", "/work/src/main.rs:120:8"],
                "{id}"
            );
        }
    }

    #[test]
    fn each_editor_gets_the_column_in_the_unit_it_counts() {
        let caret = Some(Caret::new(0, "aé→😀"));
        // VS Code columns index the line's UTF-16 string, while Zed and
        // Sublime Text count Unicode characters; all are 1-based.
        assert_eq!(argv("code", PATH, caret)[2], "/work/src/main.rs:1:6");
        assert_eq!(argv("zed", PATH, caret)[1], "/work/src/main.rs:1:5");
        assert_eq!(argv("sublime", PATH, caret)[1], "/work/src/main.rs:1:5");
    }

    #[test]
    fn editors_without_a_position_syntax_or_a_caret_get_the_plain_path() {
        let caret = Some(Caret::new(119, "let x ="));
        for id in ["ceditor", "unknown"] {
            assert_eq!(argv(id, PATH, caret), ["--", PATH], "{id}");
        }
        for id in ["code", "zed", "sublime", "ceditor"] {
            assert_eq!(argv(id, PATH, None), ["--", PATH], "{id}");
        }
    }

    /// Each editor reads the position back out of the argument by splitting
    /// on `:` (VS Code splits the whole path, Zed the file name) and Zed also
    /// reads `name(row,column)`, so a path that already has those characters
    /// would be cut in the wrong place. It opens at the top instead.
    #[test]
    fn a_path_the_editor_would_misread_opens_without_a_position() {
        let caret = Some(Caret::new(119, "let x ="));
        for id in ["code", "zed", "sublime"] {
            assert_eq!(
                argv(id, "/work/notes:5", caret),
                ["--", "/work/notes:5"],
                "{id}"
            );
        }
        for id in ["zed", "sublime"] {
            assert_eq!(
                argv(id, "/work/copy (1).md", caret),
                ["--", "/work/copy (1).md"],
                "{id}"
            );
        }
        assert_eq!(
            argv("code", "/work/copy (1).md", caret),
            ["-g", "--", "/work/copy (1).md:120:8"],
            "VS Code reads only colons"
        );
    }

    #[test]
    fn a_path_that_is_not_utf8_opens_without_a_position() {
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(std::ffi::OsStr::from_bytes(b"/work/\xff.rs"));
        let caret = Some(Caret::new(119, "let x ="));
        for id in ["code", "zed", "sublime"] {
            assert_eq!(
                arguments(id, path, caret),
                [OsString::from("--"), path.as_os_str().to_owned()],
                "{id}"
            );
        }
    }
}
