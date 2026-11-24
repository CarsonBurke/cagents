//! Verifies the signature `ssh-keygen -Y sign -n cagents-release` makes over a
//! release's metadata (OpenSSH's SSHSIG format), against the keys this build
//! trusts. A release is only as trustworthy as its signing key: GitHub, its
//! download servers and every token that can publish a release are not.
//!
//! Plain `ssh-ed25519` keys and `sk-ssh-ed25519@openssh.com` security keys are
//! accepted. A security-key signature must assert that a person touched the
//! key.
use super::{UpdateError, invalid};
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256, Sha512};

/// What the signature is for, so no other SSH signature by the same key
/// (a Git commit, a file) can stand in for a release's.
pub(crate) const NAMESPACE: &str = "cagents-release";

const MAGIC: &[u8; 6] = b"SSHSIG";
const VERSION: u32 = 1;
const ED25519: &[u8] = b"ssh-ed25519";
const SK_ED25519: &[u8] = b"sk-ssh-ed25519@openssh.com";
const USER_PRESENT: u8 = 0x01;
const ARMOR_BEGIN: &str = "-----BEGIN SSH SIGNATURE-----";
const ARMOR_END: &str = "-----END SSH SIGNATURE-----";

/// The keys that sign published releases, one OpenSSH public key per line.
/// Replacing a key is a release signed by the old one whose build trusts the
/// new one; dropping a compromised key is a release that no longer lists it.
const RELEASE_KEYS: &str = include_str!("release-keys.pub");

/// A public key in SSH wire form, as `ssh-keygen` embeds it in a signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrustedKey(Vec<u8>);

impl TrustedKey {
    /// One `authorized_keys`-style line: `TYPE BASE64 [COMMENT]`.
    pub(crate) fn parse(line: &str) -> Result<Self, UpdateError> {
        let mut fields = line.split_whitespace();
        let (Some(kind), Some(encoded)) = (fields.next(), fields.next()) else {
            return Err(invalid("malformed release signing key"));
        };
        let blob = STANDARD
            .decode(encoded)
            .map_err(|_| invalid("malformed release signing key"))?;
        let key = PublicKey::parse(&blob)?;
        if key.kind() != kind.as_bytes() {
            return Err(invalid("release signing key type does not match its key"));
        }
        Ok(Self(blob))
    }
}

/// The keys this build trusts. A build made to rehearse updates adds the
/// rehearsal key it was given at compile time; published builds have none.
pub(crate) fn trusted_keys() -> Result<Vec<TrustedKey>, UpdateError> {
    RELEASE_KEYS
        .lines()
        .chain(
            option_env!("CAGENTS_UPDATE_REHEARSAL_KEYS")
                .unwrap_or("")
                .lines(),
        )
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(TrustedKey::parse)
        .collect()
}

/// Whether this build was made to rehearse updates against a private feed.
pub(crate) fn rehearsal_build() -> bool {
    option_env!("CAGENTS_UPDATE_REHEARSAL_KEYS").is_some_and(|keys| !keys.trim().is_empty())
}

/// Accepts `message` only when `armored` is a `cagents-release` signature of
/// it by one of `trusted`.
pub(crate) fn verify(
    message: &[u8],
    armored: &str,
    trusted: &[TrustedKey],
) -> Result<(), UpdateError> {
    let blob = dearmor(armored)?;
    let mut wire = Wire(&blob);
    if wire.take(MAGIC.len())? != MAGIC || wire.u32()? != VERSION {
        return Err(malformed());
    }
    let public = wire.string()?;
    let namespace = wire.string()?;
    let reserved = wire.string()?;
    let algorithm = wire.string()?;
    let signature = wire.string()?;
    wire.end()?;
    if namespace != NAMESPACE.as_bytes() {
        return Err(invalid(
            "the release signature is not a cagents release signature",
        ));
    }
    if !trusted.iter().any(|key| key.0 == public) {
        return Err(invalid(
            "the release is signed by a key this build does not trust",
        ));
    }
    let digest = match algorithm {
        b"sha512" => Sha512::digest(message).to_vec(),
        b"sha256" => Sha256::digest(message).to_vec(),
        _ => return Err(invalid("the release signature uses an unsupported hash")),
    };
    let mut signed = MAGIC.to_vec();
    for field in [NAMESPACE.as_bytes(), reserved, algorithm, &digest] {
        put_string(&mut signed, field);
    }
    let key = PublicKey::parse(public)?;
    let mut wire = Wire(signature);
    if wire.string()? != key.kind() {
        return Err(malformed());
    }
    let signature = Signature::from_slice(wire.string()?).map_err(|_| malformed())?;
    let verified = match key {
        PublicKey::Ed25519(key) => {
            wire.end()?;
            key.verify_strict(&signed, &signature)
        }
        PublicKey::SecurityKey { key, application } => {
            let flags = wire.byte()?;
            let counter = wire.u32()?;
            wire.end()?;
            if flags & USER_PRESENT == 0 {
                return Err(invalid(
                    "the release signature was made without touching its security key",
                ));
            }
            // What a FIDO authenticator signs: the application's hash, its
            // flags and counter, then the hash of the message.
            let mut assertion = Sha256::digest(application).to_vec();
            assertion.push(flags);
            assertion.extend_from_slice(&counter.to_be_bytes());
            assertion.extend_from_slice(&Sha256::digest(&signed));
            key.verify_strict(&assertion, &signature)
        }
    };
    verified.map_err(|_| invalid("the release signature does not match its metadata"))
}

