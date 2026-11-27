use crate::{Browser, Install, Profile};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
};

/// Where a profile root lives: under the home directory, or under the
/// user's XDG config directory (`$XDG_CONFIG_HOME`, usually `~/.config`).
#[derive(Clone, Copy)]
enum Base {
    Home,
    Config,
}

/// One install location of one browser: the directory holding Chromium's
/// `Local State` and profile folders, or Firefox's `profiles.ini`.
struct Root {
    browser: Browser,
    install: Install,
    base: Base,
    path: &'static str,
}

const fn root(browser: Browser, install: Install, base: Base, path: &'static str) -> Root {
    Root {
        browser,
        install,
        base,
        path,
    }
}

use Base::{Config, Home};
use Browser::*;
use Install::{Flatpak, Snap, System};

/// Chromium-based user data directories. A Flatpak keeps its own XDG config
/// directory at `~/.var/app/<id>/config`; a Snap keeps it inside its own
/// directory under `~/snap`.
const CHROMIUM: &[Root] = &[
    root(Chrome, System, Config, "google-chrome"),
    root(ChromeBeta, System, Config, "google-chrome-beta"),
    root(ChromeDev, System, Config, "google-chrome-unstable"),
    root(ChromeCanary, System, Config, "google-chrome-canary"),
    root(Chromium, System, Config, "chromium"),
    root(Brave, System, Config, "BraveSoftware/Brave-Browser"),
    root(
        BraveBeta,
        System,
        Config,
        "BraveSoftware/Brave-Browser-Beta",
    ),
    root(
        BraveNightly,
        System,
        Config,
        "BraveSoftware/Brave-Browser-Nightly",
    ),
    root(Edge, System, Config, "microsoft-edge"),
    root(EdgeBeta, System, Config, "microsoft-edge-beta"),
    root(EdgeDev, System, Config, "microsoft-edge-dev"),
    root(EdgeCanary, System, Config, "microsoft-edge-canary"),
    root(Vivaldi, System, Config, "vivaldi"),
    root(VivaldiSnapshot, System, Config, "vivaldi-snapshot"),
    root(Opera, System, Config, "opera"),
    root(OperaBeta, System, Config, "opera-beta"),
    root(OperaDeveloper, System, Config, "opera-developer"),
    root(OperaGx, System, Config, "opera-gx"),
    root(Helium, System, Config, "net.imput.helium"),
    root(Thorium, System, Config, "thorium"),
    root(
        Chrome,
        Flatpak,
        Home,
        ".var/app/com.google.Chrome/config/google-chrome",
    ),
    // This Flatpak runs Chrome Dev without its channel, so it uses the
    // stable folder name inside its own sandbox.
    root(
        ChromeDev,
        Flatpak,
        Home,
        ".var/app/com.google.ChromeDev/config/google-chrome",
    ),
    root(
        Chromium,
        Flatpak,
        Home,
        ".var/app/org.chromium.Chromium/config/chromium",
    ),
    root(
        UngoogledChromium,
        Flatpak,
        Home,
        ".var/app/io.github.ungoogled_software.ungoogled_chromium/config/chromium",
    ),
    root(
        Brave,
        Flatpak,
        Home,
        ".var/app/com.brave.Browser/config/BraveSoftware/Brave-Browser",
    ),
    // Flathub's com.microsoft.Edge packages the Beta channel.
    root(
        EdgeBeta,
        Flatpak,
        Home,
        ".var/app/com.microsoft.Edge/config/microsoft-edge-beta",
    ),
    root(
        Edge,
        Flatpak,
        Home,
        ".var/app/com.microsoft.Edge/config/microsoft-edge",
    ),
    root(
        EdgeDev,
        Flatpak,
        Home,
        ".var/app/com.microsoft.EdgeDev/config/microsoft-edge-dev",
    ),
    root(
        Vivaldi,
        Flatpak,
        Home,
        ".var/app/com.vivaldi.Vivaldi/config/vivaldi",
    ),
    root(
        Opera,
        Flatpak,
        Home,
        ".var/app/com.opera.Opera/config/opera",
    ),
    root(
        OperaGx,
        Flatpak,
        Home,
        ".var/app/com.opera.opera-gx/config/opera-gx",
    ),
    // Canonical's Chromium snap moves its profile to `common`; the others
    // keep the usual folder in the snap's own home.
    root(Chromium, Snap, Home, "snap/chromium/common/chromium"),
    root(
        Brave,
        Snap,
        Home,
        "snap/brave/current/.config/BraveSoftware/Brave-Browser",
    ),
    root(Vivaldi, Snap, Home, "snap/vivaldi/current/.config/vivaldi"),
    root(Opera, Snap, Home, "snap/opera/current/.config/opera"),
    root(
        OperaGx,
        Snap,
        Home,
        "snap/opera-gx/current/.config/opera-gx",
    ),
];

