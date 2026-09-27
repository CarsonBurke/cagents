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

On Linux the importer discovers Chrome, Chromium, Brave, Firefox, and Zen under
their usual user profile roots, including Firefox and Zen Flatpak profiles.
Firefox profiles come from `profiles.ini`. Chromium `Network/Cookies` takes
precedence over its older `Cookies` location. Chromium encrypted cookies may
prompt to unlock the matching browser key in the desktop Secret Service.
Denial is reported for that profile; another profile can still import. Retry
after unlocking the keyring or closing a browser that holds a database lock.

Cookie values never enter daemon IPC, logs, or crash messages. The importer
passes them directly from the local extraction worker to the UI process's
browser engine. Normal browsing subsequently sends cookies to their destination
sites according to browser policy. Host-only versus domain cookies, paths,
secure and HTTP-only flags, and expiration are retained. The current browser
engine does not enforce SameSite on requests, so only cookies explicitly marked
`SameSite=None` are imported. Lax, Strict, and unspecified cookies are skipped;
this can prevent some imported logins from working. Partitioned cookies and
Firefox container cookies are also skipped because flattening them into
an ordinary jar would broaden their scope. Unsupported and expired cookies are
reported as skipped; unsupported encryption reports a profile error.

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
No database or decryption temporary files are created.

Chromium v10 and v11 use AES-128-CBC with the Linux PBKDF2-SHA1 scheme. For database
version 24 and later, the decrypted SHA-256 host binding is verified before its
32 bytes are removed. Derived keys and extraction values are zeroed on drop.
The desktop key is requested once per browser for an import attempt; denials
are also cached until the user retries. Unsupported encryption never falls
back to treating ciphertext as a cookie.

Firefox schema versions through 17 are supported. Versions 16 and 17 store
expiration in milliseconds; older schemas store seconds. Newer schemas are
rejected until their semantics have been checked. Integrated builds suppress
debug and trace diagnostics from browser network and keyring dependencies even
when requested through `RUST_LOG`, because upstream diagnostics can include
cookie values and HTTP headers.

The engine imports a bounded number of cookies per turn, allowing input and
close events between chunks. Cancellation requires confirmation and retains
already imported cookies, which the result makes explicit. Servo's cookie
policy remains authoritative: only cookies observed in its jar after insertion
count as imported.