enum PublicKey<'a> {
    Ed25519(VerifyingKey),
    SecurityKey {
        key: VerifyingKey,
        application: &'a [u8],
    },
}

impl<'a> PublicKey<'a> {
    fn parse(blob: &'a [u8]) -> Result<Self, UpdateError> {
        let mut wire = Wire(blob);
        let kind = wire.string()?;
        let key = <[u8; 32]>::try_from(wire.string()?).map_err(|_| malformed())?;
        let key = VerifyingKey::from_bytes(&key).map_err(|_| malformed())?;
        let parsed = match kind {
            ED25519 => Self::Ed25519(key),
            SK_ED25519 => Self::SecurityKey {
                key,
                application: wire.string()?,
            },
            _ => return Err(invalid("release signing keys must be Ed25519")),
        };
        wire.end()?;
        Ok(parsed)
    }

    fn kind(&self) -> &'static [u8] {
        match self {
            Self::Ed25519(_) => ED25519,
            Self::SecurityKey { .. } => SK_ED25519,
        }
    }
}

fn dearmor(armored: &str) -> Result<Vec<u8>, UpdateError> {
    let body = armored
        .trim()
        .strip_prefix(ARMOR_BEGIN)
        .and_then(|rest| rest.strip_suffix(ARMOR_END))
        .ok_or_else(malformed)?;
    let encoded: String = body.split_whitespace().collect();
    STANDARD.decode(encoded).map_err(|_| malformed())
}

fn malformed() -> UpdateError {
    invalid("the release signature is malformed")
}

fn put_string(out: &mut Vec<u8>, field: &[u8]) {
    out.extend_from_slice(&(field.len() as u32).to_be_bytes());
    out.extend_from_slice(field);
}

/// SSH wire encoding: big-endian integers and length-prefixed strings.
struct Wire<'a>(&'a [u8]);

impl<'a> Wire<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], UpdateError> {
        if self.0.len() < length {
            return Err(malformed());
        }
        let (taken, rest) = self.0.split_at(length);
        self.0 = rest;
        Ok(taken)
    }
    fn byte(&mut self) -> Result<u8, UpdateError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, UpdateError> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes(bytes.try_into().expect("four bytes")))
    }
    fn string(&mut self) -> Result<&'a [u8], UpdateError> {
        let length = self.u32()? as usize;
        self.take(length)
    }
    fn end(&self) -> Result<(), UpdateError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(malformed())
        }
    }
}

/// A release signature by `key`, and the trusted entry for that key, as
/// `ssh-keygen -Y sign` would make them.
#[cfg(test)]
pub(crate) fn sign_for_test(
    key: &ed25519_dalek::SigningKey,
    message: &[u8],
) -> (TrustedKey, String) {
    use ed25519_dalek::Signer;
    let mut public = Vec::new();
    put_string(&mut public, ED25519);
    put_string(&mut public, key.verifying_key().as_bytes());
    let mut signed = MAGIC.to_vec();
    let digest = Sha512::digest(message);
    for field in [NAMESPACE.as_bytes(), b"", b"sha512", &digest] {
        put_string(&mut signed, field);
    }
    let mut signature = Vec::new();
    put_string(&mut signature, ED25519);
    put_string(&mut signature, &key.sign(&signed).to_bytes());
    let mut blob = MAGIC.to_vec();
    blob.extend_from_slice(&VERSION.to_be_bytes());
    for field in [
        &public[..],
        NAMESPACE.as_bytes(),
        b"",
        b"sha512",
        &signature,
    ] {
        put_string(&mut blob, field);
    }
    let armored = format!("{ARMOR_BEGIN}\n{}\n{ARMOR_END}\n", STANDARD.encode(blob));
    (TrustedKey(public), armored)
}

#[cfg(all(test, not(any(feature = "host", feature = "ui"))))]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    // Made with OpenSSH: `ssh-keygen -t ed25519` and
    // `ssh-keygen -Y sign -f k -n cagents-release m`.
    const OPENSSH_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKX1+KzG3x9aX1jozVOjPNCE5ZeH0EnizkoGzEy3kz07 cagents test vector";
    const OPENSSH_MESSAGE: &[u8] = b"{\"schema_version\":2,\"version\":\"9.9.9\"}\n";
    const OPENSSH_SIGNATURE: &str = "-----BEGIN SSH SIGNATURE-----
