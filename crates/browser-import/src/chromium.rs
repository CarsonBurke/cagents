use crate::{Browser, Error, Keys};
use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

pub(super) fn decrypt(
    bytes: &[u8],
    host: &str,
    version: i64,
    browser: Browser,
    keys: &mut impl Keys,
    cached: &mut Option<Zeroizing<[u8; 16]>>,
) -> Result<Zeroizing<String>, Error> {
    let prefix = bytes.get(..3).ok_or(Error::Decryption)?;
    let key = if prefix == b"v10" {
        derive(b"peanuts")
    } else if prefix == b"v11" {
        if cached.is_none() {
            *cached = Some(derive(&keys.password(browser)?));
        }
        Zeroizing::new(**cached.as_ref().unwrap())
    } else {
        return Err(Error::Unsupported);
    };
    let cipher = cbc::Decryptor::<aes::Aes128>::new_from_slices(&key[..], &[b' '; 16])
        .map_err(|_| Error::Decryption)?;
    let mut plain = Zeroizing::new(bytes[3..].to_vec());
    let decrypted = cipher
        .decrypt_padded_mut::<Pkcs7>(&mut plain)
        .map_err(|_| Error::Decryption)?;
    let payload = if version >= 24 {
        let hash = Sha256::digest(host.as_bytes());
        if decrypted.get(..32) != Some(hash.as_slice()) {
            return Err(Error::Decryption);
        }
        &decrypted[32..]
    } else {
        decrypted
    };
    let value = std::str::from_utf8(payload).map_err(|_| Error::Decryption)?;
    Ok(Zeroizing::new(value.to_owned()))
}

fn derive(password: &[u8]) -> Zeroizing<[u8; 16]> {
    let mut key = Zeroizing::new([0; 16]);
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, b"saltysalt", 1, &mut key[..]);
    key
}

#[cfg(feature = "desktop-keyring")]
#[derive(Default)]
pub struct DesktopKeys {
    passwords: std::collections::HashMap<Browser, Zeroizing<Vec<u8>>>,
    denied: std::collections::HashSet<Browser>,
}

#[cfg(feature = "desktop-keyring")]
impl Keys for DesktopKeys {
    fn password(&mut self, browser: Browser) -> Result<Zeroizing<Vec<u8>>, Error> {
        if let Some(password) = self.passwords.get(&browser) {
            return Ok(password.clone());
        }
        if !self.denied.insert(browser) {
            return Err(Error::KeyringDenied);
        }
        use secret_service::{EncryptionType, blocking::SecretService};
        let service =
            SecretService::connect(EncryptionType::Dh).map_err(|_| Error::KeyringDenied)?;
        let items = service
            .search_items(std::collections::HashMap::from([(
                "application",
                browser.application(),
            )]))
            .map_err(|_| Error::KeyringDenied)?;
        // Only unlock this browser's matching key. Never enumerate or log
        // unrelated items, labels, or secret values from the desktop vault.
        for item in items.unlocked.into_iter().chain(items.locked) {
            item.unlock().map_err(|_| Error::KeyringDenied)?;
            let value = item.get_secret().map_err(|_| Error::KeyringDenied)?;
            if !value.is_empty() {
                let value = Zeroizing::new(value);
                self.passwords.insert(browser, value.clone());
                return Ok(value);
            }
        }
        Err(Error::KeyringDenied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockEncryptMut;
    struct Key(usize);
    impl Keys for Key {
        fn password(&mut self, _: Browser) -> Result<Zeroizing<Vec<u8>>, Error> {
            self.0 += 1;
            Ok(Zeroizing::new(b"fixture-keyring-password".to_vec()))
        }
    }
    fn encrypted(prefix: &[u8], host: &str, version: i64) -> Vec<u8> {
        let mut payload = Vec::new();
        if version >= 24 {
            payload.extend_from_slice(&Sha256::digest(host.as_bytes()));
        }
        payload.extend_from_slice(b"fixture-cookie-value");
        let key = derive(if prefix == b"v10" {
            b"peanuts"
        } else {
            b"fixture-keyring-password"
        });
        let mut result = prefix.to_vec();
        result.extend(
            cbc::Encryptor::<aes::Aes128>::new_from_slices(&key[..], &[b' '; 16])
                .unwrap()
                .encrypt_padded_vec_mut::<Pkcs7>(&payload),
        );
        result
    }
    #[test]
    fn verifies_chromium_domain_binding_and_caches_keyring_key() {
        let mut key = Key(0);
        let mut cached = None;
        for prefix in [b"v10", b"v11", b"v11"] {
            let bytes = encrypted(prefix, ".example.test", 24);
            let value = decrypt(
                &bytes,
                ".example.test",
                24,
                Browser::Chrome,
                &mut key,
                &mut cached,
            )
            .unwrap();
            assert_eq!(value.as_str(), "fixture-cookie-value");
            assert_eq!(
                decrypt(
                    &bytes,
                    ".other.test",
                    24,
                    Browser::Chrome,
                    &mut key,
                    &mut cached
                )
                .err(),
                Some(Error::Decryption)
            );
        }
        assert_eq!(key.0, 1);
        assert_eq!(
            decrypt(
                b"v20unsupported",
                "example.test",
                24,
                Browser::Chrome,
                &mut key,
                &mut cached
            )
            .err(),
            Some(Error::Unsupported)
        );
    }
}
