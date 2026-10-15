//! Indentation detection and bounded, repository-local EditorConfig reading.
use std::{io::Read, path::Path};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Overrides {
    pub tabs: Option<bool>,
    pub size: Option<u16>,
    pub tab_width: Option<u16>,
    size_is_tab: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Indentation {
    pub tabs: bool,
    pub size: u16,
    pub tab_width: u16,
}

impl Default for Indentation {
    fn default() -> Self { Self { tabs: false, size: 4, tab_width: 4 } }
}

impl Indentation {
    pub fn detect(text: &str, fallback: u16, config: Overrides) -> Self {
        let fallback = fallback.clamp(1, 16);
        let mut tab_lines = 0;
        let mut space_lines = 0;
        let mut widths = [0usize; 17];
        let mut previous = 0;
        for line in text.lines().filter(|line| !line.trim().is_empty()).take(2000) {
            if line.starts_with('\t') { tab_lines += 1; }
            let spaces = line.bytes().take_while(|byte| *byte == b' ').count();
            if spaces > 0 { space_lines += 1; }
            let delta = spaces.abs_diff(previous);
            if delta > 0 && delta <= 16 { widths[delta] += 1; }
            previous = spaces;
        }
        let detected = (1..=8).filter(|width| widths[*width] > 0).max_by_key(|width| (widths[*width], *width));
        let tabs = config.tabs.unwrap_or(tab_lines > space_lines);
        let tab_width = config.tab_width.or(config.size).unwrap_or(fallback).clamp(1, 16);
        let size = if config.size_is_tab { tab_width } else { config.size.unwrap_or(detected.unwrap_or(fallback as usize) as u16) }.clamp(1, 16);
        Self { tabs, size, tab_width }
    }

    pub fn unit(self) -> String {
        if self.tabs { "\t".into() } else { " ".repeat(self.size as usize) }
    }

    pub fn outdent_bytes(self, line: &str) -> usize {
        if line.starts_with('\t') { 1 }
        else { line.bytes().take(self.size as usize).take_while(|byte| *byte == b' ').count() }
    }
}

/// Reads at most 64 KiB per directory and never follows a configuration link
/// outside the repository. Called by the file worker, never from the UI view.
pub fn read(worktree: &Path, path: &Path) -> Overrides {
    let Ok(root) = crate::fs_safety::Root::open(worktree) else { return Overrides::default(); };
    let mut directories: Vec<_> = path.parent().unwrap_or(Path::new("")).ancestors().collect();
    directories.reverse();
    let mut result = Overrides::default();
    for directory in directories {
        let Ok(file) = crate::fs_safety::open_beneath(&root, &directory.join(".editorconfig")) else { continue; };
        let mut text = String::new();
        if file.take(64 * 1024 + 1).read_to_string(&mut text).is_err() || text.len() > 64 * 1024 { continue; }
        if text.lines().take_while(|line| !line.trim_start().starts_with('[')).any(|line| line.trim().eq_ignore_ascii_case("root = true") || line.trim().eq_ignore_ascii_case("root=true")) { result = Overrides::default(); }
        let relative = path.strip_prefix(directory).unwrap_or(path).to_string_lossy();
        parse(&text, &relative, &mut result);
    }
    result
}

fn parse(text: &str, path: &str, result: &mut Overrides) {
    let mut active = false;
    let mut glob_budget = 4_000_000usize;
    for line in text.lines().map(str::trim).filter(|line| !line.starts_with(['#', ';'])) {
        if line.starts_with('[') && line.ends_with(']') {
            let pattern = &line[1..line.len()-1];
            let target = if pattern.contains('/') { path } else { path.rsplit('/').next().unwrap_or(path) };
            active = glob_with_budget(pattern.trim_start_matches('/'), target, &mut glob_budget);
        } else if active && let Some((key, value)) = line.split_once('=') {
            let value = value.trim().to_ascii_lowercase();
            let width = || value.parse::<u16>().ok().filter(|width| (1..=16).contains(width));
            match key.trim().to_ascii_lowercase().as_str() {
                "indent_style" => result.tabs = match value.as_str() { "tab" => Some(true), "space" => Some(false), "unset" => None, _ => result.tabs },
                "indent_size" => { result.size_is_tab = value == "tab"; result.size = width(); },
                "tab_width" => result.tab_width = width(),
                _ => {},
            }
        }
    }
}

#[cfg(test)]
fn glob(pattern: &str, text: &str) -> bool { glob_with_budget(pattern, text, &mut 4_000_000) }

fn charge_glob(budget: &mut usize, steps: usize) -> bool {
    match budget.checked_sub(steps) {
        Some(remaining) => { *budget = remaining; true },
        None => { *budget = 0; false },
    }
}

fn glob_with_budget(pattern: &str, text: &str, budget: &mut usize) -> bool {
    if pattern.len() > 512 || pattern.bytes().filter(|byte| *byte == b'{').count() > 4 || !charge_glob(budget, pattern.len().max(1)) { return false; }
    // EditorConfig brace alternatives are bounded for repository input.
    // Wildcards and ** paths are matched below without regex.
    if let Some(start) = pattern.find('{') && let Some(end) = pattern[start..].find('}').map(|end| start + end) {
        return pattern[start+1..end].split(',').take(64).any(|part| {
            glob_with_budget(&format!("{}{}{}", &pattern[..start], part, &pattern[end+1..]), text, budget)
        });
    }
    // Charge the complete dynamic-programming grid before allocating it.
    // Recursion and later sections share this budget, so individually small
    // brace groups cannot multiply into unbounded worker CPU time.
    if !charge_glob(budget, pattern.len().saturating_mul(text.len().saturating_add(1))) { return false; }
    let (pattern, text) = (pattern.as_bytes(), text.as_bytes());
    let mut current = vec![false; text.len()+1]; current[0] = true;
    let mut index = 0;
    while index < pattern.len() {
        let byte = pattern[index];
        let double = byte == b'*' && pattern.get(index+1) == Some(&b'*');
        let mut next = vec![false; text.len()+1];
        if byte == b'*' {
            if double && pattern.get(index+2) == Some(&b'/') {
                let mut reachable = false;
                for at in 0..=text.len() {
                    reachable |= current[at];
                    next[at] = current[at] || (reachable && at > 0 && text[at-1] == b'/');
                }
                index += 2;
            } else {
                for at in 0..=text.len() {
                    next[at] = current[at] || (at > 0 && next[at-1] && (double || text[at-1] != b'/'));
                }
                if double { index += 1; }
            }
        } else if byte == b'[' && let Some(close) = pattern[index+1..].iter().position(|byte| *byte == b']').map(|at| index+1+at) {
            let class = &pattern[index+1..close];
            let negated = class.first().is_some_and(|byte| matches!(byte, b'!' | b'^'));
            let class = if negated { &class[1..] } else { class };
            for at in 0..text.len() {
                let mut found = false;
                let mut item = 0;
                while item < class.len() {
                    if class.get(item+1) == Some(&b'-') && item+2 < class.len() {
                        found |= (class[item]..=class[item+2]).contains(&text[at]); item += 3;
                    } else { found |= class[item] == text[at]; item += 1; }
                }
                next[at+1] = current[at] && text[at] != b'/' && (found != negated);
            }
            index = close;
        } else if byte == b'\\' && index+1 < pattern.len() {
            index += 1;
            for at in 0..text.len() { next[at+1] = current[at] && pattern[index] == text[at]; }
        } else {
            for at in 0..text.len() { next[at+1] = current[at] && (byte == text[at] || (byte == b'?' && text[at] != b'/')); }
        }
        current = next; index += 1;
    }
    current[text.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detects_tabs_and_spaces_and_honours_explicit_editorconfig() {
        assert!(Indentation::detect("fn a() {\n\tfoo();\n}", 4, Overrides::default()).tabs);
        assert_eq!(Indentation::detect("fn a() {\n  foo();\n    bar();\n}", 4, Overrides::default()).size, 2);
        let mut config = Overrides::default();
        parse("[*.{rs,py}]\nindent_style=space\nindent_size=2\ntab_width=8\n", "a.rs", &mut config);
        assert_eq!(Indentation::detect("\tfoo()", 4, config), Indentation { tabs: false, size: 2, tab_width: 8 });
    }
    #[test]
    fn editorconfig_globs_handle_directories_classes_and_alternatives() {
        for path in ["src/a.rs", "src/deep/b.rs"] { assert!(glob("src/**/[ab].{rs,py}", path)); }
        assert!(!glob("src/**/[!ab].rs", "src/a.rs"));
        assert!(glob("src/**/[!ab].rs", "src/c.rs"));
    }

    #[test]
    fn brace_expansion_and_later_sections_share_one_work_budget() {
        let alternatives = (0..32).map(|index| format!("{index:02}")).collect::<Vec<_>>().join(",");
        let pattern = format!("{{{alternatives}}}{{{alternatives}}}{{{alternatives}}}{{{alternatives}}}");
        assert!(pattern.len() < 512);
        let mut budget = 4096;
        assert!(!glob_with_budget(&pattern, "never-matches", &mut budget));
        assert_eq!(budget, 0);
        assert!(!glob_with_budget("*", "later.rs", &mut budget));
        assert!(glob("*.{rs,py}", "normal.rs"));
    }

    #[test]
    fn nested_editorconfig_overrides_parent_and_preserves_unset_properties() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("src")).unwrap();
        std::fs::write(directory.path().join(".editorconfig"), "root=true\n[*]\nindent_style=tab\nindent_size=4\n").unwrap();
        std::fs::write(directory.path().join("src/.editorconfig"), "[*.rs]\nindent_style=space\nindent_size=2\n").unwrap();
        let config = read(directory.path(), Path::new("src/a.rs"));
        assert_eq!(config.tabs, Some(false)); assert_eq!(config.size, Some(2));
    }
}
