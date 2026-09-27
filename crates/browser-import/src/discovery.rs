use crate::{Browser, Profile};
use std::{fs, path::Path};

/// Discovery opens profile metadata only after the caller's consent step.
/// Explicit roots also make tests independent of a developer's real profiles.
pub fn discover(home: &Path, config: &Path) -> Vec<Profile> {
    let mut profiles = Vec::new();
    for (browser, suffix) in [
        (Browser::Chrome, "google-chrome"),
        (Browser::Chromium, "chromium"),
        (Browser::Brave, "BraveSoftware/Brave-Browser"),
    ] {
        let root = config.join(suffix);
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        for entry in entries.take(256).flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name != "Default" && !name.starts_with("Profile ") {
                continue;
            }
            let directory = entry.path();
            if let Some(database) = [directory.join("Network/Cookies"), directory.join("Cookies")]
                .into_iter()
                .find(|path| path.is_file())
            {
                profiles.push(Profile {
                    browser,
                    name,
                    database,
                });
            }
        }
    }
    for (browser, root) in [
        (Browser::Firefox, home.join(".mozilla/firefox")),
        (Browser::Zen, home.join(".zen")),
        (Browser::Zen, home.join(".mozilla/zen")),
        (
            Browser::Firefox,
            home.join(".var/app/org.mozilla.firefox/.mozilla/firefox"),
        ),
        (Browser::Zen, home.join(".var/app/app.zen_browser.zen/.zen")),
    ] {
        let path = root.join("profiles.ini");
        if !fs::metadata(&path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() <= 256 * 1024)
        {
            continue;
        }
        let Ok(contents) = fs::read_to_string(path) else {
            continue;
        };
        let mut section = None;
        let mut fields = std::collections::BTreeMap::new();
        let mut finish =
            |section: &Option<String>, fields: &std::collections::BTreeMap<&str, &str>| {
                if !section
                    .as_ref()
                    .is_some_and(|name| name.starts_with("Profile"))
                {
                    return;
                }
                let Some(path) = fields.get("Path") else {
                    return;
                };
                let directory = if fields.get("IsRelative") == Some(&"0") {
                    std::path::PathBuf::from(path)
                } else {
                    root.join(path)
                };
                let database = directory.join("cookies.sqlite");
                if database.is_file() {
                    profiles.push(Profile {
                        browser,
                        name: fields.get("Name").copied().unwrap_or(path).to_owned(),
                        database,
                    });
                }
            };
        for line in contents
            .lines()
            .map(str::trim)
            .chain(std::iter::once("[end]"))
        {
            if let Some(name) = line
                .strip_prefix('[')
                .and_then(|line| line.strip_suffix(']'))
            {
                finish(&section, &fields);
                fields.clear();
                section = Some(name.to_owned());
            } else if let Some((key, value)) = line.split_once('=') {
                fields.insert(key.trim(), value.trim());
            }
        }
    }
    profiles.sort_by(|a, b| {
        (a.browser.label(), &a.name, &a.database).cmp(&(b.browser.label(), &b.name, &b.database))
    });
    let mut seen = std::collections::BTreeSet::new();
    profiles.retain(|profile| {
        seen.insert(
            std::fs::canonicalize(&profile.database).unwrap_or_else(|_| profile.database.clone()),
        )
    });
    profiles
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovers_modern_legacy_and_declared_firefox_profiles_only() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        for file in [
            ".config/google-chrome/Default/Network/Cookies",
            ".config/chromium/Profile 2/Cookies",
            ".mozilla/firefox/declared/cookies.sqlite",
            ".mozilla/firefox/not-declared/cookies.sqlite",
            ".zen/zen-one/cookies.sqlite",
        ] {
            let path = home.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, []).unwrap();
        }
        fs::write(
            home.join(".mozilla/firefox/profiles.ini"),
            "[Profile0]\nName=Work\nIsRelative=1\nPath=declared\n",
        )
        .unwrap();
        fs::write(
            home.join(".zen/profiles.ini"),
            "[Profile0]\nName=Zen\nIsRelative=1\nPath=zen-one\n",
        )
        .unwrap();
        let profiles = discover(home, &home.join(".config"));
        assert_eq!(profiles.len(), 4);
        assert!(
            profiles
                .iter()
                .any(|p| p.browser == Browser::Firefox && p.name == "Work")
        );
        assert!(profiles.iter().any(|p| p.browser == Browser::Zen));
    }
}