/// Firefox-based roots holding `profiles.ini`. Firefox 147 and current
/// LibreWolf and Zen create new profiles under the XDG config directory,
/// and keep using the legacy dot-folder where one already exists.
const FIREFOX: &[Root] = &[
    root(Firefox, System, Home, ".mozilla/firefox"),
    root(Firefox, System, Config, "mozilla/firefox"),
    root(LibreWolf, System, Home, ".librewolf"),
    root(LibreWolf, System, Config, "librewolf/librewolf"),
    root(Floorp, System, Home, ".floorp"),
    root(Waterfox, System, Home, ".waterfox"),
    root(Zen, System, Home, ".zen"),
    root(Zen, System, Config, "zen"),
    root(Zen, System, Home, ".mozilla/zen"),
    root(
        Firefox,
        Flatpak,
        Home,
        ".var/app/org.mozilla.firefox/.mozilla/firefox",
    ),
    root(
        Firefox,
        Flatpak,
        Home,
        ".var/app/org.mozilla.firefox/config/mozilla/firefox",
    ),
    root(
        LibreWolf,
        Flatpak,
        Home,
        ".var/app/io.gitlab.librewolf-community/.librewolf",
    ),
    root(
        LibreWolf,
        Flatpak,
        Home,
        ".var/app/io.gitlab.librewolf-community/config/librewolf/librewolf",
    ),
    root(Floorp, Flatpak, Home, ".var/app/one.ablaze.floorp/.floorp"),
    root(
        Waterfox,
        Flatpak,
        Home,
        ".var/app/net.waterfox.waterfox/.waterfox",
    ),
    root(Zen, Flatpak, Home, ".var/app/app.zen_browser.zen/.zen"),
    root(
        Zen,
        Flatpak,
        Home,
        ".var/app/app.zen_browser.zen/config/zen",
    ),
    root(Firefox, Snap, Home, "snap/firefox/common/.mozilla/firefox"),
];

/// Metadata size limits; a larger file is not a browser's. `Local State`
/// carries much more than profile names, so it gets more room.
const MAX_LOCAL_STATE: u64 = 16 * 1024 * 1024;
const MAX_PROFILES_INI: u64 = 256 * 1024;
/// As many profiles as one browser root may list.
const MAX_PROFILES: usize = 256;

/// Discovery opens profile metadata only after the caller's consent step.
/// Explicit roots also make tests independent of a developer's real profiles.
pub fn discover(home: &Path, config: &Path) -> Vec<Profile> {
    let path = |root: &Root| match root.base {
        Home => home.join(root.path),
        Config => config.join(root.path),
    };
    let mut profiles = Vec::new();
    for root in CHROMIUM {
        chromium(root, &path(root), &mut profiles);
    }
    for root in FIREFOX {
        firefox(root, &path(root), &mut profiles);
    }
    profiles.sort_by(|a, b| {
        (a.browser, a.install, &a.name, &a.database).cmp(&(
            b.browser,
            b.install,
            &b.name,
            &b.database,
        ))
    });
    // A root reachable two ways, such as through a symlink, lists once.
    let mut seen = BTreeSet::new();
    profiles.retain(|profile| {
        seen.insert(
            fs::canonicalize(&profile.database).unwrap_or_else(|_| profile.database.clone()),
        )
    });
    profiles
}

/// A regular file's contents, if it is no larger than `limit`, even when it
/// grows while it is read.
fn read_small(path: &Path, limit: u64) -> Option<Vec<u8>> {
    use std::io::Read;
    let file = fs::File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut contents = Vec::new();
    file.take(limit + 1).read_to_end(&mut contents).ok()?;
    (contents.len() as u64 <= limit).then_some(contents)
}

/// A Chromium profile keeps cookies in `Network/Cookies`, or in `Cookies`
/// before Chromium 96 and in some forks, such as Helium, still.
fn cookies(directory: &Path) -> Option<PathBuf> {
    [directory.join("Network/Cookies"), directory.join("Cookies")]
        .into_iter()
        .find(|path| path.is_file())
}

