//! Reads a Chromium-based browser's cookie password from the desktop keyring,
//! from the Secret Service (GNOME Keyring, KeePassXC and others) or KWallet,
//! the two places Chromium keeps it on Linux.
//!
//! Only the requested browser's entry is read. Unrelated items, labels and
//! secrets are never listed or logged, and failures carry no detail.

use crate::{Error, Keyring, Keys};
use std::collections::HashMap;
use zeroize::Zeroizing;

type Passwords = Vec<Zeroizing<Vec<u8>>>;

#[derive(Default)]
pub struct DesktopKeys {
    /// What each keyring name returned this import, so one prompt or denial
    /// covers every profile that shares it.
    answers: HashMap<Keyring, Result<Passwords, Error>>,
}

impl Keys for DesktopKeys {
    fn passwords(&mut self, keyring: Keyring) -> Result<Passwords, Error> {
        self.answers
            .entry(keyring)
            .or_insert_with(|| lookup(keyring))
            .clone()
    }
}

fn lookup(keyring: Keyring) -> Result<Passwords, Error> {
    // Chromium uses KWallet on KDE and the Secret Service elsewhere. Ask the
    // likely one first, and the other only when it has nothing.
    let kde = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|desktop| {
        desktop
            .split(':')
            .any(|name| name.eq_ignore_ascii_case("KDE"))
    });
    let backends: [fn(Keyring) -> Result<Passwords, Error>; 2] = if kde {
        [kwallet, secret_service]
    } else {
        [secret_service, kwallet]
    };
    let mut denied = false;
    for backend in backends {
        match backend(keyring) {
            Ok(passwords) if !passwords.is_empty() => return Ok(passwords),
            Ok(_) => {}
            Err(_) => denied = true,
        }
    }
    if denied {
        Err(Error::KeyringDenied)
    } else {
        Ok(Vec::new())
    }
}

/// Every item with this `application` attribute, which is how Chromium
/// finds its own. A missing service is no answer rather than a denial.
fn secret_service(keyring: Keyring) -> Result<Passwords, Error> {
    use secret_service::{EncryptionType, blocking::SecretService};
    let Ok(service) = SecretService::connect(EncryptionType::Dh) else {
        return Ok(Vec::new());
    };
    let items = service
        .search_items(HashMap::from([("application", keyring.application())]))
        .map_err(|_| Error::KeyringDenied)?;
    // Items already unlocked need no prompt. Locked ones are unlocked only
    // when none of those held a key, so a stale locked copy never costs a
    // prompt or turns a working key into a denial.
    let read = |item: &secret_service::blocking::Item<'_>| {
        item.get_secret()
            .ok()
            .map(Zeroizing::new)
            .filter(|secret| !secret.is_empty())
    };
    let mut passwords: Passwords = items.unlocked.iter().filter_map(read).collect();
    if passwords.is_empty() {
        for item in &items.locked {
            // Unlocking may show the keyring's prompt; dismissing it is a denial.
            item.unlock().map_err(|_| Error::KeyringDenied)?;
            passwords.extend(read(item));
        }
    }
    Ok(passwords)
}

/// One KWallet daemon's D-Bus interface.
struct KWallet<'a> {
    connection: &'a zbus::blocking::Connection,
    service: &'static str,
    path: &'static str,
}

impl KWallet<'_> {
    const APPLICATION: &'static str = "cagents";

    fn call<B, R>(&self, method: &str, body: &B) -> zbus::Result<R>
    where
        B: zbus::export::serde::Serialize + zbus::zvariant::DynamicType,
        R: for<'d> zbus::export::serde::Deserialize<'d> + zbus::zvariant::Type,
    {
        self.connection
            .call_method(
                Some(self.service),
                self.path,
                Some("org.kde.KWallet"),
                method,
                body,
            )?
            .body()
            .deserialize()
    }
}

/// Chromium's KWallet entry: "<Name> Safe Storage" in the "<Name> Keys"
/// folder of the network wallet. KWallet is only asked when it is already
/// running, so this never starts a wallet daemon or its setup on another
/// desktop.
fn kwallet(keyring: Keyring) -> Result<Passwords, Error> {
    let Ok(connection) = zbus::blocking::Connection::session() else {
        return Ok(Vec::new());
    };
    for (service, path) in [
        ("org.kde.kwalletd6", "/modules/kwalletd6"),
        ("org.kde.kwalletd5", "/modules/kwalletd5"),
    ] {
        let running = connection
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "NameHasOwner",
                &service,
            )
            .and_then(|reply| reply.body().deserialize::<bool>());
        if !matches!(running, Ok(true)) {
            continue;
        }
        let wallet = KWallet {
            connection: &connection,
            service,
            path,
        };
        if !matches!(wallet.call::<_, bool>("isEnabled", &()), Ok(true)) {
            return Ok(Vec::new());
        }
        let name: String = wallet
            .call("networkWallet", &())
            .map_err(|_| Error::KeyringDenied)?;
        // Opening may prompt for the wallet password. A negative handle
        // means the user declined.
        let handle: i32 = wallet
            .call("open", &(name, 0i64, KWallet::APPLICATION))
            .map_err(|_| Error::KeyringDenied)?;
        if handle < 0 {
            return Err(Error::KeyringDenied);
        }
        let folder = keyring.kwallet_folder();
        let entry = keyring.entry();
        let password = wallet
            .call::<_, String>(
                "readPassword",
                &(
                    handle,
                    folder.as_str(),
                    entry.as_str(),
                    KWallet::APPLICATION,
                ),
            )
            .map(Zeroizing::new);
        let _ = wallet.call::<_, i32>("close", &(handle, false, KWallet::APPLICATION));
        let password = password.map_err(|_| Error::KeyringDenied)?;
        // KWallet answers an absent entry with an empty password.
        return Ok(if password.is_empty() {
            Vec::new()
        } else {
            vec![Zeroizing::new(password.as_bytes().to_vec())]
        });
    }
    Ok(Vec::new())
}