U1NIU0lHAAAAAQAAADMAAAALc3NoLWVkMjU1MTkAAAAgpfX4rMbfH1pfWOjNU6M80ITll4
fQSeLOSgbMTLeTPTsAAAAPY2FnZW50cy1yZWxlYXNlAAAAAAAAAAZzaGE1MTIAAABTAAAA
C3NzaC1lZDI1NTE5AAAAQN78ij9Uzlbw0snfB4xB3nG/5JxHYL+AGf36GrJFJIbn9j7/wU
1mBXGyYJvEG8TmfIvJLY0EqenF7tHXbSvLwQA=
-----END SSH SIGNATURE-----
";

    fn key() -> Vec<TrustedKey> {
        vec![TrustedKey::parse(OPENSSH_KEY).unwrap()]
    }

    #[test]
    fn an_openssh_signature_by_a_trusted_key_verifies() {
        verify(OPENSSH_MESSAGE, OPENSSH_SIGNATURE, &key()).unwrap();
    }

    #[test]
    fn changed_metadata_an_untrusted_key_or_a_damaged_signature_is_refused() {
        assert!(verify(b"{\"version\":\"9.9.8\"}\n", OPENSSH_SIGNATURE, &key()).is_err());
        let other = SigningKey::from_bytes(&[7; 32]);
        let other = TrustedKey(ed25519_blob(&other));
        assert!(verify(OPENSSH_MESSAGE, OPENSSH_SIGNATURE, &[other]).is_err());
        assert!(verify(OPENSSH_MESSAGE, OPENSSH_SIGNATURE, &[]).is_err());
        let damaged = OPENSSH_SIGNATURE.replace("QN78", "QN79");
        assert!(verify(OPENSSH_MESSAGE, &damaged, &key()).is_err());
        assert!(verify(OPENSSH_MESSAGE, "not a signature", &key()).is_err());
    }

    #[test]
    fn a_signature_for_another_purpose_is_refused() {
        let signer = SigningKey::from_bytes(&[3; 32]);
        let trusted = [TrustedKey(ed25519_blob(&signer))];
        let message = b"metadata";
        let git = sign(&signer, message, "git");
        assert!(
            verify(message, &git, &trusted)
                .unwrap_err()
                .to_string()
                .contains("not a cagents release signature")
        );
        verify(message, &sign(&signer, message, NAMESPACE), &trusted).unwrap();
    }

    #[test]
    fn a_security_key_signature_needs_the_touch_it_claims() {
        let signer = SigningKey::from_bytes(&[5; 32]);
        let application = b"ssh:";
        let mut public = Vec::new();
        put_string(&mut public, SK_ED25519);
        put_string(&mut public, signer.verifying_key().as_bytes());
        put_string(&mut public, application);
        let trusted = [TrustedKey(public.clone())];
        let message = b"metadata";
        for (flags, accepted) in [(USER_PRESENT, true), (0, false)] {
            let signed = signed_data(message, NAMESPACE);
            let mut assertion = Sha256::digest(application).to_vec();
            assertion.push(flags);
            assertion.extend_from_slice(&9u32.to_be_bytes());
            assertion.extend_from_slice(&Sha256::digest(&signed));
            let mut signature = Vec::new();
            put_string(&mut signature, SK_ED25519);
            put_string(&mut signature, &signer.sign(&assertion).to_bytes());
            signature.push(flags);
            signature.extend_from_slice(&9u32.to_be_bytes());
            let armored = armor(&public, NAMESPACE, &signature);
            assert_eq!(verify(message, &armored, &trusted).is_ok(), accepted);
        }
        let line = format!("sk-ssh-ed25519@openssh.com {}", STANDARD.encode(&public));
        assert_eq!(TrustedKey::parse(&line).unwrap(), trusted[0]);
        let mislabeled = format!("ssh-ed25519 {}", STANDARD.encode(&public));
        assert!(TrustedKey::parse(&mislabeled).is_err());
    }

    #[test]
    fn the_shipped_key_list_parses() {
        trusted_keys().unwrap();
    }

    fn ed25519_blob(key: &SigningKey) -> Vec<u8> {
        let mut blob = Vec::new();
        put_string(&mut blob, ED25519);
        put_string(&mut blob, key.verifying_key().as_bytes());
        blob
    }

    fn signed_data(message: &[u8], namespace: &str) -> Vec<u8> {
        let mut signed = MAGIC.to_vec();
        let digest = Sha512::digest(message);
        for field in [namespace.as_bytes(), b"", b"sha512", &digest] {
            put_string(&mut signed, field);
        }
        signed
    }

    fn sign(key: &SigningKey, message: &[u8], namespace: &str) -> String {
        let mut signature = Vec::new();
        put_string(&mut signature, ED25519);
        put_string(
            &mut signature,
            &key.sign(&signed_data(message, namespace)).to_bytes(),
        );
        armor(&ed25519_blob(key), namespace, &signature)
    }

    fn armor(public: &[u8], namespace: &str, signature: &[u8]) -> String {
        let mut blob = MAGIC.to_vec();
        blob.extend_from_slice(&VERSION.to_be_bytes());
        for field in [public, namespace.as_bytes(), b"", b"sha512", signature] {
            put_string(&mut blob, field);
        }
        format!("{ARMOR_BEGIN}\n{}\n{ARMOR_END}\n", STANDARD.encode(blob))
    }
}