fn chromium(root: &Root, directory: &Path, profiles: &mut Vec<Profile>) {
    if !directory.is_dir() {
        return;
    }
    let mut found: Vec<(String, String, PathBuf)> = local_state(directory)
        .unwrap_or_else(|| scanned(directory))
        .into_iter()
        .filter_map(|(folder, name)| {
            let database = cookies(&directory.join(&folder))?;
            Some((folder, name, database))
        })
        .collect();
    // Opera keeps its only profile in the user data directory itself.
    if found.is_empty()
        && let Some(database) = cookies(directory)
    {
        found.push((String::new(), "Default".into(), database));
    }
    let mut names = BTreeMap::<String, usize>::new();
    for (_, name, _) in &found {
        *names.entry(name.clone()).or_default() += 1;
    }
    for (folder, name, database) in found {
        // Two profiles may share a display name; their folders tell them apart.
        let name = if names[&name] > 1 {
            format!("{name} ({folder})")
        } else {
            name
        };
        profiles.push(Profile {
            browser: root.browser,
            install: root.install,
            name,
            database,
        });
    }
}

/// The profiles the browser itself lists, as (folder, display name), from
/// `Local State`'s `profile.info_cache`. Profiles can be named anything,
/// and folders on disk can outlive a deleted profile.
fn local_state(directory: &Path) -> Option<Vec<(String, String)>> {
    let state: serde_json::Value = serde_json::from_slice(&read_small(
        &directory.join("Local State"),
        MAX_LOCAL_STATE,
    )?)
    .ok()?;
    let cache = state.get("profile")?.get("info_cache")?.as_object()?;
    let profiles: Vec<_> = cache
        .iter()
        .take(MAX_PROFILES)
        .filter(|(folder, _)| {
            let mut components = Path::new(folder.as_str()).components();
            matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
        })
        .map(|(folder, info)| {
            let name = info
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or(folder);
            (folder.clone(), name.chars().take(128).collect())
        })
        .collect();
    (!profiles.is_empty()).then_some(profiles)
}

/// Without a readable `Local State`, Chromium's own folder names.
fn scanned(directory: &Path) -> Vec<(String, String)> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .take(MAX_PROFILES)
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name == "Default" || name.starts_with("Profile "))
        .map(|name| (name.clone(), name))
        .collect()
}

