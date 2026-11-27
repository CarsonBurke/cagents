# Importing browser logins

The integrated browser offers **Import browser logins** when its first tab opens.
Choose **Skip** to continue without reading any installed browser profile. Open
the same flow later from **Settings → Links → Browser logins**.

**Scan local profiles** grants access to local profile metadata and cookie
databases. The scan lists browsers, profiles, and cookie hosts; it does not
decrypt values. All discovered sites start selected. Deselect any site or whole
profile before choosing **Import selected**. Selecting no sites is allowed.
When profiles contain the same cookie, the first selected profile wins.

Selected cookies are installed in the shared in-app browser runtime. Reload a
tab to use the imported login; tabs opened later share the same jar. The import
lasts for the current application session. It does not change the source browser,
copy saved passwords, or import localStorage. Some services require another
sign-in even with their cookies present.

On Linux the importer discovers these browsers and channels:

| Browser | Profile root under `~/.config` | Keyring entry |
| --- | --- | --- |
| Chrome, Beta, Dev, Canary | `google-chrome`, `google-chrome-beta`, `google-chrome-unstable`, `google-chrome-canary` | Chrome |
| Chromium | `chromium` | Chromium |
| Brave, Beta, Nightly | `BraveSoftware/Brave-Browser`, `-Beta`, `-Nightly` | Brave |
| Edge, Beta, Dev, Canary | `microsoft-edge`, `microsoft-edge-beta`, `microsoft-edge-dev`, `microsoft-edge-canary` | Chromium |
| Vivaldi, Snapshot | `vivaldi`, `vivaldi-snapshot` | Chrome |
| Opera, Beta, Developer, GX | `opera`, `opera-beta`, `opera-developer`, `opera-gx` | Chromium |
| Helium | `net.imput.helium` | Chromium |
| Thorium | `thorium` | Chromium |
| Firefox | `~/.mozilla/firefox`, or `~/.config/mozilla/firefox` for new profiles from Firefox 147 | none |
| LibreWolf | `~/.librewolf` or `~/.config/librewolf/librewolf` | none |
| Zen | `~/.zen` or `~/.config/zen` | none |
| Floorp, Waterfox | `~/.floorp`, `~/.waterfox` | none |

Flatpak installs are found under `~/.var/app/<app id>/`, and Snap installs
under `~/snap/<name>/`. Some differ from what their names suggest. Flathub's
`com.microsoft.Edge` is the Beta channel. `com.google.ChromeDev` uses the stable
`google-chrome` folder inside its sandbox. The Chromium snap uses its fixed
`v10` key unless its password-manager plug is connected. The tables in `crates/browser-import/src/discovery.rs`
list each path. Profiles from different installs of one browser are labeled,
for example "Chrome (Flatpak) / Work".

Chromium profiles come from `profile.info_cache` in the browser's `Local State`,
which gives the profile folders and the names the browser shows, so a profile
renamed "Work" appears as "Work". Folders the browser no longer lists are
ignored. If `Local State` cannot be read, folders named `Default` and
`Profile N` are used. Opera's single profile lives in the root itself. Chromium
`Network/Cookies` takes precedence over the older `Cookies` location that
Helium still uses. Firefox profiles come from `profiles.ini`. When two profiles
would show the same label, such as a Firefox profile left in both
`~/.mozilla/firefox` and `~/.config/mozilla/firefox`, the profile folder is
added: "Firefox / default-release (abcd.default-release)".

Chromium encrypts cookie values on Linux with a key it keeps in the desktop
keyring. That key is stored under the build's branding, not the product name. The
Secret Service item has the `application` attribute `chrome`, `chromium` or
`brave`, labeled "Chrome Safe Storage" and so on. KWallet keeps it as the
"Chrome Safe Storage" entry in the "Chrome Keys" folder of the network wallet.
The table's Keyring entry column gives each browser's name. The importer asks
the Secret Service first, or KWallet first on KDE. It asks KWallet only when
KWallet is already running. Items that are already unlocked are read first, and
locked items are unlocked only if none of those has a key. Unlocking may prompt.
A denial, or a keyring with no entry for the browser, is reported for that
profile, and other profiles can still import.

