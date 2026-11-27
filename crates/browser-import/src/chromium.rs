use crate::{Error, Keyring, Keys};
use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

type Key = Zeroizing<[u8; 16]>;

/// Decrypts one profile's Linux `v10`/`v11` cookie values (AES-128-CBC with
/// a PBKDF2-SHA1 key). The keyring is asked at most once per profile, and only
/// when a selected cookie needs it.
pub(super) struct Decryptor {
    keyring: Keyring,
    /// `None` until the first `v11` value; then the keyring's candidates,
    /// with the one that last verified first.
    stored: Option<Result<Vec<Key>, Error>>,
}

impl Decryptor {
    pub(super) fn new(keyring: Keyring) -> Self {
        Self {
            keyring,
            stored: None,
        }
    }

    /// `version` is the cookie database's `meta` version: from 24 on, each
    /// plaintext begins with the SHA-256 of its host, which verifies the key.
    pub(super) fn decrypt(
        &mut self,
        bytes: &[u8],
        host: &str,
        version: i64,
        keys: &mut impl Keys,
    ) -> Result<Zeroizing<String>, Error> {
        let (prefix, ciphertext) = bytes.split_at_checked(3).ok_or(Error::Decryption)?;
        let attempt = |key: &Key| open(key, ciphertext, host, version);
        match prefix {
            // `v10` uses a fixed password when no keyring was available.
            // Chromium also falls back to an empty password for either
            // prefix after a keyring failure, so try that last.
            b"v10" => [derive(b"peanuts"), derive(b"")]
                .iter()
                .find_map(attempt)
                .ok_or(Error::Decryption),
            b"v11" => {
                let stored = self.stored.get_or_insert_with(|| {
                    keys.passwords(self.keyring)
                        .map(|passwords| passwords.iter().map(|p| derive(p)).collect())
                });
                if let Ok(stored) = stored {
                    for index in 0..stored.len() {
                        if let Some(value) = attempt(&stored[index]) {
                            // Keep the verified key first for the next value.
                            stored[..=index].rotate_right(1);
                            return Ok(value);
                        }
                    }
                }
                if let Some(value) = attempt(&derive(b"")) {
                    return Ok(value);
                }
                match stored {
                    Ok(stored) if !stored.is_empty() => Err(Error::Decryption),
                    // Nothing to try: the keyring holds no key for this browser.
                    Ok(_) => Err(Error::KeyMissing),
                    Err(error) => Err(*error),
                }
            }
            _ => Err(Error::UnsupportedEncryption),
        }
    }
}

fn open(key: &Key, ciphertext: &[u8], host: &str, version: i64) -> Option<Zeroizing<String>> {
    let cipher = cbc::Decryptor::<aes::Aes128>::new_from_slices(&key[..], &[b' '; 16]).ok()?;
    let mut plain = Zeroizing::new(ciphertext.to_vec());
    let decrypted = cipher.decrypt_padded_mut::<Pkcs7>(&mut plain).ok()?;
    let payload = if version >= 24 {
        let (hash, payload) = decrypted.split_at_checked(32)?;
        if hash != Sha256::digest(host.as_bytes()).as_slice() {
            return None;
        }
        payload
    } else {
        decrypted
    };
    let value = std::str::from_utf8(payload).ok()?;
    Some(Zeroizing::new(value.to_owned()))
}