fn firefox(root: &Root, directory: &Path, profiles: &mut Vec<Profile>) {
    let Some(contents) = read_small(&directory.join("profiles.ini"), MAX_PROFILES_INI)
        .and_then(|contents| String::from_utf8(contents).ok())
    else {
        return;
    };
    let mut section = None;
    let mut fields = BTreeMap::new();
    let mut finish = |section: &Option<&str>, fields: &BTreeMap<&str, &str>| {
        if !section.is_some_and(|name| name.starts_with("Profile")) {
            return;
        }
        let Some(path) = fields.get("Path") else {
            return;
        };
        let profile = if fields.get("IsRelative") == Some(&"0") {
            PathBuf::from(path)
        } else {
            directory.join(path)
        };
        let database = profile.join("cookies.sqlite");
        if database.is_file() {
            profiles.push(Profile {
                browser: root.browser,
                install: root.install,
                name: fields
                    .get("Name")
                    .copied()
                    .unwrap_or(path)
                    .chars()
                    .take(128)
                    .collect(),
                database,
            });
        }
    };
    for line in contents.lines().map(str::trim).chain(["[end]"]) {
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            finish(&section, &fields);
            fields.clear();
            section = Some(name);
        } else if let Some((key, value)) = line.split_once('=') {
            fields.insert(key.trim(), value.trim());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(home: &Path, file: &str) {
        let path = home.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, []).unwrap();
    }

    fn titles(home: &Path) -> Vec<String> {
        discover(home, &home.join(".config"))
            .iter()
            .map(Profile::title)
            .collect()
    }

    #[test]
    fn discovers_modern_legacy_and_declared_firefox_profiles_only() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        for file in [
            ".config/google-chrome/Default/Network/Cookies",
            ".config/chromium/Profile 2/Cookies",
            ".config/chromium/Guest Profile/Cookies",
            ".mozilla/firefox/declared/cookies.sqlite",
            ".mozilla/firefox/not-declared/cookies.sqlite",
            ".zen/zen-one/cookies.sqlite",
        ] {
            touch(home, file);
        }
        fs::write(
            home.join(".mozilla/firefox/profiles.ini"),
            "[Install4F96D1932A9F858E]\nDefault=declared\n[Profile0]\nName=Work\nIsRelative=1\nPath=declared\n",
        )
        .unwrap();
        fs::write(
            home.join(".zen/profiles.ini"),
            "[Profile0]\nName=Zen\nIsRelative=1\nPath=zen-one\n",
        )
        .unwrap();
        assert_eq!(
            titles(home),
            [
                "Chrome / Default",
                "Chromium / Profile 2",
                "Firefox / Work",
                "Zen / Zen"
            ]
        );
    }

    /// Every supported layout, each with one profile, is found under the
    /// browser and install it belongs to.
    #[test]
    fn each_browser_layout_is_discovered() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        for root in CHROMIUM {
            let base = match root.base {
                Home => "",
                Config => ".config/",
            };
            touch(home, &format!("{base}{}/Default/Cookies", root.path));
        }
        for root in FIREFOX {
            let base = match root.base {
                Home => PathBuf::new(),
                Config => PathBuf::from(".config"),
            };
            let directory = home.join(base).join(root.path);
            touch(&directory, "fixture.default/cookies.sqlite");
            fs::write(
                directory.join("profiles.ini"),
                "[Profile0]\nName=default\nIsRelative=1\nPath=fixture.default\n",
            )
            .unwrap();
        }
        let profiles = discover(home, &home.join(".config"));
        for root in CHROMIUM.iter().chain(FIREFOX) {
            let base = match root.base {
                Home => home.to_owned(),
                Config => home.join(".config"),
            };
            assert!(
                profiles
                    .iter()
                    .any(|profile| profile.browser == root.browser
                        && profile.install == root.install
                        && profile.database.starts_with(base.join(root.path))),
                "{} ({:?}) at {}",
                root.browser.label(),
                root.install,
                root.path
            );
        }
        assert_eq!(profiles.len(), CHROMIUM.len() + FIREFOX.len());
    }

    /// Helium's layout: profile folders named by the browser, cookies beside
    /// them rather than under `Network/`, and display names in `Local State`.
    #[test]
    fn local_state_names_profiles_and_excludes_unlisted_folders() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let root = home.join(".config/net.imput.helium");
        for folder in [
            "Default",
            "Profile 1",
            "Profile 3",
            "Deleted",
            "System Profile",
        ] {
            touch(&root, &format!("{folder}/Cookies"));
        }
        fs::write(
            root.join("Local State"),
            r#"{"os_crypt":{"portal":{"prev_init_success":true}},"profile":{"last_used":"Default","info_cache":{
                "Default":{"name":"Personal"},
                "Profile 1":{"name":"  "},
                "Profile 3":{"name":"Personal"},
                "Profile 9":{"name":"No cookies yet"},
                "../escape":{"name":"Outside"}
            }}}"#,
        )
        .unwrap();
        assert_eq!(
            titles(home),
            [
                "Helium / Personal (Default)",
                "Helium / Personal (Profile 3)",
                "Helium / Profile 1",
            ]
        );
        // An unreadable Local State falls back to Chromium's folder names.
        fs::write(root.join("Local State"), "{ not json").unwrap();
        assert_eq!(
            titles(home),
            [
                "Helium / Default",
                "Helium / Profile 1",
                "Helium / Profile 3"
            ]
        );
    }

    #[test]
    fn a_profile_kept_in_the_user_data_directory_itself_is_found() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        touch(home, ".config/opera/Network/Cookies");
        touch(home, ".var/app/com.opera.Opera/config/opera/Cookies");
        assert_eq!(
            titles(home),
            ["Opera / Default", "Opera (Flatpak) / Default"]
        );
    }

    #[test]
    fn installs_are_told_apart() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        touch(home, ".config/chromium/Default/Cookies");
        touch(
            home,
            ".var/app/org.chromium.Chromium/config/chromium/Default/Cookies",
        );
        touch(home, "snap/chromium/common/chromium/Default/Cookies");
        assert_eq!(
            titles(home),
            [
                "Chromium / Default",
                "Chromium (Flatpak) / Default",
                "Chromium (Snap) / Default"
            ]
        );
    }
}