Profiles can be scanned and imported while their browser is running. A running
Firefox keeps `cookies.sqlite` under an exclusive SQLite lock. The importer
then reads the database and its write-ahead log into memory and replays the
committed transactions. If the files change while they are read, it reads them
again. A retryable error appears only when they keep changing.

Cookie values never enter daemon IPC, logs, or crash messages. The importer
passes them directly from the local extraction worker to the UI process's
browser engine. Normal browsing subsequently sends cookies to their destination
sites according to browser policy. Host-only versus domain cookies, paths,
secure and HTTP-only flags, and expiration are retained. The current browser
engine does not enforce SameSite on requests, so only cookies explicitly marked
`SameSite=None` with Secure are imported. Lax, Strict, unspecified, and insecure
cookies are skipped; this can prevent some imported logins from working.
Partitioned cookies and Firefox container cookies are also skipped because
flattening them into an ordinary jar would broaden their scope. Unsupported and
expired cookies are reported as skipped; unsupported encryption reports a profile
error.

The browserless build contains no importer and preserves saved browser cards.

## Implementation and validation

`crates/browser-import` is independent of both Servo and the terminal host. Its
tests use synthetic databases and keys only:

```sh
cargo test --locked -p cagents-browser-import --features desktop-keyring
```

SQLite sources open read-only with a short busy timeout. The online backup API
creates a consistent in-memory snapshot including committed WAL data, rather
than copying only the main file and losing recent logins. A snapshot is capped
at 256 MiB and a scan at 100,000 cookie records. An extracted profile is capped
at 20,000 cookies and 16 MiB of cookie data; one combined import is capped at
80,000 cookies and 64 MiB. Busy or oversized profiles produce a retryable error.
No database or decryption temporary files are created, including when a locked
database is read from memory. SQLite temporary storage
stays in memory; views and untrusted schema functions are disabled on both source
and snapshot connections. Cookie and Chromium metadata tables must be ordinary
tables without generated columns, so database schemas cannot execute expressions
while their records are read.

Chromium v10 and v11 use AES-128-CBC with the Linux PBKDF2-SHA1 scheme. `v10`
uses the fixed password `peanuts`. `v11` uses the keyring password. Chromium
falls back to an empty password for either after a keyring failure, so that is
tried last. When several keyring items match, each is tried, and the one that
verifies is used from then on. For database version 24 and later, the decrypted
SHA-256 host binding is verified before its 32 bytes are removed. Derived keys
and extraction values are zeroed on drop. The desktop key is requested once per
keyring name for an import attempt. Denials are also cached until the user
retries.

A value that does not decrypt is skipped and counted, and the rest of the
profile still imports. The count appears in the result, so one cookie left over
from an earlier keyring key doesn't block the profile. If no selected value
decrypts, the profile reports that the keyring's key doesn't match. Unsupported
encryption never falls back to treating ciphertext as a cookie. One example is
`v12`, which Chromium writes only when the experimental XDG portal key provider
encrypts.

Firefox schema versions through 17 are supported. Versions 16 and 17 store
expiration in milliseconds; older schemas store seconds. Newer schemas are
rejected until their semantics have been checked. Integrated builds suppress
info, debug, and trace diagnostics from browser network and keyring dependencies
even when requested through `RUST_LOG`, because upstream diagnostics can include
cookie values and HTTP headers. In particular, the pinned Servo cookie store
logs complete Cookie headers at info level during browsing.

The engine imports a bounded number of cookies per turn, allowing input and
close events between chunks. Cancellation requires confirmation and retains
already imported cookies, which the result makes explicit. Servo's cookie
policy remains authoritative: only cookies observed in its jar after insertion
count as imported.