fn derive(password: &[u8]) -> Key {
    let mut key = Zeroizing::new([0; 16]);
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, b"saltysalt", 1, &mut key[..]);
    key
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use aes::cipher::BlockEncryptMut;

    /// Answers with fixed passwords and counts how often it was asked.
    struct Fixture(Vec<&'static [u8]>, usize);
    impl Keys for Fixture {
        fn passwords(&mut self, _: Keyring) -> Result<Vec<Zeroizing<Vec<u8>>>, Error> {
            self.1 += 1;
            Ok(self.0.iter().map(|p| Zeroizing::new(p.to_vec())).collect())
        }
    }

    pub(crate) fn encrypt(prefix: &[u8], password: &[u8], host: &str, version: i64) -> Vec<u8> {
        let mut payload = Vec::new();
        if version >= 24 {
            payload.extend_from_slice(&Sha256::digest(host.as_bytes()));
        }
        payload.extend_from_slice(b"fixture-cookie-value");
        let mut result = prefix.to_vec();
        result.extend(
            cbc::Encryptor::<aes::Aes128>::new_from_slices(&derive(password)[..], &[b' '; 16])
                .unwrap()
                .encrypt_padded_vec_mut::<Pkcs7>(&payload),
        );
        result
    }

    #[test]
    fn verifies_domain_binding_and_asks_the_keyring_once() {
        let mut keys = Fixture(vec![b"fixture-keyring-password"], 0);
        let mut decryptor = Decryptor::new(Keyring::Chromium);
        for (prefix, password) in [
            (b"v10", &b"peanuts"[..]),
            (b"v11", b"fixture-keyring-password"),
            (b"v11", b"fixture-keyring-password"),
        ] {
            let bytes = encrypt(prefix, password, ".example.test", 24);
            let value = decryptor
                .decrypt(&bytes, ".example.test", 24, &mut keys)
                .unwrap();
            assert_eq!(value.as_str(), "fixture-cookie-value");
            assert_eq!(
                decryptor
                    .decrypt(&bytes, ".other.test", 24, &mut keys)
                    .err(),
                Some(Error::Decryption)
            );
        }
        assert_eq!(keys.1, 1);
        for unsupported in [&b"v12nonce-and-ciphertext"[..], b"v20x", b"v1"] {
            assert_eq!(
                decryptor
                    .decrypt(unsupported, "example.test", 24, &mut keys)
                    .err(),
                Some(if unsupported.len() < 3 {
                    Error::Decryption
                } else {
                    Error::UnsupportedEncryption
                })
            );
        }
    }

    /// Several keyring items can answer one name, such as a stale copy left
    /// by an earlier install. The one that verifies wins, wherever it is.
    #[test]
    fn tries_each_stored_key_and_the_empty_password() {
        let mut keys = Fixture(vec![b"stale", b"current"], 0);
        let mut decryptor = Decryptor::new(Keyring::Chrome);
        for password in [&b"current"[..], b"current", b"", b"stale"] {
            let bytes = encrypt(b"v11", password, "example.test", 24);
            assert_eq!(
                decryptor
                    .decrypt(&bytes, "example.test", 24, &mut keys)
                    .unwrap()
                    .as_str(),
                "fixture-cookie-value"
            );
        }
        let empty_v10 = encrypt(b"v10", b"", "example.test", 24);
        assert!(
            decryptor
                .decrypt(&empty_v10, "example.test", 24, &mut keys)
                .is_ok()
        );
        let unknown = encrypt(b"v11", b"another", "example.test", 24);
        assert_eq!(
            decryptor
                .decrypt(&unknown, "example.test", 24, &mut keys)
                .err(),
            Some(Error::Decryption)
        );
        // A database from before version 24 has no host hash to check.
        let legacy = encrypt(b"v11", b"current", "example.test", 23);
        assert_eq!(
            decryptor
                .decrypt(&legacy, "anything", 23, &mut keys)
                .unwrap()
                .as_str(),
            "fixture-cookie-value"
        );
        assert_eq!(keys.1, 1);
    }

    #[test]
    fn a_missing_or_denied_key_is_a_keyring_error() {
        struct Denied;
        impl Keys for Denied {
            fn passwords(&mut self, _: Keyring) -> Result<Vec<Zeroizing<Vec<u8>>>, Error> {
                Err(Error::KeyringDenied)
            }
        }
        let bytes = encrypt(b"v11", b"secret", "example.test", 24);
        assert_eq!(
            Decryptor::new(Keyring::Brave)
                .decrypt(&bytes, "example.test", 24, &mut Denied)
                .err(),
            Some(Error::KeyringDenied)
        );
        assert_eq!(
            Decryptor::new(Keyring::Brave)
                .decrypt(&bytes, "example.test", 24, &mut Fixture(Vec::new(), 0))
                .err(),
            Some(Error::KeyMissing)
        );
    }
}
